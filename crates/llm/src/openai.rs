//! OpenAI-compatible provider adapter: settings-declared routes speaking the
//! chat-completions wire (any gateway: OpenAI, Azure-style, vLLM, etc.).
//!
//! Routes live in the `llm-openai` settings section's `providers` map; the
//! server syncs registry routes from it on every settings change.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dshrs_core::error::{LlmError, codes};
use dshrs_credentials::CredentialStore;
use dshrs_settings::SettingsStore;
use serde::{Deserialize, Serialize};

use crate::catalog::{DiscoveredModel, LlmModelInfo, LlmResolvedModelInfo, ProviderInfo};
use crate::http::http_error_failure;
use crate::request::GenerateRequest;
use crate::sse::sse_chunk_stream;
use crate::wire::{UsageStyle, build_wire_messages, build_wire_tools};
use crate::{ChunkStream, LlmAdapter};

pub const OPENAI_SETTINGS_NS: &str = "llm-openai";

const DEFAULT_CONTEXT_WINDOW: u64 = 262_144;
const DEFAULT_MAX_TOKENS: u64 = 32_768;
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const DISCOVERY_RESPONSE_CAP: usize = 4 * 1024 * 1024;
const USER_AGENT: &str = concat!("dsh-rs/", env!("CARGO_PKG_VERSION"));

/// The `llm-openai` settings section shape.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAiSection {
    #[serde(default)]
    pub providers: BTreeMap<String, OpenAiProfile>,
}

/// One configured gateway route.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAiProfile {
    #[serde(rename = "baseURL")]
    pub base_url: String,
    #[serde(default)]
    pub display_name: Option<String>,
    /// Credential reference holding the key; absent sends no authorization.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Advisory catalog; empty routes fall back to endpoint discovery.
    #[serde(default)]
    pub models: Vec<OpenAiCatalogModel>,
    #[serde(default)]
    pub default_context_window: Option<u64>,
    #[serde(default)]
    pub default_max_tokens: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAiCatalogModel {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
}

impl OpenAiSection {
    /// Parses the resolved settings section.
    pub fn from_settings(settings: &SettingsStore) -> Result<Self, LlmError> {
        let value = settings
            .resolved(OPENAI_SETTINGS_NS)
            .map_err(|e| LlmError::new(codes::SETTINGS, e.to_string()))?;
        serde_json::from_value(value)
            .map_err(|e| LlmError::new(codes::SETTINGS, format!("invalid llm-openai section: {e}")))
    }
}

pub struct OpenAiCompatAdapter {
    settings: Arc<SettingsStore>,
    credentials: Arc<CredentialStore>,
    http: reqwest::Client,
}

impl OpenAiCompatAdapter {
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

    fn profile(&self, provider: &str) -> Result<OpenAiProfile, LlmError> {
        let section = OpenAiSection::from_settings(&self.settings)?;
        section.providers.get(provider).cloned().ok_or_else(|| {
            LlmError::new(
                codes::SETTINGS,
                format!("provider '{provider}' is not configured in {OPENAI_SETTINGS_NS}"),
            )
        })
    }

    fn resolve_api_key(&self, profile: &OpenAiProfile) -> Result<Option<String>, LlmError> {
        let Some(reference) = &profile.api_key_env else {
            return Ok(None);
        };
        match self.credentials.resolve(reference) {
            Ok(Some(resolved)) => {
                let value = resolved.value.trim().to_string();
                if value.is_empty() || !value.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
                    return Err(LlmError::new(
                        codes::INVALID_CREDENTIAL,
                        format!("the key behind {reference} is not usable"),
                    ));
                }
                Ok(Some(value))
            }
            Ok(None) => Err(LlmError::new(
                codes::MISSING_CREDENTIAL,
                format!("store {reference} through the credentials API before calling the model"),
            )),
            Err(error) => Err(LlmError::new(codes::MISSING_CREDENTIAL, error.to_string())),
        }
    }
}

#[async_trait]
impl LlmAdapter for OpenAiCompatAdapter {
    fn provider_info(&self, provider: &str) -> ProviderInfo {
        let name = self
            .profile(provider)
            .ok()
            .and_then(|profile| profile.display_name)
            .unwrap_or_else(|| provider.to_string());
        ProviderInfo {
            id: provider.to_string(),
            name,
        }
    }

    async fn list_models(&self, provider: &str) -> Result<Vec<LlmModelInfo>, LlmError> {
        let profile = self.profile(provider)?;
        if !profile.models.is_empty() {
            return Ok(profile
                .models
                .iter()
                .map(|model| LlmModelInfo {
                    provider: provider.to_string(),
                    id: model.id.clone(),
                    name: model.name.clone().unwrap_or_else(|| model.id.clone()),
                    description: None,
                    input_modalities: vec!["text".to_string()],
                })
                .collect());
        }
        // No declared catalog: interrogate the endpoint itself.
        let api_key = self.resolve_api_key(&profile)?;
        let discovered =
            discover_models(&self.http, &profile.base_url, api_key.as_deref()).await?;
        Ok(discovered
            .into_iter()
            .map(|model| LlmModelInfo {
                provider: provider.to_string(),
                id: model.id.clone(),
                name: model.name.unwrap_or(model.id),
                description: None,
                input_modalities: vec!["text".to_string()],
            })
            .collect())
    }

    async fn resolve_model(
        &self,
        provider: &str,
        model: &str,
    ) -> Result<LlmResolvedModelInfo, LlmError> {
        let profile = self.profile(provider)?;
        let context_default = profile
            .default_context_window
            .unwrap_or(DEFAULT_CONTEXT_WINDOW);
        let max_tokens_default = profile.default_max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
        let found = profile.models.iter().find(|m| m.id == model);
        let (name, context_window, max_tokens) = match found {
            Some(entry) => (
                entry.name.clone().unwrap_or_else(|| entry.id.clone()),
                entry.context_window.unwrap_or(context_default),
                entry.max_tokens.unwrap_or(max_tokens_default),
            ),
            None => (model.to_string(), context_default, max_tokens_default),
        };
        Ok(LlmResolvedModelInfo {
            info: LlmModelInfo {
                provider: provider.to_string(),
                id: model.to_string(),
                name,
                description: None,
                input_modalities: vec!["text".to_string()],
            },
            context_window: Some(context_window),
            default_max_tokens: Some(max_tokens),
            reasoning: None,
        })
    }

    async fn stream(
        &self,
        provider: &str,
        request: &GenerateRequest,
    ) -> Result<ChunkStream, LlmError> {
        let profile = self.profile(provider)?;
        let api_key = self.resolve_api_key(&profile)?;

        let body = build_openai_body(request);

        let url = format!(
            "{}/chat/completions",
            profile.base_url.trim_end_matches('/')
        );
        let mut builder = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .header("user-agent", USER_AGENT);
        if let Some(api_key) = &api_key {
            builder = builder.header("authorization", format!("Bearer {api_key}"));
        }
        let response = builder.json(&body).send().await.map_err(crate::http::transport_error)?;

        if !response.status().is_success() {
            let status = response.status();
            let headers = response.headers().clone();
            let body_text = response.text().await.unwrap_or_default();
            return Err(LlmError::from_failure(http_error_failure(status, &body_text, &headers)));
        }
        Ok(sse_chunk_stream(response, UsageStyle::OpenAi, STREAM_IDLE_TIMEOUT))
    }
}

/// The chat-completions request body for one OpenAI-compatible route.
pub(crate) fn build_openai_body(request: &GenerateRequest) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": request.model,
        "messages": build_wire_messages(request),
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if let Some(temperature) = request.temperature {
        body["temperature"] = serde_json::json!(temperature);
    }
    if let Some(max_tokens) = request.max_tokens {
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

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<ModelsEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelsEntry {
    id: String,
    #[serde(default)]
    owned_by: Option<String>,
}

/// Interrogates a gateway's `GET /models` endpoint. The key is one-shot and
/// never stored.
pub async fn discover_models(
    http: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<DiscoveredModel>, LlmError> {
    if base_url.trim().is_empty() {
        return Err(LlmError::new(
            codes::INVALID_REQUEST,
            "a base URL is required for model discovery",
        ));
    }
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut builder = http
        .get(&url)
        .header("accept", "application/json")
        .header("user-agent", USER_AGENT);
    if let Some(api_key) = api_key.filter(|key| !key.trim().is_empty()) {
        builder = builder.header("authorization", format!("Bearer {}", api_key.trim()));
    }
    let response = builder.send().await.map_err(crate::http::transport_error)?;
    if !response.status().is_success() {
        let status = response.status();
        let headers = response.headers().clone();
        let body_text = response.text().await.unwrap_or_default();
        return Err(LlmError::from_failure(http_error_failure(status, &body_text, &headers)));
    }
    let bytes = response.bytes().await.map_err(crate::http::transport_error)?;
    if bytes.len() > DISCOVERY_RESPONSE_CAP {
        return Err(LlmError::new(
            codes::MALFORMED_RESPONSE,
            "the /models response exceeded the 4 MiB cap",
        ));
    }
    let parsed: ModelsResponse = serde_json::from_slice(&bytes).map_err(|error| {
        LlmError::new(
            codes::MALFORMED_RESPONSE,
            format!("unparseable /models response: {error}"),
        )
    })?;
    Ok(parsed
        .data
        .into_iter()
        .map(|entry| DiscoveredModel {
            id: entry.id.clone(),
            name: entry.owned_by.map(|owner| format!("{} ({owner})", entry.id)),
        })
        .collect())
}
