//! 浏览器中枢:CDP 会话、tab 注册表、命令执行与事件广播。
//!
//! 与旧实现(Playwright driver)的区别:
//! - 命令全部走 chromiumoxide 的 CDP 原语(借 [`crate::elements`] 的定位与
//!   动作封装),不再有 Node driver / 编译期路径依赖;
//! - tab 身份自维护(`t1`、`t2`…),映射到 chromiumoxide 的 `Page`;
//! - `[ref=eN]` 引用表按 tab 存放(最近一次 snapshot 分配),页面导航后作废;
//! - 浏览器进程退出由双看护发现:驱动层的连接断开信号 + `Browser::try_wait`
//!   轮询(进程被杀时事件流可能不结束,必须兜底);
//! - **实例不会空转占资源**:最后一个 tab 被关就立即回收进程(Chrome 在
//!   零窗口时本就该退出,留着只是被 CDP 吊着),另有闲置看护兜底,以及模型
//!   显式的 `shutdownBrowser`。回收失败会留下占住 profile 单例锁的残留,
//!   下次冷启动先走 [`procctl::reap_stale`] 清扫。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chromiumoxide::cdp::browser_protocol::emulation::{
    SetDeviceMetricsOverrideParams,
};
use chromiumoxide::cdp::browser_protocol::input::MouseButton;
use chromiumoxide::cdp::browser_protocol::network::{
    EnableParams as NetworkEnableParams, EventLoadingFailed, EventRequestWillBeSent,
    EventResponseReceived, GetRequestPostDataParams, GetResponseBodyParams,
};
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, CaptureScreenshotParams, EventFrameNavigated,
    EventJavascriptDialogOpening, EventScreencastFrame, GetLayoutMetricsParams,
    GetNavigationHistoryParams, HandleJavaScriptDialogParams, NavigateParams,
    NavigateToHistoryEntryParams, ReloadParams, ScreencastFrameAckParams, StartScreencastFormat,
    StartScreencastParams, StopScreencastParams, Viewport as ClipViewport,
};
use chromiumoxide::cdp::browser_protocol::target::{ActivateTargetParams, EventTargetDestroyed};
use chromiumoxide::page::Page;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};

use crate::driver::{self, LaunchSpec, Session};
use crate::elements;
use crate::network::NetworkLog;
use crate::procctl;
use crate::{BrowserCommand, CommandOutcome};

/// tab 上限:防止模型无限开页把浏览器撑爆。
const TAB_LIMIT: usize = 32;

/// 本命令是否需要一个可操作的活动 tab(零页面时要不要补一个空白页)。
///
/// 排除项都是「不得凭空开页」的:查询与善后命令(`getState`/`list`/`close`)、
/// 自己建页的命令(`newTab`)、按 id 精确定位的命令(`activate`)与回收。
/// 漏一个就会让收尾路径先拉起实例再自杀,多一个则会让旧有命令报「没有活动 tab」。
fn needs_target_tab(command: &BrowserCommand) -> bool {
    !matches!(
        command,
        BrowserCommand::GetState { .. }
            | BrowserCommand::List
            | BrowserCommand::Close { .. }
            | BrowserCommand::NewTab { .. }
            | BrowserCommand::Activate { .. }
            | BrowserCommand::ShutdownBrowser
    )
}

/// 闲置多久就自动回收实例(模型忘了收尾时的兜底)。
///
/// 取 5 分钟:比一次完整任务里两次浏览器命令的常见间隔长得多,又短到不会
/// 让一个 Chrome 进程树挂满半天。可用 `DENIA_BROWSER_IDLE_SECS` 调;0 关闭。
const IDLE_RELEASE: Duration = Duration::from_secs(300);

/// 闲置看护的轮询间隔(粒度不需要比 IDLE_RELEASE 细)。
const IDLE_TICK: Duration = Duration::from_secs(10);

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

/// 活跃会话:浏览器句柄 + 上下文 + tab 注册表。
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
    /// tabId -> 当前视口(viewportSet 后记录;reset 恢复 spec 默认)。
    viewports: HashMap<String, (u32, u32)>,
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
        self.viewports.remove(tab_id);
        if self.active_tab.as_deref() == Some(tab_id) {
            self.active_tab = self.tabs.keys().next().cloned();
        }
    }

    /// 刷新某 tab 的 url/title。
    async fn refresh_meta(&mut self, tab_id: &str) {
        let Some(page) = self.pages.get(tab_id).cloned() else {
            return;
        };
        let url = elements::page_url(&page).await;
        let title = elements::page_title(&page).await;
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
    /// 活跃会话(Arc 包一层:看护任务需独立持有)。
    inner: Arc<Mutex<Option<Inner>>>,
    /// 启动互斥:并发命令同时发现未启动时,只放一个进去拉起。
    starting: Mutex<()>,
    /// 串行化命令生命周期,避免并发命令交叉使用已失效的 Page。
    /// Arc:闲置看护任务抢不到门时要能跳过本轮。
    command_gate: Arc<Mutex<()>>,
    generation: AtomicU64,
    event_broadcast: broadcast::Sender<BrowserEvent>,
    /// 挂起 JS dialog(tabId -> 信息):handleDialog 消费。
    pending_dialogs: Arc<std::sync::Mutex<HashMap<String, Value>>>,
    /// 网络抓包缓冲。
    network_log: Arc<std::sync::Mutex<NetworkLog>>,
    /// 每 tab 的 `[ref=eN]` → backendNodeId(最近一次 snapshot 分配)。
    ref_tables: Arc<std::sync::Mutex<HashMap<String, HashMap<String, i64>>>>,
    /// tabId -> 浏览器 target id(用于 Target.targetDestroyed 反查)。
    target_ids: Arc<std::sync::Mutex<HashMap<String, String>>>,
    /// 启动参数。
    spec: LaunchSpec,
    /// 用户数据目录(profile 落盘位置)。
    home: PathBuf,
    /// tab 归属:owner(会话 id) → 它产生/操作过的 tab id。
    /// Arc:回收逻辑与闲置看护任务需共用同一份记录。
    tab_owners: Arc<TabOwners>,
    /// 回收句柄:工具、零 tab 自动回收与闲置看护三条路径共用。
    releaser: Releaser,
    /// 最近一条浏览器命令的时刻(epoch ms):闲置看护据此回收进程。
    last_activity_ms: Arc<AtomicU64>,
    /// 闲置看护只起一个(跟 manager 同生命周期,不随会话重启而换)。
    watchdog_started: AtomicU64,
}

/// tab 归属登记表。
///
/// 唯一用途:子代理结束时只清理**它自己**产生的 tab(计划 6.4)。父会话同样
/// 记账,但没有对应的清理触发点,记录只是审计数据。
///
/// 记录与清理都是幂等的:同一 tab 重复登记只留一份,多个 owner 共用一个 tab 时
/// 各自持有自己的记录(`take` 只清自己的那一份)。
#[derive(Default)]
pub struct TabOwners {
    inner: std::sync::Mutex<std::collections::BTreeMap<String, std::collections::BTreeSet<String>>>,
}

impl TabOwners {
    pub fn record(&self, owner: &str, tab_id: &str) {
        if let Ok(mut guard) = self.inner.lock() {
            guard
                .entry(owner.to_string())
                .or_default()
                .insert(tab_id.to_string());
        }
    }

    pub fn forget(&self, owner: &str, tab_id: &str) {
        if let Ok(mut guard) = self.inner.lock()
            && let Some(tabs) = guard.get_mut(owner)
        {
            tabs.remove(tab_id);
        }
    }

    /// 取出并清空某 owner 的记录(顺序确定,便于测试与诊断)。
    pub fn take(&self, owner: &str) -> Vec<String> {
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poison) => poison.into_inner(),
        };
        guard
            .remove(owner)
            .map(|tabs| tabs.into_iter().collect())
            .unwrap_or_default()
    }

    /// 只读查看(不清理)。
    pub fn owned(&self, owner: &str) -> Vec<String> {
        let guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poison) => poison.into_inner(),
        };
        guard
            .get(owner)
            .map(|tabs| tabs.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// 实例被回收后清空全部归属:tab 随进程一起没了,留着记录会让后续
    /// `close_owned` 把新实例的同名 tab(t1/t2…)当成旧tab 关掉。
    fn clear_all(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.clear();
        }
    }
}

/// 回收所需状态的共享句柄。
///
/// 抽出来的原因:同一个回收动作有三个发起方——模型显式 `shutdownBrowser`、
/// 最后一个 tab 被关、闲置看护。看护是独立 tokio 任务,拿不到 `&self`,
/// 所以把要碰的几张表集中到这个可 `Clone` 的句柄上,三条路径跑同一实现
/// 否则三处副本迟早不一致,而这里不一致的代价就是进程回收不干净。
#[derive(Clone)]
struct Releaser {
    inner: Arc<Mutex<Option<Inner>>>,
    ref_tables: Arc<std::sync::Mutex<HashMap<String, HashMap<String, i64>>>>,
    target_ids: Arc<std::sync::Mutex<HashMap<String, String>>>,
    dialogs: Arc<std::sync::Mutex<HashMap<String, Value>>>,
    network_log: Arc<std::sync::Mutex<NetworkLog>>,
    tab_owners: Arc<TabOwners>,
    broadcast: broadcast::Sender<BrowserEvent>,
}

impl Releaser {
    /// 只清会话与 per-tab 表(不动进程):浏览器已自杀或被外部杀掉时用。
    async fn clear_session_if(&self, generation: u64) {
        let mut guard = self.inner.lock().await;
        let tabs: Vec<String> = match guard.as_mut() {
            Some(current) if current.session.generation == generation => {
                current.pages.keys().cloned().collect()
            }
            // 代次不符 = 已是新会话,不归本次清理。
            _ => return,
        };
        *guard = None;
        drop(guard);
        self.forget_tabs(&tabs);
        self.tab_owners.clear_all();
        let _ = self.broadcast.send(BrowserEvent::Exited);
    }

    /// 回收一个实例:关页面 → `Browser.close` → 超时强杀进程树 → 清状态。
    ///
    /// 幂等:没有活跃会话时返回 `false`(收尾动作重复执行是常态)。
    /// 调用方需已持命令门,确保不与在跑的命令交叉。
    async fn release(&self, cause: &str) -> bool {
        let taken = {
            let mut guard = self.inner.lock().await;
            guard.take()
        };
        let Some(mut inner) = taken else {
            return false;
        };
        // 清理清单趁还能读时拿下来,drop 之后就没机会了。
        let tabs: Vec<String> = inner.pages.keys().cloned().collect();
        // 画面先停:帧回调往已死的连接上推只会刷警告。
        if let Some(tab_id) = inner.screencast_tab.take()
            && let Some(page) = inner.pages.get(&tab_id)
        {
            let _ = page.execute(StopScreencastParams::default()).await;
        }
        // 给回收加上限,不让一个拖着的 Chrome 把调用挂死。
        let gone = match tokio::time::timeout(
            Duration::from_secs(15),
            driver::shutdown(&mut inner.session),
        )
        .await
        {
            Ok(shutdown) => shutdown.gone,
            Err(_) => {
                tracing::warn!("浏览器实例({cause})回收超时,补一次进程树强杀");
                procctl::reap_own(inner.session.pid, Some(&inner.session.exe)).await
            }
        };
        if gone {
            tracing::info!("浏览器实例({cause})已回收");
            inner.session.release_profile_token();
        } else {
            // 进程还在:归属记录留着,下次冷启动的 reap_stale 按 PID+身份再清算。
            tracing::warn!("浏览器实例({cause})未收干净,保留记录交由下次启动清算");
            inner.session.keep_profile_record();
        }
        drop(inner);

        self.forget_tabs(&tabs);
        // tab 随进程一起没了:归属记录必须同步清空,否则下次拉起后
        // `close_owned` 会把新实例的同名 tab(t1/t2…)当成旧 tab 关掉。
        self.tab_owners.clear_all();
        let _ = self.broadcast.send(BrowserEvent::Exited);
        true
    }

    /// 丢弃已消失 tab 的引用表/目标映射/弹窗/抓包缓冲。
    fn forget_tabs(&self, tabs: &[String]) {
        for tab_id in tabs {
            if let Ok(mut tables) = self.ref_tables.lock() {
                tables.remove(tab_id);
            }
            if let Ok(mut ids) = self.target_ids.lock() {
                ids.remove(tab_id);
            }
            if let Ok(mut dialogs) = self.dialogs.lock() {
                dialogs.remove(tab_id);
            }
            if let Ok(mut log) = self.network_log.lock() {
                log.clear(tab_id);
            }
        }
    }
}

impl BrowserManager {
    pub fn new(home: PathBuf) -> Self {
        let (event_broadcast, _) = broadcast::channel(256);
        let spec = LaunchSpec::new(&home);
        let inner: Arc<Mutex<Option<Inner>>> = Arc::new(Mutex::new(None));
        let pending_dialogs = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let network_log = Arc::new(std::sync::Mutex::new(NetworkLog::default()));
        let ref_tables = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let target_ids = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let tab_owners = Arc::new(TabOwners::default());
        let releaser = Releaser {
            inner: inner.clone(),
            ref_tables: ref_tables.clone(),
            target_ids: target_ids.clone(),
            dialogs: pending_dialogs.clone(),
            network_log: network_log.clone(),
            tab_owners: tab_owners.clone(),
            broadcast: event_broadcast.clone(),
        };
        Self {
            inner,
            starting: Mutex::new(()),
            command_gate: Arc::new(Mutex::new(())),
            generation: AtomicU64::new(0),
            event_broadcast,
            pending_dialogs,
            network_log,
            ref_tables,
            target_ids,
            spec,
            home,
            tab_owners,
            releaser,
            last_activity_ms: Arc::new(AtomicU64::new(0)),
            watchdog_started: AtomicU64::new(0),
        }
    }

    /// 带归属执行:执行后把作用的 tab 记到 `owner` 名下。
    ///
    /// 归属判定:显式 `tabId` 优先,否则用结果里的 `activeTabId`,再退到
    /// `value.tabId`(newTab)。`close` 成功则解除归属,避免重复关闭。
    pub async fn execute_owned(
        &self,
        command: BrowserCommand,
        owner: Option<&str>,
    ) -> CommandOutcome {
        let requested = command.tab_id_probe().map(str::to_string);
        let closing = matches!(command, BrowserCommand::Close { .. });
        let outcome = self.execute(command).await;
        if let Some(owner) = owner {
            if closing && outcome.ok {
                if let Some(tab) = &requested {
                    self.tab_owners.forget(owner, tab);
                }
            } else {
                let acting = requested
                    .or_else(|| {
                        outcome
                            .state
                            .as_ref()
                            .and_then(|state| state.get("activeTabId"))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .or_else(|| {
                        outcome
                            .value
                            .as_ref()
                            .and_then(|value| value.get("tabId"))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    });
                if let Some(tab) = acting {
                    self.tab_owners.record(owner, &tab);
                }
            }
        }
        outcome
    }

    /// 关闭 `owner` 名下的 tab,返回实际关闭数量。
    ///
    /// 子代理停止/结束时调用:只清理它自己的 tab,父代理与其他子代理的 tab
    /// 不受影响(计划 6.4 / L04)。
    pub async fn close_owned(&self, owner: &str) -> usize {
        let tabs = self.tab_owners.take(owner);
        let mut closed = 0;
        for tab in tabs {
            let outcome = self
                .execute(BrowserCommand::Close { tab_id: Some(tab) })
                .await;
            if outcome.ok {
                closed += 1;
            }
        }
        closed
    }

    /// 归属表的只读视图（集成测试断言"只关自己的 tab"用）。
    pub fn tab_owners_for_test(&self) -> &TabOwners {
        &self.tab_owners
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
        guard.get(tab_id).cloned()
    }

    /// 取走某 tab 的挂起 dialog 信息。
    pub fn take_dialog(&self, tab_id: &str) -> Option<Value> {
        self.pending_dialogs
            .lock()
            .ok()
            .and_then(|mut map| map.remove(tab_id))
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

    /// 取某 tab 最近的 ref 表(快照分配)。
    fn ref_table(&self, tab_id: &str) -> HashMap<String, i64> {
        self.ref_tables
            .lock()
            .map(|tables| tables.get(tab_id).cloned().unwrap_or_default())
            .unwrap_or_default()
    }

    /// 清除某 tab 的引用/目标/弹窗记录(关闭或导航时)。
    fn forget_tab(&self, tab_id: &str) {
        if let Ok(mut tables) = self.ref_tables.lock() {
            tables.remove(tab_id);
        }
        if let Ok(mut ids) = self.target_ids.lock() {
            ids.remove(tab_id);
        }
        if let Ok(mut dialogs) = self.pending_dialogs.lock() {
            dialogs.remove(tab_id);
        }
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
                        .map(|(k, v)| json!({ "tabId": k, "dialog": v }))
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
        // 拉起即刷新活动时钟:闲置预算从「本次开始用」算起,而不是从上一条
        // 命令算起 —— 否则空窗期超过预算后,刚拉起的实例会被立刻当陈闲收掉。
        self.last_activity_ms.store(now_ms(), Ordering::SeqCst);
        let (session, exit) = driver::launch(&self.spec, generation).await?;

        let mut inner = Inner {
            session,
            pages: HashMap::new(),
            tabs: HashMap::new(),
            active_tab: None,
            next_tab_seq: 0,
            screencast_tab: None,
            viewports: HashMap::new(),
        };

        // 持久化 profile 会恢复上次的标签页;把它们纳入注册表。
        let existing = inner
            .session
            .browser
            .pages()
            .await
            .map_err(|error| format!("枚举已有标签页失败: {error}"))?;
        for page in existing {
            let url = elements::page_url(&page).await;
            if url.is_empty() || url == "about:blank" {
                continue;
            }
            let title = elements::page_title(&page).await;
            let tab_id = inner.register(page, url, title);
            if inner.active_tab.is_none() {
                inner.active_tab = Some(tab_id);
            }
        }
        // 不再预建空白页:持久化 profile 恢复回来的页面照常登记,没有就空着。
        //
        // 两个理由:① 占位页让 `pages.is_empty()` 永远不成立,「最后一个 tab
        // 关闭即回收」就永远不会触发,而用户自己的 tab 还会被它一并收掉;② 模型
        // 拿不到活动 tab 时,命令层会把错误原样传回(报 tab 不存在),比默默多一个
        // 没人要的窗口好。真需要落点的 navigate/newTab/snapshot 都会自己开页。

        // 浏览器级事件:target 销毁(页面被关闭)时清注册表。
        match inner
            .session
            .browser
            .event_listener::<EventTargetDestroyed>()
            .await
        {
            Ok(events) => self.spawn_target_destroyed_watcher(events),
            Err(error) => tracing::warn!("注册 targetDestroyed 监听失败: {error}"),
        }

        // 给每个 Page 挂监听(先于任何后续导航,避免错过早期事件)。
        let tab_ids: Vec<String> = inner.tabs.keys().cloned().collect();
        for tab_id in tab_ids {
            if let Some(page) = inner.pages.get(&tab_id).cloned() {
                self.wire_page(tab_id, page).await;
            }
        }

        *self.inner.lock().await = Some(inner);

        // 闲置看护:模型忘了收尾(或子代理中途挂)时兜底回收。
        self.ensure_idle_watchdog();

        // 退出看护 1:驱动层退出信号(连接断开/主动关闭)。
        // 走 releaser 统一入口:进程没了也要把 per-tab 表与归属记录带走,
        // 否则下次拉起会撞上同名的陈引用。
        let releaser = self.releaser.clone();
        tokio::spawn(async move {
            let _ = exit.await;
            releaser.clear_session_if(generation).await;
        });
        // 退出看护 2:进程存活轮询(浏览器被杀时事件流可能静默停摆,这里兜底)。
        let releaser = self.releaser.clone();
        let inner_slot = self.inner.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(1000)).await;
                let dead = {
                    let mut slot = inner_slot.lock().await;
                    // 会话已不在(别的看护收走了):本轮清理到此为止。
                    let Some(current) = slot.as_mut() else { break };
                    if current.session.generation != generation {
                        break;
                    }
                    matches!(current.session.browser.try_wait(), Ok(Some(_)) | Err(_))
                };
                if dead {
                    releaser.clear_session_if(generation).await;
                    break;
                }
            }
        });
        Ok(())
    }

    /// 浏览器级 target 销毁监听(页面被外部关闭时清注册表)。
    fn spawn_target_destroyed_watcher(
        &self,
        mut events: chromiumoxide::listeners::EventStream<EventTargetDestroyed>,
    ) {
        let inner_slot = self.inner.clone();
        let broadcast = self.event_broadcast.clone();
        let target_ids = self.target_ids.clone();
        let ref_tables = self.ref_tables.clone();
        let dialogs = self.pending_dialogs.clone();
        tokio::spawn(async move {
            while let Some(event) = events.next().await {
                let target = serde_str_id(&event.target_id);
                let tab = target_ids.lock().ok().and_then(|mut map| {
                    let found = map
                        .iter()
                        .find(|(_, value)| **value == target)
                        .map(|(key, _)| key.clone());
                    if let Some(tab) = &found {
                        map.remove(tab);
                    }
                    found
                });
                let Some(tab) = tab else { continue };
                if let Some(inner) = inner_slot.lock().await.as_mut() {
                    inner.unregister(&tab);
                }
                if let Ok(mut tables) = ref_tables.lock() {
                    tables.remove(&tab);
                }
                if let Ok(mut map) = dialogs.lock() {
                    map.remove(&tab);
                }
                let _ = broadcast.send(BrowserEvent::DialogClosed { tab_id: tab.clone() });
                let _ = broadcast.send(BrowserEvent::TabsChanged);
            }
        });
    }

    /// 给单个 Page 挂事件监听(帧/导航/弹窗/网络)。
    async fn wire_page(&self, tab_id: String, page: Page) {
        // 记录 target 映射(页面被外部关闭时按 targetId 反查 tab)。
        if let Ok(mut ids) = self.target_ids.lock() {
            ids.insert(tab_id.clone(), serde_str_id(page.target_id()));
        }

        // 网络:先 enable 再注册(否则早期请求会丢)。
        if let Err(error) = page.execute(NetworkEnableParams::builder().build()).await {
            tracing::warn!("Network 域启用失败(网络抓包可能不完整): {error}");
        }

        // screencast 帧 → SSE;帧需回执,不回执会停发。
        let broadcast = self.event_broadcast.clone();
        let page_frames = page.clone();
        let tab_frames = tab_id.clone();
        if let Ok(mut frames) = page.event_listener::<EventScreencastFrame>().await {
            tokio::spawn(async move {
                while let Some(frame) = frames.next().await {
                    let data: &str = frame.data.as_ref();
                    let viewport = FrameViewport {
                        width: frame.metadata.device_width.max(0.0) as u32,
                        height: frame.metadata.device_height.max(0.0) as u32,
                    };
                    let _ = broadcast.send(BrowserEvent::Frame {
                        tab_id: tab_frames.clone(),
                        data: data.to_string(),
                        viewport: Some(viewport),
                    });
                    let _ = page_frames
                        .execute(ScreencastFrameAckParams::new(frame.session_id))
                        .await;
                }
            });
        }

        // 主框架导航 → 清 ref 表(backendNodeId 跨文档失效)+ 广播。
        let broadcast = self.event_broadcast.clone();
        let tab_nav = tab_id.clone();
        let ref_tables = self.ref_tables.clone();
        if let Ok(mut events) = page.event_listener::<EventFrameNavigated>().await {
            tokio::spawn(async move {
                while let Some(event) = events.next().await {
                    if event.frame.parent_id.is_some() {
                        continue;
                    }
                    if let Ok(mut tables) = ref_tables.lock() {
                        tables.remove(&tab_nav);
                    }
                    let _ = broadcast.send(BrowserEvent::Navigated {
                        tab_id: tab_nav.clone(),
                        url: event.frame.url.clone(),
                        title: String::new(),
                    });
                }
            });
        }

        // JS 弹窗:挂起记录,等模型/面板用 getDialog/handleDialog 处置。
        let broadcast = self.event_broadcast.clone();
        let dialogs = self.pending_dialogs.clone();
        let tab_dialog = tab_id.clone();
        if let Ok(mut events) = page.event_listener::<EventJavascriptDialogOpening>().await {
            tokio::spawn(async move {
                while let Some(event) = events.next().await {
                    let kind = serde_str_id(&event.r#type);
                    let message = event.message.clone();
                    if let Ok(mut map) = dialogs.lock() {
                        map.insert(tab_dialog.clone(), json!({ "type": kind, "message": message }));
                    }
                    let _ = broadcast.send(BrowserEvent::DialogOpened {
                        tab_id: tab_dialog.clone(),
                        kind,
                        message,
                    });
                }
            });
        }

        // 网络请求/响应 → 环形缓冲(requestId 直接用 CDP 原生 id)。
        let log = self.network_log.clone();
        let tab_net = tab_id.clone();
        if let Ok(mut events) = page.event_listener::<EventRequestWillBeSent>().await {
            tokio::spawn(async move {
                while let Some(event) = events.next().await {
                    let request = &event.request;
                    let resource_type = event.r#type.as_ref().map(serde_str_id).unwrap_or_default();
                    let entry = json!({
                        "url": request.url,
                        "method": request.method,
                        "resourceType": resource_type,
                        "hasPostData": request.has_post_data,
                        "headers": serde_json::to_value(&request.headers).unwrap_or(Value::Null),
                        "atMs": now_ms(),
                    });
                    if let Ok(mut guard) = log.lock() {
                        guard.record(&tab_net, &serde_str_id(&event.request_id), entry);
                    }
                }
            });
        }
        let log = self.network_log.clone();
        let tab_net = tab_id.clone();
        if let Ok(mut events) = page.event_listener::<EventResponseReceived>().await {
            tokio::spawn(async move {
                while let Some(event) = events.next().await {
                    let response = &event.response;
                    let entry = json!({
                        "status": response.status,
                        "contentType": response.mime_type,
                        "atMs": now_ms(),
                    });
                    if let Ok(mut guard) = log.lock() {
                        guard.record(&tab_net, &serde_str_id(&event.request_id), entry);
                    }
                }
            });
        }
        let log = self.network_log.clone();
        let tab_net = tab_id.clone();
        if let Ok(mut events) = page.event_listener::<EventLoadingFailed>().await {
            tokio::spawn(async move {
                while let Some(event) = events.next().await {
                    let entry = json!({ "failed": event.error_text, "atMs": now_ms() });
                    if let Ok(mut guard) = log.lock() {
                        guard.record(&tab_net, &serde_str_id(&event.request_id), entry);
                    }
                }
            });
        }
    }

    /// 执行一条浏览器命令。
    pub async fn execute(&self, command: BrowserCommand) -> CommandOutcome {
        let started = Instant::now();
        // 命令串行化:并发命令交叉使用同一 Page/会话易触发竞态。
        let _gate = self.command_gate.lock().await;
        self.last_activity_ms.store(now_ms(), Ordering::SeqCst);

        // 善后类命令不得拉起实例:否则模型收尾时「确认已回收」本身会把
        // 浏览器再开回来。没在跑就原地报成功(released=false)。
        if matches!(command, BrowserCommand::ShutdownBrowser) {
            let released = self.releaser.release("tool").await;
            return CommandOutcome {
                elapsed_ms: started.elapsed().as_millis() as u64,
                ..CommandOutcome::ok_value(
                    json!({ "released": released, "running": false }),
                    0,
                )
            };
        }
        // 未运行时的 list/close:原地返回。close 若走下面的通用路径,会先把
        // 实例拉起来(带一个初始空白页)再把它关掉 —— 收尾动作自己把浏览器
        // 开了一个来回,既浪费也可能误碰刚恢复的会话。
        if matches!(command, BrowserCommand::List | BrowserCommand::Close { .. })
            && self.inner.lock().await.is_none()
        {
            let value = match &command {
                BrowserCommand::Close { tab_id } => {
                    json!({ "closed": tab_id, "running": false, "note": "浏览器本来就没在跑" })
                }
                _ => json!({ "tabs": [], "running": false }),
            };
            return CommandOutcome {
                elapsed_ms: started.elapsed().as_millis() as u64,
                ..CommandOutcome::ok_value(value, 0)
            };
        }
        if let Err(error) = self.get_or_start().await {
            return CommandOutcome::err("not_running", error, started.elapsed().as_millis() as u64);
        }
        // 弹窗竞速:命令执行中页面弹出 JS dialog 时,动作调用会被挂起的
        // dialog 卡到超时;这里订阅事件流,一旦 DialogOpened 就提前返回弹窗
        // 信息,模型用 getDialog/handleDialog 处置——对齐"命令即时返回、
        // 弹窗挂起等待处理"的语义。
        let mut event_rx = self.event_broadcast.subscribe();
        let outcome = tokio::select! {
            result = self.dispatch(command.clone()) => match result {
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
                None => CommandOutcome::err(
                    "command_failed",
                    "事件流中断",
                    started.elapsed().as_millis() as u64,
                ),
            },
        };
        // 自动回收:最后一个 tab 被关(或被外部关掉)后,实例不得空转 ——
        // Chrome 零窗口时本就该退,留着只是白占资源与 profile 单例锁,且一旦
        // 变成孤儿,下次冷启动会直接报 `ExitStatus(21)`。
        if matches!(command, BrowserCommand::Close { .. }) {
            let empty = {
                let guard = self.inner.lock().await;
                guard.as_ref().is_some_and(|inner| inner.pages.is_empty())
            };
            if empty {
                tracing::info!("最后一个 tab 已关闭,自动回收浏览器进程");
                self.releaser.release("last-tab").await;
            }
        }
        CommandOutcome {
            elapsed_ms: started.elapsed().as_millis() as u64,
            ..outcome
        }
    }

    /// 当前是否有活跃会话(不触发拉起)。
    pub async fn is_running(&self) -> bool {
        self.inner.lock().await.is_some()
    }

    /// 供工具与 REST 层显式调用:回收浏览器进程。
    ///
    /// 不走命令分发,因此**不会拉起实例**;本来就没在跑也算成功。
    pub async fn shutdown_browser(&self) -> CommandOutcome {
        let _gate = self.command_gate.lock().await;
        let released = self.releaser.release("tool").await;
        CommandOutcome::ok_value(json!({ "released": released, "running": false }), 0)
    }

    /// 闲置自动回收的时长;`DENIA_BROWSER_IDLE_SECS=0` 关闭该看护。
    fn idle_release() -> Duration {
        std::env::var("DENIA_BROWSER_IDLE_SECS")
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(IDLE_RELEASE)
    }

    /// 起闲置看护(全 manager 一个,永不退出)。
    ///
    /// 兜的是「模型忘了收尾」与「子代理中途挂」两种情况:前者靠工具纪律,
    /// 后者只能靠这里。不收正在看画面的实例 —— 侧栏开着 = 用户在用。
    ///
    /// 不按代挂任务、也不在退出时复位标志:那样与下一次拉起有竞态(旧任务还
    /// 没复位、新一代 compare_exchange 失败 → 那一代就没看护了),而常驻一个
    /// 任务只需多睡一个 tick,没这个麻烦。
    fn ensure_idle_watchdog(&self) {
        let budget = Self::idle_release();
        if budget.is_zero() {
            return;
        }
        if self
            .watchdog_started
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let releaser = self.releaser.clone();
        let gate = self.command_gate.clone();
        let last_activity = self.last_activity_ms.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(IDLE_TICK).await;
                let idle_ms = now_ms().saturating_sub(last_activity.load(Ordering::SeqCst));
                let watching = {
                    let guard = releaser.inner.lock().await;
                    match guard.as_ref() {
                        // 没会话:无事可做,继续等(不退出)。
                        None => continue,
                        Some(current) => current.screencast_tab.is_some(),
                    }
                };
                if idle_ms < budget.as_millis() as u64 || watching {
                    continue;
                }
                // 抢命令门:确保不与在跑的命令交叉;抢不到就下一轮再看。
                let Ok(_gate_guard) = gate.try_lock() else {
                    continue;
                };
                tracing::info!("浏览器空闲 {}s 未使用,自动回收进程", idle_ms / 1000);
                releaser.release("idle").await;
            }
        });
    }

    /// 命令分发(已确保会话就绪)。
    async fn dispatch(&self, command: BrowserCommand) -> Result<CommandOutcome, String> {
        let mut guard = self.inner.lock().await;

        // 惰性落点:启动不再预建空白页(那会让 `pages.is_empty()` 永远不成立,
        // 最后一个 tab 的自动回收就永远不会触发)。只有确实要操作页面的命令才补
        // 一个 about:blank —— 保留旧有「模型总能拿到落点」的行为;
        // close/list/getState 等善后与查询路径绝不能开页,否则「收尾动作
        // 自己把浏览器开了回来」又会出现。
        if guard
            .as_ref()
            .is_some_and(|inner| inner.pages.is_empty())
            && needs_target_tab(&command)
        {
            let (tab_id, page) = {
                let inner = guard
                    .as_mut()
                    .ok_or_else(|| "浏览器未启动(会话已被回收)".to_string())?;
                let page = inner
                    .session
                    .browser
                    .new_page("about:blank")
                    .await
                    .map_err(|error| format!("创建标签页失败: {error}"))?;
                let tab_id =
                    inner.register(page.clone(), "about:blank".to_string(), String::new());
                inner.active_tab = Some(tab_id.clone());
                (tab_id, page)
            };
            // 挂监听要 `&self` 但不能持会话锁(与 newTab 同一做法):先放锁。
            drop(guard);
            self.wire_page(tab_id, page).await;
            guard = self.inner.lock().await;
        }
        let inner = guard
            .as_mut()
            .ok_or_else(|| "浏览器未启动(会话已被回收)".to_string())?;

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
            // 回收在 execute 里拦下:dispatch 已持 inner 锁,而 release 也要拿
            // 那把锁 —— 在这儿做会自锁。留分支只为 enum 新成员在此可见。
            BrowserCommand::ShutdownBrowser => {
                unreachable!("shutdownBrowser 由 execute 直接处理,不进分发")
            }
            BrowserCommand::NewTab { url } => {
                if inner.pages.len() >= TAB_LIMIT {
                    return Err(format!("标签页已达上限({TAB_LIMIT});先关闭不用的标签页"));
                }
                let page = inner
                    .session
                    .browser
                    .new_page("about:blank")
                    .await
                    .map_err(|error| format!("新建标签页失败: {error}"))?;
                let tab_id = inner.register(page.clone(), "about:blank".to_string(), String::new());
                inner.active_tab = Some(tab_id.clone());
                drop(guard);
                // 先挂监听再导航,避免错过早期事件(网络/导航/弹窗)。
                self.wire_page(tab_id.clone(), page.clone()).await;
                if let Some(url) = url
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty() && *value != "about:blank")
                {
                    let response = page
                        .execute(
                            NavigateParams::builder()
                                .url(url.to_string())
                                .build()
                                .unwrap(),
                        )
                        .await
                        .map_err(|error| format!("导航失败: {error}"))?;
                    if let Some(error_text) = response.result.error_text.clone() {
                        return Err(format!("导航失败: {error_text}"));
                    }
                    wait_for_load(&page, Duration::from_secs(15)).await;
                }
                let (target, title) = {
                    let mut guard = self.inner.lock().await;
                    let inner = guard.as_mut().ok_or_else(|| "浏览器未启动".to_string())?;
                    inner.refresh_meta(&tab_id).await;
                    match inner.tabs.get(&tab_id) {
                        Some(info) => (info.url.clone(), info.title.clone()),
                        None => (String::new(), String::new()),
                    }
                };
                self.broadcast(BrowserEvent::TabsChanged);
                Ok(CommandOutcome::ok_value(
                    json!({ "tabId": tab_id, "url": target, "title": title }),
                    0,
                ))
            }
            BrowserCommand::Close { tab_id } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                if inner.screencast_tab.as_deref() == Some(tab_id.as_str()) {
                    let _ = page.execute(StopScreencastParams::default()).await;
                    inner.screencast_tab = None;
                }
                page.close()
                    .await
                    .map_err(|error| format!("关闭标签页失败: {error}"))?;
                inner.unregister(&tab_id);
                drop(guard);
                self.forget_tab(&tab_id);
                self.broadcast(BrowserEvent::TabsChanged);
                Ok(CommandOutcome::ok_value(json!({ "closed": tab_id }), 0))
            }

            BrowserCommand::Activate { tab_id } => {
                if !inner.pages.contains_key(&tab_id) {
                    return Err(format!("tab 不存在: {tab_id}"));
                }
                inner.active_tab = Some(tab_id.clone());
                let page = inner.page(&tab_id)?;
                let target = page.target_id().clone();
                let _ = inner
                    .session
                    .browser
                    .execute(ActivateTargetParams::new(target))
                    .await;
                drop(guard);
                self.broadcast(BrowserEvent::TabsChanged);
                Ok(CommandOutcome::ok_value(json!({ "activeTabId": tab_id }), 0))
            }
            BrowserCommand::Navigate { url, tab_id } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let response = page
                    .execute(
                        NavigateParams::builder()
                            .url(url.clone())
                            .build()
                            .unwrap(),
                    )
                    .await
                    .map_err(|error| format!("导航失败: {error}"))?;
                if let Some(error_text) = response.result.error_text.clone() {
                    return Err(format!("导航失败: {error_text}"));
                }
                wait_for_load(&page, Duration::from_secs(15)).await;
                inner.refresh_meta(&tab_id).await;
                drop(guard);
                let status = self.latest_document_status(&tab_id);
                self.broadcast(BrowserEvent::Navigated {
                    tab_id: tab_id.clone(),
                    url: url.clone(),
                    title: String::new(),
                });
                Ok(CommandOutcome::ok_value(json!({ "url": url, "status": status }), 0))
            }
            BrowserCommand::Back { tab_id } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let history = page
                    .execute(GetNavigationHistoryParams::default())
                    .await
                    .map_err(|error| format!("取导航历史失败: {error}"))?;
                let index = history.current_index;
                if index <= 0 {
                    return Err("没有可后退的历史".to_string());
                }
                let entry = history
                    .entries
                    .get((index - 1) as usize)
                    .ok_or_else(|| "没有可后退的历史".to_string())?;
                page.execute(NavigateToHistoryEntryParams::new(entry.id))
                    .await
                    .map_err(|error| format!("后退失败: {error}"))?;
                wait_for_load(&page, Duration::from_secs(5)).await;
                inner.refresh_meta(&tab_id).await;
                let url = elements::page_url(&page).await;
                Ok(CommandOutcome::ok_value(json!({ "url": url }), 0))
            }
            BrowserCommand::Forward { tab_id } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let history = page
                    .execute(GetNavigationHistoryParams::default())
                    .await
                    .map_err(|error| format!("取导航历史失败: {error}"))?;
                let next = (history.current_index + 1) as usize;
                if next >= history.entries.len() {
                    return Err("没有可前进的历史".to_string());
                }
                let entry = &history.entries[next];
                page.execute(NavigateToHistoryEntryParams::new(entry.id))
                    .await
                    .map_err(|error| format!("前进失败: {error}"))?;
                wait_for_load(&page, Duration::from_secs(5)).await;
                inner.refresh_meta(&tab_id).await;
                let url = elements::page_url(&page).await;
                Ok(CommandOutcome::ok_value(json!({ "url": url }), 0))
            }
            BrowserCommand::Reload { tab_id } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                page.execute(ReloadParams::builder().build())
                    .await
                    .map_err(|error| format!("刷新失败: {error}"))?;
                wait_for_load(&page, Duration::from_secs(10)).await;
                inner.refresh_meta(&tab_id).await;
                let url = elements::page_url(&page).await;
                Ok(CommandOutcome::ok_value(json!({ "url": url }), 0))
            }
            BrowserCommand::Snapshot {
                tab_id,
                max_elements,
                include_hidden,
            } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let _ = include_hidden;
                let snapshot = elements::snapshot(&page).await?;
                let mut text = snapshot.text;
                if let Ok(mut tables) = self.ref_tables.lock() {
                    tables.insert(tab_id.clone(), snapshot.refs);
                }
                let mut truncated = false;
                if let Some(max) = max_elements {
                    let mut seen = 0usize;
                    let mut cut: Option<usize> = None;
                    for (idx, _) in text.match_indices("[ref=") {
                        seen += 1;
                        if seen > max as usize {
                            cut = Some(idx);
                            break;
                        }
                    }
                    if let Some(idx) = cut {
                        text.truncate(idx);
                        truncated = true;
                    }
                }
                inner.refresh_meta(&tab_id).await;
                let url = elements::page_url(&page).await;
                let title = inner
                    .tabs
                    .get(&tab_id)
                    .map(|info| info.title.clone())
                    .unwrap_or_default();
                Ok(CommandOutcome::ok_value(
                    json!({
                        "url": url,
                        "title": title,
                        "elements": text,
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
                let mut builder =
                    CaptureScreenshotParams::builder().format(CaptureScreenshotFormat::Png);
                if let Some(rect) = clip {
                    builder = builder
                        .clip(ClipViewport {
                            x: rect.x,
                            y: rect.y,
                            width: rect.width,
                            height: rect.height,
                            scale: 1.0,
                        })
                        .capture_beyond_viewport(true);
                } else if full_page.unwrap_or(false) {
                    let metrics = page
                        .execute(GetLayoutMetricsParams::default())
                        .await
                        .map_err(|error| format!("取页面尺寸失败: {error}"))?;
                    let size = metrics.result.css_content_size;
                    builder = builder
                        .clip(ClipViewport {
                            x: 0.0,
                            y: 0.0,
                            width: size.width,
                            height: size.height,
                            scale: 1.0,
                        })
                        .capture_beyond_viewport(true);
                }
                let result = page
                    .execute(builder.build())
                    .await
                    .map_err(|error| format!("截图失败: {error}"))?;
                let base64: &str = result.data.as_ref();
                Ok(CommandOutcome {
                    ok: true,
                    error: None,
                    value: None,
                    state: None,
                    image: Some(crate::ImagePayload {
                        base64: base64.to_string(),
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
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let button = parse_button(button.as_deref());
                let click_count = if double_click.unwrap_or(false) { 2 } else { 1 };
                match pick_target(r#ref.as_deref(), locator.as_deref()) {
                    Some(target) => {
                        let backend =
                            elements::resolve_element(&page, &self.ref_table(&tab_id), &target)
                                .await?;
                        elements::click_backend(&page, backend, button, click_count).await?;
                        Ok(CommandOutcome::ok_value(json!({ "clicked": target }), 0))
                    }
                    None => {
                        let x = x.ok_or_else(|| "click 需要 ref/locator 或 x,y".to_string())?;
                        let y = y.ok_or_else(|| "click 需要 ref/locator 或 x,y".to_string())?;
                        elements::click_point(&page, x, y, button, click_count).await?;
                        Ok(CommandOutcome::ok_value(json!({ "clickedAt": [x, y] }), 0))
                    }
                }
            }
            BrowserCommand::Fill {
                tab_id,
                r#ref,
                value,
            } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let target = elements::normalize_ref(&r#ref);
                let backend = elements::resolve_element(&page, &self.ref_table(&tab_id), &target)
                    .await?;
                elements::fill_backend(&page, backend, &value).await?;
                Ok(CommandOutcome::ok_value(json!({ "filled": target }), 0))
            }
            BrowserCommand::Type {
                tab_id,
                r#ref,
                text,
            } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                if let Some(reference) = r#ref.as_deref().map(elements::normalize_ref) {
                    let backend =
                        elements::resolve_element(&page, &self.ref_table(&tab_id), &reference)
                            .await?;
                    elements::focus_backend(&page, backend).await?;
                }
                elements::type_text(&page, &text).await?;
                Ok(CommandOutcome::ok_value(json!({ "typed": text.len() }), 0))
            }
            BrowserCommand::Press { tab_id, key, r#ref } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                if let Some(reference) = r#ref.as_deref().map(elements::normalize_ref) {
                    let backend =
                        elements::resolve_element(&page, &self.ref_table(&tab_id), &reference)
                            .await?;
                    elements::focus_backend(&page, backend).await?;
                }
                elements::press_key(&page, &key).await?;
                Ok(CommandOutcome::ok_value(json!({ "pressed": key }), 0))
            }
            BrowserCommand::Hover { tab_id, r#ref, x, y } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                match (r#ref.as_deref().map(elements::normalize_ref), x, y) {
                    (Some(reference), _, _) => {
                        let backend =
                            elements::resolve_element(&page, &self.ref_table(&tab_id), &reference)
                                .await?;
                        elements::hover_backend(&page, backend).await?;
                        Ok(CommandOutcome::ok_value(json!({ "hovered": reference }), 0))
                    }
                    (None, Some(x), Some(y)) => {
                        elements::hover_point(&page, x, y).await?;
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
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                match r#ref.as_deref().map(elements::normalize_ref) {
                    Some(reference) => {
                        let backend =
                            elements::resolve_element(&page, &self.ref_table(&tab_id), &reference)
                                .await?;
                        elements::scroll_into_view(&page, backend).await?;
                        Ok(CommandOutcome::ok_value(json!({ "scrolledTo": reference }), 0))
                    }
                    None => {
                        let dx = delta_x.unwrap_or(0.0);
                        let dy = delta_y.unwrap_or(360.0);
                        match (x, y) {
                            (Some(x), Some(y)) => {
                                elements::wheel(&page, x, y, dx, dy).await?;
                            }
                            _ => {
                                elements::wheel_at_center(&page, dx, dy).await?;
                            }
                        }
                        Ok(CommandOutcome::ok_value(json!({ "scrolled": [dx, dy] }), 0))
                    }
                }
            }
            BrowserCommand::Select {
                tab_id,
                r#ref,
                values,
            } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let target = elements::normalize_ref(&r#ref);
                let backend = elements::resolve_element(&page, &self.ref_table(&tab_id), &target)
                    .await?;
                let selected = elements::select_backend(&page, backend, &values).await?;
                Ok(CommandOutcome::ok_value(json!({ "selected": selected }), 0))
            }
            BrowserCommand::Check {
                tab_id,
                r#ref,
                checked,
            } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let target = elements::normalize_ref(&r#ref);
                let backend = elements::resolve_element(&page, &self.ref_table(&tab_id), &target)
                    .await?;
                let state =
                    elements::check_backend(&page, backend, checked.unwrap_or(true)).await?;
                Ok(CommandOutcome::ok_value(json!({ "checked": state }), 0))
            }

            BrowserCommand::Drag {
                tab_id,
                from_ref,
                to_ref,
                from,
                to,
            } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let refs = self.ref_table(&tab_id);
                match (from_ref, to_ref) {
                    (Some(from_ref), Some(to_ref)) => {
                        let from_target = elements::normalize_ref(&from_ref);
                        let to_target = elements::normalize_ref(&to_ref);
                        let from_backend =
                            elements::resolve_element(&page, &refs, &from_target).await?;
                        let to_backend =
                            elements::resolve_element(&page, &refs, &to_target).await?;
                        let _ = elements::scroll_into_view(&page, from_backend).await;
                        let start = elements::element_center(&page, from_backend).await?;
                        let end = elements::element_center(&page, to_backend).await?;
                        elements::drag(&page, start, end).await?;
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
                        elements::drag(&page, (from.x, from.y), (to.x, to.y)).await?;
                        Ok(CommandOutcome::ok_value(json!({ "dragged": true }), 0))
                    }
                }
            }
            BrowserCommand::ElementInfo { tab_id, x, y } => {
                let (_tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let script = format!(
                    "() => {{ const el = document.elementFromPoint({x}, {y}); \
                     if (!el) return null; const r = el.getBoundingClientRect(); \
                     return {{ tag: el.tagName, id: el.id, cls: el.className, \
                     text: (el.innerText||'').slice(0,200), \
                     rect: {{x:r.x,y:r.y,width:r.width,height:r.height}} }}; }}"
                );
                let info = elements::evaluate(&page, &script).await?;
                Ok(CommandOutcome::ok_value(info, 0))
            }
            BrowserCommand::Evaluate { tab_id, expression } => {
                let (_tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let result = elements::evaluate(&page, &expression).await?;
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
                let timeout = Duration::from_millis(timeout_ms.unwrap_or(30_000));
                if let Some(selector) = selector {
                    let escaped = escape_js_string(&selector);
                    let script = format!("() => !!document.querySelector('{escaped}')");
                    wait_for_condition(&page, &script, timeout)
                        .await
                        .map_err(|error| format!("等待选择器失败({selector}): {error}"))?;
                    Ok(CommandOutcome::ok_value(json!({ "found": selector }), 0))
                } else if let Some(text) = text {
                    let escaped = escape_js_string(&text);
                    let script = format!(
                        "() => document.body && document.body.innerText.includes('{escaped}')"
                    );
                    wait_for_condition(&page, &script, timeout)
                        .await
                        .map_err(|error| format!("等待文本失败({text}): {error}"))?;
                    Ok(CommandOutcome::ok_value(json!({ "foundText": text }), 0))
                } else if let Some(text) = text_gone {
                    let escaped = escape_js_string(&text);
                    let script = format!(
                        "() => !(document.body && document.body.innerText.includes('{escaped}'))"
                    );
                    wait_for_condition(&page, &script, timeout)
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
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let Some(_info) = self.take_dialog(&tab_id) else {
                    return Ok(CommandOutcome::err(
                        "not_found",
                        format!("tab {tab_id} 没有挂起的弹窗(先 getDialog 确认)"),
                        0,
                    ));
                };
                let mut builder = HandleJavaScriptDialogParams::builder().accept(accept);
                if let Some(prompt) = prompt_text {
                    builder = builder.prompt_text(prompt);
                }
                page.execute(builder.build().unwrap())
                    .await
                    .map_err(|error| format!("处置弹窗失败: {error}"))?;
                drop(guard);
                self.broadcast(BrowserEvent::DialogClosed { tab_id });
                Ok(CommandOutcome::ok_value(json!({ "accepted": accept }), 0))
            }

            BrowserCommand::ViewportSet {
                tab_id,
                width,
                height,
            } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                page.execute(SetDeviceMetricsOverrideParams::new(
                    width as i64,
                    height as i64,
                    1.0,
                    false,
                ))
                .await
                .map_err(|error| format!("设置视口失败: {error}"))?;
                inner.viewports.insert(tab_id, (width, height));
                Ok(CommandOutcome::ok_value(
                    json!({ "width": width, "height": height }),
                    0,
                ))
            }
            BrowserCommand::ViewportReset { tab_id } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let (width, height) = self.spec.viewport;
                page.execute(SetDeviceMetricsOverrideParams::new(
                    width as i64,
                    height as i64,
                    1.0,
                    false,
                ))
                .await
                .map_err(|error| format!("重置视口失败: {error}"))?;
                inner.viewports.remove(&tab_id);
                Ok(CommandOutcome::ok_value(
                    json!({ "width": width, "height": height }),
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
                let (_tab_id, page) = inner.resolve(tab_id.as_deref())?;
                let kind = body_kind.unwrap_or_else(|| "response".to_string());
                if kind == "request" {
                    let post_data = match page
                        .execute(GetRequestPostDataParams::new(request_id.clone()))
                        .await
                    {
                        Ok(response) => Value::String(response.result.post_data),
                        Err(_) => Value::Null,
                    };
                    return Ok(CommandOutcome::ok_value(
                        json!({ "requestId": request_id, "body": post_data }),
                        0,
                    ));
                }
                // 响应体在响应刚发出后的短暂窗口内不可读(CDP 报
                // "No data found"),~100ms 后就绪;这里带延迟重试,
                // 覆盖"加载完立刻取 body"的真实使用场景。
                let mut last_error = String::new();
                for attempt in 0..4 {
                    if attempt > 0 {
                        tokio::time::sleep(Duration::from_millis(120)).await;
                    }
                    match page
                        .execute(GetResponseBodyParams::new(request_id.clone()))
                        .await
                    {
                        Ok(response) => {
                            return Ok(CommandOutcome::ok_value(
                                json!({
                                    "requestId": request_id,
                                    "body": response.body,
                                    "base64Encoded": response.base64_encoded,
                                }),
                                0,
                            ));
                        }
                        Err(error) => last_error = error.to_string(),
                    }
                }
                Ok(CommandOutcome::err(
                    "not_found",
                    format!(
                        "找不到请求 {request_id} 的响应体(可能已被清理或请求尚未完成): {last_error}"
                    ),
                    0,
                ))
            }
            BrowserCommand::StartScreencast { tab_id } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                // 幂等:前端在挂载/重连/tabs-changed 时都会调一次。
                if inner.screencast_tab.as_deref() == Some(tab_id.as_str()) {
                    return Ok(CommandOutcome::ok_value(json!({ "streaming": true }), 0));
                }
                // 单实例单画面:切 tab 时先停旧流。
                if let Some(prev) = inner.screencast_tab.clone()
                    && let Some(prev_page) = inner.pages.get(&prev).cloned()
                {
                    let _ = prev_page.execute(StopScreencastParams::default()).await;
                }
                page.execute(
                    StartScreencastParams::builder()
                        .format(StartScreencastFormat::Jpeg)
                        .quality(60)
                        .build(),
                )
                .await
                .map_err(|error| format!("开启画面失败: {error}"))?;
                inner.screencast_tab = Some(tab_id);
                // 踢一次重绘拿首帧(headless 静态页面否则没有合成器帧)。
                kick_repaint(&page).await;
                Ok(CommandOutcome::ok_value(json!({ "streaming": true }), 0))
            }
            BrowserCommand::StopScreencast { tab_id } => {
                let (tab_id, page) = inner.resolve(tab_id.as_deref())?;
                page.execute(StopScreencastParams::default())
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

/// 把 CDP newtype/枚举安全地转成字符串(与协议原文一致)。
fn serde_str_id<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
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
        .map(|duration| duration.as_millis() as u64)
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
    let _ = elements::evaluate(
        page,
        "() => { const el = document.documentElement; \
         el.style.opacity = el.style.opacity === '0.999' ? '' : '0.999'; }",
    )
    .await;
}

/// 解析鼠标按键。
fn parse_button(button: Option<&str>) -> MouseButton {
    match button.unwrap_or("left") {
        "right" => MouseButton::Right,
        "middle" => MouseButton::Middle,
        _ => MouseButton::Left,
    }
}

/// 从 ref / locator 里挑一个目标(ref 优先)。
fn pick_target(r#ref: Option<&str>, locator: Option<&str>) -> Option<String> {
    r#ref
        .map(elements::normalize_ref)
        .or_else(|| locator.map(|value| value.to_string()))
}

/// JS 字符串字面量转义(嵌入轮询脚本用)。
fn escape_js_string(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// 轮询等待页面 JS 条件为真(50ms 间隔;超时报错)。
async fn wait_for_condition(page: &Page, script: &str, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        match elements::evaluate(page, script).await {
            Ok(value) if value.as_bool().unwrap_or(false) => return Ok(()),
            Ok(_) => {}
            Err(error) => tracing::debug!("等待条件求值失败: {error}"),
        }
        if Instant::now() >= deadline {
            return Err(format!("超时({}ms)", timeout.as_millis()));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 等待页面完成加载(document.readyState === "complete")。
/// 超时不报错:回退/前进/刷新后尽力等待,让调用方拿到新的 url。
async fn wait_for_load(page: &Page, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let state = elements::evaluate(page, "document.readyState").await.ok();
        if state.as_ref().and_then(Value::as_str) == Some("complete") {
            return;
        }
        if Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

impl BrowserManager {
    /// 从网络日志里找最近一条主文档响应状态(navigate 返回用;拿不到给 null)。
    fn latest_document_status(&self, tab_id: &str) -> Value {
        let entries = self.network_list(tab_id, None, 200);
        entries
            .iter()
            .rev()
            .find(|entry| {
                entry.get("status").is_some()
                    && entry.get("resourceType").and_then(Value::as_str) == Some("Document")
            })
            .and_then(|entry| entry.get("status").cloned())
            .unwrap_or(Value::Null)
    }
}

/// 默认命令超时。
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(test)]
mod tests {
    use super::*;

    /// tab 归属登记：记录幂等、owner 之间隔离、take 清空、顺序确定。
    #[test]
    fn tab_owners_track_and_isolate_per_owner() {
        let owners = TabOwners::default();
        owners.record("child-a", "t2");
        owners.record("child-a", "t1");
        owners.record("child-a", "t1");
        owners.record("child-b", "t3");
        owners.record("parent", "t1");
        assert_eq!(
            owners.owned("child-a"),
            vec!["t1".to_string(), "t2".to_string()]
        );
        assert_eq!(owners.owned("child-b"), vec!["t3".to_string()]);
        // 多个 owner 共用一个 tab：各自持有自己的记录。
        assert_eq!(owners.owned("parent"), vec!["t1".to_string()]);
        // take 只清自己的那一份。
        assert_eq!(
            owners.take("child-a"),
            vec!["t1".to_string(), "t2".to_string()]
        );
        assert!(owners.owned("child-a").is_empty());
        assert_eq!(owners.owned("parent"), vec!["t1".to_string()]);
        assert_eq!(owners.owned("child-b"), vec!["t3".to_string()]);
        // 重复 take 无害。
        assert!(owners.take("child-a").is_empty());
        // 显式关闭后解除归属，避免重复关闭。
        owners.forget("child-b", "t3");
        assert!(owners.owned("child-b").is_empty());
        assert!(owners.owned("unknown").is_empty());
    }

    #[test]
    fn pick_target_prefers_ref_over_locator() {
        assert_eq!(pick_target(Some("e6"), Some("#x")).as_deref(), Some("e6"));
        // @ 前缀归一化(快照产出 e6,模型可能带 @ 传)。
        assert_eq!(pick_target(Some("@e12"), None).as_deref(), Some("e12"));
        assert_eq!(pick_target(None, Some("#x")).as_deref(), Some("#x"));
        assert_eq!(pick_target(None, Some("text=登录")).as_deref(), Some("text=登录"));
        assert_eq!(pick_target(None, None), None);
    }

    #[test]
    fn error_code_classifies_common_failures() {
        assert_eq!(error_code("导航超时"), "timeout");
        assert_eq!(error_code("request timeout"), "timeout");
        assert_eq!(error_code("目标已关闭"), "target_closed");
        assert_eq!(error_code("元素不存在"), "not_found");
        assert_eq!(error_code("其它错误"), "command_failed");
    }

    #[test]
    fn escape_handles_quotes_and_newlines() {
        assert_eq!(escape_js_string("a'b"), "a\\'b");
        assert_eq!(escape_js_string("a\nb"), "a\\nb");
        assert_eq!(escape_js_string("a\\b"), "a\\\\b");
    }

    #[test]
    fn parse_button_maps_known_names() {
        assert!(matches!(parse_button(None), MouseButton::Left));
        assert!(matches!(parse_button(Some("left")), MouseButton::Left));
        assert!(matches!(parse_button(Some("right")), MouseButton::Right));
        assert!(matches!(parse_button(Some("middle")), MouseButton::Middle));
        assert!(matches!(parse_button(Some("other")), MouseButton::Left));
    }
}
