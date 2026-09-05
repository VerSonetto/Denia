//! Model management endpoints: provider listing, catalog, endpoint
//! discovery, and the streaming chat smoke test.
//!
//! 模型选择不再有服务端默认:每个请求(会话 prompt / chat 冒烟)都必须
//! 显式携带 provider/model,由前端记忆"上次使用的模型"负责初始值。

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use denia_core::message::ChatMessage;
use denia_llm::{GenerateRequest, WireProtocol, build_model_catalog, discover_models};
use futures::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::json;

use crate::error::ApiError;
use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/llm/providers", get(list_providers))
        .route("/api/llm/catalog", get(model_catalog))
        .route("/api/llm/discover", post(discover))
        .route("/api/llm/chat", post(chat))
}

async fn list_providers(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({
        "providers": state.registry.list_providers(),
        "configurable": state.registry.list_configurable(),
    }))
}

async fn model_catalog(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let catalog = build_model_catalog(&state.registry).await;
    Json(catalog)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiscoverBody {
    #[serde(rename = "baseURL")]
    base_url: String,
    /// Wire protocol the endpoint speaks; defaults to chat-completions.
    #[serde(default)]
    protocol: Option<String>,
    /// One-shot key for the probe; never stored.
    #[serde(default)]
    api_key: Option<String>,
    /// Credential reference to resolve the key from.
    #[serde(default)]
    api_key_env: Option<String>,
}

async fn discover(
    State(state): State<Arc<AppState>>,
    Json(body): Json<DiscoverBody>,
) -> Result<impl IntoResponse, ApiError> {
    let protocol = match body.protocol.as_deref() {
        None | Some("") => WireProtocol::default(),
        Some(raw) => WireProtocol::parse(raw).ok_or_else(|| {
            ApiError::bad_request(
                "model/unknown-protocol",
                format!("unknown wire protocol '{raw}'"),
            )
        })?,
    };
    let api_key = match (&body.api_key, &body.api_key_env) {
        (Some(key), _) => Some(key.trim().to_string()),
        (None, Some(reference)) => state
            .credentials
            .resolve(reference)
            .map_err(ApiError::from_credential)?
            .map(|resolved| resolved.value),
        (None, None) => None,
    };
    let models = discover_models(&state.http, &body.base_url, protocol, api_key.as_deref())
        .await
        .map_err(ApiError::from_llm)?;
    Ok(Json(json!({ "models": models })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChatBody {
    provider: String,
    model: String,
    #[serde(default)]
    reasoning_effort: Option<String>,
    /// Single user prompt convenience; `messages` wins when both are set.
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    messages: Option<Vec<ChatMessage>>,
    #[serde(default)]
    system: Option<String>,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    max_tokens: Option<u64>,
}

type SseItem = Result<Event, Infallible>;
type SseStream = std::pin::Pin<Box<dyn Stream<Item = SseItem> + Send>>;

/// The chat response body: chunk events as `data:` frames, mid-stream
/// failures as one `error` frame before the stream closes.
async fn chat(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ChatBody>,
) -> Result<impl IntoResponse, ApiError> {
    let provider = body.provider.trim().to_string();
    let model = body.model.trim().to_string();
    if provider.is_empty() || model.is_empty() {
        return Err(ApiError::bad_request(
            "chat/incomplete",
            "provider and model are required",
        ));
    }
    let reasoning_effort = body.reasoning_effort;

    let messages = if let Some(messages) = body.messages.filter(|messages| !messages.is_empty()) {
        messages
    } else if let Some(prompt) = body.prompt.filter(|prompt| !prompt.trim().is_empty()) {
        vec![ChatMessage::user(prompt)]
    } else {
        return Err(ApiError::bad_request(
            "chat/empty",
            "provide a prompt or a messages array",
        ));
    };

    state
        .registry
        .resolve_call(&provider, &model, reasoning_effort.as_deref())
        .await
        .map_err(ApiError::from_llm)?;

    let request = GenerateRequest {
        model: model.clone(),
        reasoning_effort,
        messages,
        system: body.system,
        temperature: body.temperature,
        max_tokens: body.max_tokens,
        stop: Vec::new(),
        tools: Vec::new(),
    };

    let stream: SseStream = match state.registry.stream(&provider, &request, None).await {
        Ok(chunks) => Box::pin(futures::stream::unfold(chunks, |mut chunks| async move {
            match chunks.next().await {
                Some(Ok(chunk)) => Some((
                    Ok(Event::default().data(serde_json::to_string(&chunk).unwrap_or_default())),
                    chunks,
                )),
                Some(Err(failure)) => Some((
                    Ok(Event::default()
                        .event("error")
                        .data(serde_json::to_string(&failure).unwrap_or_default())),
                    chunks,
                )),
                None => None,
            }
        })),
        Err(error) => Box::pin(futures::stream::iter(vec![Ok(Event::default()
            .event("error")
            .data(serde_json::to_string(&error.failure).unwrap_or_default()))])),
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
