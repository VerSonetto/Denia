//! 自定义系统提示词读写:`<home>/SYSTEM.md`。

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::ApiError;
use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/system-prompt", get(get_system_prompt).put(put_system_prompt).delete(delete_system_prompt))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SystemPromptView {
    text: String,
    source: &'static str,
    path: String,
}

#[derive(Debug, Deserialize)]
struct PutBody {
    text: String,
}

async fn get_system_prompt(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let (text, source) = state.system_prompt.text_and_source();
    Json(SystemPromptView {
        text,
        source,
        path: state.system_prompt.file_path().display().to_string(),
    })
}

async fn put_system_prompt(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PutBody>,
) -> Result<impl IntoResponse, ApiError> {
    state
        .system_prompt
        .write(&body.text)
        .map_err(|error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "system-prompt/io", error.to_string()))?;
    let (text, source) = state.system_prompt.text_and_source();
    Ok(Json(SystemPromptView {
        text,
        source,
        path: state.system_prompt.file_path().display().to_string(),
    }))
}

async fn delete_system_prompt(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, ApiError> {
    state
        .system_prompt
        .reset()
        .map_err(|error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "system-prompt/io", error.to_string()))?;
    Ok(Json(json!({
        "text": "",
        "source": "default",
        "path": state.system_prompt.file_path().display().to_string(),
    })))
}
