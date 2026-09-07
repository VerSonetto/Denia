//! 浏览器管理器:一个 Chrome 实例 + 多 tab,命令统一入口。
//!
//! 对照 ZCode:`BrowserGuestManager`(tab 注册表/命令分发)+ `BrowserTabResidencyCoordinator`
//! (活跃保护/LRU 驱逐)。v1 简化:tab 上限 32,超过按 lastActivity LRU 关闭最旧 tab。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tokio::sync::Mutex;

use serde_json::{Value, json};

use crate::cdp::{CdpEvent, CdpHandle};
use crate::commands;
use crate::launch;
use crate::recon::ReconStore;
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
    /// 运行时实例(Arc 包一层:看护任务需要独立持有,收尸时整体取走)。
    inner: std::sync::Arc<Mutex<Option<Inner>>>,
    /// 启动互斥:并发 execute 同时发现掉线时,只放一个进 get_or_start,
    /// 其余等它完成后重查状态(否则会拉起两个 Chrome 抢同一 profile)。
    starting: tokio::sync::Mutex<()>,
    /// 串行化命令的完整生命周期，避免并发命令在 tab 刚关闭/重连时
    /// 交叉使用失效的 CDP session。
    command_gate: tokio::sync::Mutex<()>,
    generation: AtomicU64,
    event_broadcast: tokio::sync::broadcast::Sender<BrowserEvent>,
    /// 断线重启后恢复导航视图用:上次活跃 tab 的 URL(跨实例存活)。
    /// 重启后 profile 恢复的 tab 里 URL 匹配的就优先激活,模型视角不变。
    last_active_url: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// 挂起 JS dialog(tabId -> {type,message});事件泵写入,getDialog/handleDialog 读写。
    pending_dialogs: std::sync::Arc<std::sync::Mutex<HashMap<String, Value>>>,
    /// 网络抓包缓冲(事件泵写入,networkList/GetBody 读取)。
    network_log: std::sync::Arc<std::sync::Mutex<NetworkLog>>,
    /// JS 逆向侦察状态(脚本表/断点/暂停态/控制台)。
    recon: Arc<ReconStore>,
    /// 导航视图(tabId -> (url, title)):事件泵写 url,命令写 url+title;
    /// 快照/列表合并覆盖 TabInfo 的启动时值,保证 tab 栏/地址栏即时刷新。
    tab_views: std::sync::Arc<std::sync::Mutex<HashMap<String, (String, String)>>>,
    /// 当前 screencast 流归属的 tab(事件泵按它过滤帧;StartScreencast 迁移)。
    screencast_tab: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// 事件泵掉线时置位的信号(watch 通道):通知管理器把 Chrome 进程
    /// 与 per-tab 残留状态清干净,别等下一次 ensure_running 才收尸。
    exited_ping: tokio::sync::watch::Sender<bool>,
    /// 正在/已经收尸(事件泵看护或 Drop 只执行一次)。
    shutting_down: std::sync::atomic::AtomicBool,
    /// 启动时暂存的 profile 路径(Drop 收尸用;真实实例的在 Inner 里)。
    home: PathBuf,
}

impl Drop for BrowserManager {
    fn drop(&mut self) {
        // server 关闭时兜底:同步清掉本 profile 的 Chrome 进程,保证
        // "所有资源彻底释放"在进程退出路径上也成立(不看护任务的脸色)。
        // 异步 Inner.process 的 kill_on_drop 仍会处理主进程;这里清子进程。
        if !self.shutting_down.swap(true, Ordering::SeqCst) {
            let profile = crate::default_profile_dir(&self.home);
            launch::shutdown_profile_processes_sync(&profile);
        }
    }
}

impl BrowserManager {
    /// 事件泵掉线收尾:终止 Chrome 进程树 + 清 per-tab 状态 + 重置 watch。
    async fn on_pump_exit(&self) {
        reap_browser_instance(
            &self.inner,
            &self.pending_dialogs,
            &self.network_log,
            &self.recon,
            &self.tab_views,
            &self.screencast_tab,
        )
        .await;
        let _ = self.exited_ping.send(true);
    }
}

/// 收尸:杀掉该实例的 Chrome 进程树并清空 per-tab 状态(看护任务与
/// shutdown 共用;幂等,重复调用只清空数据,不会重复杀进程)。
async fn reap_browser_instance(
    inner_slot: &std::sync::Arc<Mutex<Option<Inner>>>,
    pending_dialogs: &std::sync::Arc<std::sync::Mutex<HashMap<String, Value>>>,
    network_log: &std::sync::Arc<std::sync::Mutex<NetworkLog>>,
    recon: &Arc<ReconStore>,
    tab_views: &std::sync::Arc<std::sync::Mutex<HashMap<String, (String, String)>>>,
    screencast_tab: &std::sync::Arc<std::sync::Mutex<Option<String>>>,
) {
    let exited = { inner_slot.lock().await.take() };
    if let Some(inner) = exited {
        // Chrome 是多进程:只 kill 主进程会留下渲染器子进程继续挤占
        // profile,下次启动即"启动即退出"。按 profile 命令行整组清。
        let mut process = inner.process;
        let _ = process.kill().await;
        let profile = inner.profile_dir.clone();
        let _ = tokio::task::spawn_blocking(move || {
            crate::launch::shutdown_profile_processes_sync(&profile)
        })
        .await;
    }
    // 状态与 CDP 会话一起失效:dialog/网络缓冲/导航视图/侦察/screencast 流全清掉。
    pending_dialogs.lock().unwrap().clear();
    network_log.lock().unwrap().clear_all();
    tab_views.lock().unwrap().clear();
    *screencast_tab.lock().unwrap() = None;
    recon.clear_all();
}

/// 面板订阅的推送事件。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum BrowserEvent {
    /// tab 列表/活跃 tab 变化。
    TabsChanged,
    /// 某个 tab 的新画面帧(页面 base64 JPEG);viewport 为页面 CSS 视口尺寸
    /// (screencast metadata),前端据此把画面点击/滚轮坐标换算成 CDP 坐标。
    Frame {
        tab_id: String,
        data: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        viewport: Option<FrameViewport>,
    },
    /// tab 出现 JS dialog。
    DialogOpened {
        tab_id: String,
        kind: String,
        message: String,
    },
    /// dialog 被处理。
    DialogClosed { tab_id: String },
    /// tab 导航完成(刷新地址栏)。
    Navigated {
        tab_id: String,
        url: String,
        title: String,
    },
    /// 逆向:调试器暂停(断点命中/异常;frames 为帧摘要数组)。
    DebuggerPaused {
        tab_id: String,
        reason: String,
        frames: Vec<serde_json::Value>,
    },
    /// 逆向:调试器恢复执行。
    DebuggerResumed { tab_id: String },
    /// 浏览器实例退出。
    Exited,
}

/// screencast 帧附带的页面视口尺寸(CSS 像素),前端坐标换算用。
#[derive(Debug, Clone, serde::Serialize)]
pub struct FrameViewport {
    pub width: u32,
    pub height: u32,
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
        items
            .into_iter()
            .skip(start)
            .map(|(_, entry)| entry)
            .collect()
    }

    fn get(&self, tab_id: &str, request_id: &str) -> Option<Value> {
        self.entries
            .get(tab_id)
            .and_then(|m| m.get(request_id))
            .cloned()
    }

    fn clear(&mut self, tab_id: &str) {
        self.order.remove(tab_id);
        self.entries.remove(tab_id);
    }

    /// 浏览器实例退出后全部作废(内存释放)。
    fn clear_all(&mut self) {
        self.order.clear();
        self.entries.clear();
    }
}

impl BrowserManager {
    pub fn new(home: PathBuf) -> Self {
        let (event_broadcast, _) = tokio::sync::broadcast::channel(64);
        Self {
            inner: std::sync::Arc::new(Mutex::new(None)),
            starting: tokio::sync::Mutex::new(()),
            command_gate: tokio::sync::Mutex::new(()),
            generation: AtomicU64::new(0),
            event_broadcast,
            pending_dialogs: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            network_log: std::sync::Arc::new(std::sync::Mutex::new(NetworkLog::default())),
            recon: ReconStore::new(),
            tab_views: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            screencast_tab: std::sync::Arc::new(std::sync::Mutex::new(None)),
            last_active_url: std::sync::Arc::new(std::sync::Mutex::new(None)),
            exited_ping: tokio::sync::watch::channel(false).0,
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            home: home.clone(),
        }
    }

    /// 记录当前活跃 tab 的 URL(断线重启后恢复导航视图用)。
    pub(crate) fn track_active_url(&self, url: &str) {
        if !url.is_empty() {
            *self.last_active_url.lock().unwrap() = Some(url.to_string());
        }
    }

    /// 命令路径回写导航视图(导航/reload/后退等成功后,tab 栏 url+title 即时刷新)。
    pub(crate) fn update_tab_view(&self, tab_id: &str, url: &str, title: &str) {
        self.tab_views
            .lock()
            .unwrap()
            .insert(tab_id.to_string(), (url.to_string(), title.to_string()));
    }

    /// tab 关闭时清理视图残留。
    pub(crate) fn forget_tab_view(&self, tab_id: &str) {
        self.tab_views.lock().unwrap().remove(tab_id);
    }

    /// 当前导航视图(快照合并用)。
    pub(crate) fn tab_view(&self, tab_id: &str) -> Option<(String, String)> {
        self.tab_views.lock().unwrap().get(tab_id).cloned()
    }

    /// 读取当前 screencast 流归属的 tab(StartScreencast 迁移判定用)。
    pub(crate) fn screencast_tab(&self) -> Option<String> {
        self.screencast_tab.lock().unwrap().clone()
    }

    /// 把 screencast 流归属切到指定 tab。
    pub(crate) fn set_screencast_tab(&self, tab_id: Option<&str>) {
        *self.screencast_tab.lock().unwrap() = tab_id.map(str::to_string);
    }

    /// JS 逆向侦察状态句柄(脚本表/断点/暂停/控制台)。
    pub fn recon(&self) -> Arc<ReconStore> {
        self.recon.clone()
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
        self.network_log
            .lock()
            .unwrap()
            .list(tab_id, url_filter, max)
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
    ///
    /// `starting` 锁防并发重入:两个命令同时发现掉线时,先到的拉实例,
    /// 后到的等锁再重查健康位(实例已被前者拉起就直接用,不再启动)。
    pub async fn get_or_start(&self) -> Result<(), String> {
        if self.is_healthy().await {
            return Ok(());
        }
        let _start_guard = self.starting.lock().await;
        // 拿到锁后再查一次:等锁期间别的调用方可能已经把实例拉起来了。
        if self.is_healthy().await {
            return Ok(());
        }
        let executable = launch::locate_browser_executable()
            .ok_or_else(|| "未找到 Chrome/Edge,请安装后重试".to_string())?;
        // 上一次会话可能留了没收的尸(事件泵掉线但看护任务还没跑完,
        // 或上次启动失败半途):启动前先把旧 Chrome 清干净再拉新的。
        self.on_pump_exit().await;
        let profile = self.profile_dir();
        // 启动失败常见于 Chrome 在退出竞态中仍持有 profile 锁；清场后
        // 只做一次受控重试，避免把瞬时启动竞态暴露给调用方。
        let mut launched = match launch::launch_headless(&executable, &profile).await {
            Ok(value) => value,
            Err(first_error) => {
                tracing::warn!(error = %first_error, "浏览器首次启动失败，清场后重试");
                launch::shutdown_profile_processes_sync(&profile);
                launch::launch_headless(&executable, &profile)
                    .await
                    .map_err(|second_error| format!("{first_error};重试失败: {second_error}"))?
            }
        };
        // 有头模式端口监听可能晚于 DevToolsActivePort 落盘:指数退避重试连接。
        let mut connected_pair: Option<(
            CdpHandle,
            tokio::sync::mpsc::UnboundedReceiver<CdpEvent>,
        )> = None;
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
                    tokio::time::sleep(std::time::Duration::from_millis(
                        300 * u64::from(attempt + 1),
                    ))
                    .await;
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
        // 断线重启后恢复导航视图:优先激活上次活跃 tab 对应的 URL
        // (profile 恢复的会话里 URL 相同),其次才取第一个 tab。
        let remembered = { self.last_active_url.lock().unwrap().clone() };
        let restored = remembered.and_then(|url| {
            inner
                .tabs
                .values()
                .find(|info| info.url == url || self.tab_view(&info.tab_id).map(|v| v.0) == Some(url.clone()))
                .map(|info| info.tab_id.clone())
        });
        let first = inner.tabs.keys().next().cloned();
        inner.active_tab = restored.or(first);

        let event_broadcast = self.event_broadcast.clone();
        let handle_for_pump = inner.handle.clone();
        let inner_event_rx = inner.event_rx.take().expect("event_rx 只取一次");
        {
            let mut guard = self.inner.lock().await;
            *guard = Some(inner);
        }
        // 事件泵:掉线→广播 Exited;dialog/screencast/导航/逆向侦察→广播。
        let pending_dialogs = self.pending_dialogs.clone();
        let network_log = self.network_log.clone();
        let recon = self.recon.clone();
        let tab_views = self.tab_views.clone();
        let screencast_tab_for_pump = self.screencast_tab.clone();
        let exited_pinger = self.exited_ping.clone();
        tokio::spawn(async move {
            Self::event_pump(
                handle_for_pump,
                inner_event_rx,
                event_broadcast,
                pending_dialogs,
                network_log,
                recon,
                tab_views,
                screencast_tab_for_pump,
                exited_pinger,
            )
            .await;
        });
        // 看护任务:事件泵退出(浏览器掉线)后立即收尸——杀 Chrome 进程树、
        // 清 per-tab 状态;不等下一次 ensure_running 才处理。所有需要共享的
        // 状态(inner/各状态桶)本就是 Arc/Mutex,收尸用独立函数,无需 Arc<Self>。
        let inner_slot = self.inner.clone();
        let pending_dialogs = self.pending_dialogs.clone();
        let network_log = self.network_log.clone();
        let recon = self.recon.clone();
        let tab_views = self.tab_views.clone();
        let screencast_tab_for_watch = self.screencast_tab.clone();
        let mut exited_rx = self.exited_ping.subscribe();
        tokio::spawn(async move {
            // borrow_and_update:订阅瞬间若已是退出态(pump 先于本任务退出的
            // 竞态),立即收尸;否则等下一次置位。
            if !*exited_rx.borrow_and_update() && exited_rx.changed().await.is_ok() {
                if !*exited_rx.borrow() {
                    return;
                }
            }
            reap_browser_instance(
                &inner_slot,
                &pending_dialogs,
                &network_log,
                &recon,
                &tab_views,
                &screencast_tab_for_watch,
            )
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
        // 同一 target 重连(重启/refresh_targets 重扫)时,先摘掉旧会话记录,
        // 避免旧 tabId 残留在 tab 表里泄漏(旧 session 已随断线失效)。
        if let Some(previous) = inner.by_target.remove(&target_id) {
            inner.tabs.remove(&previous);
        }
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
        // 调试器常开:scriptParsed 补发已加载脚本 → 源码检索/断点可用。
        // 不设断点则页面不会暂停,零干扰。
        let _ = inner
            .handle
            .send_with_session("Debugger.enable", json!({}), Some(&session_id))
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

    pub(crate) async fn create_tab_inner(
        inner: &mut Inner,
        url: Option<&str>,
    ) -> Result<String, String> {
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
        let _command_guard = self.command_gate.lock().await;
        if Self::is_passive(&command) && !self.is_healthy().await {
            return CommandOutcome::err("backend_unavailable", "浏览器未运行", elapsed(&started));
        }
        if let Err(error) = self.get_or_start().await {
            return CommandOutcome::err("backend_unavailable", error, elapsed(&started));
        }
        // 逆向断点暂停会冻结页面:主动命令执行前自动恢复目标 tab(断点保留,
        // 模型导航/交互不会卡死在断点上;面板用户从暂停横幅接管调试节奏)。
        if !Self::is_passive(&command) && self.recon.is_paused(None) {
            let explicit = command.tab_id_probe().map(str::to_string);
            let active = {
                let inner_guard = self.inner.lock().await;
                inner_guard
                    .as_ref()
                    .and_then(|inner| inner.active_tab.clone())
            };
            let target = explicit.or(active);
            if let (Some(paused_tab), Some(target)) = (self.recon.paused_tab(), target) {
                if paused_tab == target {
                    let _ = self.recon.resume(self, &target).await;
                }
            }
        }
        // CDP 在页面崩溃/浏览器更新时可能在命令中途断开。重建实例并
        // 重放一次命令，避免把瞬时传输错误暴露给上层；业务错误不会重放。
        // 断线重连:最多 3 次重试,每次先收尸(杀残留进程树+清陈旧状态)再
        // 重启实例;重启后 tab 表/active_tab 由 refresh_targets 重建,模型
        // 视角的导航状态没有丢失(profile 持久,登录态/URL 都在)。
        let mut attempt = 0u8;
        let outcome = loop {
            let outcome = {
                let mut guard = self.inner.lock().await;
                let Some(inner) = guard.as_mut() else {
                    break CommandOutcome::err(
                        "backend_unavailable",
                        "浏览器未就绪",
                        elapsed(&started),
                    );
                };
                commands::dispatch(self, inner, command.clone()).await
            };
            let transient = outcome.error.as_ref().is_some_and(|error| {
                matches!(error.code.as_str(), "cdp_error" | "backend_unavailable")
                    || error.message.contains("cdp_disconnected")
                    || error.message.contains("cdp_writer_closed")
                    || error.message.contains("cdp_timeout")
            });
            if !transient || attempt >= 3 {
                break outcome;
            }
            attempt += 1;
            tracing::warn!(attempt, "浏览器命令瞬时失败,断线重连并重试");
            self.on_pump_exit().await;
            if self.get_or_start().await.is_err() {
                break outcome;
            }
        };
        // 关闭最后一个 tab 是明确的资源生命周期边界：立即销毁实例，
        // 使 profile 不再被 Chrome 子进程锁住。下一条主动命令会按需重启。
        let no_tabs = {
            let guard = self.inner.lock().await;
            guard.as_ref().is_some_and(|inner| inner.tabs.is_empty())
        };
        if no_tabs {
            self.on_pump_exit().await;
            let _ = self.event_broadcast.send(BrowserEvent::Exited);
        }
        self.maybe_evict_tabs().await;
        outcome
    }

    /// 解析目标 tab(缺省 active)并返回 (tab_id, session_id);浏览器未跑报错。
    /// recon REST 端点入口;不拉起浏览器(侦察操作需要浏览器本就运行)。
    pub async fn recon_resolve_tab(
        &self,
        tab_id: Option<&str>,
    ) -> Result<(String, String), String> {
        if !self.is_healthy().await {
            return Err("浏览器未运行;先用 getState/navigate 启动".to_string());
        }
        let guard = self.inner.lock().await;
        let Some(inner) = guard.as_ref() else {
            return Err("浏览器未就绪".to_string());
        };
        let resolved = match tab_id {
            Some(id) => id.to_string(),
            None => inner
                .active_tab
                .clone()
                .ok_or_else(|| "没有打开的 tab".to_string())?,
        };
        let info = inner
            .tabs
            .get(&resolved)
            .ok_or_else(|| format!("tab {resolved} 不存在"))?;
        Ok((resolved, info.session_id.clone()))
    }

    /// CDP 句柄 + 目标 tab session(浏览器须已运行;recon 操作入口)。
    /// 返回的句柄可跨会话发命令(flatten sessionId 由调用方带)。
    pub async fn recon_cdp_pair(&self, tab_id: &str) -> Result<(CdpHandle, String), String> {
        if !self.is_healthy().await {
            return Err("浏览器未运行;先用 getState/navigate 启动".to_string());
        }
        let guard = self.inner.lock().await;
        let Some(inner) = guard.as_ref() else {
            return Err("浏览器未就绪".to_string());
        };
        let info = inner
            .tabs
            .get(tab_id)
            .ok_or_else(|| format!("tab {tab_id} 不存在"))?;
        Ok((inner.handle.clone(), info.session_id.clone()))
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
            // 驱逐即彻底释放 per-tab 资源:抓包缓冲 + 侦察状态 + 视图 + dialog。
            self.network_log.lock().unwrap().clear(&tab_id);
            self.recon.clear(&tab_id);
            self.tab_views.lock().unwrap().remove(&tab_id);
            self.pending_dialogs.lock().unwrap().remove(&tab_id);
        }
        let _ = self.event_broadcast.send(BrowserEvent::TabsChanged);
    }

    /// 事件泵:消费 CDP 事件,广播给面板。
    #[allow(clippy::too_many_arguments)]
    async fn event_pump(
        handle: CdpHandle,
        mut event_rx: tokio::sync::mpsc::UnboundedReceiver<CdpEvent>,
        broadcast: tokio::sync::broadcast::Sender<BrowserEvent>,
        pending_dialogs: std::sync::Arc<std::sync::Mutex<HashMap<String, Value>>>,
        network_log: std::sync::Arc<std::sync::Mutex<NetworkLog>>,
        recon: Arc<ReconStore>,
        tab_views: std::sync::Arc<std::sync::Mutex<HashMap<String, (String, String)>>>,
        screencast_tab: std::sync::Arc<std::sync::Mutex<Option<String>>>,
        exited_ping: tokio::sync::watch::Sender<bool>,
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
                    let ack_id = event
                        .params
                        .get("sessionId")
                        .cloned()
                        .unwrap_or(Value::Null);
                    // 必须回执,否则 Chrome 停发帧。
                    if !tab_id.is_empty() {
                        let _ = handle
                            .send_with_session(
                                "Page.screencastFrameAck",
                                json!({"sessionId": ack_id}),
                                Some(&tab_id),
                            )
                            .await;
                        // 只广播当前 screencast 流归属 tab 的帧:单实例单画面,
                        // 前端不做过滤(首帧可能早于面板状态刷新,前端过滤会永久丢帧)。
                        let is_stream_tab = {
                            let guard = screencast_tab.lock().unwrap();
                            guard.as_deref() == Some(tab_id.as_str())
                        };
                        if is_stream_tab && !data.is_empty() {
                            // screencastFrame metadata 带页面 CSS 视口尺寸:
                            // 前端画面点击/滚轮坐标依赖它做 CSS 像素换算。
                            let viewport = event
                                .params
                                .pointer("/metadata/deviceWidth")
                                .and_then(Value::as_u64)
                                .zip(
                                    event
                                        .params
                                        .pointer("/metadata/deviceHeight")
                                        .and_then(Value::as_u64),
                                )
                                .map(|(w, h)| FrameViewport {
                                    width: w as u32,
                                    height: h as u32,
                                });
                            let _ = broadcast.send(BrowserEvent::Frame {
                                tab_id,
                                data,
                                viewport,
                            });
                        }
                    }
                }
                // ---- 网络抓包:按 requestId 聚合请求/响应/失败 ----
                "Network.requestWillBeSent" => {
                    let request_id = event
                        .params
                        .get("requestId")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if request_id.is_empty() || tab_id.is_empty() {
                        continue;
                    }
                    let url = event
                        .params
                        .pointer("/request/url")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let method = event
                        .params
                        .pointer("/request/method")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let headers = event
                        .params
                        .pointer("/request/headers")
                        .cloned()
                        .unwrap_or(Value::Null);
                    let post_data = event
                        .params
                        .pointer("/request/postData")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    // 发起者调用栈(逆向定位加密函数的关键,js-reverse initiator 语义)。
                    let initiator = initiator_summary(event.params.get("initiator"));
                    network_log.lock().unwrap().record(
                        &tab_id,
                        &request_id,
                        json!({
                            "requestId": request_id,
                            "url": url,
                            "method": method,
                            "requestHeaders": headers,
                            "postData": post_data,
                            "initiator": initiator,
                            "status": Value::Null,
                            "responseHeaders": Value::Null,
                            "mimeType": Value::Null,
                            "failed": Value::Null,
                        }),
                    );
                }
                // 真实请求头(含最终 Cookie:HttpOnly 页内读不到,只能这里拿)。
                "Network.requestWillBeSentExtraInfo" => {
                    let request_id = extra_info_request_id(&event.params);
                    if request_id.is_empty() || tab_id.is_empty() {
                        continue;
                    }
                    let extra_headers = event.params.get("headers").cloned().unwrap_or(Value::Null);
                    network_log.lock().unwrap().record(
                        &tab_id,
                        &request_id,
                        json!({ "requestId": request_id, "extraRequestHeaders": extra_headers }),
                    );
                }
                // 真实响应头(含 Set-Cookie,含 HttpOnly)。
                "Network.responseReceivedExtraInfo" => {
                    let request_id = extra_info_request_id(&event.params);
                    if request_id.is_empty() || tab_id.is_empty() {
                        continue;
                    }
                    let extra_headers = event.params.get("headers").cloned().unwrap_or(Value::Null);
                    network_log.lock().unwrap().record(
                        &tab_id,
                        &request_id,
                        json!({ "requestId": request_id, "extraResponseHeaders": extra_headers }),
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
                    let direction = if event.method.ends_with("Sent") {
                        "sent"
                    } else {
                        "received"
                    };
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
                    network_log.lock().unwrap().record(
                        &tab_id,
                        &key,
                        json!({ "wsFrames": frames }),
                    );
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
                    pending_dialogs
                        .lock()
                        .unwrap()
                        .insert(tab_id.clone(), json!({ "type": kind, "message": message }));
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
                        // 事件路径只更新 url(title 等命令路径的 state_after 回写)。
                        tab_views
                            .lock()
                            .unwrap()
                            .entry(tab_id.clone())
                            .or_insert_with(|| (String::new(), String::new()))
                            .0 = url.clone();
                        let _ = broadcast.send(BrowserEvent::Navigated {
                            tab_id,
                            url,
                            title: String::new(),
                        });
                    }
                }
                // ---- JS 逆向侦察:脚本表/断点暂停/控制台(状态存 ReconStore,广播暂停) ----
                "Debugger.scriptParsed"
                | "Debugger.paused"
                | "Debugger.resumed"
                | "Runtime.consoleAPICalled"
                | "Runtime.exceptionThrown" => {
                    if !tab_id.is_empty() {
                        for revent in recon.on_event(&tab_id, &event.method, &event.params) {
                            let _ = broadcast.send(revent);
                        }
                    }
                }
                _ => {}
            }
        }
        // 浏览器退出:通知管理器收尸(杀进程树 + 清状态),并广播 Exited。
        // 进程树终止与状态清理由看护任务完成,泵这里只置信号避免重复收尸。
        let _ = exited_ping.send(true);
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
                // 合并导航视图:命令/事件更新过的 url/title 优先(启动时值只作兜底)。
                let (url, title) = self
                    .tab_view(&info.tab_id)
                    .unwrap_or_else(|| (info.url.clone(), info.title.clone()));
                json!({
                    "tabId": info.tab_id,
                    "url": url,
                    "title": title,
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

    /// 优雅关闭:终止 Chrome 进程树并清空全部 per-tab 状态(server 退出/
    /// 面板关闭时调用;幂等)。
    pub async fn shutdown(&self) {
        self.on_pump_exit().await;
        let _ = self.event_broadcast.send(BrowserEvent::Exited);
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

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// requestWillBeSent 的 initiator → 精简摘要(类型 + 发起脚本 + 最内 8 帧调用栈)。
fn initiator_summary(initiator: Option<&Value>) -> Value {
    let Some(initiator) = initiator else {
        return Value::Null;
    };
    let r#type = initiator.get("type").and_then(Value::as_str).unwrap_or("");
    let frames: Vec<Value> = initiator
        .pointer("/stack/callFrames")
        .and_then(Value::as_array)
        .map(|frames| {
            frames
                .iter()
                .rev()
                .take(8)
                .rev()
                .map(|frame| {
                    json!({
                        "functionName": frame.get("functionName").and_then(Value::as_str).unwrap_or(""),
                        "url": frame.get("url").and_then(Value::as_str).unwrap_or(""),
                        "lineNumber": frame.get("lineNumber").and_then(Value::as_i64).unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "type": r#type,
        "url": initiator.get("url").and_then(Value::as_str).unwrap_or(""),
        "functionName": initiator.get("functionName").and_then(Value::as_str).unwrap_or(""),
        "stack": frames,
    })
}

/// extraInfo 事件的 requestId(新协议叫 responseId,兼容两者)。
fn extra_info_request_id(params: &Value) -> String {
    params
        .get("requestId")
        .or_else(|| params.get("responseId"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

pub(crate) fn elapsed(started: &Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

/// tab 关闭的统一清理:摘表 + 关 target + 清 per-tab 资源 + 活跃 tab 迁移。
pub(crate) async fn close_tab_full(inner: &mut Inner, manager: &BrowserManager, tab_id: &str) {
    if let Some(info) = inner.tabs.remove(tab_id) {
        manager.forget_tab_view(tab_id);
        let _ = inner
            .handle
            .send("Target.closeTarget", json!({"targetId": info.target_id}))
            .await;
        inner.by_target.remove(&info.target_id);
        // per-tab 资源彻底释放:抓包/侦察/dialog/viewport。
        manager.network_log.lock().unwrap().clear(tab_id);
        manager.recon.clear(tab_id);
        manager.clear_dialog(tab_id);
        inner.viewport.remove(tab_id);
        // screencast 流归属清理:关闭的 tab 若正在推流,置空(前端空态等重启流)。
        {
            let mut guard = manager.screencast_tab.lock().unwrap();
            if guard.as_deref() == Some(tab_id) {
                *guard = None;
            }
        }
        if inner.active_tab.as_deref() == Some(tab_id) {
            inner.active_tab = inner.tabs.keys().next().cloned();
        }
        manager.broadcast(BrowserEvent::TabsChanged);
    }
}

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
            None => self.inner.active_tab.clone().ok_or_else(|| {
                CommandOutcome::err("no_tab", "没有打开的 tab", elapsed(&started))
            })?,
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
        self.inner
            .tabs
            .get(tab_id)
            .map(|info| info.session_id.clone())
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
        serde_json::from_str(&serialized).map_err(|error| {
            CommandOutcome::err("execution_error", format!("页面返回非法 JSON: {error}"), 0)
        })
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
