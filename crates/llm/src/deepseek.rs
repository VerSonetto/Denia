//! The DeepSeek official provider adapter (route `deepseek-official`).
//!
//! Every operation re-resolves its settings section, so console edits reach
//! the next request without re-registration.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dshrs_core::error::{LlmError, codes};
use dshrs_credentials::CredentialStore;
use dshrs_settings::SettingsStore;
use serde::{Deserialize, Serialize};

use crate::catalog::{LlmModelInfo, LlmResolvedModelInfo, ProviderInfo, ReasoningEffortInfo, ReasoningInfo};
use crate::http::http_error_failure;
use crate::request::GenerateRequest;
use crate::sse::sse_chunk_stream;
use crate::wire::{UsageStyle, build_wire_messages, build_wire_tools};
use crate::{ChunkStream, LlmAdapter};

pub const DEEPSEEK_PROVIDER: &str = "deepseek-official";
pub const DEEPSEEK_SETTINGS_NS: &str = "llm-deepseek";

const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
const DEFAULT_CONTEXT_WINDOW: u64 = 1_000_000;
const DEFAULT_MAX_TOKENS: u64 = 256_000;
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const USER_AGENT: &str = concat!("dsh-rs/", env!("CARGO_PKG_VERSION"));

/// The `llm-deepseek` settings section shape.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepSeekSection {
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    #[serde(default, rename = "baseURL")]
    pub base_url: Option<String>,
    /// Deployment thinking mode.
    #[serde(default)]
    pub thinking: ThinkingMode,
    /// Default effort when a request carries none.
    #[serde(default)]
    pub reasoning_effort: ReasoningEffort,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub default_context_window: Option<u64>,
    /// Advisory model catalog; absent falls back to the shipped defaults.
    #[serde(default)]
    pub models: Option<Vec<DeepSeekCatalogModel>>,
}

/// Deployment thinking mode; `enabled` allows non-`off` efforts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingMode {
    #[default]
    Enabled,
    Disabled,
}

/// The four DeepSeek reasoning effort levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Off,
    Low,
    #[default]
    High,
    Max,
}

impl ReasoningEffort {
    /// The wire/settings identifier.
    pub fn as_id(self) -> &'static str {
        match self {
            ReasoningEffort::Off => "off",
            ReasoningEffort::Low => "low",
            ReasoningEffort::High => "high",
            ReasoningEffort::Max => "max",
        }
    }

    /// Parses one identifier, rejecting unknown efforts.
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "off" => Some(Self::Off),
            "low" => Some(Self::Low),
            "high" => Some(Self::High),
            "max" => Some(Self::Max),
            _ => None,
        }
    }
}

fn default_api_key_env() -> String {
    "DEEPSEEK_API_KEY".to_string()
}

/// One advisory catalog row in the settings section.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepSeekCatalogModel {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub input_modalities: Option<Vec<String>>,
}

fn default_catalog() -> Vec<DeepSeekCatalogModel> {
    vec![
        DeepSeekCatalogModel {
            id: "deepseek-v4-flash".to_string(),
            name: Some("DeepSeek V4 Flash".to_string()),
            description: Some("Fast general-purpose model".to_string()),
            context_window: None,
            max_tokens: None,
            input_modalities: Some(vec!["text".to_string()]),
        },
        DeepSeekCatalogModel {
            id: "deepseek-v4-pro".to_string(),
            name: Some("DeepSeek V4 Pro".to_string()),
            description: Some("High-capability reasoning model".to_string()),
            context_window: None,
            max_tokens: None,
            input_modalities: Some(vec!["text".to_string()]),
        },
        DeepSeekCatalogModel {
            id: "deepseek-v4-flash-vision-exp".to_string(),
            name: Some("DeepSeek V4 Flash Vision (experimental)".to_string()),
            description: Some("Text + image input".to_string()),
            context_window: None,
            max_tokens: None,
            input_modalities: Some(vec!["text".to_string(), "image".to_string()]),
        },
    ]
}

const KNOWN_EFFORTS: &[(&str, &str)] = &[
    ("off", "Off"),
    ("low", "Low"),
    ("high", "High"),
    ("max", "Max"),
];

/// The chat-completions request body for one resolved effort.
pub(crate) fn build_deepseek_body(
    request: &GenerateRequest,
    effort: ReasoningEffort,
    section: &DeepSeekSection,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": request.model,
        "messages": build_wire_messages(request),
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if effort == ReasoningEffort::Off {
        body["thinking"] = serde_json::json!({ "type": "disabled" });
    } else {
        body["thinking"] = serde_json::json!({ "type": "enabled" });
        body["reasoning_effort"] = serde_json::json!(effort.as_id());
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = serde_json::json!(temperature);
    }
    if let Some(max_tokens) = request.max_tokens.or(section.max_tokens) {
        body["max_tokens"] = serde_json::json!(max_tokens);
    }
    if !request.stop.is_empty() {
        body["stop"] = serde_json::json!(request.stop);
    }
    let tools = build_wire_tools(&request.tools);
    if !tools.is_empty() {
        body["tools"] = serde_json::Value::Array(tools);
    }
    body
}

pub struct DeepSeekAdapter {
    settings: Arc<SettingsStore>,
    credentials: Arc<CredentialStore>,
    http: reqwest::Client,
}

impl DeepSeekAdapter {
    pub fn new(settings: Arc<SettingsStore>, credentials: Arc<CredentialStore>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client builds");
        Self {
            settings,
            credentials,
            http,
        }
    }

    fn section(&self) -> Result<DeepSeekSection, LlmError> {
        let value = self
            .settings
            .resolved(DEEPSEEK_SETTINGS_NS)
            .map_err(|e| LlmError::new(codes::SETTINGS, e.to_string()))?;
        serde_json::from_value(value)
            .map_err(|e| LlmError::new(codes::SETTINGS, format!("invalid llm-deepseek section: {e}")))
    }

    fn base_url(section: &DeepSeekSection) -> String {
        section
            .base_url
            .clone()
            .or_else(|| std::env::var("DEEPSEEK_BASE_URL").ok().filter(|v| !v.is_empty()))
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
    }

    fn catalog(section: &DeepSeekSection) -> Vec<DeepSeekCatalogModel> {
        section.models.clone().unwrap_or_else(default_catalog)
    }

    fn resolve_api_key(&self, section: &DeepSeekSection) -> Result<String, LlmError> {
        match self.credentials.resolve(&section.api_key_env) {
            Ok(Some(resolved)) => {
                let value = resolved.value.trim().to_string();
                if value.is_empty() || !value.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
                    return Err(LlmError::new(
                        codes::INVALID_CREDENTIAL,
                        format!("the key behind {} is not usable", section.api_key_env),
                    ));
                }
                Ok(value)
            }
            Ok(None) => Err(LlmError::new(
                codes::MISSING_CREDENTIAL,
                format!(
                    "store {} through the credentials API (or export it) before calling the model",
                    section.api_key_env
                ),
            )),
            Err(error) => Err(LlmError::new(codes::MISSING_CREDENTIAL, error.to_string())),
        }
    }

    fn reasoning_info(section: &DeepSeekSection) -> ReasoningInfo {
        if section.thinking == ThinkingMode::Disabled {
            return ReasoningInfo {
                efforts: vec![ReasoningEffortInfo {
                    id: "off".to_string(),
                    name: "Off".to_string(),
                    description: None,
                }],
                default_effort: Some("off".to_string()),
            };
        }
        ReasoningInfo {
            efforts: KNOWN_EFFORTS
                .iter()
                .map(|(id, name)| ReasoningEffortInfo {
                    id: id.to_string(),
                    name: name.to_string(),
                    description: None,
                })
                .collect(),
            default_effort: Some(section.reasoning_effort.as_id().to_string()),
        }
    }
}

#[async_trait]
impl LlmAdapter for DeepSeekAdapter {
    fn provider_info(&self, _provider: &str) -> ProviderInfo {
        ProviderInfo {
            id: DEEPSEEK_PROVIDER.to_string(),
            name: "DeepSeek".to_string(),
        }
    }

    async fn list_models(&self, _provider: &str) -> Result<Vec<LlmModelInfo>, LlmError> {
        let section = self.section()?;
        Ok(Self::catalog(&section)
            .into_iter()
            .map(|model| {
                let name = model.name.clone().unwrap_or_else(|| model.id.clone());
                LlmModelInfo {
                    provider: DEEPSEEK_PROVIDER.to_string(),
                    id: model.id,
                    name,
                    description: model.description,
                    input_modalities: model.input_modalities.unwrap_or_default(),
                }
            })
            .collect())
    }

    async fn resolve_model(
        &self,
        _provider: &str,
        model: &str,
    ) -> Result<LlmResolvedModelInfo, LlmError> {
        let section = self.section()?;
        let context_window_default = section
            .default_context_window
            .unwrap_or(DEFAULT_CONTEXT_WINDOW);
        let max_tokens_default = section.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
        let found = Self::catalog(&section).into_iter().find(|m| m.id == model);
        let (name, description, modalities, context_window, max_tokens) = match found {
            Some(model) => (
                model.name.unwrap_or_else(|| model.id.clone()),
                model.description,
                model.input_modalities.unwrap_or_else(|| vec!["text".to_string()]),
                model.context_window.unwrap_or(context_window_default),
                model.max_tokens.unwrap_or(max_tokens_default),
            ),
            // The catalog is advisory: unknown models pass as text-only.
            None => (
                model.to_string(),
                None,
                vec!["text".to_string()],
                context_window_default,
                max_tokens_default,
            ),
        };
        Ok(LlmResolvedModelInfo {
            info: LlmModelInfo {
                provider: DEEPSEEK_PROVIDER.to_string(),
                id: model.to_string(),
                name,
                description,
                input_modalities: modalities,
            },
            context_window: Some(context_window),
            default_max_tokens: Some(max_tokens),
            reasoning: Some(Self::reasoning_info(&section)),
        })
    }

    async fn stream(
        &self,
        _provider: &str,
        request: &GenerateRequest,
    ) -> Result<ChunkStream, LlmError> {
        let section = self.section()?;
        let api_key = self.resolve_api_key(&section)?;

        let effort = match &request.reasoning_effort {
            Some(raw) => ReasoningEffort::from_id(raw).ok_or_else(|| {
                LlmError::new(
                    codes::UNSUPPORTED_REASONING_EFFORT,
                    format!("unknown DeepSeek reasoning effort: '{raw}'"),
                )
            })?,
            None => section.reasoning_effort,
        };
        if effort != ReasoningEffort::Off && section.thinking == ThinkingMode::Disabled {
            return Err(LlmError::new(
                codes::UNSUPPORTED_REASONING_EFFORT,
                "this deployment disables thinking; only the 'off' effort is supported",
            ));
        }

        let body = build_deepseek_body(request, effort, &section);

        let url = format!("{}/chat/completions", Self::base_url(&section).trim_end_matches('/'));
        let response = self
            .http
            .post(&url)
            .header("authorization", format!("Bearer {api_key}"))
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .header("user-agent", USER_AGENT)
            .json(&body)
            .send()
            .await
            .map_err(crate::http::transport_error)?;

        if !response.status().is_success() {
            let status = response.status();
            let headers = response.headers().clone();
            let body_text = response.text().await.unwrap_or_default();
            return Err(LlmError::from_failure(http_error_failure(status, &body_text, &headers)));
        }
        Ok(sse_chunk_stream(response, UsageStyle::DeepSeek, STREAM_IDLE_TIMEOUT))
    }
}
