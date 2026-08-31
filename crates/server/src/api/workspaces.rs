//! 工作区端点:列表/幂等创建/删除(抄 dsh workspace registry 语义)。

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::error::ApiError;
use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/workspaces", get(list_workspaces).post(create_workspace))
        .route("/api/workspaces/{id}", delete(delete_workspace))
}

async fn list_workspaces(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({ "workspaces": state.workspaces.list() }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateBody {
    path: String,
    #[serde(default)]
    title: Option<String>,
}

async fn create_workspace(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateBody>,
) -> Result<impl IntoResponse, ApiError> {
    let record = state
        .workspaces
        .create(std::path::Path::new(body.path.trim()), body.title)
        .map_err(|message| ApiError::bad_request("workspace/bad-path", message))?;
    let _ = state.events.send(crate::state::ServerEvent::SessionsUpdated);
    Ok((
        axum::http::StatusCode::CREATED,
        Json(json!({ "workspace": record })),
    ))
}

async fn delete_workspace(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    if !state.workspaces.delete(&id) {
        return Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "workspace/not-found",
            "workspace not found",
        ));
    }
    let _ = state.events.send(crate::state::ServerEvent::SessionsUpdated);
    Ok(Json(json!({ "ok": true })))
}
