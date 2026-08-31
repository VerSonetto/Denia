//! Session endpoints: list/create/read/delete, prompt, cancel, follow.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use dshrs_core::config::ModelSelection;
use dshrs_core::session::SessionEnvelope;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;

use crate::error::ApiError;
use crate::state::{AppState, ServerEvent, current_default_selection};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route(
            "/api/sessions/{id}",
            get(get_session).delete(delete_session),
        )
        .route("/api/sessions/{id}/prompt", post(prompt_session))
        .route("/api/sessions/{id}/cancel", post(cancel_session))
        .route("/api/sessions/{id}/follow", get(follow_session))
}

async fn list_sessions(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, ApiError> {
    let sessions = state.sessions.list().map_err(ApiError::from_session)?;
    Ok(Json(json!({ "sessions": sessions })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateBody {
    /// Real working directory; ignored while `sandbox` is true.
    #[serde(default)]
    cwd: Option<String>,
    /// Default true: run inside the home workspace sandbox.
    #[serde(default)]
    sandbox: Option<bool>,
}

async fn create_session(
    State(state): State<Arc<AppState>>,
    body: Option<Json<CreateBody>>,
) -> Result<impl IntoResponse, ApiError> {
    let body = body.map(|Json(body)| body).unwrap_or(CreateBody {
        cwd: None,
        sandbox: None,
    });
    let sandboxed = body.sandbox.unwrap_or(true) || body.cwd.is_none();
    let cwd = if sandboxed {
        state.workspace.clone()
    } else {
        let raw = body.cwd.unwrap_or_default();
        let dir = std::path::PathBuf::from(&raw);
        if !dir.is_dir() {
            return Err(ApiError::bad_request(
                "session/bad-cwd",
                format!("'{raw}' is not a directory"),
            ));
        }
        dir
    };
    let session = state
        .sessions
        .create(&cwd)
        .map_err(ApiError::from_session)?;
    let summary = json!({
        "id": session.id(),
        "created_at": session.header().created_at,
        "excerpt": null,
        "cwd": session.header().cwd,
    });
    let _ = state.events.send(ServerEvent::SessionsUpdated);
    Ok((StatusCode::CREATED, Json(json!({ "session": summary }))))
}

async fn get_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let live = state.live.get_or_load(&state.sessions, &id).map_err(ApiError::from_session)?;
    let session = live.session.lock().await;
    Ok(Json(json!({
        "header": session.header(),
        "events": session.events(),
    })))
}

async fn delete_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    if let Ok(live) = state.live.get_or_load(&state.sessions, &id) {
        if live.running.load(Ordering::SeqCst) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "session/running",
                "cancel the running turn before deleting",
            ));
        }
    }
    state.sessions.delete(&id).map_err(ApiError::from_session)?;
    state.live.remove(&id);
    let _ = state.events.send(ServerEvent::SessionsUpdated);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptBody {
    prompt: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
}

async fn prompt_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<PromptBody>,
) -> Result<impl IntoResponse, ApiError> {
    let prompt = body.prompt.trim().to_string();
    if prompt.is_empty() {
        return Err(ApiError::bad_request("session/empty-prompt", "prompt is empty"));
    }
    let live = state
        .live
        .get_or_load(&state.sessions, &id)
        .map_err(ApiError::from_session)?;
    if live
        .running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "session/running",
            "a turn is already running on this session",
        ));
    }

    let default = current_default_selection(&state.settings);
    let selection = ModelSelection {
        provider: body.provider.unwrap_or(default.provider),
        model: body.model.unwrap_or(default.model),
        reasoning_effort: body.reasoning_effort.or(default.reasoning_effort),
    };
    if let Err(error) = state
        .registry
        .resolve_call(
            &selection.provider,
            &selection.model,
            selection.reasoning_effort.as_deref(),
        )
        .await
    {
        live.running.store(false, Ordering::SeqCst);
        return Err(ApiError::from_llm(error));
    }

    let token = CancellationToken::new();
    *live.cancel.lock().await = Some(token.clone());

    let driver = state.driver.clone();
    tokio::spawn(async move {
        let mut session = live.session.lock().await;
        let followers = live.followers.clone();
        driver
            .run_turn(
                &mut session,
                &selection,
                &prompt,
                token,
                &move |envelope: &SessionEnvelope| {
                    let _ = followers.send(envelope.clone());
                },
            )
            .await;
        drop(session);
        live.running.store(false, Ordering::SeqCst);
        *live.cancel.lock().await = None;
    });

    Ok((StatusCode::ACCEPTED, Json(json!({ "accepted": true }))))
}

async fn cancel_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let live = state.live.get_or_load(&state.sessions, &id).map_err(ApiError::from_session)?;
    if let Some(token) = live.cancel.lock().await.take() {
        token.cancel();
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
struct FollowQuery {
    #[serde(default)]
    after: u64,
}

/// SSE: replays persisted envelopes with `seq > after`, then live frames.
/// Lag closes the stream; the client resumes with a fresh snapshot.
async fn follow_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<FollowQuery>,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let live = state
        .live
        .get_or_load(&state.sessions, &id)
        .map_err(ApiError::from_session)?;
    let replay: Vec<SessionEnvelope> = {
        let session = live.session.lock().await;
        session
            .events()
            .iter()
            .filter(|envelope| envelope.seq > query.after)
            .cloned()
            .collect()
    };
    let live_stream = BroadcastStream::new(live.followers.subscribe()).filter_map(
        |result| async move {
            match result {
                Ok(envelope) => Some(envelope),
                // Lagged: close so the client re-snapshots.
                Err(_) => None,
            }
        },
    );
    let stream = futures::stream::iter(replay)
        .chain(live_stream)
        .map(|envelope| {
            Ok(Event::default().data(serde_json::to_string(&envelope).unwrap_or_default()))
        });
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15))))
}
