//! 浏览器中枢:Playwright 驱动的会话、tab 注册表、命令执行与事件广播。
//!
//! 与旧实现(自建 headless Chrome + 自写 CDP 客户端)的区别:
//! - **进程与协议由 Playwright 负责**:driver 自带 Node,浏览器由其拉起与守护;
//! - **元素引用用 Playwright 官方 aria snapshot**:AI 模式产出 `[ref=eN]`,
//!   定位走 `aria-ref=eN` 引擎,不再往页面全局注入 `__zcodeRefs`;
//! - **tab 身份用自维护 id**:Playwright 的 Page 不暴露 guid,这里分配稳定
//!   tabId(`t1`、`t2`…),映射到 `Page` 句柄。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use playwright_rs::protocol::{
    AriaSnapshotMode, AriaSnapshotOptions, Dialog, MouseButton, MouseOptions, Page,
    ResponseObject, ScreencastStartOptions, ScreenshotClip, ScreenshotOptions, SelectOption,
    Viewport, WaitForOptions,
};
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};

use crate::driver::{self, LaunchSpec, Session};
use crate::network::NetworkLog;
use crate::{BrowserCommand, CommandOutcome};

/// tab 上限:防止模型无限开页把浏览器撑爆。
const TAB_LIMIT: usize = 32;

/// 挂起的 JS dialog:给模型/面板看的信息 + 处置句柄。
struct PendingDialog {
    info: Value,
    handle: Dialog,
}

/// 单个 tab 的运行时状态。
#[derive(Debug, Clone)]
pub struct TabInfo {
    pub tab_id: String,
    pub url: String,
    pub title: String,
    /// 最后活跃时刻(epoch ms):命令执行/激活都会刷新。
    pub last_activity_ms: u64,
}

/// 浏览器事件(SSE 推给前端)。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum BrowserEvent {
    /// tab 列表/活跃 tab 变化。
    TabsChanged,
    /// 某个 tab 的新画面帧(页面 base64 JPEG)。
    Frame {
        #[serde(rename = "tabId")]
        tab_id: String,
        data: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        viewport: Option<FrameViewport>,
    },
    /// tab 出现 JS dialog。
    DialogOpened {
        #[serde(rename = "tabId")]
        tab_id: String,
        kind: String,
        message: String,
    },
    /// dialog 被处理。
    DialogClosed {
        #[serde(rename = "tabId")]
        tab_id: String,
    },
    /// tab 导航完成(刷新地址栏)。
    Navigated {
        #[serde(rename = "tabId")]
        tab_id: String,
        url: String,
        title: String,
    },
    /// 模型请求打开可视化模式:前端展开浏览器侧栏。
    VisualModeRequested,
    /// 浏览器实例退出。
    Exited,
}

/// screencast 帧附带的页面视口尺寸(CSS 像素)。
#[derive(Debug, Clone, serde::Serialize)]
pub struct FrameViewport {
    pub width: u32,
    pub height: u32,
}

/// 活跃会话:driver + 上下文 + tab 注册表。
struct Inner {
    session: Session,
    /// tabId -> Page 句柄。
    pages: HashMap<String, Page>,
    /// tabId -> 元信息。
    tabs: HashMap<String, TabInfo>,
    active_tab: Option<String>,
    /// tabId 分配计数器(单调,不回收,避免旧引用误命中新 tab)。
    next_tab_seq: u64,
    /// 当前 screencast 归属的 tab(单实例单画面;切 tab 时迁移)。
    screencast_tab: Option<String>,
}

impl Inner {
    fn alloc_tab_id(&mut self) -> String {
        self.next_tab_seq += 1;
        format!("t{}", self.next_tab_seq)
    }

    /// 登记一个 Page。
    fn register(&mut self, page: Page, url: String, title: String) -> String {
        let tab_id = self.alloc_tab_id();
        self.pages.insert(tab_id.clone(), page);
        self.tabs.insert(
            tab_id.clone(),
            TabInfo {
                tab_id: tab_id.clone(),
                url,
                title,
                last_activity_ms: now_ms(),
            },
        );
        tab_id
    }

    /// 从 tabId 取 Page。
    fn page(&self, tab_id: &str) -> Result<Page, String> {
        self.pages
            .get(tab_id)
            .cloned()
            .ok_or_else(|| format!("tab 不存在或已关闭: {tab_id}(用 tabs 查看当前标签页)"))
    }

    /// 当前活跃 tab(缺省落到第一个)。
    fn active(&self) -> Result<(String, Page), String> {
        if let Some(tab_id) = &self.active_tab
            && let Some(page) = self.pages.get(tab_id)
        {
            return Ok((tab_id.clone(), page.clone()));
        }
        let tab_id = self
            .tabs
            .keys()
            .next()
            .cloned()
            .ok_or_else(|| "没有可用的标签页(先 open 一个页面)".to_string())?;
        let page = self.page(&tab_id)?;
        Ok((tab_id, page))
    }

    /// 解析目标 tab:显式 tab_id 优先,否则用活跃 tab。
    fn resolve(&self, tab_id: Option<&str>) -> Result<(String, Page), String> {
        match tab_id {
            Some(id) => Ok((id.to_string(), self.page(id)?)),
            None => self.active(),
        }
    }

    /// 移除 tab。
    fn unregister(&mut self, tab_id: &str) {
        self.pages.remove(tab_id);
        self.tabs.remove(tab_id);
        if self.active_tab.as_deref() == Some(tab_id) {
            self.active_tab = self.tabs.keys().next().cloned();
        }
    }

    /// 刷新某 tab 的 url/title。
    async fn refresh_meta(&mut self, tab_id: &str) {
        let Some(page) = self.pages.get(tab_id).cloned() else {
            return;
        };
        let url = page.url();
        let title = page.title().await.unwrap_or_default();
        if let Some(info) = self.tabs.get_mut(tab_id) {
            info.url = url;
            info.title = title;
            info.last_activity_ms = now_ms();
        }
    }

    /// tab 列表(带 url/title)。
    fn tab_list(&self) -> Vec<Value> {
        self.tabs
            .values()
            .map(|info| {
                json!({
                    "tabId": info.tab_id,
                    "url": info.url,
                    "title": info.title,
                    "active": self.active_tab.as_deref() == Some(info.tab_id.as_str()),
                    "lastActivityMs": info.last_activity_ms,
                })
            })
            .collect()
    }
}

/// 浏览器中枢:server 持有,工具与 REST API 共用。
pub struct BrowserManager {
    /// 活跃会话(Arc 包一层:事件闭包需独立持有)。
    inner: Arc<Mutex<Option<Inner>>>,
    /// 启动互斥:并发命令同时发现未启动时,只放一个进去拉起。
    starting: Mutex<()>,
    /// 串行化命令生命周期,避免并发命令交叉使用已失效的 Page。
    command_gate: Mutex<()>,
    generation: AtomicU64,
    event_broadcast: broadcast::Sender<BrowserEvent>,
    /// 挂起 JS dialog(tabId -> {info, handle}):handle 用于 accept/dismiss。
    pending_dialogs: Arc<std::sync::Mutex<HashMap<String, PendingDialog>>>,
    /// 网络抓包缓冲。
    network_log: Arc<std::sync::Mutex<NetworkLog>>,
    /// 响应体句柄(合成 requestId -> (ResponseObject, content-type)):
    /// networkGetBody 取响应体用。
    bodies: Arc<Mutex<HashMap<String, (ResponseObject, String)>>>,
    /// (tabId, url) -> 最近一次请求的合成 id:on_response 与 on_request 对齐用。
    inflight: Arc<std::sync::Mutex<HashMap<(String, String), String>>>,
    /// 合成 requestId 计数器。
    request_seq: Arc<AtomicU64>,
    /// 启动参数。
    spec: LaunchSpec,
    /// 用户数据目录(profile 落盘位置)。
    home: PathBuf,
}

impl BrowserManager {
    pub fn new(home: PathBuf) -> Self {
        let (event_broadcast, _) = broadcast::channel(256);
        let spec = LaunchSpec::new(&home);
        Self {
            inner: Arc::new(Mutex::new(None)),
            starting: Mutex::new(()),
            command_gate: Mutex::new(()),
            generation: AtomicU64::new(0),
            event_broadcast,
            pending_dialogs: Arc::new(std::sync::Mutex::new(HashMap::new())),
            network_log: Arc::new(std::sync::Mutex::new(NetworkLog::default())),
            bodies: Arc::new(Mutex::new(HashMap::new())),
            inflight: Arc::new(std::sync::Mutex::new(HashMap::new())),
            request_seq: Arc::new(AtomicU64::new(0)),
            spec,
            home,
        }
    }

    /// 订阅浏览器事件。
    pub fn subscribe(&self) -> broadcast::Receiver<BrowserEvent> {
        self.event_broadcast.subscribe()
    }

    /// 广播一条事件。
    pub fn broadcast(&self, event: BrowserEvent) {
        let _ = self.event_broadcast.send(event);
    }

    /// 查看某 tab 的挂起 dialog(不取走;handleDialog 才消费)。
    pub fn peek_dialog(&self, tab_id: &str) -> Option<Value> {
        let guard = self.pending_dialogs.lock().ok()?;
        guard.get(tab_id).map(|pending| pending.info.clone())
    }

    /// 取走并清空某 tab 的挂起 dialog(信息 + 处置句柄)。
    pub fn take_dialog_handle(&self, tab_id: &str) -> Option<(Value, Dialog)> {
        self.pending_dialogs
            .lock()
            .ok()
            .and_then(|mut map| map.remove(tab_id))
            .map(|pending| (pending.info, pending.handle))
    }

    /// 清掉某 tab 的挂起 dialog。
    pub fn clear_dialog(&self, tab_id: &str) {
        if let Ok(mut map) = self.pending_dialogs.lock() {
            map.remove(tab_id);
        }
    }

    /// 网络抓包列表。
    pub fn network_list(&self, tab_id: &str, url_filter: Option<&str>, max: u32) -> Vec<Value> {
        self.network_log
            .lock()
            .map(|log| log.list(tab_id, url_filter, max))
            .unwrap_or_default()
    }

    /// 单条网络请求详情。
    pub fn network_entry(&self, tab_id: &str, request_id: &str) -> Option<Value> {
        self.network_log
            .lock()
            .ok()
            .and_then(|log| log.entry(tab_id, request_id))
    }

    /// 清空某 tab 的网络缓冲。
    pub fn network_clear(&self, tab_id: &str) {
        if let Ok(mut log) = self.network_log.lock() {
            log.clear(tab_id);
        }
    }

    /// 用户数据目录。
    pub fn home(&self) -> PathBuf {
        self.home.clone()
    }

    /// 可视化模式请求(前端展开侧栏)。
    pub fn request_visual_mode(&self) {
        self.broadcast(BrowserEvent::VisualModeRequested);
    }

    /// 状态快照(前端轮询用)。
    pub async fn snapshot_state(&self) -> Value {
        let guard = self.inner.lock().await;
        match guard.as_ref() {
            Some(inner) => json!({
                "running": true,
                "activeTabId": inner.active_tab,
                "tabs": inner.tab_list(),
                "dialogs": self
                    .pending_dialogs
                    .lock()
                    .map(|map| map
                        .iter()
                        .map(|(k, v)| json!({ "tabId": k, "dialog": v.info }))
                        .collect::<Vec<_>>())
                    .unwrap_or_default(),
            }),
            None => json!({
                "running": false,
                "activeTabId": Value::Null,
                "tabs": [],
                "dialogs": [],
            }),
        }
    }

    /// 确保浏览器已启动(不重复拉起)。
    pub async fn ensure_running(&self) -> Result<(), String> {
        self.get_or_start().await
    }

    /// 取现有会话或拉起新会话。
    async fn get_or_start(&self) -> Result<(), String> {
        if self.inner.lock().await.is_some() {
            return Ok(());
        }
        let _starting = self.starting.lock().await;
        if self.inner.lock().await.is_some() {
            return Ok(());
        }
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let session = driver::launch(&self.spec, generation).await?;
        let mut inner = Inner {
            session,
            pages: HashMap::new(),
            tabs: HashMap::new(),
            active_tab: None,
            next_tab_seq: 0,
            screencast_tab: None,
        };
        // 持久化 profile 会恢复上次的标签页;把它们纳入注册表。
        for page in inner.session.context.pages() {
            let url = page.url();
            if url.is_empty() || url == "about:blank" {
                continue;
            }
            let title = page.title().await.unwrap_or_default();
            let tab_id = inner.register(page, url, title);
            if inner.active_tab.is_none() {
                inner.active_tab = Some(tab_id);
            }
        }
        // 没有任何可用页面时开一个空白页,保证模型有落点。
        if inner.pages.is_empty() {
            let page = inner
                .session
                .context
                .new_page()
                .await
                .map_err(|error| format!("创建初始标签页失败: {error}"))?;
            let tab_id = inner.register(page, "about:blank".to_string(), String::new());
            inner.active_tab = Some(tab_id);
        }
        // 给每个 Page 挂监听(导航/控制台/dialog/网络/关闭)。
        let tab_ids: Vec<String> = inner.tabs.keys().cloned().collect();
        for tab_id in tab_ids {
            if let Some(page) = inner.pages.get(&tab_id).cloned() {
                self.wire_page(tab_id, page);
            }
        }
        // 浏览器被用户关掉/崩溃:清空会话,下次命令自动重新拉起。
        // 不注册这个的话,`inner` 会一直指向死会话,所有命令都报
        // TargetClosed 且永远不自愈。
        let context = inner.session.context.clone();
        let session_generation = inner.session.generation;
        let inner_slot = self.inner.clone();
        let broadcast = self.event_broadcast.clone();
        tokio::spawn(async move {
            let _ = context
                .on_close(move || {
                    let inner_slot = inner_slot.clone();
                    let broadcast = broadcast.clone();
                    async move {
                        let mut slot = inner_slot.lock().await;
                        // 只清理本代会话(避免把刚拉起的新会话误清)。
                        let same_generation = slot
                            .as_ref()
                            .map(|current| current.session.generation == session_generation)
                            .unwrap_or(false);
                        if same_generation {
                            *slot = None;
                            drop(slot);
                            let _ = broadcast.send(BrowserEvent::Exited);
                        }
                        Ok(())
                    }
                })
                .await;
        });
        *self.inner.lock().await = Some(inner);
        self.broadcast(BrowserEvent::TabsChanged);
        Ok(())
    }

    /// 事件回注句柄(闭包捕获用;与 BrowserManager 解耦避免自引用)。
    fn event_sink(&self) -> EventSink {
        EventSink {
            broadcast: self.event_broadcast.clone(),
            dialogs: self.pending_dialogs.clone(),
            network: self.network_log.clone(),
        }
    }

    /// 给单个 Page 挂监听。
    fn wire_page(&self, tab_id: String, page: Page) {
        let sink = self.event_sink();

        // screencast 帧回调:每个 Page 只注册一次。
        // playwright-rs 的 `on_frame` 只支持追加(没有反注册 API),而前端
        // 会在挂载/重连/切 tab 时反复调 startScreencast——若在那里注册,
        // 帧会被重复推送 N 次。这里一次性注册,start/stop 只控制开关。
        let frame_broadcast = self.event_broadcast.clone();
        let frame_tab = tab_id.clone();
        let frame_page = page.clone();
        page.screencast().on_frame(move |frame| {
            let broadcast = frame_broadcast.clone();
            let tab = frame_tab.clone();
            let viewport = frame_page.viewport_size().map(|size| FrameViewport {
                width: size.width,
                height: size.height,
            });
            async move {
                use base64::Engine;
                let data = base64::engine::general_purpose::STANDARD.encode(&frame.data);
                let _ = broadcast.send(BrowserEvent::Frame {
                    tab_id: tab,
                    data,
                    viewport,
                });
                Ok(())
            }
        });

        // 导航完成。
        let nav_sink = sink.clone();
        let nav_tab = tab_id.clone();
        let nav_page = page.clone();
        tokio::spawn(async move {
            let tab = nav_tab.clone();
            let _ = nav_page
                .on_load(move || {
                    let sink = nav_sink.clone();
                    let tab = tab.clone();
                    async move {
                        sink.broadcast(BrowserEvent::Navigated {
                            tab_id: tab,
                            url: String::new(),
                            title: String::new(),
                        });
                        Ok(())
                    }
                })
                .await;
        });

        // JS dialog:挂起记录,等模型/面板处理。
        let dialog_sink = sink.clone();
        let dialog_tab = tab_id.clone();
        let dialog_page = page.clone();
        tokio::spawn(async move {
            let _ = dialog_page
                .on_dialog(move |dialog| {
                    let sink = dialog_sink.clone();
                    let tab = dialog_tab.clone();
                    async move {
                        let kind = dialog.type_().to_string();
                        let message = dialog.message().to_string();
                        sink.remember_dialog(&tab, &kind, &message, dialog.clone());
                        sink.broadcast(BrowserEvent::DialogOpened {
                            tab_id: tab,
                            kind,
                            message,
                        });
                        // 交给模型/面板处置:不自动 accept。
                        Ok(())
                    }
                })
                .await;
        });

        // 网络请求:记录进环形缓冲;requestId 用合成序号(request+response
        // 两路共用,networkGetBody 据此取响应体)。
        let net_sink = sink.clone();
        let net_tab = tab_id.clone();
        let net_page = page.clone();
        let net_inflight = self.inflight.clone();
        let net_seq = self.request_seq.clone();
        tokio::spawn(async move {
            let _ = net_page
                .on_request(move |request| {
                    let sink = net_sink.clone();
                    let tab = net_tab.clone();
                    let inflight = net_inflight.clone();
                    let seq = net_seq.clone();
                    async move {
                        let url = request.url().to_string();
                        let request_id = format!("{tab}-r{}", seq.fetch_add(1, Ordering::Relaxed));
                        if let Ok(mut map) = inflight.lock() {
                            map.insert((tab.clone(), url), request_id.clone());
                        }
                        let entry = json!({
                            "requestId": request_id,
                            "url": request.url(),
                            "method": request.method(),
                            "resourceType": request.resource_type(),
                            "postData": request.post_data(),
                            "headers": request.headers(),
                            "atMs": now_ms(),
                        });
                        sink.network_record(&tab, &request_id, entry);
                        Ok(())
                    }
                })
                .await;
        });

        // 网络响应:补记状态码/响应头,并存 Response 句柄供 networkGetBody。
        let resp_tab = tab_id.clone();
        let resp_page = page.clone();
        let resp_inflight = self.inflight.clone();
        let manager_bodies = self.bodies.clone();
        let resp_sink = sink.clone();
        tokio::spawn(async move {
            let tab = resp_tab.clone();
            let inflight = resp_inflight.clone();
            let bodies = manager_bodies.clone();
            let _ = resp_page
                .on_response(move |response| {
                    let sink = resp_sink.clone();
                    let tab = tab.clone();
                    let inflight = inflight.clone();
                    let bodies = bodies.clone();
                    async move {
                        let url = response.url().to_string();
                        let request_id = inflight
                            .lock()
                            .ok()
                            .and_then(|map| map.get(&(tab.clone(), url)).cloned());
                        let Some(request_id) = request_id else {
                            return Ok(());
                        };
                        // 元数据合并进抓包条目(status + content-type)。
                        let content_type = response
                            .raw_headers()
                            .await
                            .ok()
                            .and_then(|headers| {
                                headers
                                    .iter()
                                    .find(|entry| {
                                        entry.name.eq_ignore_ascii_case("content-type")
                                    })
                                    .map(|entry| entry.value.clone())
                            })
                            .unwrap_or_default();
                        sink.network_record(
                            &tab,
                            &request_id,
                            json!({ "status": response.status(), "contentType": content_type }),
                        );
                        // 存 Response 句柄:networkGetBody 据此取响应体。
                        // 环形语义:超上限整表丢弃(旧响应体不再可取)。
                        let mut map = bodies.lock().await;
                        if map.len() > 400 {
                            map.clear();
                        }
                        map.insert(request_id, (response.clone(), content_type));
                        Ok(())
                    }
                })
                .await;
        });

        // 页面关闭:清 dialog 并广播。
        let close_sink = sink.clone();
        let close_tab = tab_id;
        tokio::spawn(async move {
            let _ = page
                .on_close(move || {
                    let sink = close_sink.clone();
                    let tab = close_tab.clone();
                    async move {
                        sink.clear_dialog(&tab);
                        sink.broadcast(BrowserEvent::DialogClosed { tab_id: tab });
                        Ok(())
                    }
                })
                .await;
        });
    }

    /// 执行一条浏览器命令。
    pub async fn execute(&self, command: BrowserCommand) -> CommandOutcome {
        let started = Instant::now();
        // 命令串行化:Playwright 的 Page 句柄在 tab 关闭后会失效,
        // 并发命令交叉使用会报 TargetClosed。
        let _gate = self.command_gate.lock().await;
        if let Err(error) = self.get_or_start().await {
            return CommandOutcome::err("not_running", error, started.elapsed().as_millis() as u64);
        }
        // 弹窗竞速:命令执行中页面弹出 JS dialog 时,Playwright 的动作调用
        // 会被挂起的 dialog 卡到超时(30s)。这里订阅事件流,一旦 DialogOpened
        // 就提前返回弹窗信息,模型用 getDialog/handleDialog 处置——对齐旧版
        // "命令即时返回、弹窗挂起等待处理"的语义。广播是多消费者,这里
        // 消费不影响 SSE 前端;非 DialogOpened 事件(导航/tab 变化)继续等。
        let mut event_rx = self.event_broadcast.subscribe();
        let outcome = tokio::select! {
            result = self.dispatch(command) => match result {
                Ok(outcome) => outcome,
                Err(error) => CommandOutcome::err(
                    error_code(&error),
                    error,
                    started.elapsed().as_millis() as u64,
                ),
            },
            dialog = wait_dialog_opened(&mut event_rx) => match dialog {
                Some((tab_id, kind, message)) => CommandOutcome::ok_value(
                    json!({
                        "dialogOpened": {
                            "tabId": tab_id,
                            "type": kind,
                            "message": message,
                        }
                    }),
                    started.elapsed().as_millis() as u64,
                ),
                None => {
                    CommandOutcome::err("command_failed", "事件流中断", started.elapsed().as_millis() as u64)
                }
            },
        };
        CommandOutcome {
            elapsed_ms: started.elapsed().as_millis() as u64,
            ..outcome
        }
    }

    /// 命令分发(已确保会话就绪)。
    async fn dispatch(&self, command: BrowserCommand) -> Result<CommandOutcome, String> {
        let mut guard = self.inner.lock().await;
        let inner = guard
            .as_mut()
            .ok_or_else(|| "浏览器未启动".to_string())?;

        match command {
            BrowserCommand::GetState { tab_id } => {
                let _ = tab_id;
                Ok(CommandOutcome::ok_value(
                    json!({
                        "activeTabId": inner.active_tab,
                        "tabs": inner.tab_list(),
                    }),
                    0,
                ))
            }
            BrowserCommand::List => Ok(CommandOutcome::ok_value(
                json!({ "tabs": inner.tab_list() }),
                0,
            )),
            BrowserCommand::NewTab { url } => {
                if inner.pages.len() >= TAB_LIMIT {
                    return Err(format!("标签页已达上限({TAB_LIMIT});先关闭不用的标签页"));
                }
                let page = inner
                    .session
                    .context
                    .new_page()
                    .await
                    .map_err(|error| format!("新建标签页失败: {error}"))?;
                let target = url.unwrap_or_else(|| "about:blank".to_string());
                let mut title = String::new();
                if target != "about:blank" {
                    page.goto(&target, None)
                        .await
                        .map_err(|error| format!("导航失败: {error}"))?;
                    title = page.title().await.unwrap_or_default();
                }
                let tab_id = inner.register(page.clone(), target.clone(), title.clone());
                inner.active_tab = Some(tab_id.clone());
                drop(guard);
                self.wire_page(tab_id.clone(), page);
                self.broadcast(BrowserEvent::TabsChanged);
                Ok(CommandOutcome::ok_value(
                    json!({ "tabId": tab_id, "url": target, "title": title }),
                    0,
                ))
            }
            BrowserCommand::Close { tab_id } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                // 关掉的正是被投屏的 tab:停流,免得前端停在最后一帧。
                if inner.screencast_tab.as_deref() == Some(tab_id.as_str()) {
                    let _ = page.screencast().stop().await;
                    inner.screencast_tab = None;
                }
                page.close()
                    .await
                    .map_err(|error| format!("关闭标签页失败: {error}"))?;
                inner.unregister(&tab_id);
                drop(guard);
                self.broadcast(BrowserEvent::TabsChanged);
                Ok(CommandOutcome::ok_value(json!({ "closed": tab_id }), 0))
            }
            BrowserCommand::Activate { tab_id } => {
                if !inner.pages.contains_key(&tab_id) {
                    return Err(format!("tab 不存在: {tab_id}"));
                }
                inner.active_tab = Some(tab_id.clone());
                let page = inner.page(&tab_id)?;
                let _ = page.bring_to_front().await;
                drop(guard);
                self.broadcast(BrowserEvent::TabsChanged);
                Ok(CommandOutcome::ok_value(json!({ "activeTabId": tab_id }), 0))
            }
            BrowserCommand::Navigate { url, tab_id } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let response = page
                    .goto(&url, None)
                    .await
                    .map_err(|error| format!("导航失败: {error}"))?;
                inner.refresh_meta(&tab_id).await;
                let status = response.as_ref().map(|r| r.status());
                drop(guard);
                self.broadcast(BrowserEvent::Navigated {
                    tab_id: tab_id.clone(),
                    url: url.clone(),
                    title: String::new(),
                });
                Ok(CommandOutcome::ok_value(
                    json!({ "url": url, "status": status }),
                    0,
                ))
            }
            BrowserCommand::Back { tab_id } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let response = page
                    .go_back(None)
                    .await
                    .map_err(|error| format!("后退失败: {error}"))?;
                inner.refresh_meta(&tab_id).await;
                let url = page.url();
                Ok(CommandOutcome::ok_value(
                    json!({ "url": url, "status": response.as_ref().map(|r| r.status()) }),
                    0,
                ))
            }
            BrowserCommand::Forward { tab_id } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let response = page
                    .go_forward(None)
                    .await
                    .map_err(|error| format!("前进失败: {error}"))?;
                inner.refresh_meta(&tab_id).await;
                let url = page.url();
                Ok(CommandOutcome::ok_value(
                    json!({ "url": url, "status": response.as_ref().map(|r| r.status()) }),
                    0,
                ))
            }
            BrowserCommand::Reload { tab_id } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let response = page
                    .reload(None)
                    .await
                    .map_err(|error| format!("刷新失败: {error}"))?;
                inner.refresh_meta(&tab_id).await;
                let url = page.url();
                Ok(CommandOutcome::ok_value(
                    json!({ "url": url, "status": response.as_ref().map(|r| r.status()) }),
                    0,
                ))
            }
            BrowserCommand::Snapshot {
                tab_id,
                max_elements,
                include_hidden,
            } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let _ = include_hidden;
                let options = AriaSnapshotOptions::default().mode(AriaSnapshotMode::Ai);
                let mut snapshot = page
                    .aria_snapshot(Some(options))
                    .await
                    .map_err(|error| format!("生成页面快照失败: {error}"))?;
                // 按元素数截断(每个 `[ref=` 是一个可交互元素)。
                let mut truncated = false;
                if let Some(max) = max_elements {
                    let mut seen = 0usize;
                    let mut cut: Option<usize> = None;
                    for (idx, _) in snapshot.match_indices("[ref=") {
                        seen += 1;
                        if seen > max as usize {
                            cut = Some(idx);
                            break;
                        }
                    }
                    if let Some(idx) = cut {
                        snapshot.truncate(idx);
                        truncated = true;
                    }
                }
                inner.refresh_meta(&tab_id).await;
                let url = page.url();
                let title = page.title().await.unwrap_or_default();
                Ok(CommandOutcome::ok_value(
                    json!({
                        "url": url,
                        "title": title,
                        "elements": snapshot,
                        "truncated": truncated,
                    }),
                    0,
                ))
            }
            BrowserCommand::Screenshot {
                tab_id,
                full_page,
                clip,
            } => {
                let (_tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let mut builder = ScreenshotOptions::builder()
                    .full_page(full_page.unwrap_or(false));
                if let Some(rect) = clip {
                    builder = builder.clip(ScreenshotClip {
                        x: rect.x,
                        y: rect.y,
                        width: rect.width,
                        height: rect.height,
                    });
                }
                let bytes = page
                    .screenshot(Some(builder.build()))
                    .await
                    .map_err(|error| format!("截图失败: {error}"))?;
                use base64::Engine;
                let base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                Ok(CommandOutcome {
                    ok: true,
                    error: None,
                    value: None,
                    state: None,
                    image: Some(crate::ImagePayload {
                        base64,
                        mime_type: "image/png".to_string(),
                    }),
                    snapshot: None,
                    tabs: None,
                    dialog: None,
                    elapsed_ms: 0,
                })
            }
            BrowserCommand::Click {
                tab_id,
                r#ref,
                locator,
                x,
                y,
                button,
                double_click,
            } => {
                let (_tab_id, page) = inner.resolve(tab_id.as_deref())?;
                match pick_target(r#ref.as_deref(), locator.as_deref()) {
                    Some(target) => {
                        let options = playwright_rs::protocol::ClickOptions::builder()
                            .button(parse_button(button.as_deref()))
                            .click_count(if double_click.unwrap_or(false) { 2 } else { 1 })
                            .build();
                        page.locator(&target)
                            .click(Some(options))
                            .await
                            .map_err(|error| format!("点击失败({target}): {error}"))?;
                        Ok(CommandOutcome::ok_value(json!({ "clicked": target }), 0))
                    }
                    None => {
                        let x = x.ok_or_else(|| "click 需要 ref/locator 或 x,y".to_string())?;
                        let y = y.ok_or_else(|| "click 需要 ref/locator 或 x,y".to_string())?;
                        page.mouse()
                            .click(x, y, None)
                            .await
                            .map_err(|error| format!("点击失败({x},{y}): {error}"))?;
                        Ok(CommandOutcome::ok_value(json!({ "clickedAt": [x, y] }), 0))
                    }
                }
            }
            BrowserCommand::Fill { tab_id, r#ref, value } => {
                let (_tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let target = normalize_target(&r#ref);
                page.locator(&target)
                    .fill(&value, None)
                    .await
                    .map_err(|error| format!("填值失败({target}): {error}"))?;
                Ok(CommandOutcome::ok_value(json!({ "filled": target }), 0))
            }
            BrowserCommand::Type {
                tab_id,
                r#ref,
                text,
            } => {
                let (_tab_id, page) = inner.resolve(tab_id.as_deref())?;
                match r#ref {
                    Some(target) => {
                        let target = normalize_target(&target);
                        page.locator(&target)
                            .press_sequentially(&text, None)
                            .await
                            .map_err(|error| format!("输入失败({target}): {error}"))?;
                    }
                    None => {
                        page.keyboard()
                            .type_text(&text, None)
                            .await
                            .map_err(|error| format!("输入失败: {error}"))?;
                    }
                }
                Ok(CommandOutcome::ok_value(json!({ "typed": text.len() }), 0))
            }
            BrowserCommand::Press { tab_id, key, r#ref } => {
                let (_tab_id, page) = inner.resolve(tab_id.as_deref())?;
                match r#ref {
                    Some(target) => {
                        let target = normalize_target(&target);
                        page.locator(&target)
                            .press(&key, None)
                            .await
                            .map_err(|error| format!("按键失败({target}): {error}"))?;
                    }
                    None => {
                        page.keyboard()
                            .press(&key, None)
                            .await
                            .map_err(|error| format!("按键失败: {error}"))?;
                    }
                }
                Ok(CommandOutcome::ok_value(json!({ "pressed": key }), 0))
            }
            BrowserCommand::Hover { tab_id, r#ref, x, y } => {
                let (_tab_id, page) = inner.resolve(tab_id.as_deref())?;
                match (r#ref, x, y) {
                    (Some(target), _, _) => {
                        let target = normalize_target(&target);
                        page.locator(&target)
                            .hover(None)
                            .await
                            .map_err(|error| format!("悬停失败({target}): {error}"))?;
                        Ok(CommandOutcome::ok_value(json!({ "hovered": target }), 0))
                    }
                    (None, Some(x), Some(y)) => {
                        page.mouse()
                            .move_to(x, y, None)
                            .await
                            .map_err(|error| format!("悬停失败({x},{y}): {error}"))?;
                        Ok(CommandOutcome::ok_value(json!({ "hoveredAt": [x, y] }), 0))
                    }
                    _ => Err("hover 需要 ref/locator 或 x,y".to_string()),
                }
            }
            BrowserCommand::Scroll {
                tab_id,
                r#ref,
                x,
                y,
                delta_x,
                delta_y,
            } => {
                let (_tab_id, page) = inner.resolve(tab_id.as_deref())?;
                match r#ref {
                    Some(target) => {
                        let target = normalize_target(&target);
                        page.locator(&target)
                            .scroll_into_view_if_needed()
                            .await
                            .map_err(|error| format!("滚动到元素失败({target}): {error}"))?;
                        Ok(CommandOutcome::ok_value(json!({ "scrolledTo": target }), 0))
                    }
                    None => {
                        let dx = delta_x.unwrap_or(0.0);
                        let dy = delta_y.unwrap_or(360.0);
                        if let (Some(x), Some(y)) = (x, y) {
                            page.mouse()
                                .move_to(x, y, None)
                                .await
                                .map_err(|error| format!("移动鼠标失败: {error}"))?;
                        }
                        page.mouse()
                            .wheel(dx, dy)
                            .await
                            .map_err(|error| format!("滚动失败: {error}"))?;
                        Ok(CommandOutcome::ok_value(json!({ "scrolled": [dx, dy] }), 0))
                    }
                }
            }
            BrowserCommand::Select { tab_id, r#ref, values } => {
                let (_tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let target = normalize_target(&r#ref);
                let locator = page.locator(&target);
                let mut selected = Vec::new();
                for value in &values {
                    let picked = locator
                        .select_option(SelectOption::Value(value.clone()), None)
                        .await
                        .map_err(|error| format!("选择失败({target}): {error}"))?;
                    selected.extend(picked);
                }
                Ok(CommandOutcome::ok_value(json!({ "selected": selected }), 0))
            }
            BrowserCommand::Check {
                tab_id,
                r#ref,
                checked,
            } => {
                let (_tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let target = normalize_target(&r#ref);
                let loc = page.locator(&target);
                if checked.unwrap_or(true) {
                    loc.check(None)
                        .await
                        .map_err(|error| format!("勾选失败({target}): {error}"))?;
                } else {
                    loc.uncheck(None)
                        .await
                        .map_err(|error| format!("取消勾选失败({target}): {error}"))?;
                }
                Ok(CommandOutcome::ok_value(json!({ "checked": target }), 0))
            }
            BrowserCommand::Drag {
                tab_id,
                from_ref,
                to_ref,
                from,
                to,
            } => {
                let (_tab_id, page) = inner.resolve(tab_id.as_deref())?;
                match (from_ref, to_ref) {
                    (Some(from_ref), Some(to_ref)) => {
                        let from_target = normalize_target(&from_ref);
                        let to_target = normalize_target(&to_ref);
                        page.locator(&from_target)
                            .drag_to(&page.locator(&to_target), None)
                            .await
                            .map_err(|error| format!("拖拽失败: {error}"))?;
                        Ok(CommandOutcome::ok_value(
                            json!({ "dragged": [from_target, to_target] }),
                            0,
                        ))
                    }
                    _ => {
                        let from =
                            from.ok_or_else(|| "drag 需要 fromRef/toRef 或 from/to".to_string())?;
                        let to =
                            to.ok_or_else(|| "drag 需要 fromRef/toRef 或 from/to".to_string())?;
                        let mouse = page.mouse();
                        mouse
                            .move_to(from.x, from.y, None)
                            .await
                            .map_err(|error| format!("拖拽失败: {error}"))?;
                        mouse
                            .down(None)
                            .await
                            .map_err(|error| format!("拖拽失败: {error}"))?;
                        mouse
                            .move_to(to.x, to.y, None)
                            .await
                            .map_err(|error| format!("拖拽失败: {error}"))?;
                        mouse
                            .up(None)
                            .await
                            .map_err(|error| format!("拖拽失败: {error}"))?;
                        Ok(CommandOutcome::ok_value(json!({ "dragged": true }), 0))
                    }
                }
            }
            BrowserCommand::ElementInfo { tab_id, x, y } => {
                let (_tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let info: Value = page
                    .evaluate(
                        &format!(
                            "() => {{ const el = document.elementFromPoint({x}, {y}); \
                             if (!el) return null; const r = el.getBoundingClientRect(); \
                             return {{ tag: el.tagName, id: el.id, cls: el.className, \
                             text: (el.innerText||'').slice(0,200), \
                             rect: {{x:r.x,y:r.y,width:r.width,height:r.height}} }}; }}"
                        ),
                        None::<&()>,
                    )
                    .await
                    .map_err(|error| format!("取元素信息失败: {error}"))?;
                Ok(CommandOutcome::ok_value(info, 0))
            }
            BrowserCommand::Evaluate { tab_id, expression } => {
                let (_tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let result: Value = page
                    .evaluate(&expression, None::<&()>)
                    .await
                    .map_err(|error| format!("求值失败: {error}"))?;
                Ok(CommandOutcome::ok_value(result, 0))
            }
            BrowserCommand::WaitFor {
                tab_id,
                selector,
                text,
                text_gone,
                timeout_ms,
            } => {
                let (_tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let timeout = timeout_ms.unwrap_or(30_000) as f64;
                let options = WaitForOptions::builder().timeout(timeout).build();
                if let Some(selector) = selector {
                    page.locator(&selector)
                        .wait_for(Some(options))
                        .await
                        .map_err(|error| format!("等待选择器失败({selector}): {error}"))?;
                    Ok(CommandOutcome::ok_value(json!({ "found": selector }), 0))
                } else if let Some(text) = text {
                    let escaped = escape_js_string(&text);
                    let script = format!(
                        "() => document.body && document.body.innerText.includes('{escaped}')"
                    );
                    page.wait_for_function(
                        &script,
                        Some(
                            playwright_rs::protocol::WaitForFunctionOptions::default()
                                .timeout(timeout),
                        ),
                    )
                    .await
                    .map_err(|error| format!("等待文本失败({text}): {error}"))?;
                    Ok(CommandOutcome::ok_value(json!({ "foundText": text }), 0))
                } else if let Some(text) = text_gone {
                    let escaped = escape_js_string(&text);
                    let script = format!(
                        "() => !(document.body && document.body.innerText.includes('{escaped}'))"
                    );
                    page.wait_for_function(
                        &script,
                        Some(
                            playwright_rs::protocol::WaitForFunctionOptions::default()
                                .timeout(timeout),
                        ),
                    )
                    .await
                    .map_err(|error| format!("等待文本消失失败({text}): {error}"))?;
                    Ok(CommandOutcome::ok_value(json!({ "textGone": text }), 0))
                } else {
                    Err("waitFor 需要 selector / text / textGone 之一".to_string())
                }
            }
            BrowserCommand::GetDialog { tab_id } => {
                let (tab_id, _page) = inner.resolve(tab_id.as_deref())?;
                // 只查看不消费:让模型可以先 getDialog 再 handleDialog。
                let dialog = self.peek_dialog(&tab_id).unwrap_or(Value::Null);
                Ok(CommandOutcome {
                    ok: true,
                    error: None,
                    value: None,
                    state: None,
                    image: None,
                    snapshot: None,
                    tabs: None,
                    dialog: Some(dialog),
                    elapsed_ms: 0,
                })
            }
            BrowserCommand::HandleDialog {
                tab_id,
                accept,
                prompt_text,
            } => {
                let (tab_id, _page) = inner.resolve(tab_id.as_deref())?;
                // 用 dialog 事件带回的句柄处置:新建 CDP 会话看不到挂在
                // Playwright 自己会话上的弹窗("No dialog is showing")。
                let Some((_info, handle)) = self.take_dialog_handle(&tab_id) else {
                    return Ok(CommandOutcome::err(
                        "not_found",
                        format!("tab {tab_id} 没有挂起的弹窗(先 getDialog 确认)"),
                        0,
                    ));
                };
                if accept {
                    handle
                        .accept(prompt_text.as_deref())
                        .await
                        .map_err(|error| format!("接受弹窗失败: {error}"))?;
                } else {
                    handle
                        .dismiss()
                        .await
                        .map_err(|error| format!("关闭弹窗失败: {error}"))?;
                }
                drop(guard);
                self.broadcast(BrowserEvent::DialogClosed { tab_id });
                Ok(CommandOutcome::ok_value(json!({ "accepted": accept }), 0))
            }
            BrowserCommand::ViewportSet {
                tab_id,
                width,
                height,
            } => {
                let (_tab_id, page) = inner.resolve(tab_id.as_deref())?;
                page.set_viewport_size(Viewport { width, height })
                    .await
                    .map_err(|error| format!("设置视口失败: {error}"))?;
                Ok(CommandOutcome::ok_value(json!({ "width": width, "height": height }), 0))
            }
            BrowserCommand::ViewportReset { tab_id } => {
                let (_tab_id, page) = inner.resolve(tab_id.as_deref())?;
                page.set_viewport_size(Viewport {
                    width: self.spec.viewport.0,
                    height: self.spec.viewport.1,
                })
                .await
                .map_err(|error| format!("重置视口失败: {error}"))?;
                Ok(CommandOutcome::ok_value(
                    json!({ "width": self.spec.viewport.0, "height": self.spec.viewport.1 }),
                    0,
                ))
            }
            BrowserCommand::NetworkList {
                tab_id,
                url_filter,
                max,
            } => {
                let (tab_id, _page) = inner.resolve(tab_id.as_deref())?;
                let entries = self.network_list(&tab_id, url_filter.as_deref(), max.unwrap_or(100));
                Ok(CommandOutcome::ok_value(json!({ "requests": entries }), 0))
            }
            BrowserCommand::NetworkGetBody {
                tab_id,
                request_id,
                body_kind,
            } => {
                let (tab_id, _page) = inner.resolve(tab_id.as_deref())?;
                let _ = tab_id;
                let kind = body_kind.unwrap_or_else(|| "response".to_string());
                if kind == "request" {
                    let post_data = self
                        .network_entry(&request_id.split("-r").next().unwrap_or(""), &request_id)
                        .and_then(|value| value.get("postData").cloned())
                        .unwrap_or(Value::Null);
                    return Ok(CommandOutcome::ok_value(
                        json!({ "requestId": request_id, "body": post_data }),
                        0,
                    ));
                }
                // 响应体:取开流时缓存的 Response 句柄直读。
                let stored = self.bodies.lock().await.get(&request_id).cloned();
                let Some((response, content_type)) = stored else {
                    return Ok(CommandOutcome::err(
                        "not_found",
                        format!("找不到请求 {request_id} 的响应体(可能已被清理或请求尚未完成)"),
                        0,
                    ));
                };
                let bytes = response
                    .body()
                    .await
                    .map_err(|error| format!("取响应体失败: {error}"))?;
                // 文本判定:声明为文本类 content-type,或未知类型但字节是
                // 合法 UTF-8(嗅探;JSON/HTML/文本都覆盖)。其余按二进制 base64。
                let lower = content_type.to_lowercase();
                let is_text = lower.contains("text/")
                    || lower.contains("json")
                    || lower.contains("javascript")
                    || lower.contains("xml")
                    || lower.contains("x-www-form-urlencoded")
                    || (lower.is_empty() && std::str::from_utf8(&bytes).is_ok());
                let (body, base64_encoded) = if is_text {
                    (
                        Value::String(String::from_utf8_lossy(&bytes).into_owned()),
                        false,
                    )
                } else {
                    use base64::Engine;
                    (
                        Value::String(base64::engine::general_purpose::STANDARD.encode(&bytes)),
                        true,
                    )
                };
                Ok(CommandOutcome::ok_value(
                    json!({
                        "requestId": request_id,
                        "body": body,
                        "base64Encoded": base64_encoded,
                    }),
                    0,
                ))
            }
            BrowserCommand::StartScreencast { tab_id } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                // 幂等:前端在挂载/重连/tabs-changed 时都会调一次,
                // 而 Playwright 对已开的流会报 "Screencast is already running"。
                if inner.screencast_tab.as_deref() == Some(tab_id.as_str()) {
                    return Ok(CommandOutcome::ok_value(json!({ "streaming": true }), 0));
                }
                // 单实例单画面:切 tab 时先停旧流,否则旧 tab 的帧会继续推给
                // 前端,画面不跟随切换。
                if let Some(prev) = inner.screencast_tab.clone()
                    && let Some(prev_page) = inner.pages.get(&prev).cloned()
                {
                    let _ = prev_page.screencast().stop().await;
                }
                // 帧回调在 wire_page 里一次性注册(见那里的说明);
                // 这里只负责开流。
                page.screencast()
                    .start(ScreencastStartOptions::default().quality(60))
                    .await
                    .map_err(|error| format!("开启画面失败: {error}"))?;
                inner.screencast_tab = Some(tab_id);
                // 踢一次重绘拿首帧:headless 静态页面没有合成器帧输出,
                // 开流后不重绘就永远收不到第一帧,面板会一直黑屏。
                kick_repaint(&page).await;
                Ok(CommandOutcome::ok_value(json!({ "streaming": true }), 0))
            }
            BrowserCommand::StopScreencast { tab_id } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                page.screencast()
                    .stop()
                    .await
                    .map_err(|error| format!("停止画面失败: {error}"))?;
                if inner.screencast_tab.as_deref() == Some(tab_id.as_str()) {
                    inner.screencast_tab = None;
                }
                Ok(CommandOutcome::ok_value(json!({ "streaming": false }), 0))
            }
        }
    }
}

/// 事件回注句柄(闭包捕获用;与 BrowserManager 解耦避免自引用)。
#[derive(Clone)]
struct EventSink {
    broadcast: broadcast::Sender<BrowserEvent>,
    dialogs: Arc<std::sync::Mutex<HashMap<String, PendingDialog>>>,
    network: Arc<std::sync::Mutex<NetworkLog>>,
}

impl EventSink {
    fn broadcast(&self, event: BrowserEvent) {
        let _ = self.broadcast.send(event);
    }

    fn remember_dialog(&self, tab_id: &str, kind: &str, message: &str, handle: Dialog) {
        if let Ok(mut map) = self.dialogs.lock() {
            map.insert(
                tab_id.to_string(),
                PendingDialog {
                    info: json!({ "type": kind, "message": message }),
                    handle,
                },
            );
        }
    }

    fn clear_dialog(&self, tab_id: &str) {
        if let Ok(mut map) = self.dialogs.lock() {
            map.remove(tab_id);
        }
    }

    fn network_record(&self, tab_id: &str, request_id: &str, entry: Value) {
        if let Ok(mut log) = self.network.lock() {
            log.record(tab_id, request_id, entry);
        }
    }
}

/// 错误码归类(前端/模型据此判断可重试性)。
fn error_code(message: &str) -> &'static str {
    let lower = message.to_lowercase();
    if lower.contains("timeout") || message.contains("超时") {
        "timeout"
    } else if lower.contains("target closed") || message.contains("已关闭") {
        "target_closed"
    } else if lower.contains("not exist") || message.contains("不存在") {
        "not_found"
    } else {
        "command_failed"
    }
}

/// 现在时刻(epoch ms)。
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 等待下一条 DialogOpened 事件(跳过其它事件);流关闭返回 None。
async fn wait_dialog_opened(
    rx: &mut broadcast::Receiver<BrowserEvent>,
) -> Option<(String, String, String)> {
    loop {
        match rx.recv().await {
            Ok(BrowserEvent::DialogOpened {
                tab_id,
                kind,
                message,
            }) => return Some((tab_id, kind, message)),
            Ok(_) => continue,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return None,
        }
    }
}

/// 踢一次合成器重绘,逼出 screencast 首帧。
///
/// headless 下画面帧由合成器在有失效区域时才产出;开流时若页面处于
/// 静止状态(如已加载完的静态页),CDP 层不会再送帧,面板会一直黑屏。
/// 这里翻转 documentElement 的 opacity(0.999 ↔ 默认,肉眼不可感知),
/// 制造一次失效区域。失败静默忽略:重绘只是辅助,不该让开流命令报错。
async fn kick_repaint(page: &Page) {
    let _: Option<Value> = page
        .evaluate(
            "() => { const el = document.documentElement; \
             el.style.opacity = el.style.opacity === '0.999' ? '' : '0.999'; }",
            None::<&()>,
        )
        .await
        .ok();
}

/// 把工具层的元素引用翻译成 Playwright 选择器。
///
/// 快照产出 `[ref=e6]`,模型据此传 `e6`(或带 `@` 前缀的 `@e6`)。
/// Playwright 官方约定:形如 `e<数字>` 的引用走 `aria-ref` 引擎,
/// 其余按 CSS/Playwright 定位器原样传递。
fn normalize_target(target: &str) -> String {
    let trimmed = target.trim();
    let bare = trimmed.strip_prefix('@').unwrap_or(trimmed);
    if is_aria_ref(bare) {
        return format!("aria-ref={bare}");
    }
    trimmed.to_string()
}

/// 是否是快照产出的元素引用(`e` 后跟纯数字)。
fn is_aria_ref(target: &str) -> bool {
    let Some(digits) = target.strip_prefix('e') else {
        return false;
    };
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

/// 从 ref / locator 里挑一个目标。
fn pick_target(r#ref: Option<&str>, locator: Option<&str>) -> Option<String> {
    r#ref
        .map(normalize_target)
        .or_else(|| locator.map(|value| value.to_string()))
}

/// 解析鼠标按键。
fn parse_button(button: Option<&str>) -> MouseButton {
    match button.unwrap_or("left") {
        "right" => MouseButton::Right,
        "middle" => MouseButton::Middle,
        _ => MouseButton::Left,
    }
}

/// JS 字符串字面量转义(嵌入 wait_for_function 脚本用)。
fn escape_js_string(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// 供 `MouseOptions` 复用的构造(避免调用点重复 import)。
pub fn mouse_options(button: Option<&str>) -> MouseOptions {
    MouseOptions::builder()
        .button(parse_button(button))
        .build()
}

/// 默认命令超时。
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_ref_uses_aria_ref_engine() {
        // 快照产出 [ref=e6],模型传 e6 —— 必须走 aria-ref 引擎,
        // 否则会被当作 CSS 选择器(元素不存在 → 超时)。
        assert_eq!(normalize_target("e6"), "aria-ref=e6");
        assert_eq!(normalize_target("e123"), "aria-ref=e123");
    }

    #[test]
    fn at_prefixed_ref_is_accepted() {
        assert_eq!(normalize_target("@e6"), "aria-ref=e6");
        assert_eq!(normalize_target(" @e6 "), "aria-ref=e6");
    }

    #[test]
    fn selectors_pass_through_untouched() {
        assert_eq!(normalize_target("#submit"), "#submit");
        assert_eq!(normalize_target(".btn.primary"), ".btn.primary");
        assert_eq!(normalize_target("text=登录"), "text=登录");
        assert_eq!(
            normalize_target("getByRole('button')"),
            "getByRole('button')"
        );
    }

    #[test]
    fn e_prefixed_non_numeric_is_not_a_ref() {
        // `em` / `entry` 这类是选择器,不能被误判成引用。
        assert_eq!(normalize_target("em"), "em");
        assert_eq!(normalize_target("entry-row"), "entry-row");
        assert_eq!(normalize_target("e"), "e");
    }

    #[test]
    fn pick_target_prefers_ref_over_locator() {
        assert_eq!(pick_target(Some("e6"), Some("#x")).as_deref(), Some("aria-ref=e6"));
        assert_eq!(pick_target(None, Some("#x")).as_deref(), Some("#x"));
        assert_eq!(pick_target(None, None), None);
    }

    #[test]
    fn escape_handles_quotes_and_newlines() {
        assert_eq!(escape_js_string("a'b"), "a\\'b");
        assert_eq!(escape_js_string("a\nb"), "a\\nb");
        assert_eq!(escape_js_string("a\\b"), "a\\\\b");
    }
}
