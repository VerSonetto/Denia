//! Composition root: opens stores, registers namespaces and adapters, and
//! keeps OpenAI-compatible routes in sync with settings.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use dshrs_agent_loop::{LoopLimits, SessionDriver};
use dshrs_core::config::ModelSelection;
use dshrs_credentials::{CredentialEvent, CredentialStore};
use dshrs_llm::{
    DEEPSEEK_PROVIDER, DEEPSEEK_SETTINGS_NS, DeepSeekAdapter, DeepSeekSection, LlmRegistry,
    OPENAI_SETTINGS_NS, OpenAiCompatAdapter, OpenAiSection, ReasoningEffort, RetryPolicy,
    ThinkingMode,
};
use dshrs_session::{Session, SessionError, SessionStore};
use dshrs_settings::{Applies, NamespaceSpec, SettingsEvent, SettingsStore};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_MODEL_NS: &str = "agent-default-model";

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
    /// The sandboxed default working directory for sessions.
    pub workspace: PathBuf,
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

/// One materialized session: durable log plus live fan-out.
pub struct LiveSession {
    pub session: tokio::sync::Mutex<Session>,
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
            session: tokio::sync::Mutex::new(session),
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

pub fn build_state(home: &Path) -> Result<AppState, Box<dyn std::error::Error>> {
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

    // DeepSeek route is always live.
    let deepseek = Arc::new(DeepSeekAdapter::new(
        settings.clone(),
        credentials.clone(),
    ));
    registry.register(
        &[DEEPSEEK_PROVIDER.to_string()],
        deepseek.clone(),
        RetryPolicy::default(),
    )?;

    // OpenAI-compatible routes follow their settings section.
    let openai = Arc::new(OpenAiCompatAdapter::new(
        settings.clone(),
        credentials.clone(),
    ));
    sync_openai_routes(&registry, &settings, &openai);

    // Sessions, tools, and the driver.
    let workspace = home.join("workspace");
    std::fs::create_dir_all(&workspace)?;
    let sessions = Arc::new(SessionStore::open(home)?);
    let live = Arc::new(LiveSessions::default());
    let tools = Arc::new(dshrs_tools::default_registry());
    let driver = Arc::new(SessionDriver::new(
        registry.clone(),
        tools,
        LoopLimits::default(),
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
        workspace,
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
            "provider": DEEPSEEK_PROVIDER,
            "model": "deepseek-v4-flash",
        }),
    )?;
    settings.register(
        DEEPSEEK_SETTINGS_NS,
        NamespaceSpec {
            defaults: serde_json::to_value(DeepSeekSection {
                api_key_env: "DEEPSEEK_API_KEY".to_string(),
                base_url: None,
                thinking: ThinkingMode::Enabled,
                reasoning_effort: ReasoningEffort::High,
                max_tokens: Some(256_000),
                default_context_window: Some(1_000_000),
                models: None,
            })?,
            validate: validate_with::<DeepSeekSection>,
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
            provider: DEEPSEEK_PROVIDER.to_string(),
            model: "deepseek-v4-flash".to_string(),
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
                    } else if ns == DEEPSEEK_SETTINGS_NS {
                        let _ = events.send(ServerEvent::LlmUpdated);
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
