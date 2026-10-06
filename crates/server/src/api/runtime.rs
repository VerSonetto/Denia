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
    let live = tokio::task::spawn_blocking(move || live.get_or_load(&sessions, &id))
        .await
        .map_err(|e| error(e.to_string()))?
        .map_err(ApiError::from_session)?;
    Ok(live)
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
    state
        .live
        .ensure_hot(&live)
        .map_err(ApiError::from_session)?;
    use denia_tools::capabilities::AgentRuntime;
    let ctx = denia_tools::ToolContext {
        output_store: None,
        session_id: Some(id.clone()),
        selection: None,
        cwd: live.session.header().cwd.clone().into(),
        cancel: tokio_util::sync::CancellationToken::new(),
        confined: true,
        vision_supported: false,
        emit_event: None,
        file_history: None,
        permission_mode: live.session.permission_mode(),
        ask: None,
        call_id: None,
        goal_reader: None,
        // 与 agent-loop 共享同一张读状态表:这个入口发起的工具调用
        // 也参与重复读取去重,不会在会话历史里留下"假新鲜"的标记。
        read_state: Some(state.driver.read_state_for(&id)),
        // 控制台代理控制入口按该会话本身的有效授权计算。
        granted_tools: live
            .session
            .header()
            .subagent
            .as_ref()
            .map(|child| std::sync::Arc::new(child.effective_tools())),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn polling_runtime_keeps_session_history_cold() {
        let home =
            std::env::temp_dir().join(format!("denia-runtime-memory-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let state = Arc::new(crate::state::build_state(&home, false, 0).await.unwrap());
        let session = state.sessions.create(&home, true).unwrap();
        session
            .append(denia_core::session::SessionEvent::UserMessage {
                text: "x".repeat(100_000),
                injected: false,
                channel: None,
                images: Vec::new(),
            })
            .unwrap();
        session.flush().unwrap();
        let id = session.id().to_string();
        drop(session);
        let _ = agents(State(state.clone()), Path(id.clone()))
            .await
            .unwrap();
        let _ = jobs(State(state.clone()), Path(id.clone())).await.unwrap();
        let live = state.live.get(&id).unwrap();
        assert!(!live.session.is_hot());
        assert_eq!(live.session.resident_bytes(), 0);
        assert_eq!(live.session.first_prompt_excerpt(1).as_deref(), Some("x…"));
        drop(live);
        drop(state);
        std::fs::remove_dir_all(home).unwrap();
    }
}
