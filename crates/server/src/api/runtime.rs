//! 人类控制入口与模型工具共用同一运行时、身份边界和资源限制。
use crate::{error::ApiError, state::AppState};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/sessions/{id}/agents", get(agents))
        .route("/api/sessions/{id}/agents/{target}/message", post(message))
        .route(
            "/api/sessions/{id}/agents/{target}/interrupt",
            post(interrupt),
        )
        .route("/api/sessions/{id}/jobs", get(jobs))
        .route("/api/sessions/{id}/jobs/{job}/output", get(output))
        .route("/api/sessions/{id}/jobs/{job}/kill", post(kill))
        .route("/api/sessions/{id}/skills", get(skills))
        .route("/api/sessions/{id}/skills/{name}", get(skill))
}
fn error(e: String) -> ApiError {
    ApiError::bad_request("runtime/operation-failed", e)
}
async fn ensure(state: &AppState, id: &str) -> Result<Arc<crate::state::LiveSession>, ApiError> {
    let live = state.live.clone();
    let sessions = state.sessions.clone();
    let id = id.to_string();
    tokio::task::spawn_blocking(move || live.get_or_load(&sessions, &id))
        .await
        .map_err(|e| error(e.to_string()))?
        .map_err(ApiError::from_session)
}
async fn agents(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    ensure(&state, &id).await?;
    Ok(Json(json!({"agents":state.runtime.list(&id)})))
}
#[derive(Deserialize)]
struct Message {
    message: String,
}
async fn message(
    State(state): State<Arc<AppState>>,
    Path((id, target)): Path<(String, String)>,
    Json(body): Json<Message>,
) -> Result<Json<Value>, ApiError> {
    let live = ensure(&state, &id).await?;
    use denia_tools::capabilities::AgentRuntime;
    let ctx = denia_tools::ToolContext {
        session_id: Some(id),
        selection: None,
        cwd: live.session.header().cwd.clone().into(),
        cancel: tokio_util::sync::CancellationToken::new(),
        confined: true,
        vision_supported: false,
        emit_event: None,
        file_history: None,
        permission_mode: live.session.permission_mode(),
        permission_override: None,
    };
    Ok(Json(
        state
            .runtime
            .execute(
                "send_message",
                json!({"target":target,"message":body.message}),
                &ctx,
            )
            .await
            .map_err(error)?,
    ))
}
async fn interrupt(
    State(state): State<Arc<AppState>>,
    Path((id, target)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    ensure(&state, &id).await?;
    state.runtime.interrupt(&id, &target).await.map_err(error)?;
    Ok(Json(json!({"ok":true})))
}
async fn jobs(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    ensure(&state, &id).await?;
    Ok(Json(json!({"jobs":state.runtime.jobs().list(&id)})))
}
async fn output(
    State(state): State<Arc<AppState>>,
    Path((id, job)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    ensure(&state, &id).await?;
    Ok(Json(state.runtime.jobs().peek(&job, &id).map_err(error)?))
}
async fn kill(
    State(state): State<Arc<AppState>>,
    Path((id, job)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    ensure(&state, &id).await?;
    state.runtime.jobs().kill(&job, &id).map_err(error)?;
    Ok(Json(json!({"ok":true})))
}
async fn skills(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let live = ensure(&state, &id).await?;
    Ok(Json(
        json!({"skills":state.runtime.skills(live.session.header().cwd.clone().into()).await.map_err(error)?}),
    ))
}
async fn skill(
    State(state): State<Arc<AppState>>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let live = ensure(&state, &id).await?;
    Ok(Json(
        state
            .runtime
            .load_skill(live.session.header().cwd.clone().into(), name, true)
            .await
            .map_err(error)?,
    ))
}
