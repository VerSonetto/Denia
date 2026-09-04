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
use denia_llm::{RetryPolicy, OPENAI_SETTINGS_NS, OpenAiCompatAdapter, OpenAiSection, LlmRegistry};
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
    /// Confine file tools to the working directory.
    #[serde(default = "default_true")]
    pub sandbox: bool,
    /// `system` | `light` | `dark`.
    #[serde(default = "default_theme")]
    pub theme: String,
    /// `zh` | `en`;zh 是源语言(学 dsh 的 i18n 约定)。
    #[serde(default = "default_locale")]
    pub locale: String,
}


fn default_true() -> bool {
    true
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
        return Err(format!("locale must be 'zh' or 'en'; got '{}'", parsed.locale));
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
            sandbox: true,
            theme: "system".to_string(),
            locale: "zh".to_string(),
        })
}


/// Process-wide shared state handed to every handler.
pub struct AppState {
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
    RunningChanged { id: String, running: bool },
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
    /// 等待用户决策的审批请求(request_id → oneshot)。
    pub pending_approvals: std::sync::Mutex<
        HashMap<String, tokio::sync::oneshot::Sender<ApprovalOutcome>>,
    >,
}

/// Live sessions keyed by id; loads (and repairs) on first touch.
///
/// ## 生命周期(内存上限)
///
/// - 每次 `get_or_load` / `touch` 都会刷新 `last_touch`。
/// - 后台淘汰任务(见 [`spawn_live_evictor`])周期清理:非运行中、
///   无 SSE 订阅者、且超过 `idle_after` 未使用的会话被卸载,事件内存
///   随之释放;下次访问按日志重新加载。运行中的会话永不淘汰。
#[derive(Default)]
pub struct LiveSessions {
    inner: std::sync::Mutex<HashMap<String, Arc<LiveSession>>>,
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
            live.last_touch.store(now_millis(), std::sync::atomic::Ordering::Relaxed);
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
        });
        map.insert(id.to_string(), live.clone());
        Ok(live)
    }

    /// 读取已加载的 live 会话(不触发磁盘加载)。
    pub fn get(&self, id: &str) -> Option<Arc<LiveSession>> {
        self.inner.lock().unwrap().get(id).cloned()
    }

    /// 记录一次外部访问(follow 连接等),防止被空闲淘汰。
    pub fn touch(&self, id: &str) {
        if let Some(live) = self.inner.lock().unwrap().get(id) {
            live.last_touch.store(now_millis(), std::sync::atomic::Ordering::Relaxed);
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
                let idle = now.saturating_sub(
                    live.last_touch.load(std::sync::atomic::Ordering::Relaxed),
                ) >= idle_ms;
                if !running && !subscribed && idle {
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
    ) -> ApprovalOutcome {
        let Some(live) = self.live.get(session_id) else {
            return ApprovalOutcome::Unavailable;
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
                    let _ = tx.send(ApprovalOutcome::Cancelled);
                }
                ApprovalOutcome::Cancelled
            }
            result = rx => {
                let _ = live
                    .pending_approvals
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(request_id);
                result.unwrap_or(ApprovalOutcome::Cancelled)
            }
        }
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
        self.live.running.store(false, Ordering::SeqCst);
        // 任务无论正常/panic 结束,未答审批一律按 cancelled 结算,
        // 避免 pending oneshot 泄漏。
        {
            let mut pending = self
                .live
                .pending_approvals
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            for tx in pending.drain() {
                let _ = tx.1.send(ApprovalOutcome::Cancelled);
            }
        }
        let mut cancel = self.live.cancel.lock().unwrap();
        *cancel = None;
        let _ = self.events.send(ServerEvent::RunningChanged {
            id: self.live.session.id().to_string(),
            running: false,
        });
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

pub fn build_state(home: &Path, bound_remote: bool) -> Result<AppState, Box<dyn std::error::Error>> {
    let events = broadcast::channel::<ServerEvent>(64).0;

    let settings_events = broadcast::channel::<SettingsEvent>(64);
    let credentials_events = broadcast::channel::<CredentialEvent>(64);

    let settings = Arc::new(
        SettingsStore::open(home)?.with_events(settings_events.0.clone()),
    );
    let credentials = Arc::new(
        CredentialStore::open(home)?.with_events(credentials_events.0.clone()),
    );
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
    // 工作区注册表:独立持久化域;首启按会话头 cwd 自动分组。
    let workspaces = Arc::new(crate::workspace::WorkspaceRegistry::open(home)?);
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
    let tools = denia_tools::default_registry_with_browser_and_recon(Some(browser_hub), Some(recon_hub));
    let approval = Arc::new(ServerApprovalBridge::new(live.clone()));
    let driver = Arc::new(
        SessionDriver::new(
            registry.clone(),
            Arc::new(tools),
            system_prompt.handle(),
        )
        .with_file_history(file_history.clone())
        .with_approval(approval),
    );

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
        CONSOLE_NS,
        NamespaceSpec {
            defaults: json!({ "sandbox": true, "theme": "system", "locale": "zh" }),
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
