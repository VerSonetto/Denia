//! Session endpoints: list/create/read/delete, prompt, cancel, follow.
//!
//! ## 生命周期保证
//!
//! - **prompt**:`running` 位由 [`RunningGuard`] 兜底复位——任务 panic 或
//!   提前 drop 都不会把会话锁死在运行态。
//! - **delete**:先阻止运行中删除,再删目录 + live 卸载 + **所有**工作区
//!   账本去引用;任何路径都不留残留。
//! - **create**:工作区账本 attach 失败会回滚会话(不产生孤儿记录)。
//! - **follow**:重放只取 `seq > after` 的增量区间,不克隆全量日志。

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use denia_core::config::ModelSelection;
use denia_core::session::SessionEnvelope;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;

use crate::error::ApiError;
use crate::state::{AppState, RunningGuard, ServerEvent, current_default_selection};

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
        .route("/api/sessions/{id}/context-breakdown", get(context_breakdown))
}

async fn list_sessions(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, ApiError> {
    // 内存索引:O(会话数) stat 校验,零全文读取。
    let sessions = state.sessions.list().map_err(ApiError::from_session)?;
    Ok(Json(json!({ "sessions": sessions })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateBody {
    /// 目标工作区;与 cwd 互斥(抄 dsh:二者同时给 → bad-request)。
    #[serde(default)]
    workspace_id: Option<String>,
    /// 直接指定目录;不挂任何工作区(落"未分组")。
    #[serde(default)]
    cwd: Option<String>,
    /// Overrides the console `sandbox` default for this session.
    #[serde(default)]
    sandbox: Option<bool>,
}

async fn create_session(
    State(state): State<Arc<AppState>>,
    body: Option<Json<CreateBody>>,
) -> Result<impl IntoResponse, ApiError> {
    let body = body.map(|Json(body)| body).unwrap_or(CreateBody {
        workspace_id: None,
        cwd: None,
        sandbox: None,
    });
    if body.workspace_id.is_some() && body.cwd.is_some() {
        return Err(ApiError::bad_request(
            "gateway/bad-request",
            "workspaceId and cwd are mutually exclusive",
        ));
    }
    let workspace = match &body.workspace_id {
        Some(id) => Some(
            state
                .workspaces
                .get(id)
                .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "workspace/not-found", "workspace not found"))?,
        ),
        None => None,
    };
    let cwd = match &workspace {
        Some(ws) => std::path::PathBuf::from(ws.path.clone()),
        None => {
            let Some(raw) = body.cwd.filter(|cwd| !cwd.trim().is_empty()) else {
                return Err(ApiError::bad_request(
                    "session/workspace-required",
                    "先选择工作区,再开始会话",
                ));
            };
            let dir = std::path::PathBuf::from(raw.trim());
            if !dir.is_dir() {
                return Err(ApiError::bad_request(
                    "session/bad-cwd",
                    format!("'{raw}' 不是目录"),
                ));
            }
            dir
        }
    };
    let console = crate::state::console_settings(&state.settings);
    let sandbox = body.sandbox.unwrap_or(console.sandbox);
    let session = state
        .sessions
        .create(&cwd, sandbox)
        .map_err(ApiError::from_session)?;
    if let Some(ws) = &workspace {
        // 会话头 cwd == 工作区路径(构造保证);账本 prepend。attach 失败
        // (工作区刚被删)时回滚会话,不留孤儿记录。
        if !state.workspaces.attach(&ws.id, session.id()) {
            let _ = state.sessions.delete(session.id());
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "workspace/not-found",
                "workspace not found",
            ));
        }
    }
    let summary = json!({
        "id": session.id(),
        "created_at": session.header().created_at,
        "excerpt": null,
        "cwd": session.header().cwd,
        "sandbox": session.header().sandbox,
    });
    let _ = state.events.send(ServerEvent::SessionsUpdated);
    Ok((StatusCode::CREATED, Json(json!({ "session": summary }))))
}

async fn get_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let live = state.live.get_or_load(&state.sessions, &id).map_err(ApiError::from_session)?;
    let session = live.session.clone();
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
    // 全局账本去引用:工作区列表里不残留已删会话。
    state.workspaces.detach_session(&id);
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
    /// 粘贴的内联图片(base64);模型必须标记为可识图。
    #[serde(default)]
    images: Vec<PromptImage>,
    /// 已上传文件(绝对路径);作为注入上下文随消息发送。
    #[serde(default)]
    files: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptImage {
    #[serde(default)]
    name: Option<String>,
    mime: String,
    data: String,
}

/// 模型是否声明了图片输入能力(input_modalities 含 image)。
fn model_vision_supported(resolved: &denia_llm::LlmResolvedModelInfo) -> bool {
    resolved
        .info
        .input_modalities
        .iter()
        .any(|modality| modality.eq_ignore_ascii_case("image"))
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
    {
        let session = live.session.clone();
        let cwd = session.header().cwd.clone();
        if !std::path::Path::new(&cwd).is_dir() {
            return Err(ApiError::bad_request(
                "session/dead-cwd",
                format!(
                    "this session's working directory no longer exists: {cwd} — start a new session in an existing directory"
                ),
            ));
        }
    }
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
    // 运行状态立即推给控制台(侧栏圆点/发送按钮,无需 follow 长连接)。
    let _ = state.events.send(ServerEvent::RunningChanged {
        id: id.clone(),
        running: true,
    });
    // 消息落库后摘要变化:通知控制台刷新列表(excerpt 即时可见)。
    let _ = state.events.send(ServerEvent::SessionsUpdated);

    let default = current_default_selection(&state.settings);
    let selection = ModelSelection {
        provider: body.provider.unwrap_or(default.provider),
        model: body.model.unwrap_or(default.model),
        reasoning_effort: body.reasoning_effort.or(default.reasoning_effort),
    };
    let resolved = match state
        .registry
        .resolve_call(
            &selection.provider,
            &selection.model,
            selection.reasoning_effort.as_deref(),
        )
        .await
    {
        Ok(resolved) => resolved,
        Err(error) => {
            live.running.store(false, Ordering::SeqCst);
            return Err(ApiError::from_llm(error));
        }
    };
    let vision_supported = model_vision_supported(&resolved);
    // 用户要求:图片只允许发给标记为可识图的模型。
    if !body.images.is_empty() && !vision_supported {
        live.running.store(false, Ordering::SeqCst);
        let shown = if selection.model.is_empty() {
            "当前模型".to_string()
        } else {
            format!("模型 '{}'", selection.model)
        };
        return Err(ApiError::bad_request(
            "model/no-vision",
            format!("{shown} 未标记为可识图,粘贴的图片不能发送;请切换到支持图片输入的模型。"),
        ));
    }
    // 上传文件校验:必须存在且位于会话工作区或本会话上传目录内。
    let mut upload_files = Vec::new();
    {
        let cwd = live.session.header().cwd.clone();
        let uploads_root = state.home.join("uploads").join(&id);
        for file in &body.files {
            let path = std::path::PathBuf::from(file);
            let allowed = (path.starts_with(&cwd) || path.starts_with(&uploads_root)) && path.is_file();
            if !allowed {
                live.running.store(false, Ordering::SeqCst);
                return Err(ApiError::bad_request(
                    "session/bad-attachment",
                    format!("文件 '{}' 不在会话工作区或上传目录内", file),
                ));
            }
            upload_files.push(path.to_string_lossy().to_string());
        }
    }
    let images: Vec<denia_core::message::ImageData> = body
        .images
        .into_iter()
        .map(|image| denia_core::message::ImageData {
            mime: image.mime,
            data: image.data,
        })
        .collect();

    let token = CancellationToken::new();
    *live.cancel.lock().unwrap() = Some(token.clone());

    let followers_for_turn = live.followers.clone();
    let events_for_guard = state.events.clone();
    let driver = state.driver.clone();
    let session = live.session.clone();
    let live = live.clone();
    tokio::spawn(async move {
        // RAII:任务结束(含 panic)自动复位 running + 清 cancel + 广播结束。
        let _guard = RunningGuard::new(live.clone(), events_for_guard);
        let _reason = driver
            .run_turn(
                &session,
                &selection,
                &prompt,
                images,
                upload_files,
                vision_supported,
                token,
                Arc::new(move |envelope: &SessionEnvelope| {
                    let _ = followers_for_turn.send(envelope.clone());
                }),
            )
            .await;
    });

    Ok((StatusCode::ACCEPTED, Json(json!({ "accepted": true }))))
}

async fn cancel_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let live = state.live.get_or_load(&state.sessions, &id).map_err(ApiError::from_session)?;
    if let Some(token) = live.cancel.lock().unwrap().take() {
        token.cancel();
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
struct FollowQuery {
    #[serde(default)]
    after: u64,
}

/// 当前上下文 token 拆分(provider 精确 + 启发式)。
/// 供前端上下文占用圆环面板展示(总占比 = pressure / window;拆分 = breakdown)。
async fn context_breakdown(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let live = state
        .live
        .get_or_load(&state.sessions, &id)
        .map_err(ApiError::from_session)?;
    let breakdown = live.session.context_breakdown();
    let pressure = live.session.context_pressure();
    let usage = live.session.turn_token_usage();
    Ok(Json(json!({
        "breakdown": breakdown,
        "pressure": pressure,
        "usage": usage,
    })))
}

/// SSE: replays persisted envelopes with `seq > after`, then live frames.
/// Lag closes the stream; the client resumes with a fresh snapshot.
///
/// The broadcast subscription is created BEFORE the replay snapshot: events
/// appended between the two are delivered twice (replay + live) instead of
/// being lost, and the client dedupes by seq. Losing them would leave a
/// permanent hole from the client's cursor and force an endless re-snapshot
/// loop while a turn is appending fast.
async fn follow_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<FollowQuery>,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let live = state
        .live
        .get_or_load(&state.sessions, &id)
        .map_err(ApiError::from_session)?;
    state.live.touch(&id);
    let live_stream = BroadcastStream::new(live.followers.subscribe()).filter_map(
        |result| async move {
            match result {
                Ok(envelope) => Some(envelope),
                // Lagged: close so the client re-snapshots.
                Err(_) => None,
            }
        },
    );
    // 只拉增量区间(after 到尾部),长会话断线重连不再克隆全量日志。
    let replay: Vec<SessionEnvelope> = live.session.events_after(query.after);
    let stream = futures::stream::iter(replay)
        .chain(live_stream)
        .map(|envelope| {
            Ok(Event::default().data(serde_json::to_string(&envelope).unwrap_or_default()))
        });
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15))))
}
