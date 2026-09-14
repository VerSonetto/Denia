//! Agent preset 端点:名册、只读查看、复制创作、删除。
//!
//! 创作即复制(抄 dsh):调用方从不提交组装文本,只给出"复制哪个、叫什么
//! 名字、新 id 是什么",因此一次复制不会授予名册尚未携带的任何能力。改
//! 组装靠用户自己的编辑器改 `preset.yml`——文件就是编辑器。

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::error::ApiError;
use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/agent-presets",
            get(list_presets).post(copy_preset),
        )
        .route(
            "/api/agent-presets/{id}",
            get(get_preset).delete(delete_preset),
        )
}

/// 名册快照:随附集合在前,用户集合在后;每一行都带健康状态。
async fn list_presets(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({
        "default": state.agent_presets.default_id(),
        "root": state.agent_presets.root().display().to_string(),
        "presets": state.agent_presets.rows().as_ref(),
    }))
}

/// 单个 preset 的组装文本(设置页只读查看器用)。
async fn get_preset(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let row = state
        .agent_presets
        .rows()
        .iter()
        .find(|row| row.id == id)
        .cloned()
        .ok_or_else(|| unknown_preset(&id))?;
    let text = state.agent_presets.describe_text(&id).map_err(|reason| {
        ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "agent-preset/broken", reason)
    })?;
    Ok(Json(json!({ "preset": row, "text": text })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CopyBody {
    /// 来源 preset id(随附或用户 preset 都可)。
    from: String,
    /// 新 preset id(同时是目录名)。
    id: String,
    /// 可选的显示名;缺省为"<来源名> 副本"。
    #[serde(default)]
    name: Option<String>,
}

/// 复制创作:整个目录复制到用户根目录,组装文件重写元数据。
async fn copy_preset(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CopyBody>,
) -> Result<impl IntoResponse, ApiError> {
    let store = state.agent_presets.clone();
    let name = body.name.clone();
    let row = tokio::task::spawn_blocking(move || store.copy(&body.from, &body.id, name.as_deref()))
        .await
        .map_err(|error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "api/join", error.to_string()))?
        .map_err(|reason| {
            ApiError::bad_request("agent-preset/copy-rejected", reason)
        })?;
    let _ = state.events.send(crate::state::ServerEvent::AgentPresetsUpdated);
    Ok((StatusCode::CREATED, Json(json!({ "preset": row }))))
}

/// 删除本地创作的 preset;随附 preset 拒绝删除。
async fn delete_preset(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let store = state.agent_presets.clone();
    let id_for_task = id.clone();
    tokio::task::spawn_blocking(move || store.remove(&id_for_task))
        .await
        .map_err(|error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "api/join", error.to_string()))?
        .map_err(|reason| {
            let code = if reason.contains("不可删除") {
                StatusCode::CONFLICT
            } else if reason.contains("不存在") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            ApiError::new(code, "agent-preset/remove-rejected", reason)
        })?;
    let _ = state.events.send(crate::state::ServerEvent::AgentPresetsUpdated);
    Ok(StatusCode::NO_CONTENT)
}

fn unknown_preset(id: &str) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "agent-preset/not-found",
        format!("未知的 preset:{id}"),
    )
}
