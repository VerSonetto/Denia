//! Composition root: opens stores, registers namespaces and adapters, and
//! keeps OpenAI-compatible routes in sync with settings.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use denia_agent_loop::{ApprovalBridge, SessionDriver};
use denia_core::session::ApprovalOutcome;
use denia_credentials::{CredentialEvent, CredentialStore};
use denia_llm::{LlmRegistry, OPENAI_SETTINGS_NS, OpenAiCompatAdapter, OpenAiSection, RetryPolicy};
use denia_session::{Session, SessionError, SessionStore};
use denia_settings::{Applies, NamespaceSpec, SettingsEvent, SettingsStore};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Console preferences namespace (settings page).
pub const CONSOLE_NS: &str = "console";

/// The `console` settings section shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleSettings {
    /// `system` | `light` | `dark`.
    #[serde(default = "default_theme")]
    pub theme: String,
    /// `zh` | `en`;zh 是源语言(学 dsh 的 i18n 约定)。
    #[serde(default = "default_locale")]
    pub locale: String,
    /// 层叠上下文管理(学 dsh 压力驱动 + Claude Code compact):
    /// 低压力不动(前缀缓存稳定),高压力 LLM 总结压缩。
    /// 全部字段带默认值;见 `ConsoleCompactionSettings`。
    #[serde(default)]
    pub compaction: ConsoleCompactionSettings,
    /// 工具并行执行上限(学 codex `ToolCallRuntime` + dsh `maxParallelToolCalls`)。
    /// 一次 step 内模型返回的多个工具调用并发执行,受此上限约束。
    #[serde(default = "default_max_parallel_tool_calls")]
    pub max_parallel_tool_calls: usize,
}

fn default_max_parallel_tool_calls() -> usize {
    10
}

/// `console.compaction` 段(层叠上下文管理配置,默认对齐 Claude Code
/// auto-compact 缓冲语义)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ConsoleCompactionSettings {
    /// LLM 总结压缩总开关。
    pub compact_enabled: bool,
    /// 压力 ≥ 窗口 × 该比例时触发 LLM 总结压缩。
    pub compact_ratio: f64,
    /// 压缩后保留窗口下限 token。
    pub compact_min_keep_tokens: u64,
    /// 压缩后保留窗口上限 token。
    pub compact_max_keep_tokens: u64,
    /// 保留窗口至少包含的文本消息数。
    pub compact_min_text_messages: usize,
    /// 摘要请求的输出预算。
    pub compact_summary_max_tokens: u64,
    /// 摘要请求 PTL/失败的截断重试上限。
    pub compact_max_attempts: u32,
}

impl Default for ConsoleCompactionSettings {
    fn default() -> Self {
        Self {
            compact_enabled: true,
            compact_ratio: 0.90,
            compact_min_keep_tokens: 10_000,
            compact_max_keep_tokens: 40_000,
            compact_min_text_messages: 5,
            compact_summary_max_tokens: 20_000,
            compact_max_attempts: 3,
        }
    }
}

/// 把 console compaction 配置翻译成 driver 的 [`CompactionSettings`]。
pub fn compaction_settings_from(console: &ConsoleSettings) -> denia_agent_loop::CompactionSettings {
    let c = &console.compaction;
    denia_agent_loop::CompactionSettings {
        compact_enabled: c.compact_enabled,
        compact_ratio: c.compact_ratio,
        min_keep_tokens: c.compact_min_keep_tokens,
        max_keep_tokens: c.compact_max_keep_tokens,
        min_text_messages: c.compact_min_text_messages,
        summary_max_tokens: c.compact_summary_max_tokens,
        max_attempts: c.compact_max_attempts,
    }
}

fn default_theme() -> String {
    "system".to_string()
}

fn default_locale() -> String {
    "zh".to_string()
}

fn validate_console(value: Value) -> Result<Value, String> {
    let parsed: ConsoleSettings = serde_json::from_value(value).map_err(|e| e.to_string())?;
    if !matches!(parsed.theme.as_str(), "system" | "light" | "dark") {
        return Err(format!(
            "theme must be 'system', 'light', or 'dark'; got '{}'",
            parsed.theme
        ));
    }
    if !matches!(parsed.locale.as_str(), "zh" | "en") {
        return Err(format!(
            "locale must be 'zh' or 'en'; got '{}'",
            parsed.locale
        ));
    }
    let c = &parsed.compaction;
    if !(0.0..=1.0).contains(&c.compact_ratio) {
        return Err(format!(
            "compaction.compactRatio must be in [0, 1]; got {}",
            c.compact_ratio
        ));
    }
    if parsed.max_parallel_tool_calls == 0 || parsed.max_parallel_tool_calls > 64 {
        return Err(format!(
            "maxParallelToolCalls must be in [1, 64]; got {}",
            parsed.max_parallel_tool_calls
        ));
    }
    serde_json::to_value(parsed).map_err(|e| e.to_string())
}

/// Reads the resolved console settings, tolerating an absent provider.
pub fn console_settings(settings: &SettingsStore) -> ConsoleSettings {
    settings
        .resolved(CONSOLE_NS)
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or(ConsoleSettings {
            theme: "system".to_string(),
            locale: "zh".to_string(),
            compaction: ConsoleCompactionSettings::default(),
            max_parallel_tool_calls: 10,
        })
}

/// Process-wide shared state handed to every handler.
pub struct AppState {
    pub runtime: Arc<crate::agent_runtime::Runtime>,
    pub home: std::path::PathBuf,
    pub settings: Arc<SettingsStore>,
    pub credentials: Arc<CredentialStore>,
    pub registry: Arc<LlmRegistry>,
    pub events: broadcast::Sender<ServerEvent>,
    pub http: reqwest::Client,
    pub sessions: Arc<SessionStore>,
    pub live: Arc<LiveSessions>,
    pub driver: Arc<SessionDriver>,
    pub workspaces: Arc<crate::workspace::WorkspaceRegistry>,
    pub system_prompt: Arc<crate::system_prompt_store::SystemPromptState>,
    pub file_history: Arc<crate::file_history::FileHistoryStore>,
    /// 内嵌浏览器中枢(工具与 REST API 共用)。
    pub browser: Arc<denia_browser::BrowserManager>,
    /// 绑定地址非回环 ⇒ 远程浏览器 ⇒ 目录选择器走 browse。
    pub bound_remote: bool,
}

/// Wire push events for the console.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ServerEvent {
    SettingsUpdated,
    CredentialsUpdated,
    LlmUpdated,
    SessionsUpdated,
    /// 会话运行状态变化(发消息/轮次结束/删除);前端据此维护 running 集合,
    /// 无需为后台会话各维持一条 SSE 长连接。
    RunningChanged {
        id: String,
        running: bool,
    },
    /// `/api/events` 连接时的运行中会话快照(只发给该连接,不走广播)。
    /// 刷新/重连后前端用它还原中断按钮,不必等下一次增量。
    RunningSnapshot {
        ids: Vec<String>,
    },
    /// 自定义系统提示词文件变更后广播(前端可选订阅)。
    SystemPromptChanged,
}

/// One materialized session: durable log plus live fan-out. `session` is an
/// internally synchronized log, so readers never wait behind a running turn.
pub struct LiveSession {
    pub session: Arc<Session>,
    pub followers: broadcast::Sender<denia_core::session::SessionEnvelope>,
    pub running: AtomicBool,
    /// 最近一次被访问(挂载/发消息/follow)的 epoch ms;空闲淘汰依据。
    pub last_touch: AtomicU64,
    pub cancel: std::sync::Mutex<Option<CancellationToken>>,
    /// 等待用户决策的审批请求(request_id → oneshot)。计划审批的决策
    /// 可携带执行档位/模型/补充建议(PlanReviewDecision)。
    pub pending_approvals: std::sync::Mutex<
        HashMap<String, tokio::sync::oneshot::Sender<denia_core::session::PlanReviewDecision>>,
    >,
    /// 等待用户作答的提问请求(request_id → oneshot)。
    ///
    /// 与审批分表:dsh 用同一 id 空间承担两种交互,重试/多问题批次时
    /// 应答可能误配;这里按 request_id 严格隔离,且答题是一次性原子结算。
    pub pending_asks:
        std::sync::Mutex<HashMap<String, tokio::sync::oneshot::Sender<denia_core::session::AskResolution>>>,
}

/// Live sessions keyed by id; loads (and repairs) on first touch.
///
/// ## 生命周期(内存上限)
///
/// - 每次 `get_or_load` / `touch` 都会刷新 `last_touch`。
/// - 后台淘汰任务(见 [`spawn_live_evictor`])周期清理:非运行中、
///   无 SSE 订阅者、且超过 `idle_after` 未使用的会话被卸载,事件内存
///   随之释放;下次访问按日志重新加载。运行中的会话永不淘汰。
pub struct LiveSessions {
    inner: std::sync::Mutex<HashMap<String, Arc<LiveSession>>>,
    /// 最多同时驻留的完整会话数。超出后按 LRU 淘汰非运行、无订阅者会话,
    /// 防止连续打开长会话把后端常驻内存撑到 O(全部历史)。
    max_resident: usize,
}

impl Default for LiveSessions {
    fn default() -> Self {
        Self::new(32)
    }
}

impl LiveSessions {
    pub fn new(max_resident: usize) -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
            max_resident: max_resident.max(1),
        }
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl LiveSessions {
    pub fn get_or_load(
        &self,
        store: &SessionStore,
        id: &str,
    ) -> Result<Arc<LiveSession>, SessionError> {
        let mut map = self.inner.lock().unwrap();
        if let Some(live) = map.get(id) {
            live.last_touch
                .store(now_millis(), std::sync::atomic::Ordering::Relaxed);
            return Ok(live.clone());
        }
        let session = store.load(id)?;
        let session = Arc::new(session);
        store.track_session(&session);
        let live = Arc::new(LiveSession {
            session,
            followers: broadcast::channel(1024).0,
            running: AtomicBool::new(false),
            last_touch: AtomicU64::new(now_millis()),
            cancel: std::sync::Mutex::new(None),
            pending_approvals: std::sync::Mutex::new(HashMap::new()),
            pending_asks: std::sync::Mutex::new(HashMap::new()),
        });
        map.insert(id.to_string(), live.clone());
        self.trim_capacity_locked(&mut map);
        Ok(live)
    }

    /// 在已持锁的 map 上执行容量裁剪:超过上限时,按 last_touch 从旧到新
    /// 淘汰“非运行、无订阅者”的会话。运行中的会话绝不卸载。
    fn trim_capacity_locked(&self, map: &mut HashMap<String, Arc<LiveSession>>) {
        let max = self.max_resident;
        if map.len() <= max {
            return;
        }
        let mut candidates: Vec<(String, u64)> = map
            .iter()
            .filter(|(_, live)| {
                !live.running.load(std::sync::atomic::Ordering::SeqCst)
                    && live.followers.receiver_count() == 0
                    && Arc::strong_count(live) == 1
            })
            .map(|(id, live)| {
                (
                    id.clone(),
                    live.last_touch.load(std::sync::atomic::Ordering::Relaxed),
                )
            })
            .collect();
        candidates.sort_by_key(|(_, touched)| *touched);
        let remove_count = map.len() - max;
        for (id, _) in candidates.into_iter().take(remove_count) {
            if let Some(live) = map.get(&id) {
                if !live.running.load(std::sync::atomic::Ordering::SeqCst)
                    && live.followers.receiver_count() == 0
                    && Arc::strong_count(live) == 1
                {
                    map.remove(&id);
                }
            }
        }
    }

    /// 读取已加载的 live 会话(不触发磁盘加载)。
    pub fn get(&self, id: &str) -> Option<Arc<LiveSession>> {
        self.inner.lock().unwrap().get(id).cloned()
    }

    /// 当前正在跑 turn 的会话 id。运行中会话不会被空闲淘汰,只扫内存 live 表。
    pub fn running_ids(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, live)| live.running.load(Ordering::SeqCst))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// 记录一次外部访问(follow 连接等),防止被空闲淘汰。
    pub fn touch(&self, id: &str) {
        if let Some(live) = self.inner.lock().unwrap().get(id) {
            live.last_touch
                .store(now_millis(), std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// 淘汰满足条件的空闲会话,返回被卸载的 id 列表。
    pub fn evict_idle(&self, idle_after_secs: u64) -> Vec<String> {
        let now = now_millis();
        let idle_ms = idle_after_secs.saturating_mul(1000);
        let mut to_remove = Vec::new();
        {
            let map = self.inner.lock().unwrap();
            for (id, live) in map.iter() {
                let running = live.running.load(std::sync::atomic::Ordering::SeqCst);
                let subscribed = live.followers.receiver_count() > 0;
                let idle = now
                    .saturating_sub(live.last_touch.load(std::sync::atomic::Ordering::Relaxed))
                    >= idle_ms;
                if !running && !subscribed && idle && Arc::strong_count(live) == 1 {
                    to_remove.push(id.clone());
                }
            }
        }
        let mut map = self.inner.lock().unwrap();
        for id in &to_remove {
            // 双检:可能已被其他路径移除。
            if let Some(live) = map.get(id) {
                if !live.running.load(std::sync::atomic::Ordering::SeqCst)
                    && live.followers.receiver_count() == 0
                    && Arc::strong_count(live) == 1
                {
                    map.remove(id);
                }
            }
        }
        to_remove
    }

    pub fn remove(&self, id: &str) {
        self.inner.lock().unwrap().remove(id);
    }
}

/// 服务端审批桥:把确认框挂到 LiveSession 的 pending 表,等待 REST 应答。
pub struct ServerApprovalBridge {
    live: Arc<LiveSessions>,
}

impl ServerApprovalBridge {
    pub fn new(live: Arc<LiveSessions>) -> Self {
        Self { live }
    }
}

#[async_trait]
impl ApprovalBridge for ServerApprovalBridge {
    async fn request(
        &self,
        session_id: &str,
        request_id: &str,
        cancel: CancellationToken,
    ) -> denia_core::session::PlanReviewDecision {
        use denia_core::session::PlanReviewDecision;
        let unavailable = || PlanReviewDecision {
            outcome: ApprovalOutcome::Unavailable,
            execute_mode: None,
            selection: None,
            vision_supported: None,
            feedback: None,
        };
        let cancelled = || PlanReviewDecision {
            outcome: ApprovalOutcome::Cancelled,
            execute_mode: None,
            selection: None,
            vision_supported: None,
            feedback: None,
        };
        let Some(live) = self.live.get(session_id) else {
            return unavailable();
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut pending = live
                .pending_approvals
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            pending.insert(request_id.to_string(), tx);
        }
        tokio::select! {
            _ = cancel.cancelled() => {
                let mut pending = live
                    .pending_approvals
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if let Some(tx) = pending.remove(request_id) {
                    let _ = tx.send(cancelled());
                }
                cancelled()
            }
            result = rx => {
                let _ = live
                    .pending_approvals
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(request_id);
                result.unwrap_or_else(|_| cancelled())
            }
        }
    }
}

/// 服务端提问桥:把 `ask` 工具的提问挂到 LiveSession 的 pending 表,
/// 落 `ask-requested` 事件(刷新页面可恢复挂起卡片),等待 REST 应答。
///
/// 与 dsh 的差异(见 `docs/ask-tool.md` 不足分析):
/// - **有超时**:`timeout_ms` 到点按 `TimedOut` 结算,工具不会永久挂起;
/// - **结局区分**:超时/取消/无通道各有独立语义,模型知道下一步怎么办;
/// - **先落盘再等待**:请求进日志后即使前端断线重连也能重建挂起态;
/// - **单次原子结算**:一个 request_id 只结算一次,重复应答返回 404。
pub struct ServerAskBridge {
    live: Arc<LiveSessions>,
}

impl ServerAskBridge {
    pub fn new(live: Arc<LiveSessions>) -> Self {
        Self { live }
    }
}

#[async_trait]
impl denia_tools::AskBridge for ServerAskBridge {
    async fn ask(
        &self,
        session_id: &str,
        request_id: &str,
        call_id: &str,
        questions: &[denia_core::session::AskQuestion],
        timeout_ms: u64,
        cancel: CancellationToken,
    ) -> denia_core::session::AskResolution {
        use denia_core::session::{AskOutcome, AskResolution, SessionEvent};
        let Some(live) = self.live.get(session_id) else {
            return AskResolution {
                outcome: AskOutcome::Unavailable,
                answers: Vec::new(),
                reason: Some("会话不在运行中".to_string()),
            };
        };
        // 子代理不与用户交互:提问语义不成立。
        //
        // 主机制在工具授予层——子代理默认只拿到只读工具集
        // (`denia_tools::SUBAGENT_READ_ONLY_TOOLS`,不含 ask),正常路径
        // 根本调不到本函数。这里保留一道服务层兜底:万一将来有路径绕过
        // 授予(如外部直接调用),也不至于把提问挂到一个没人看的会话上。
        // 与 dsh 的区别:dsh 把这种情况做成终局错误(DELEGATED_CALLER),
        // 这里按 unavailable 结算并给出可执行的下一步,模型自行决策继续。
        if live.session.header().subagent.is_some() {
            return AskResolution {
                outcome: AskOutcome::Unavailable,
                answers: Vec::new(),
                reason: Some(
                    "子代理不与用户交互,不能提问;请自行决策,并在最终结果中说明未决问题与采用的假设"
                        .to_string(),
                ),
            };
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut pending = live.pending_asks.lock().unwrap_or_else(|p| p.into_inner());
            pending.insert(request_id.to_string(), tx);
        }
        // 先落盘再等待:前端重连后按日志恢复挂起卡片。
        if let Ok(envelope) = live.session.append(SessionEvent::AskRequested {
            request_id: request_id.to_string(),
            call_id: call_id.to_string(),
            questions: questions.to_vec(),
            timeout_ms,
        }) {
            let _ = live.followers.send(envelope);
        }
        let settle = |resolution: AskResolution| {
            if let Ok(envelope) = live.session.append(SessionEvent::AskResolved {
                request_id: request_id.to_string(),
                resolution: resolution.clone(),
            }) {
                let _ = live.followers.send(envelope);
            }
            resolution
        };
        let outcome = tokio::select! {
            _ = cancel.cancelled() => {
                let mut pending = live.pending_asks.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(tx) = pending.remove(request_id) {
                    // 取消与超时同形:应答方已离场,不再有人写这条通道。
                    let _ = tx.send(AskResolution {
                        outcome: AskOutcome::Cancelled,
                        answers: Vec::new(),
                        reason: None,
                    });
                }
                AskResolution {
                    outcome: AskOutcome::Cancelled,
                    answers: Vec::new(),
                    reason: None,
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)) => {
                let mut pending = live.pending_asks.lock().unwrap_or_else(|p| p.into_inner());
                pending.remove(request_id);
                AskResolution {
                    outcome: AskOutcome::TimedOut,
                    answers: Vec::new(),
                    reason: None,
                }
            }
            result = rx => {
                let _ = live
                    .pending_asks
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(request_id);
                result.unwrap_or(AskResolution {
                    outcome: AskOutcome::Cancelled,
                    answers: Vec::new(),
                    reason: None,
                })
            }
        };
        settle(outcome)
    }
}

/// RAII 运行保护:drop 时复位 running、清空 cancel token,并广播
/// `RunningChanged { running: false }` 给控制台。即使任务 panic,
/// 会话也不会永久锁死在 running 状态,前端圆点也不会卡死。
pub struct RunningGuard {
    live: Arc<LiveSession>,
    events: broadcast::Sender<ServerEvent>,
}

impl RunningGuard {
    pub fn new(live: Arc<LiveSession>, events: broadcast::Sender<ServerEvent>) -> Self {
        Self { live, events }
    }
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        // 任务无论正常/panic 结束,未答审批一律按 cancelled 结算,
        // 避免 pending oneshot 泄漏。
        {
            let mut pending = self
                .live
                .pending_approvals
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            for tx in pending.drain() {
                let _ = tx.1.send(denia_core::session::PlanReviewDecision {
                    outcome: ApprovalOutcome::Cancelled,
                    execute_mode: None,
                    selection: None,
                    vision_supported: None,
                    feedback: None,
                });
            }
        }
        // 未答提问同样按 cancelled 结算,避免 pending oneshot 泄漏
        // (工具侧会把它渲染成"用户取消",模型据此收束)。
        {
            let mut pending = self
                .live
                .pending_asks
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            for (_, tx) in pending.drain() {
                let _ = tx.send(denia_core::session::AskResolution {
                    outcome: denia_core::session::AskOutcome::Cancelled,
                    answers: Vec::new(),
                    reason: None,
                });
            }
        }
        let mut cancel = self.live.cancel.lock().unwrap();
        *cancel = None;
        let _ = self.events.send(ServerEvent::RunningChanged {
            id: self.live.session.id().to_string(),
            running: false,
        });
        self.live.running.store(false, Ordering::SeqCst);
    }
}

/// 后台空闲淘汰:每 `interval` 扫描一次,卸载 idle 会话。运行中永不淘汰。
pub fn spawn_live_evictor(live: Arc<LiveSessions>, interval_secs: u64, idle_after_secs: u64) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let evicted = live.evict_idle(idle_after_secs);
            if !evicted.is_empty() {
                tracing::debug!(count = evicted.len(), "evicted idle live sessions");
            }
        }
    });
}

/// Validates by round-tripping through the section's typed shape.
fn validate_with<T>(value: Value) -> Result<Value, String>
where
    T: serde::de::DeserializeOwned + Serialize,
{
    let parsed: T = serde_json::from_value(value).map_err(|e| e.to_string())?;
    serde_json::to_value(parsed).map_err(|e| e.to_string())
}

pub fn build_state(
    home: &Path,
    bound_remote: bool,
) -> Result<AppState, Box<dyn std::error::Error>> {
    let events = broadcast::channel::<ServerEvent>(64).0;

    let settings_events = broadcast::channel::<SettingsEvent>(64);
    let credentials_events = broadcast::channel::<CredentialEvent>(64);

    let settings = Arc::new(SettingsStore::open(home)?.with_events(settings_events.0.clone()));
    let credentials =
        Arc::new(CredentialStore::open(home)?.with_events(credentials_events.0.clone()));
    let registry = Arc::new(LlmRegistry::new());

    register_namespaces(&settings)?;

    // OpenAI-compatible routes follow their settings section.
    let openai = Arc::new(OpenAiCompatAdapter::new(
        settings.clone(),
        credentials.clone(),
    ));
    sync_openai_routes(&registry, &settings, &openai);

    // Sessions, tools, and the driver.
    let sessions = Arc::new(SessionStore::open(home)?);
    // 启动一次性清扫:删掉"从未发过消息"的残留空白会话(浏览器直接关闭、
    // 进程被杀留下的空壳)。放在数据层而不是前端,是因为前端每个页面只知道
    // 自己的焦点,却能看到全局会话列表 —— 在那里做"非活跃空白即删"会误删
    // 别的页面(或共用同一数据目录的另一实例)正在使用的会话。
    // 10 分钟年龄门槛给并发实例留缓冲,避免删掉用户正停着的草稿。
    // 必须在 bootstrap 之前跑,否则这些空壳会被写进工作区账本。
    let swept = sessions.sweep_stale_blanks(10 * 60 * 1000);
    // 工作区注册表:独立持久化域;首启按会话头 cwd 自动分组。
    let workspaces = Arc::new(crate::workspace::WorkspaceRegistry::open(home)?);
    // 已初始化过的账本里可能还挂着上次运行留下的空壳引用,一并去引用。
    for id in &swept {
        workspaces.detach_session(id);
    }
    if !swept.is_empty() {
        tracing::info!(count = swept.len(), "swept stale blank sessions at startup");
    }
    workspaces.bootstrap(
        &sessions
            .list()?
            .iter()
            .map(|summary| {
                (
                    summary.id.clone(),
                    summary.cwd.clone().unwrap_or_default(),
                    summary.created_at,
                )
            })
            .collect::<Vec<_>>(),
    );
    let live = Arc::new(LiveSessions::default());
    // 会话轮次淘汰:30s 一轮,10 分钟未使用的会话卸载(保内存/下盘的会话不受影响)。
    spawn_live_evictor(live.clone(), 30, 600);
    let browser = Arc::new(denia_browser::BrowserManager::new(home.to_path_buf()));
    let browser_hub: denia_tools::BrowserHub = browser.clone();
    let recon_hub: denia_tools::ReconHub = browser.clone();
    let system_prompt = Arc::new(crate::system_prompt_store::SystemPromptState::load(
        home,
        Some(browser_hub.clone()),
        Some(recon_hub.clone()),
    ));
    let file_history = Arc::new(crate::file_history::FileHistoryStore::new(home));
    let runtime = crate::agent_runtime::Runtime::new(
        home,
        sessions.clone(),
        live.clone(),
        settings.clone(),
        events.clone(),
        registry.clone(),
        workspaces.clone(),
    )?;
    let mut tools =
        denia_tools::default_registry_with_browser_and_recon(Some(browser_hub), Some(recon_hub));
    denia_tools::capabilities::register(&mut tools, runtime.clone());
    tools.replace(Arc::new(
        denia_tools::BashTool::new().with_runtime(runtime.clone()),
    ));
    // `ask` 工具:控制台部署有应答通道,注册工具并挂桥。
    tools.register(Arc::new(denia_tools::AskTool::new()));
    let approval = Arc::new(ServerApprovalBridge::new(live.clone()));
    let ask = Arc::new(ServerAskBridge::new(live.clone()));
    let console = console_settings(&settings);
    let driver = Arc::new(
        SessionDriver::new(registry.clone(), Arc::new(tools), system_prompt.handle())
            .with_file_history(file_history.clone())
            .with_approval(approval)
            .with_ask(ask)
            .with_runtime(runtime.clone())
            .with_compaction(compaction_settings_from(&console))
            .with_parallel(denia_agent_loop::ParallelSettings {
                max_parallel_tool_calls: console.max_parallel_tool_calls,
            }),
    );

    runtime.attach(&driver);

    spawn_forwarders(
        settings_events.1,
        credentials_events.1,
        events.clone(),
        settings.clone(),
        registry.clone(),
        openai.clone(),
    );
    system_prompt.spawn_watcher(events.clone());

    let state = AppState {
        runtime,
        home: home.to_path_buf(),
        settings,
        credentials,
        registry,
        events,
        http: reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .build()?,
        sessions,
        live,
        driver,
        workspaces,
        system_prompt,
        file_history,
        browser,
        bound_remote,
    };

    Ok(state)
}

fn register_namespaces(settings: &SettingsStore) -> Result<(), Box<dyn std::error::Error>> {
    settings.register(
        "runtime",
        NamespaceSpec {
            defaults: serde_json::to_value(crate::agent_runtime::RuntimeConfig::default())?,
            validate: crate::agent_runtime::validate_config,
            secrets: &[],
            applies: Applies::Live,
        },
        json!({}),
    )?;
    settings.register(
        CONSOLE_NS,
        NamespaceSpec {
            defaults: json!({ "theme": "system", "locale": "zh" }),
            validate: validate_console,
            secrets: &[],
            applies: Applies::Live,
        },
        json!({}),
    )?;
    settings.register(
        OPENAI_SETTINGS_NS,
        NamespaceSpec {
            defaults: json!({ "providers": {} }),
            validate: validate_with::<OpenAiSection>,
            secrets: &[],
            applies: Applies::Live,
        },
        json!({}),
    )?;
    Ok(())
}

fn sync_openai_routes(
    registry: &LlmRegistry,
    settings: &SettingsStore,
    adapter: &Arc<OpenAiCompatAdapter>,
) {
    let routes: Vec<String> = OpenAiSection::from_settings(settings)
        .map(|section| section.providers.keys().cloned().collect())
        .unwrap_or_default();
    registry.sync_routes(&routes, adapter.clone(), RetryPolicy::default());
}

/// Forwards store notifications onto the console push channel and keeps
/// OpenAI-compatible routes aligned with settings writes.
fn spawn_forwarders(
    mut settings_rx: broadcast::Receiver<SettingsEvent>,
    mut credentials_rx: broadcast::Receiver<CredentialEvent>,
    events: broadcast::Sender<ServerEvent>,
    settings: Arc<SettingsStore>,
    registry: Arc<LlmRegistry>,
    openai: Arc<OpenAiCompatAdapter>,
) {
    let credential_events = events.clone();
    tokio::spawn(async move {
        loop {
            match settings_rx.recv().await {
                Ok(SettingsEvent::DocumentUpdated { ns, .. }) => {
                    let _ = events.send(ServerEvent::SettingsUpdated);
                    if ns == OPENAI_SETTINGS_NS {
                        let before = registry.list_providers().len();
                        sync_openai_routes(&registry, &settings, &openai);
                        let after = registry.list_providers().len();
                        if before != after {
                            let _ = events.send(ServerEvent::LlmUpdated);
                        }
                    }
                }
                Ok(SettingsEvent::Updated { .. }) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    tokio::spawn(async move {
        loop {
            match credentials_rx.recv().await {
                Ok(CredentialEvent::ReferenceUpdated { .. }) => {
                    let _ = credential_events.send(ServerEvent::CredentialsUpdated);
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_tools::AskBridge as _;
    use std::sync::atomic::Ordering;

    fn temp_root() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("denia-live-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn running_ids_lists_only_active_turns() {
        let root = temp_root();
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let store = SessionStore::open(&root).unwrap();
        let idle = store.create(&cwd, true).unwrap();
        let active = store.create(&cwd, true).unwrap();
        let live = LiveSessions::new(8);
        let idle_live = live.get_or_load(&store, idle.id()).unwrap();
        let active_live = live.get_or_load(&store, active.id()).unwrap();
        active_live.running.store(true, Ordering::SeqCst);
        let mut ids = live.running_ids();
        ids.sort();
        assert_eq!(ids, vec![active.id().to_string()]);
        drop(idle_live);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn running_snapshot_serializes_for_sse() {
        let event = ServerEvent::RunningSnapshot {
            ids: vec!["s1".into(), "s2".into()],
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["type"], "running-snapshot");
        assert_eq!(value["ids"], serde_json::json!(["s1", "s2"]));
    }

    fn ask_questions() -> Vec<denia_core::session::AskQuestion> {
        use denia_core::session::{AskOption, AskQuestion};
        vec![AskQuestion {
            id: "q1".to_string(),
            question: "继续吗?".to_string(),
            header: None,
            detail: None,
            options: vec![AskOption {
                label: "继续".to_string(),
                description: None,
                recommended: true,
            }],
            multi_select: false,
            allow_custom: true,
        }]
    }

    fn ask_setup() -> (std::path::PathBuf, Arc<LiveSessions>, String) {
        let root = temp_root();
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let store = SessionStore::open(&root).unwrap();
        let session = store.create(&cwd, true).unwrap();
        let id = session.id().to_string();
        let live = Arc::new(LiveSessions::new(8));
        live.get_or_load(&store, &id).unwrap();
        (root, live, id)
    }

    /// 作答路径:挂起请求 → 外部应答 → 桥返回 Answered,并把结果落进日志。
    #[tokio::test]
    async fn ask_bridge_settles_on_answer() {
        use denia_core::session::{AskAnswer, AskOutcome, AskResolution};
        let (root, live, id) = ask_setup();
        let bridge = ServerAskBridge::new(live.clone());
        let live_for_answer = live.clone();
        let session_id = id.clone();
        // 等请求挂起后再应答(桥先落盘 AskRequested,再进 select)。
        tokio::spawn(async move {
            loop {
                let sender = {
                    let live = live_for_answer.get(&session_id).unwrap();
                    let mut pending = live.pending_asks.lock().unwrap();
                    pending.remove("req-1")
                };
                if let Some(sender) = sender {
                    let _ = sender.send(AskResolution {
                        outcome: AskOutcome::Answered,
                        answers: vec![AskAnswer {
                            id: "q1".to_string(),
                            selected: vec!["继续".to_string()],
                            custom: None,
                            skipped: false,
                        }],
                        reason: None,
                    });
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        });
        let resolution = bridge
            .ask(
                &id,
                "req-1",
                "call-1",
                &ask_questions(),
                5_000,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(resolution.outcome, AskOutcome::Answered);
        assert_eq!(resolution.answers[0].selected, vec!["继续".to_string()]);
        // 请求与结算都落盘:刷新页面能重建挂起态与结果。
        let events = live.get(&id).unwrap().session.events();
        assert!(events
            .iter()
            .any(|envelope| matches!(&envelope.event, denia_core::session::SessionEvent::AskRequested { request_id, .. } if request_id == "req-1")));
        assert!(events
            .iter()
            .any(|envelope| matches!(&envelope.event, denia_core::session::SessionEvent::AskResolved { request_id, .. } if request_id == "req-1")));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// 超时路径:无人应答时按 TimedOut 结算,工具不会永久挂起。
    #[tokio::test]
    async fn ask_bridge_times_out_without_answer() {
        use denia_core::session::AskOutcome;
        let (root, live, id) = ask_setup();
        let bridge = ServerAskBridge::new(live.clone());
        let resolution = bridge
            .ask(
                &id,
                "req-2",
                "call-2",
                &ask_questions(),
                30,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(resolution.outcome, AskOutcome::TimedOut);
        // 超时后挂起表必须清空,重复应答会得到 404(不覆盖已结算结果)。
        assert!(live.get(&id).unwrap().pending_asks.lock().unwrap().is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// 取消路径:轮次中断时挂起提问立刻结算为 Cancelled。
    #[tokio::test]
    async fn ask_bridge_cancels_on_token() {
        use denia_core::session::AskOutcome;
        let (root, live, id) = ask_setup();
        let bridge = ServerAskBridge::new(live.clone());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let resolution = bridge
            .ask(&id, "req-3", "call-3", &ask_questions(), 60_000, cancel)
            .await;
        assert_eq!(resolution.outcome, AskOutcome::Cancelled);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// 子代理不与用户交互:提问一律 unavailable,并给出可执行的下一步。
    #[tokio::test]
    async fn ask_bridge_refuses_subagent_sessions() {
        use denia_core::session::{AskOutcome, SubagentDescriptor};
        let root = temp_root();
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let store = SessionStore::open(&root).unwrap();
        let parent = store.create(&cwd, true).unwrap();
        let child = store
            .create_subagent(
                &cwd,
                true,
                parent.id(),
                SubagentDescriptor {
                    label: "worker".to_string(),
                    depth: 1,
                    mode: "default".to_string(),
                    selection: denia_core::config::ModelSelection {
                        provider: "test".to_string(),
                        model: "test".to_string(),
                        reasoning_effort: None,
                    },
                    persona: None,
                    allowed_tools: None,
                },
            )
            .unwrap();
        let live = Arc::new(LiveSessions::new(8));
        live.get_or_load(&store, child.id()).unwrap();
        let bridge = ServerAskBridge::new(live);
        let resolution = bridge
            .ask(
                child.id(),
                "req-5",
                "call-5",
                &ask_questions(),
                60_000,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(resolution.outcome, AskOutcome::Unavailable);
        let reason = resolution.reason.expect("子代理拒绝必须给出原因");
        assert!(reason.contains("子代理"), "{reason}");
        assert!(reason.contains("自行决策"), "{reason}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// 无会话时按 unavailable 结算(模型自行决策),而不是硬错误。
    #[tokio::test]
    async fn ask_bridge_reports_unavailable_without_session() {
        use denia_core::session::AskOutcome;
        let live = Arc::new(LiveSessions::new(8));
        let bridge = ServerAskBridge::new(live);
        let resolution = bridge
            .ask(
                "missing",
                "req-4",
                "call-4",
                &ask_questions(),
                1_000,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(resolution.outcome, AskOutcome::Unavailable);
        assert!(resolution.reason.is_some());
    }
}
