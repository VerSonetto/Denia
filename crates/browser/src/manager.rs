//! 浏览器管理器:一个 Chrome 实例 + 多 tab,命令统一入口。
//!
//! 对照 ZCode:`BrowserGuestManager`(tab 注册表/命令分发)+ `BrowserTabResidencyCoordinator`
//! (活跃保护/LRU 驱逐)。v1 简化:tab 上限 32,超过按 lastActivity LRU 关闭最旧 tab。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;
use std::time::Instant;

use serde_json::{json, Value};

use crate::cdp::{CdpEvent, CdpHandle};
use crate::commands;
use crate::launch;
use crate::{BrowserCommand, CommandOutcome};

/// 每个 tab 的运行时状态。
#[derive(Debug, Clone)]
pub struct TabInfo {
    pub tab_id: String,
    /// CDP targetId。
    pub target_id: String,
    /// flatten session id(Target.attachToTarget 返回)。
    pub session_id: String,
    pub url: String,
    pub title: String,
    /// 最后活跃时刻(epoch ms):命令执行/用户激活都会刷新。
    pub last_activity_ms: u64,
}

pub(crate) struct Inner {
    pub(crate) handle: CdpHandle,
    pub(crate) event_rx: Option<tokio::sync::mpsc::UnboundedReceiver<CdpEvent>>,
    #[allow(dead_code)]
    pub(crate) process: tokio::process::Child,
    #[allow(dead_code)]
    pub(crate) executable: PathBuf,
    #[allow(dead_code)]
    pub(crate) profile_dir: PathBuf,
    /// tabId -> 状态。
    pub(crate) tabs: HashMap<String, TabInfo>,
    /// targetId -> tabId(事件反查)。
    #[allow(dead_code)]
    pub(crate) by_target: HashMap<String, String>,
    /// CDP flatten sessionId -> tabId(事件回流反查;sessionId ≠ tabId)。
    /// 挂起的 JS dialog(tabId -> {type,message})。
    pub(crate) dialogs: HashMap<String, Value>,
    /// viewport 覆盖(tabId -> {width,height})。
    #[allow(dead_code)]
    pub(crate) viewport: HashMap<String, (u32, u32)>,
    pub(crate) active_tab: Option<String>,
}

/// 共享管理器:命令入口 + 事件泵控制。
pub struct BrowserManager {
    inner: Mutex<Option<Inner>>,
    home: PathBuf,
    generation: AtomicU64,
    event_broadcast: tokio::sync::broadcast::Sender<BrowserEvent>,
    /// 挂起 JS dialog(tabId -> {type,message});事件泵写入,getDialog/handleDialog 读写。
    pending_dialogs: std::sync::Arc<std::sync::Mutex<HashMap<String, Value>>>,
    /// 网络抓包缓冲(事件泵写入,networkList/GetBody 读取)。
    network_log: std::sync::Arc<std::sync::Mutex<NetworkLog>>,
}

/// 面板订阅的推送事件。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum BrowserEvent {
    /// tab 列表/活跃 tab 变化。
    TabsChanged,
    /// 某个 tab 的新画面帧(页面 base64 JPEG)。
    Frame { tab_id: String, data: String },
    /// tab 出现 JS dialog。
    DialogOpened { tab_id: String, kind: String, message: String },
    /// dialog 被处理。
    DialogClosed { tab_id: String },
    /// tab 导航完成(刷新地址栏)。
    Navigated { tab_id: String, url: String, title: String },
    /// 浏览器实例退出。
    Exited,
}

const TAB_LIMIT: usize = 32;

/// 每 tab 保留的最大网络条目(环形,超出丢最旧)。
const NETWORK_LOG_CAP: usize = 200;

/// 网络抓包缓冲:tabId -> (插入序, requestId -> 条目)。
#[derive(Default)]
pub struct NetworkLog {
    /// tabId -> 有序 requestId(便于按时间列出与截断)。
    order: HashMap<String, Vec<(String, u64)>>,
    /// tabId -> requestId -> 条目。
    entries: HashMap<String, HashMap<String, Value>>,
    /// 全局单调序号(排序用)。
    counter: u64,
}

impl NetworkLog {
    fn record(&mut self, tab_id: &str, request_id: &str, entry: Value) {
        let order = self.order.entry(tab_id.to_string()).or_default();
        let is_new = !order.iter().any(|(id, _)| id == request_id);
        if is_new {
            self.counter += 1;
            order.push((request_id.to_string(), self.counter));
        }
        // merge:同一请求的多个事件(requestWillBeSent → responseReceived → loadingFailed)
        // 按字段合并,新值非 null 才覆盖,保留既有字段。
        let map = self.entries.entry(tab_id.to_string()).or_default();
        let slot = map
            .entry(request_id.to_string())
            .or_insert_with(|| entry.clone());
        if let (Some(existing), Some(incoming)) = (slot.as_object_mut(), entry.as_object()) {
            for (key, value) in incoming {
                if !value.is_null() {
                    existing.insert(key.clone(), value.clone());
                }
            }
        }
        // 环形截断。
        if order.len() > NETWORK_LOG_CAP {
            let excess = order.len() - NETWORK_LOG_CAP;
            let evicted: Vec<String> = order.drain(..excess).map(|(id, _)| id).collect();
            if let Some(map) = self.entries.get_mut(tab_id) {
                for id in evicted {
                    map.remove(&id);
                }
            }
        }
    }

    fn list(&self, tab_id: &str, url_filter: Option<&str>, max: u32) -> Vec<Value> {
        let Some(order) = self.order.get(tab_id) else {
            return Vec::new();
        };
        let map = self.entries.get(tab_id);
        let mut items: Vec<(u64, Value)> = order
            .iter()
            .filter_map(|(id, seq)| {
                let entry = map.and_then(|m| m.get(id))?;
                if let Some(filter) = url_filter {
                    let url = entry.get("url").and_then(Value::as_str).unwrap_or("");
                    if !url.contains(filter) {
                        return None;
                    }
                }
                Some((*seq, entry.clone()))
            })
            .collect();
        items.sort_by_key(|(seq, _)| *seq);
        let start = items.len().saturating_sub(max as usize);
        items.into_iter().skip(start).map(|(_, entry)| entry).collect()
    }

    fn get(&self, tab_id: &str, request_id: &str) -> Option<Value> {
        self.entries.get(tab_id).and_then(|m| m.get(request_id)).cloned()
    }

    fn clear(&mut self, tab_id: &str) {
        self.order.remove(tab_id);
        self.entries.remove(tab_id);
    }
}

impl BrowserManager {
    pub fn new(home: PathBuf) -> Self {
        let (event_broadcast, _) = tokio::sync::broadcast::channel(64);
        Self {
            inner: Mutex::new(None),
            home,
            generation: AtomicU64::new(0),
            event_broadcast,
            pending_dialogs: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            network_log: std::sync::Arc::new(std::sync::Mutex::new(NetworkLog::default())),
        }
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<BrowserEvent> {
        self.event_broadcast.subscribe()
    }

    /// 挂起 dialog 查询/清除(getDialog/handleDialog 用)。
    pub fn take_dialog(&self, tab_id: &str) -> Option<Value> {
        self.pending_dialogs.lock().unwrap().get(tab_id).cloned()
    }

    pub fn clear_dialog(&self, tab_id: &str) {
        self.pending_dialogs.lock().unwrap().remove(tab_id);
    }

    /// 网络抓包:列出该 tab 的请求条目(按发生顺序,环形缓冲上限内)。
    pub fn network_list(&self, tab_id: &str, url_filter: Option<&str>, max: u32) -> Vec<Value> {
        self.network_log.lock().unwrap().list(tab_id, url_filter, max)
    }

    /// 网络抓包:取单条条目。
    pub fn network_entry(&self, tab_id: &str, request_id: &str) -> Option<Value> {
        self.network_log.lock().unwrap().get(tab_id, request_id)
    }

    /// 网络抓包:清空某 tab 的缓冲。
    pub fn network_clear(&self, tab_id: &str) {
        self.network_log.lock().unwrap().clear(tab_id);
    }

    fn profile_dir(&self) -> PathBuf {
        crate::default_profile_dir(&self.home)
    }

    /// 确保浏览器实例在跑;掉线自动重启(登录态在 profile 里,不丢)。
    pub async fn ensure_running(&self) -> Result<(), String> {
        if self.is_healthy().await {
            return Ok(());
        }
        self.get_or_start().await
    }

    async fn is_healthy(&self) -> bool {
        let guard = self.inner.lock().await;
        matches!(guard.as_ref(), Some(inner) if !inner.handle.is_closed())
    }

    /// (重启)拉起浏览器实例:连接 + 事件泵 + 首个 tab。
    pub async fn get_or_start(&self) -> Result<(), String> {
        if self.is_healthy().await {
            return Ok(());
        }
        let executable = launch::locate_browser_executable()
            .ok_or_else(|| "未找到 Chrome/Edge,请安装后重试".to_string())?;
        let profile = self.profile_dir();
        let mut launched = launch::launch_headless(&executable, &profile).await?;
        // 有头模式端口监听可能晚于 DevToolsActivePort 落盘:指数退避重试连接。
        let mut connected_pair: Option<(CdpHandle, tokio::sync::mpsc::UnboundedReceiver<CdpEvent>)> =
            None;
        for attempt in 0..5u32 {
            match CdpHandle::connect(&launched.websocket_url).await {
                Ok(pair) => {
                    connected_pair = Some(pair);
                    break;
                }
                Err(error) => {
                    if attempt == 4 {
                        let _ = launched.process.kill().await;
                        return Err(error);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(300 * u64::from(attempt + 1))).await;
                }
            }
        }
        let (handle, event_rx) = connected_pair.expect("connect retry exhausted");

        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        tracing::info!(%generation, exe = %executable.display(), "内嵌浏览器已启动");

        let mut inner = Inner {
            handle,
            event_rx: Some(event_rx),
            process: launched.process,
            executable,
            profile_dir: profile,
            tabs: HashMap::new(),
            by_target: HashMap::new(),
            dialogs: HashMap::new(),
            viewport: HashMap::new(),
            active_tab: None,
        };

        // 同步现有 page targets(重启后 profile 恢复的会话)。
        Self::refresh_targets(&mut inner).await?;
        if inner.tabs.is_empty() {
            Self::create_tab_inner(&mut inner, None).await?;
        }
        let first = inner.tabs.keys().next().cloned();
        inner.active_tab = first;

        let event_broadcast = self.event_broadcast.clone();
        let handle_for_pump = inner.handle.clone();
        let inner_event_rx = inner.event_rx.take().expect("event_rx 只取一次");
        {
            let mut guard = self.inner.lock().await;
            *guard = Some(inner);
        }
        // 事件泵:掉线→广播 Exited;dialog/screencast/导航→广播。
        let pending_dialogs = self.pending_dialogs.clone();
        let network_log = self.network_log.clone();
        tokio::spawn(async move {
            Self::event_pump(handle_for_pump, inner_event_rx, event_broadcast, pending_dialogs, network_log)
                .await;
        });
        let _ = self.event_broadcast.send(BrowserEvent::TabsChanged);
        Ok(())
    }

    /// 同步 CDP target 列表到本地 tab 表(page 类型)。
    async fn refresh_targets(inner: &mut Inner) -> Result<(), String> {
        let _ = inner
            .handle
            .send("Target.setDiscoverTargets", json!({"discover": true}))
            .await;
        let targets = inner.handle.send("Target.getTargets", json!({})).await?;
        for info in targets
            .get("targetInfos")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            if info.get("type").and_then(Value::as_str) != Some("page") {
                continue;
            }
            let target_id = info
                .get("targetId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if target_id.is_empty() || inner.by_target.contains_key(&target_id) {
                continue;
            }
            let url = info
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("about:blank")
                .to_string();
            let title = info
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let _ = Self::attach_session(inner, target_id.clone(), url, title).await;
        }
        Ok(())
    }

    /// 给 target 建 flatten session 并登记。tabId 直接采用 CDP flatten sessionId:
    /// 事件回流天然匹配(避免 pump 里再反查),且会话期间稳定。
    async fn attach_session(
        inner: &mut Inner,
        target_id: String,
        url: String,
        title: String,
    ) -> Result<String, String> {
        let attached = inner
            .handle
            .send(
                "Target.attachToTarget",
                json!({"targetId": target_id, "flatten": true}),
            )
            .await?;
        let session_id = attached
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or("attachToTarget 未返回 sessionId")?
            .to_string();
        let tab_id = session_id.clone();
        let _ = inner
            .handle
            .send_with_session("Page.enable", json!({}), Some(&session_id))
            .await;
        let _ = inner
            .handle
            .send_with_session("Runtime.enable", json!({}), Some(&session_id))
            .await;
        let _ = inner
            .handle
            .send_with_session(
                "Network.enable",
                json!({"maxTotalBufferSize": 10_000_000, "maxResourceBufferSize": 5_000_000}),
                Some(&session_id),
            )
            .await;
        inner.tabs.insert(
            tab_id.clone(),
            TabInfo {
                tab_id: tab_id.clone(),
                target_id: target_id.clone(),
                session_id: session_id.clone(),
                url,
                title,
                last_activity_ms: now_ms(),
            },
        );
        inner.by_target.insert(target_id, tab_id.clone());
        Ok(tab_id)
    }

    pub(crate) async fn create_tab_inner(inner: &mut Inner, url: Option<&str>) -> Result<String, String> {
        let created = inner
            .handle
            .send(
                "Target.createTarget",
                json!({"url": url.unwrap_or("about:blank")}),
            )
            .await?;
        let target_id = created
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or("createTarget 未返回 targetId")?
            .to_string();
        Self::attach_session(
            inner,
            target_id,
            url.unwrap_or("about:blank").to_string(),
            String::new(),
        )
        .await
    }

    /// 命令执行入口(工具与 REST API 共用)。
    ///
    /// 只读/善后命令不自发拉起浏览器:浏览器没跑时直接报 backend_unavailable,
    /// 避免面板打开/关闭就带起一个 Chrome 进程。
    pub async fn execute(&self, command: BrowserCommand) -> CommandOutcome {
        let started = Instant::now();
        if Self::is_passive(&command) && !self.is_healthy().await {
            return CommandOutcome::err("backend_unavailable", "浏览器未运行", elapsed(&started));
        }
        if let Err(error) = self.get_or_start().await {
            return CommandOutcome::err("backend_unavailable", error, elapsed(&started));
        }
        let outcome = {
            let mut guard = self.inner.lock().await;
            let Some(inner) = guard.as_mut() else {
                return CommandOutcome::err("backend_unavailable", "浏览器未就绪", elapsed(&started));
            };
            commands::dispatch(self, inner, command).await
        };
        self.maybe_evict_tabs().await;
        outcome
    }

    /// tab 上限:超出按 LRU 关闭最旧的非活跃 tab。
    async fn maybe_evict_tabs(&self) {
        let mut guard = self.inner.lock().await;
        let Some(inner) = guard.as_mut() else { return };
        if inner.tabs.len() <= TAB_LIMIT {
            return;
        }
        let mut victims: Vec<(String, u64)> = inner
            .tabs
            .iter()
            .filter(|(id, _)| Some(id.to_string()) != inner.active_tab)
            .map(|(id, info)| (id.clone(), info.last_activity_ms))
            .collect();
        victims.sort_by_key(|(_, at)| *at);
        let excess = inner.tabs.len() - TAB_LIMIT;
        for (tab_id, _) in victims.into_iter().take(excess) {
            if let Some(info) = inner.tabs.remove(&tab_id) {
                let _ = inner
                    .handle
                    .send("Target.closeTarget", json!({"targetId": info.target_id}))
                    .await;
                inner.by_target.remove(&info.target_id);
            }
        }
        let _ = self.event_broadcast.send(BrowserEvent::TabsChanged);
    }

    /// 事件泵:消费 CDP 事件,广播给面板。
    async fn event_pump(
        handle: CdpHandle,
        mut event_rx: tokio::sync::mpsc::UnboundedReceiver<CdpEvent>,
        broadcast: tokio::sync::broadcast::Sender<BrowserEvent>,
        pending_dialogs: std::sync::Arc<std::sync::Mutex<HashMap<String, Value>>>,
        network_log: std::sync::Arc<std::sync::Mutex<NetworkLog>>,
    ) {
        while let Some(event) = event_rx.recv().await {
            // flatten 事件带 CDP sessionId;反查成我们的 tabId(会话期间恒定)。
            let session_id = event.session_id.clone().unwrap_or_default();
            let tab_id = session_id.clone();
            match event.method.as_str() {
                "Page.screencastFrame" => {
                    let data = event
                        .params
                        .get("data")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let ack_id = event.params.get("sessionId").cloned().unwrap_or(Value::Null);
                    // 必须回执,否则 Chrome 停发帧。
                    if !tab_id.is_empty() {
                        let _ = handle
                            .send_with_session(
                                "Page.screencastFrameAck",
                                json!({"sessionId": ack_id}),
                                Some(&tab_id),
                            )
                            .await;
                        if !data.is_empty() {
                            let _ = broadcast.send(BrowserEvent::Frame { tab_id, data });
                        }
                    }
                }
                // ---- 网络抓包:按 requestId 聚合请求/响应/失败 ----
                "Network.requestWillBeSent" => {
                    let _ = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(std::env::temp_dir().join("denia-net-debug.log"))
                        .map(|mut file| {
                            use std::io::Write;
                            let _ = writeln!(
                                file,
                                "reqWillBeSent tab={tab_id}",
                            );
                        });
                    let request_id = event
                        .params
                        .get("requestId")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if request_id.is_empty() || tab_id.is_empty() {
                        continue;
                    }
                    let url = event.params
                        .pointer("/request/url")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let method = event.params
                        .pointer("/request/method")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let headers = event.params.pointer("/request/headers").cloned().unwrap_or(Value::Null);
                    let post_data = event.params
                        .pointer("/request/postData")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    network_log.lock().unwrap().record(
                        &tab_id,
                        &request_id,
                        json!({
                            "requestId": request_id,
                            "url": url,
                            "method": method,
                            "requestHeaders": headers,
                            "postData": post_data,
                            "status": Value::Null,
                            "responseHeaders": Value::Null,
                            "mimeType": Value::Null,
                            "failed": Value::Null,
                        }),
                    );
                }
                "Network.responseReceived" => {
                    let request_id = event
                        .params
                        .get("requestId")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if request_id.is_empty() || tab_id.is_empty() {
                        continue;
                    }
                    network_log.lock().unwrap().record(
                        &tab_id,
                        &request_id,
                        json!({
                            "requestId": request_id,
                            "status": event.params.pointer("/response/status").cloned().unwrap_or(Value::Null),
                            "statusText": event.params.pointer("/response/statusText").cloned().unwrap_or(Value::Null),
                            "responseHeaders": event.params.pointer("/response/headers").cloned().unwrap_or(Value::Null),
                            "mimeType": event.params.pointer("/response/mimeType").cloned().unwrap_or(Value::Null),
                        }),
                    );
                }
                "Network.loadingFailed" => {
                    let request_id = event
                        .params
                        .get("requestId")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if request_id.is_empty() || tab_id.is_empty() {
                        continue;
                    }
                    let error_text = event
                        .params
                        .get("errorText")
                        .and_then(Value::as_str)
                        .unwrap_or("failed")
                        .to_string();
                    network_log.lock().unwrap().record(
                        &tab_id,
                        &request_id,
                        json!({ "requestId": request_id, "failed": error_text }),
                    );
                }
                // WebSocket 帧:按 ws 会话聚合到 networkLog(requestId=ws:<id>)。
                "Network.webSocketCreated" => {
                    let ws_id = event
                        .params
                        .get("requestId")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if ws_id.is_empty() || tab_id.is_empty() {
                        continue;
                    }
                    let url = event
                        .params
                        .get("url")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    network_log.lock().unwrap().record(
                        &tab_id,
                        &format!("ws:{ws_id}"),
                        json!({
                            "requestId": format!("ws:{ws_id}"),
                            "url": url,
                            "method": "WS",
                            "status": 101,
                            "mimeType": "websocket",
                            "wsFrames": [],
                        }),
                    );
                }
                "Network.webSocketFrameReceived" | "Network.webSocketFrameSent" => {
                    let ws_id = event
                        .params
                        .get("requestId")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if ws_id.is_empty() || tab_id.is_empty() {
                        continue;
                    }
                    let direction = if event.method.ends_with("Sent") { "sent" } else { "received" };
                    let payload = event
                        .params
                        .pointer("/response/payloadData")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let key = format!("ws:{ws_id}");
                    let Some(entry) = network_log.lock().unwrap().get(&tab_id, &key) else {
                        continue;
                    };
                    let mut frames = entry
                        .get("wsFrames")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    if frames.len() < 100 {
                        frames.push(json!({ "dir": direction, "payload": payload }));
                    }
                    network_log.lock().unwrap().record(&tab_id, &key, json!({ "wsFrames": frames }));
                }
                "Page.javascriptDialogOpening" => {
                    let kind = event
                        .params
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("alert")
                        .to_string();
                    let message = event
                        .params
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    pending_dialogs.lock().unwrap().insert(
                        tab_id.clone(),
                        json!({ "type": kind, "message": message }),
                    );
                    let _ = broadcast.send(BrowserEvent::DialogOpened {
                        tab_id: tab_id.clone(),
                        kind,
                        message,
                    });
                }
                "Page.javascriptDialogClosed" => {
                    pending_dialogs.lock().unwrap().remove(&tab_id);
                    let _ = broadcast.send(BrowserEvent::DialogClosed { tab_id });
                }
                "Page.frameNavigated" | "Page.navigatedWithinDocument" => {
                    let url = event
                        .params
                        .pointer("/frame/url")
                        .or_else(|| event.params.get("url"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if !url.is_empty() && !tab_id.is_empty() {
                        let _ = broadcast.send(BrowserEvent::Navigated {
                            tab_id,
                            url,
                            title: String::new(),
                        });
                    }
                }
                _ => {}
            }
        }
        let _ = broadcast.send(BrowserEvent::Exited);
    }

    /// 广播事件(命令执行后调用)。
    pub fn broadcast(&self, event: BrowserEvent) {
        let _ = self.event_broadcast.send(event);
    }

    /// 当前状态快照(面板用)。
    pub async fn snapshot_state(&self) -> Value {
        let guard = self.inner.lock().await;
        let Some(inner) = guard.as_ref() else {
            return json!({"running": false, "tabs": []});
        };
        let tabs: Vec<Value> = inner
            .tabs
            .values()
            .map(|info| {
                json!({
                    "tabId": info.tab_id,
                    "url": info.url,
                    "title": info.title,
                    "active": Some(&info.tab_id) == inner.active_tab.as_ref(),
                })
            })
            .collect();
        json!({
            "running": true,
            "activeTabId": inner.active_tab,
            "tabs": tabs,
            "dialogs": inner.dialogs.values().cloned().collect::<Vec<_>>(),
        })
    }

    pub fn home(&self) -> PathBuf {
        self.home.clone()
    }
}

impl BrowserManager {
    /// 不应自发拉起浏览器的命令:面板打开/关闭、轮询状态时发的那些。
    fn is_passive(command: &BrowserCommand) -> bool {
        matches!(
            command,
            BrowserCommand::GetState { .. }
                | BrowserCommand::List
                | BrowserCommand::GetDialog { .. }
                | BrowserCommand::StartScreencast { .. }
                | BrowserCommand::StopScreencast { .. }
        )
    }
}

pub(crate) fn now_ms() -> u64 {    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) fn elapsed(started: &Instant) -> u64 {
    started.elapsed().as_millis() as u64
}


/// 供 commands.rs 使用的执行上下文。
pub(crate) struct Ctx<'a> {
    pub manager: &'a BrowserManager,
    pub inner: &'a mut Inner,
}

impl<'a> Ctx<'a> {
    /// 解析目标 tab(缺省 = active);刷新活跃时间。
    pub(crate) fn resolve_tab(&mut self, tab_id: Option<&str>) -> Result<String, CommandOutcome> {
        let started = Instant::now();
        let tab_id = match tab_id {
            Some(id) => id.to_string(),
            None => self
                .inner
                .active_tab
                .clone()
                .ok_or_else(|| CommandOutcome::err("no_tab", "没有打开的 tab", elapsed(&started)))?,
        };
        if !self.inner.tabs.contains_key(&tab_id) {
            return Err(CommandOutcome::err(
                "tab_not_found",
                format!("tab {tab_id} 不存在"),
                elapsed(&started),
            ));
        }
        if let Some(entry) = self.inner.tabs.get_mut(&tab_id) {
            entry.last_activity_ms = now_ms();
        }
        Ok(tab_id)
    }

    pub(crate) fn session_of(&self, tab_id: &str) -> Option<String> {
        self.inner.tabs.get(tab_id).map(|info| info.session_id.clone())
    }

    /// 给某个 tab 执行 CDP 命令。
    pub(crate) async fn cdp(
        &mut self,
        tab_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, CommandOutcome> {
        let started = Instant::now();
        let Some(session) = self.session_of(tab_id) else {
            return Err(CommandOutcome::err(
                "tab_not_found",
                format!("tab {tab_id} 不存在"),
                elapsed(&started),
            ));
        };
        self.inner
            .handle
            .send_with_session(method, params, Some(&session))
            .await
            .map_err(|error| CommandOutcome::err("cdp_error", error, elapsed(&started)))
    }

    /// 在页面上求值并解析 JSON 字符串结果。
    pub(crate) async fn eval_json(
        &mut self,
        tab_id: &str,
        expression: &str,
    ) -> Result<Value, CommandOutcome> {
        let result = self
            .cdp(
                tab_id,
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true,
                    "userGesture": true,
                }),
            )
            .await?;
        if let Some(details) = result.get("exceptionDetails") {
            let text = details
                .pointer("/exception/description")
                .or_else(|| details.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("页面脚本执行失败")
                .to_string();
            return Err(CommandOutcome::err("execution_error", text, 0));
        }
        let value = result.get("result").cloned().unwrap_or(Value::Null);
        let serialized = value
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or("null")
            .to_string();
        serde_json::from_str(&serialized)
            .map_err(|error| CommandOutcome::err("execution_error", format!("页面返回非法 JSON: {error}"), 0))
    }

    pub(crate) fn set_active(&mut self, tab_id: &str) {
        if self.inner.tabs.contains_key(tab_id) {
            self.inner.active_tab = Some(tab_id.to_string());
            if let Some(info) = self.inner.tabs.get_mut(tab_id) {
                info.last_activity_ms = now_ms();
            }
        }
    }

    pub(crate) fn broadcast_tabs_changed(&self) {
        self.manager.broadcast(BrowserEvent::TabsChanged);
    }
}
