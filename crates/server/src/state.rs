//! Composition root: opens stores, registers namespaces and adapters, and
//! keeps OpenAI-compatible routes in sync with settings.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use dshrs_agent_loop::SessionDriver;
use dshrs_core::config::ModelSelection;
use dshrs_credentials::{CredentialEvent, CredentialStore};
use dshrs_llm::{RetryPolicy, OPENAI_SETTINGS_NS, OpenAiCompatAdapter, OpenAiSection, LlmRegistry};
use dshrs_session::{Session, SessionError, SessionStore};
use dshrs_settings::{Applies, NamespaceSpec, SettingsEvent, SettingsStore};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_MODEL_NS: &str = "agent-default-model";

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
    pub settings: Arc<SettingsStore>,
    pub credentials: Arc<CredentialStore>,
    pub registry: Arc<LlmRegistry>,
    pub events: broadcast::Sender<ServerEvent>,
    pub http: reqwest::Client,
    pub sessions: Arc<SessionStore>,
    pub live: Arc<LiveSessions>,
    pub driver: Arc<SessionDriver>,
    pub workspaces: Arc<crate::workspace::WorkspaceRegistry>,
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
}

/// One materialized session: durable log plus live fan-out. `session` is an
/// internally synchronized log, so readers never wait behind a running turn.
pub struct LiveSession {
    pub session: Arc<Session>,
    pub followers: broadcast::Sender<dshrs_core::session::SessionEnvelope>,
    pub running: AtomicBool,
    pub cancel: tokio::sync::Mutex<Option<CancellationToken>>,
}

/// Live sessions keyed by id; loads (and repairs) on first touch.
#[derive(Default)]
pub struct LiveSessions {
    inner: std::sync::Mutex<HashMap<String, Arc<LiveSession>>>,
}

impl LiveSessions {
    pub fn get_or_load(
        &self,
        store: &SessionStore,
        id: &str,
    ) -> Result<Arc<LiveSession>, SessionError> {
        let mut map = self.inner.lock().unwrap();
        if let Some(live) = map.get(id) {
            return Ok(live.clone());
        }
        let session = store.load(id)?;
        let live = Arc::new(LiveSession {
            session: Arc::new(session),
            followers: broadcast::channel(1024).0,
            running: AtomicBool::new(false),
            cancel: tokio::sync::Mutex::new(None),
        });
        map.insert(id.to_string(), live.clone());
        Ok(live)
    }

    pub fn remove(&self, id: &str) {
        self.inner.lock().unwrap().remove(id);
    }
}

/// The persisted `agent-default-model` section shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DefaultModelSettings {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

impl From<DefaultModelSettings> for ModelSelection {
    fn from(settings: DefaultModelSettings) -> Self {
        ModelSelection {
            provider: settings.provider,
            model: settings.model,
            reasoning_effort: settings.reasoning_effort,
        }
    }
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
    let (prompt, tools) = dshrs_tools::default_shipped();
    let driver = Arc::new(SessionDriver::new(
        registry.clone(),
        Arc::new(tools),
        Arc::new(prompt),
    ));

    spawn_forwarders(
        settings_events.1,
        credentials_events.1,
        events.clone(),
        settings.clone(),
        registry.clone(),
        openai.clone(),
    );

    let state = AppState {
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
        bound_remote,
    };

    Ok(state)
}

fn register_namespaces(settings: &SettingsStore) -> Result<(), Box<dyn std::error::Error>> {
    settings.register(
        DEFAULT_MODEL_NS,
        NamespaceSpec {
            defaults: json!({}),
            validate: validate_with::<DefaultModelSettings>,
            secrets: &[],
            applies: Applies::Live,
        },
        json!({
            "provider": "",
            "model": "",
        }),
    )?;
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

/// Reads the current default model selection from settings.
pub fn current_default_selection(settings: &SettingsStore) -> ModelSelection {
    let selection = settings
        .resolved(DEFAULT_MODEL_NS)
        .ok()
        .and_then(|value| serde_json::from_value::<DefaultModelSettings>(value).ok());
    match selection {
        Some(selection) => selection.into(),
        None => ModelSelection {
            provider: String::new(),
            model: String::new(),
            reasoning_effort: None,
        },
    }
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
