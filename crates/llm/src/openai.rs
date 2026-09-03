//! Multi-protocol gateway adapter: settings-declared routes speaking one of
//! three wire protocols — OpenAI chat-completions, OpenAI Responses, or
//! Anthropic Messages (any gateway: OpenAI, Azure-style, vLLM, etc.).
//!
//! Routes live in the `llm-openai` settings section's `providers` map; the
//! server syncs registry routes from it on every settings change.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use denia_core::error::{LlmError, codes};
use denia_credentials::CredentialStore;
use denia_settings::SettingsStore;
use serde::{Deserialize, Serialize};

use crate::catalog::{highest_reasoning_effort, DiscoveredModel, LlmModelInfo, LlmResolvedModelInfo, ProviderInfo, ReasoningEffortInfo, ReasoningInfo};
use crate::http::http_error_failure;
use crate::protocols::{
    self, CompletionsStream, EventTranslator, ResponsesStream, AnthropicStream, WireProtocol,
};
use crate::request::GenerateRequest;
use crate::sse::sse_chunk_stream;
use crate::wire::UsageStyle;
use crate::{ChunkStream, LlmAdapter};

pub const OPENAI_SETTINGS_NS: &str = "llm-openai";

const DEFAULT_CONTEXT_WINDOW: u64 = 262_144;
const DEFAULT_MAX_TOKENS: u64 = 32_768;
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const DISCOVERY_RESPONSE_CAP: usize = 4 * 1024 * 1024;
const USER_AGENT: &str = concat!("denia/", env!("CARGO_PKG_VERSION"));

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
    /// The wire protocol this route speaks; defaults to chat-completions.
    #[serde(default)]
    pub protocol: WireProtocol,
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
    pub description: Option<String>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub input_modalities: Option<Vec<String>>,
    #[serde(default)]
    pub thinking_supported: Option<bool>,
    #[serde(default)]
    pub reasoning_efforts: Option<Vec<String>>,
}

const OPENAI_KNOWN_EFFORTS: &[(&str, &str)] = &[
    ("off", "Off"),
    ("low", "Low"),
    ("medium", "Medium"),
    ("high", "High"),
    ("xhigh", "XHigh"),
    ("max", "Max"),
];

fn openai_reasoning_info(model: &OpenAiCatalogModel) -> Option<ReasoningInfo> {
    if model.thinking_supported != Some(true) {
        return None;
    }
    let effort_ids = model
        .reasoning_efforts
        .clone()
        .filter(|efforts| !efforts.is_empty())
        .unwrap_or_else(|| vec!["off".to_string()]);
    let efforts: Vec<ReasoningEffortInfo> = effort_ids
        .iter()
        .filter_map(|id| {
            OPENAI_KNOWN_EFFORTS
                .iter()
                .find(|(known_id, _)| known_id == id)
                .map(|(_, name)| ReasoningEffortInfo {
                    id: id.clone(),
                    name: name.to_string(),
                    description: None,
                })
        })
        .collect();
    if efforts.is_empty() {
        return None;
    }
    let known_order: Vec<&str> = OPENAI_KNOWN_EFFORTS.iter().map(|(id, _)| *id).collect();
    let default_effort = highest_reasoning_effort(efforts.iter().map(|effort| effort.id.as_str()), &known_order)
        .or_else(|| efforts.first().map(|effort| effort.id.clone()));
    Some(ReasoningInfo {
        efforts,
        default_effort,
    })
}

fn openai_modalities(model: &OpenAiCatalogModel) -> Vec<String> {
    model
        .input_modalities
        .clone()
        .filter(|modalities| !modalities.is_empty())
        .unwrap_or_else(|| vec!["text".to_string()])
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
            .connect_timeout(Duration::from_secs(60))
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
                    description: model.description.clone(),
                    input_modalities: openai_modalities(model),
                })
                .collect());
        }
        // No declared catalog: interrogate the endpoint itself.
        let api_key = self.resolve_api_key(&profile)?;
        let discovered =
            discover_models(&self.http, &profile.base_url, profile.protocol, api_key.as_deref())
                .await?;
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
        let (name, description, modalities, context_window, max_tokens, reasoning) = match found {
            Some(entry) => (
                entry.name.clone().unwrap_or_else(|| entry.id.clone()),
                entry.description.clone(),
                openai_modalities(entry),
                entry.context_window.unwrap_or(context_default),
                entry.max_tokens.unwrap_or(max_tokens_default),
                openai_reasoning_info(entry),
            ),
            None => (
                model.to_string(),
                None,
                vec!["text".to_string()],
                context_default,
                max_tokens_default,
                None,
            ),
        };
        Ok(LlmResolvedModelInfo {
            info: LlmModelInfo {
                provider: provider.to_string(),
                id: model.to_string(),
                name,
                description,
                input_modalities: modalities,
            },
            context_window: Some(context_window),
            default_max_tokens: Some(max_tokens),
            reasoning,
        })
    }

    async fn stream(
        &self,
        provider: &str,
        request: &GenerateRequest,
    ) -> Result<ChunkStream, LlmError> {
        let profile = self.profile(provider)?;
        let api_key = self.resolve_api_key(&profile)?;

        let protocol = profile.protocol;
        let max_tokens = request.max_tokens.or(profile.default_max_tokens);
        let body = match protocol {
            WireProtocol::ChatCompletions => protocols::build_openai_body(request),
            WireProtocol::Responses => protocols::build_responses_body(request),
            WireProtocol::AnthropicMessages => protocols::build_anthropic_body(
                request,
                max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            ),
        };

        let url = match protocol {
            WireProtocol::ChatCompletions => protocols::chat_completions_url(&profile.base_url),
            WireProtocol::Responses => protocols::responses_url(&profile.base_url),
            WireProtocol::AnthropicMessages => {
                protocols::anthropic_messages_url(&profile.base_url)
            }
        };
        let mut builder = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .header("user-agent", USER_AGENT);
        match protocol {
            WireProtocol::AnthropicMessages => {
                if let Some(api_key) = &api_key {
                    builder = builder.header("x-api-key", api_key);
                }
                builder = builder.header("anthropic-version", protocols::ANTHROPIC_VERSION);
            }
            _ => {
                if let Some(api_key) = &api_key {
                    builder = builder.header("authorization", format!("Bearer {api_key}"));
                }
            }
        }
        // 超时合理化(优化项):连接阶段 30s;请求总超时按请求体规模放宽——
        // 大体量请求(xhigh 推理 + 长上下文)提供方处理慢,实测 200K 字符
        // payload 正常 TTFB 21.8s,固定短超时会系统性错杀;30s 仅覆盖
        // 连接建立(connect_timeout 已在 client 上),这里管整个请求。
        let body_size = serde_json::to_string(&body).map(|s| s.len()).unwrap_or(0);
        let request_timeout_secs = if body_size > 100_000 { 120 } else { 60 };
        builder = builder.timeout(Duration::from_secs(request_timeout_secs));
        let response = builder.json(&body).send().await.map_err(crate::http::transport_error)?;

        if !response.status().is_success() {
            let status = response.status();
            let headers = response.headers().clone();
            let body_text = response.text().await.unwrap_or_default();
            return Err(LlmError::from_failure(http_error_failure(status, &body_text, &headers)));
        }
        let translator: Box<dyn EventTranslator> = match protocol {
            WireProtocol::ChatCompletions => Box::new(CompletionsStream::new(UsageStyle::OpenAi)),
            WireProtocol::Responses => Box::new(ResponsesStream::default()),
            WireProtocol::AnthropicMessages => Box::new(AnthropicStream::default()),
        };
        Ok(sse_chunk_stream(response, translator, STREAM_IDLE_TIMEOUT))
    }
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

/// Interrogates a gateway's model-listing endpoint. The key is one-shot and
/// never stored. OpenAI protocols authenticate with a bearer token at
/// `{base}/models`; Anthropic Messages uses `x-api-key` + `anthropic-version`
/// at `{root}/v1/models`.
pub async fn discover_models(
    http: &reqwest::Client,
    base_url: &str,
    protocol: WireProtocol,
    api_key: Option<&str>,
) -> Result<Vec<DiscoveredModel>, LlmError> {
    if base_url.trim().is_empty() {
        return Err(LlmError::new(
            codes::INVALID_REQUEST,
            "a base URL is required for model discovery",
        ));
    }
    let url = protocols::listing_url(protocol, base_url);
    let mut builder = http
        .get(&url)
        .header("accept", "application/json")
        .header("user-agent", USER_AGENT);
    if protocols::listing_is_anthropic(protocol) {
        builder = builder.header("anthropic-version", protocols::ANTHROPIC_VERSION);
        if let Some(api_key) = api_key.filter(|key| !key.trim().is_empty()) {
            builder = builder.header("x-api-key", api_key.trim());
        }
    } else if let Some(api_key) = api_key.filter(|key| !key.trim().is_empty()) {
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
