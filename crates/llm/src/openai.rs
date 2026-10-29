//! Multi-protocol gateway adapter: settings-declared routes speaking one of
//! three wire protocols — OpenAI chat-completions, OpenAI Responses, or
//! Anthropic Messages (any gateway: OpenAI, Azure-style, vLLM, etc.).
//!
//! Routes live in the `llm-openai` settings section's `providers` map; the
//! server syncs registry routes from it on every settings change.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use denia_core::error::{LlmError, codes};
use denia_credentials::CredentialStore;
use denia_settings::SettingsStore;
use serde::{Deserialize, Serialize};

use crate::catalog::{
    DiscoveredModel, LlmModelInfo, LlmResolvedModelInfo, ProviderInfo, ReasoningEffortInfo,
    ReasoningInfo, highest_reasoning_effort,
};
use crate::http::{http_error_failure, send_sse};
use crate::protocols::{
    self, AnthropicStream, CompletionsStream, EventTranslator, ResponsesStream, WireProtocol,
};
use crate::request::GenerateRequest;
use crate::sse::sse_chunk_stream;
use crate::wire::UsageStyle;
use crate::{ChunkStream, LlmAdapter};

pub const OPENAI_SETTINGS_NS: &str = "llm-openai";

const DEFAULT_CONTEXT_WINDOW: u64 = 262_144;
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
    /// 附加请求头;值支持 `${REF}` 引用凭据/环境变量(整段占位),每次请求前解析。
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// 定向兼容:有的 chat-completions 网关把推理内联进 `content`(以 `</think>`
    /// 收尾,开头的 `<think>` 被上游模板吃掉)而不是标准 `reasoning_content`。
    /// 开启后翻译层会把首个 think 收尾标记之前的部分拆成推理块。默认关闭——
    /// 只有确实内联推理的网关才需要,否则会给正常流式首字带来缓冲延迟。
    #[serde(default)]
    pub inline_think_tags: bool,
    /// Enable OpenAI Responses `previous_response_id` chaining. Defaults to
    /// true only for the official `api.openai.com` endpoint; gateways that
    /// reject stored responses must leave it off.
    #[serde(default)]
    pub stateful_responses: Option<bool>,
    /// Request the provider's long cache tier: Anthropic `ttl: "1h"` (with
    /// the extended-cache-ttl beta) and OpenAI `prompt_cache_retention: "24h"`.
    #[serde(default)]
    pub long_cache: bool,
    /// Whether the route accepts OpenAI's `prompt_cache_key` body field.
    /// Strict gateways that reject unknown fields can turn it off.
    #[serde(default = "default_true")]
    pub supports_prompt_cache_key: bool,
}

fn default_true() -> bool {
    true
}

impl OpenAiProfile {
    fn stateful_responses_enabled(&self) -> bool {
        self.stateful_responses
            .unwrap_or_else(|| self.base_url.contains("api.openai.com"))
    }
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
    let default_effort = highest_reasoning_effort(
        efforts.iter().map(|effort| effort.id.as_str()),
        &known_order,
    )
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

/// 输出预算只影响线上的标量字段,不为调整它复制整份会话历史。
fn request_body(
    request: &GenerateRequest,
    protocol: WireProtocol,
    max_tokens: Option<u64>,
    long_cache: bool,
) -> serde_json::Value {
    let mut body = match protocol {
        WireProtocol::ChatCompletions => protocols::build_openai_body(request),
        WireProtocol::Responses => protocols::build_responses_body(request),
        WireProtocol::AnthropicMessages => {
            protocols::build_anthropic_body(request, max_tokens, long_cache)
        }
    };
    let key = match protocol {
        WireProtocol::Responses => "max_output_tokens",
        _ => "max_tokens",
    };
    if let Some(max_tokens) = max_tokens {
        body[key] = max_tokens.into();
    } else if let Some(fields) = body.as_object_mut() {
        fields.remove(key);
    }
    if long_cache && protocol == WireProtocol::Responses {
        body["prompt_cache_retention"] = serde_json::json!("24h");
    }
    body
}

#[cfg(test)]
mod memory_tests {
    use super::*;

    #[test]
    fn borrowed_request_budget_matches_the_clone_based_wire_body() {
        let request = GenerateRequest {
            model: "fixture".into(),
            reasoning_effort: Some("high".into()),
            messages: vec![denia_core::message::ChatMessage::user("unchanged history")],
            system: Some("system".into()),
            tools: vec![],
            temperature: Some(0.5),
            max_tokens: Some(8192),
            stop: vec![],
            cache_key: None,
        };
        for protocol in [
            WireProtocol::ChatCompletions,
            WireProtocol::Responses,
            WireProtocol::AnthropicMessages,
        ] {
            for budget in [None, Some(1), Some(4096), Some(8192)] {
                let mut cloned = request.clone();
                cloned.max_tokens = budget;
                let expected = match protocol {
                    WireProtocol::ChatCompletions => protocols::build_openai_body(&cloned),
                    WireProtocol::Responses => protocols::build_responses_body(&cloned),
                    WireProtocol::AnthropicMessages => {
                        protocols::build_anthropic_body(&cloned, budget, false)
                    }
                };
                assert_eq!(request_body(&request, protocol, budget, false), expected);
                assert_eq!(request.max_tokens, Some(8192));
                assert_eq!(request.messages[0].content, "unchanged history");
            }
        }
    }

    #[test]
    fn responses_delta_input_reuses_the_canonical_prefix() {
        let previous = serde_json::json!({
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
        });
        let output = vec![serde_json::json!({
            "type": "message",
            "id": "m1",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "hello"}],
        })];
        let current = vec![
            serde_json::json!({
                "role": "user",
                "content": [{"type": "input_text", "text": "hi"}],
            }),
            serde_json::json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "hello"}],
            }),
            serde_json::json!({
                "type": "function_call_output",
                "call_id": "c1",
                "output": "ok",
            }),
        ];
        let delta = responses_delta_input(&previous, &current, &output).expect("prefix matches");
        assert_eq!(delta, vec![current[2].clone()]);
    }

    #[test]
    fn responses_chain_rejects_a_changed_tool_surface() {
        let previous = serde_json::json!({"instructions": "s", "tools": [{"name": "a"}]});
        let current = serde_json::json!({"instructions": "s", "tools": [{"name": "b"}]});
        assert!(!responses_prefix_compatible(&previous, &current));
        assert!(responses_prefix_compatible(&previous, &previous));
    }
}

/// Each entry holds a full request body, so the cap is deliberately small:
/// 64 live conversations bound the retained JSON without churning an active
/// session's chain.
const RESPONSES_CHAIN_CAP: usize = 64;

/// One conversation's last successful Responses exchange. `body` is the full
/// request sent (never the delta), so the next turn can prove its own history
/// extends it before appending.
#[derive(Clone)]
struct ResponsesChain {
    body: serde_json::Value,
    output: Vec<serde_json::Value>,
    response_id: String,
}

#[derive(Default)]
struct ResponsesChainState {
    entries: HashMap<String, ResponsesChain>,
    order: VecDeque<String>,
}

impl ResponsesChainState {
    fn insert(&mut self, key: String, chain: ResponsesChain) {
        self.entries.insert(key.clone(), chain);
        self.order.retain(|existing| existing != &key);
        self.order.push_back(key);
        while self.order.len() > RESPONSES_CHAIN_CAP {
            if let Some(evicted) = self.order.pop_front() {
                self.entries.remove(&evicted);
            }
        }
    }

    fn remove(&mut self, key: &str) {
        self.entries.remove(key);
        self.order.retain(|existing| existing != key);
    }
}

fn responses_chain_key(base_url: &str, model: &str, cache_key: &str) -> String {
    format!("{base_url}\u{0}{model}\u{0}{cache_key}")
}

/// Server output items carry lifecycle fields (`id`, `status`) that the next
/// request reconstructs from the local transcript. Strip them before the
/// prefix comparison; everything else must match byte for byte.
fn canonical_responses_item(item: &serde_json::Value) -> serde_json::Value {
    let mut item = item.clone();
    if let Some(object) = item.as_object_mut() {
        object.remove("id");
        object.remove("status");
    }
    item
}

fn responses_delta_input(
    previous: &serde_json::Value,
    current: &[serde_json::Value],
    output: &[serde_json::Value],
) -> Option<Vec<serde_json::Value>> {
    let previous_input = previous.get("input")?.as_array()?;
    let baseline_len = previous_input.len() + output.len();
    if current.len() <= baseline_len {
        return None;
    }
    for (index, item) in previous_input.iter().enumerate() {
        if canonical_responses_item(item) != canonical_responses_item(&current[index]) {
            return None;
        }
    }
    for (offset, item) in output.iter().enumerate() {
        let index = previous_input.len() + offset;
        if canonical_responses_item(item) != canonical_responses_item(&current[index]) {
            return None;
        }
    }
    Some(current[baseline_len..].to_vec())
}

/// The server stores instructions, tools and sampling controls with the
/// response. A difference in any of them makes the old response unusable as a
/// prefix, so the caller must replay in full.
fn responses_prefix_compatible(previous: &serde_json::Value, current: &serde_json::Value) -> bool {
    const IGNORED: [&str; 6] = [
        "input",
        "store",
        "previous_response_id",
        "prompt_cache_key",
        "stream",
        "stream_options",
    ];
    let (Some(previous), Some(current)) = (previous.as_object(), current.as_object()) else {
        return false;
    };
    for (key, value) in previous {
        if IGNORED.contains(&key.as_str()) {
            continue;
        }
        if current.get(key) != Some(value) {
            return false;
        }
    }
    for key in current.keys() {
        if IGNORED.contains(&key.as_str()) {
            continue;
        }
        if !previous.contains_key(key) {
            return false;
        }
    }
    true
}

fn is_stale_responses_error(error: &LlmError) -> bool {
    let text = format!("{} {}", error.code, error.message).to_ascii_lowercase();
    (text.contains("previous_response") || text.contains("previous response"))
        && (text.contains("not found")
            || text.contains("invalid")
            || text.contains("expired")
            || text.contains("stale")
            || text.contains("unsupported"))
}

pub struct OpenAiCompatAdapter {
    settings: Arc<SettingsStore>,
    credentials: Arc<CredentialStore>,
    http: reqwest::Client,
    responses_chains: Arc<Mutex<ResponsesChainState>>,
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
            responses_chains: Arc::new(Mutex::new(ResponsesChainState::default())),
        }
    }

    async fn send_request(
        &self,
        profile: &OpenAiProfile,
        api_key: &Option<String>,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response, LlmError> {
        let mut builder = self
            .http
            .post(url)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .header("user-agent", USER_AGENT);
        match profile.protocol {
            WireProtocol::AnthropicMessages => {
                if let Some(api_key) = api_key {
                    builder = builder.header("x-api-key", api_key);
                }
                builder = builder.header("anthropic-version", protocols::ANTHROPIC_VERSION);
            }
            _ => {
                if let Some(api_key) = api_key {
                    builder = builder.header("authorization", format!("Bearer {api_key}"));
                }
            }
        }
        // 自定义头最后应用:可覆盖同名内置头(含 authorization,支持自定义认证网关)。
        let mut custom_headers = resolve_profile_headers(&profile.headers, &self.credentials)?;
        if profile.long_cache && profile.protocol == WireProtocol::AnthropicMessages {
            // `ttl: "1h"` needs the extended-cache-ttl beta on API-key
            // requests. Merge with a caller-supplied beta list instead of
            // overwriting it.
            const BETA: &str = "extended-cache-ttl-2025-04-11";
            let existing = custom_headers
                .get("anthropic-beta")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("");
            let merged = if existing.is_empty() {
                BETA.to_string()
            } else if existing.contains(BETA) {
                existing.to_string()
            } else {
                format!("{existing},{BETA}")
            };
            if let Ok(value) = reqwest::header::HeaderValue::from_str(&merged) {
                custom_headers.insert("anthropic-beta", value);
            }
        }
        builder = builder.headers(custom_headers);
        // 超时纪律:连接阶段由 client 的 connect_timeout 负责;这里只给
        // 「发出请求 → 响应头返回」设 TTFB 超时——大体量 + 深度思考请求
        // 超时按 TIMEOUT 计、由 RetryPolicy 自动重发(实测 200K 字符
        // payload 的正常 TTFB 曾到 21.8s,30s 的余量并不宽裕)。
        // 流式体不设任何 denia 侧超时:上游不报错就一直流
        // (reqwest 的 .timeout() 是覆盖响应体读完的总超时,会把长思考
        // 模型在流中途掐死,绝不能挂)。
        let response = send_sse(builder.json(body), crate::http::TTFB_TIMEOUT).await?;
        if !response.status().is_success() {
            let status = response.status();
            let headers = response.headers().clone();
            let body_text = response.text().await.unwrap_or_default();
            return Err(LlmError::from_failure(http_error_failure(
                status, &body_text, &headers,
            )));
        }
        Ok(response)
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

/// Resolves one custom-header value: a whole-value `${REF}` placeholder is
/// resolved through the credential chain (env > file > .env); anything else
/// (including partial placeholder text) is sent verbatim.
fn resolve_header_value(credentials: &CredentialStore, raw: &str) -> Result<String, LlmError> {
    let trimmed = raw.trim();
    let Some(reference) = trimmed
        .strip_prefix("\u{24}{")
        .and_then(|rest| rest.strip_suffix('}'))
    else {
        return Ok(raw.to_string());
    };
    // 混合文本(多个占位符相连)不解析,原样发送,避免歧义。
    if reference.is_empty() || reference.contains('\u{24}') {
        return Ok(raw.to_string());
    }
    match credentials.resolve(reference) {
        Ok(Some(resolved)) => {
            let value = resolved.value.trim().to_string();
            if value.is_empty() {
                return Err(LlmError::new(
                    codes::INVALID_CREDENTIAL,
                    format!("the value behind {reference} is empty"),
                ));
            }
            Ok(value)
        }
        Ok(None) => Err(LlmError::new(
            codes::MISSING_CREDENTIAL,
            format!("store {reference} through the credentials API before calling the model"),
        )),
        Err(error) => Err(LlmError::new(codes::MISSING_CREDENTIAL, error.to_string())),
    }
}

/// Resolves and validates a profile's custom headers into a ready-to-attach
/// `HeaderMap`. Invalid names or values fail loudly before the request goes
/// out (fail loud, never silently skip a misconfigured header).
pub fn resolve_profile_headers(
    headers: &BTreeMap<String, String>,
    credentials: &CredentialStore,
) -> Result<reqwest::header::HeaderMap, LlmError> {
    let mut map = reqwest::header::HeaderMap::new();
    for (name, raw) in headers {
        if name.trim().is_empty() {
            return Err(LlmError::new(
                codes::INVALID_REQUEST,
                "a custom header name must not be empty",
            ));
        }
        let header_name =
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                LlmError::new(
                    codes::INVALID_REQUEST,
                    format!("'{name}' is not a valid HTTP header name"),
                )
            })?;
        let value = resolve_header_value(credentials, raw)?;
        let header_value = reqwest::header::HeaderValue::from_str(&value).map_err(|_| {
            LlmError::new(
                codes::INVALID_REQUEST,
                format!("header '{name}' has an invalid value"),
            )
        })?;
        map.insert(header_name, header_value);
    }
    Ok(map)
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
        let extra_headers = resolve_profile_headers(&profile.headers, &self.credentials)?;
        let discovered = discover_models(
            &self.http,
            &profile.base_url,
            profile.protocol,
            api_key.as_deref(),
            &extra_headers,
        )
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
        let max_tokens_default = profile.default_max_tokens;
        let found = profile.models.iter().find(|m| m.id == model);
        let (name, description, modalities, context_window, max_tokens, reasoning) = match found {
            Some(entry) => (
                entry.name.clone().unwrap_or_else(|| entry.id.clone()),
                entry.description.clone(),
                openai_modalities(entry),
                entry.context_window.unwrap_or(context_default),
                entry.max_tokens.or(max_tokens_default),
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
            default_max_tokens: max_tokens,
            reasoning,
        })
    }

    fn route_identity(&self, provider: &str) -> String {
        self.profile(provider)
            .map(|profile| {
                format!(
                    "{provider}:{}:{}",
                    profile.protocol.as_id(),
                    profile.base_url
                )
            })
            .unwrap_or_else(|_| provider.into())
    }

    async fn output_budget(
        &self,
        provider: &str,
        model: &str,
        explicit: Option<u64>,
    ) -> Result<Option<u64>, LlmError> {
        let profile = self.profile(provider)?;
        let limit = profile
            .models
            .iter()
            .find(|entry| entry.id == model)
            .and_then(|entry| entry.max_tokens);
        Ok(explicit
            .or(profile.default_max_tokens)
            .map(|budget| budget.min(limit.unwrap_or(u64::MAX)).max(1)))
    }


    async fn stream(
        &self,
        provider: &str,
        request: &GenerateRequest,
    ) -> Result<ChunkStream, LlmError> {
        let profile = self.profile(provider)?;
        let api_key = self.resolve_api_key(&profile)?;

        let protocol = profile.protocol;
        let limit = profile
            .models
            .iter()
            .find(|entry| entry.id == request.model)
            .and_then(|entry| entry.max_tokens);
        let max_tokens = request
            .max_tokens
            .or(profile.default_max_tokens)
            .map(|budget| budget.min(limit.unwrap_or(u64::MAX)).max(1));
        let mut body = request_body(request, protocol, max_tokens, profile.long_cache);
        let mut full_body = body.clone();
        if !profile.supports_prompt_cache_key {
            for value in [&mut body, &mut full_body] {
                if let Some(object) = value.as_object_mut() {
                    object.remove("prompt_cache_key");
                }
            }
        }
        let mut chain_key: Option<String> = None;
        let mut used_chain = false;
        if protocol == WireProtocol::Responses && profile.stateful_responses_enabled() {
            if let Some(cache_key) = request.cache_key.as_deref() {
                let key = responses_chain_key(&profile.base_url, &request.model, cache_key);
                chain_key = Some(key.clone());
                let chain = {
                    let state = self
                        .responses_chains
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner());
                    state.entries.get(&key).cloned()
                };
                match chain {
                    Some(chain) if responses_prefix_compatible(&chain.body, &body) => {
                        let current = body
                            .get("input")
                            .and_then(|value| value.as_array())
                            .cloned()
                            .unwrap_or_default();
                        if let Some(delta) =
                            responses_delta_input(&chain.body, &current, &chain.output)
                        {
                            body["input"] = serde_json::Value::Array(delta);
                            body["previous_response_id"] =
                                serde_json::json!(chain.response_id);
                            used_chain = true;
                        }
                        body["store"] = serde_json::json!(true);
                    }
                    Some(_) => {
                        self.responses_chains
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .remove(&key);
                        body["store"] = serde_json::json!(true);
                    }
                    None => body["store"] = serde_json::json!(true),
                }
                full_body["store"] = serde_json::json!(true);
            }
        }

        let url = match protocol {
            WireProtocol::ChatCompletions => protocols::chat_completions_url(&profile.base_url),
            WireProtocol::Responses => protocols::responses_url(&profile.base_url),
            WireProtocol::AnthropicMessages => protocols::anthropic_messages_url(&profile.base_url),
        };
        let response = match self.send_request(&profile, &api_key, &url, &body).await {
            Ok(response) => response,
            Err(error) if used_chain && is_stale_responses_error(&error) => {
                if let Some(key) = chain_key.as_deref() {
                    self.responses_chains
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .remove(key);
                }
                self.send_request(&profile, &api_key, &url, &full_body)
                    .await?
            }
            Err(error) => return Err(error),
        };

        let on_terminal: Option<Box<dyn FnMut(&serde_json::Value) + Send>> =
            chain_key.map(|key| {
                let chains = Arc::clone(&self.responses_chains);
                let body = full_body.clone();
                Box::new(move |response: &serde_json::Value| {
                    let Some(response_id) = response["id"].as_str().filter(|id| !id.is_empty())
                    else {
                        return;
                    };
                    let output = response["output"].as_array().cloned().unwrap_or_default();
                    let mut state = chains
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner());
                    state.insert(
                        key.clone(),
                        ResponsesChain {
                            body: body.clone(),
                            output,
                            response_id: response_id.to_string(),
                        },
                    );
                }) as Box<dyn FnMut(&serde_json::Value) + Send>
            });
        let translator: Box<dyn EventTranslator> = match protocol {
            WireProtocol::ChatCompletions => Box::new(CompletionsStream::with_inline_think_tags(
                UsageStyle::OpenAi,
                profile.inline_think_tags,
            )),
            WireProtocol::Responses => Box::new(ResponsesStream::with_terminal(on_terminal)),
            WireProtocol::AnthropicMessages => Box::new(AnthropicStream::default()),
        };
        Ok(sse_chunk_stream(response, translator))
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
    extra_headers: &reqwest::header::HeaderMap,
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
    if !extra_headers.is_empty() {
        builder = builder.headers(extra_headers.clone());
    }
    let response = builder.send().await.map_err(crate::http::transport_error)?;
    if !response.status().is_success() {
        let status = response.status();
        let headers = response.headers().clone();
        let body_text = response.text().await.unwrap_or_default();
        return Err(LlmError::from_failure(http_error_failure(
            status, &body_text, &headers,
        )));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(crate::http::transport_error)?;
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
            name: entry
                .owned_by
                .map(|owner| format!("{} ({owner})", entry.id)),
        })
        .collect())
}

#[cfg(test)]
mod header_tests {
    use super::{resolve_header_value, resolve_profile_headers};
    use denia_credentials::CredentialStore;
    use std::collections::BTreeMap;
    use std::io::Write;

    const REF: &str = "MY_HEADER_TOKEN_XYZ";

    fn fresh_store() -> CredentialStore {
        let dir = std::env::temp_dir().join(format!(
            "denia-llm-headers-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let env_path = dir.join(".env");
        let mut file = std::fs::File::create(&env_path).unwrap();
        writeln!(file, "{REF}=secret-123").unwrap();
        CredentialStore::open(&dir).unwrap()
    }

    #[test]
    fn placeholder_resolves_through_credential_chain() {
        let store = fresh_store();
        let mut headers = BTreeMap::new();
        headers.insert(
            "X-Token".to_string(),
            "\u{24}{MY_HEADER_TOKEN_XYZ}".to_string(),
        );
        let map = resolve_profile_headers(&headers, &store).unwrap();
        assert_eq!(map.get("x-token").unwrap().to_str().unwrap(), "secret-123");
    }

    #[test]
    fn missing_reference_fails_loud() {
        let store = fresh_store();
        let mut headers = BTreeMap::new();
        headers.insert("X-Token".to_string(), "\u{24}{NO_SUCH_REF_ABC}".to_string());
        let error = resolve_profile_headers(&headers, &store).unwrap_err();
        assert_eq!(error.code, denia_core::error::codes::MISSING_CREDENTIAL);
    }

    #[test]
    fn plain_value_is_sent_verbatim() {
        let store = fresh_store();
        let mut headers = BTreeMap::new();
        headers.insert("X-Foo".to_string(), "bar baz".to_string());
        let map = resolve_profile_headers(&headers, &store).unwrap();
        assert_eq!(map.get("x-foo").unwrap().to_str().unwrap(), "bar baz");
    }

    #[test]
    fn mixed_placeholder_text_stays_verbatim() {
        let store = fresh_store();
        // 多个占位符相连或占位符内嵌文本:不解析,原样发送(避免歧义)。
        assert_eq!(
            resolve_header_value(&store, "\u{24}{A}\u{24}{B}").unwrap(),
            "\u{24}{A}\u{24}{B}"
        );
    }

    #[test]
    fn invalid_header_name_fails_loud() {
        let store = fresh_store();
        let mut headers = BTreeMap::new();
        headers.insert("Bad Name".to_string(), "x".to_string());
        let error = resolve_profile_headers(&headers, &store).unwrap_err();
        assert_eq!(error.code, denia_core::error::codes::INVALID_REQUEST);
    }

    #[test]
    fn empty_headers_map_is_compatible() {
        let store = fresh_store();
        let map = resolve_profile_headers(&BTreeMap::new(), &store).unwrap();
        assert!(map.is_empty());
    }
}
