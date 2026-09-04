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
use denia_core::session::{ApprovalOutcome, PermissionMode, SessionEnvelope, SessionEvent};
use denia_tools::permission::parse_permission_mode;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;

use crate::error::ApiError;
use crate::state::{AppState, RunningGuard, ServerEvent};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route(
            "/api/sessions/{id}",
            get(get_session).delete(delete_session),
        )
        .route("/api/sessions/{id}/events", get(get_session_events))
        .route("/api/sessions/{id}/prompt", post(prompt_session))
        .route("/api/sessions/{id}/cancel", post(cancel_session))
        .route("/api/sessions/{id}/permission", axum::routing::put(set_session_permission))
        .route("/api/sessions/{id}/approvals/{request_id}", post(answer_approval))
        .route("/api/sessions/{id}/fork", post(fork_session))
        .route("/api/sessions/{id}/follow", get(follow_session))
        .route("/api/sessions/{id}/context-breakdown", get(context_breakdown))
        .route("/api/sessions/{id}/compact", post(compact_session))
        .route("/api/sessions/{id}/checkpoints", get(list_checkpoints))
        .route(
            "/api/sessions/{id}/checkpoints/{seq}/diff",
            get(checkpoint_diff),
        )
        .route("/api/sessions/{id}/rewind", post(rewind_session))
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
    // 新会话固定写入默认权限事件,让前端/回放都能读到当前档位。
    session
        .set_permission_mode(PermissionMode::WorkspaceWrite)
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionPageQuery {
    /// 只取该 seq 之前的事件;缺省取日志尾部。
    #[serde(default)]
    before: Option<u64>,
    #[serde(default = "default_session_page_limit")]
    limit: usize,
}

fn default_session_page_limit() -> usize {
    500
}

/// 分页读取会话事件:流式扫描文件,只返回一个有限窗口,不把完整历史
/// 加载到服务端常驻内存(打开长会话时前端不再依赖全量快照)。
async fn get_session_events(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<SessionPageQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let sessions = state.sessions.clone();
    let page = tokio::task::spawn_blocking(move || sessions.read_page(&id, query.before, query.limit))
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "task/join",
                error.to_string(),
            )
        })?
        .map_err(ApiError::from_session)?;
    Ok(Json(json!({
        "header": page.header,
        "events": page.events,
        "total": page.total,
        "hasMoreBefore": page.has_more_before,
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
    /// 轨迹引用(标题, 正文);以 injected 上下文消息随本轮注入。
    #[serde(default)]
    quoted: Vec<PromptQuote>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptQuote {
    title: String,
    text: String,
}

/// 引用正文上限:单条 64k 字符、合计 256k,防异常体积拖垮本轮。
const QUOTE_TEXT_LIMIT: usize = 64 * 1024;
const QUOTE_TOTAL_LIMIT: usize = 256 * 1024;

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

    // 模型选择无服务端默认:前端为每个请求携带"上次使用的模型"。
    let provider = body.provider.unwrap_or_default().trim().to_string();
    let model = body.model.unwrap_or_default().trim().to_string();
    if provider.is_empty() || model.is_empty() {
        live.running.store(false, Ordering::SeqCst);
        return Err(ApiError::bad_request(
            "session/model-required",
            "请先在输入栏选择模型再发送消息",
        ));
    }
    let selection = ModelSelection {
        provider,
        model,
        reasoning_effort: body.reasoning_effort,
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
    // 轨迹引用:非空校验 + 体积上限(fail loud,不静默丢弃)。
    let mut quoted: Vec<(String, String)> = Vec::with_capacity(body.quoted.len());
    let mut quoted_total = 0usize;
    for quote in body.quoted {
        let title = quote.title.trim().to_string();
        let text = quote.text.trim().to_string();
        if text.is_empty() {
            live.running.store(false, Ordering::SeqCst);
            return Err(ApiError::bad_request(
                "session/empty-quote",
                "引用的轨迹内容为空",
            ));
        }
        if title.len() > 200 || text.len() > QUOTE_TEXT_LIMIT {
            live.running.store(false, Ordering::SeqCst);
            return Err(ApiError::bad_request(
                "session/quote-too-large",
                format!(
                    "引用过大:标题 ≤ 200 字符、单条正文 ≤ {QUOTE_TEXT_LIMIT} 字符"
                ),
            ));
        }
        quoted_total += text.len();
        if quoted_total > QUOTE_TOTAL_LIMIT {
            live.running.store(false, Ordering::SeqCst);
            return Err(ApiError::bad_request(
                "session/quote-too-large",
                format!("引用总量超过 {QUOTE_TOTAL_LIMIT} 字符上限"),
            ));
        }
        quoted.push((title, text));
    }

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
        let emit: Arc<dyn Fn(&SessionEnvelope) + Send + Sync> = Arc::new(move |envelope: &SessionEnvelope| {
            let _ = followers_for_turn.send(envelope.clone());
        });
        let _reason = driver
            .run_turn(
                &session,
                &selection,
                &prompt,
                images,
                upload_files,
                quoted,
                vision_supported,
                token,
                emit,
            )
            .await;
        // 上下文管理(投影剪枝 + LLM 压缩)已内建于 driver 的请求构造前
        // (denia_agent_loop::SessionDriver):轮次闭合后不再落盘替换,
        // 日志保持 append-only,provider 前缀缓存不被破坏。
    });

    Ok((StatusCode::ACCEPTED, Json(json!({ "accepted": true }))))
}

async fn cancel_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let live = state.live.get_or_load(&state.sessions, &id).map_err(ApiError::from_session)?;
    // 幂等取消:保留 token(不 take),轮次结束由 RunningGuard 清空;
    // 若首次点击时 turn 恰好卡在无取消意识的等待上,后续再点仍能生效,
    // 不会出现"点了一次之后再点就没反应"。
    if let Some(token) = live.cancel.lock().unwrap().as_ref() {
        token.cancel();
    }
    // 顺带结算挂起的审批,driver 的 select 也能通过 cancel token 退出。
    {
        let mut pending = live
            .pending_approvals
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        for tx in pending.drain() {
            let _ = tx.1.send(ApprovalOutcome::Cancelled);
        }
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PermissionBody {
    mode: String,
}

/// 切换当前会话的权限预设(抄 dsh `/permission <preset>` 的写路径)。
/// 事件落日志并通过 SSE 广播;driver 在下一次工具调用时读取新模式。
async fn set_session_permission(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<PermissionBody>,
) -> Result<impl IntoResponse, ApiError> {
    let mode = parse_permission_mode(&body.mode).ok_or_else(|| {
        ApiError::bad_request(
            "permission/unknown-mode",
            format!(
                "unknown permission mode '{}' (expected read-only, workspace-write, or danger-full-access)",
                body.mode
            ),
        )
    })?;
    let live = state
        .live
        .get_or_load(&state.sessions, &id)
        .map_err(ApiError::from_session)?;
    live.session
        .set_permission_mode(mode)
        .map_err(ApiError::from_session)?;
    Ok(Json(json!({ "mode": mode.as_str() })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApprovalAnswerBody {
    /// `allow-once` | `reject`
    decision: String,
}

/// 应答一次挂起的审批(抄 dsh ui approval panel 的 allow-once/reject)。
async fn answer_approval(
    State(state): State<Arc<AppState>>,
    Path((id, request_id)): Path<(String, String)>,
    Json(body): Json<ApprovalAnswerBody>,
) -> Result<impl IntoResponse, ApiError> {
    let outcome = match body.decision.as_str() {
        "allow-once" => ApprovalOutcome::AllowedOnce,
        "reject" => ApprovalOutcome::Rejected,
        _ => {
            return Err(ApiError::bad_request(
                "approval/bad-decision",
                "decision must be 'allow-once' or 'reject'",
            ));
        }
    };
    let Some(live) = state.live.get(&id) else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "approval/session-not-loaded",
            "审批会话不在运行中或不存在",
        ));
    };
    let sender = {
        let mut pending = live
            .pending_approvals
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        pending.remove(&request_id)
    };
    let Some(sender) = sender else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "approval/not-found",
            "该审批请求不存在或已结算",
        ));
    };
    let _ = sender.send(outcome);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ForkBody {
    /// 分支锚点:锚定到第一个 `seq >= atSeq` 的已完成轮次边界。
    /// 缺省 = 会话末尾(最后一个已完成轮次)。
    #[serde(default)]
    at_seq: Option<u64>,
}

/// 从源会话某个已完成轮次边界分支出一个全新会话(抄 dsh `session.fork`):
/// 种子 = 源日志前缀原样回放(seq 重排、时间戳保留),血缘写进子会话头,
/// 并挂到源会话所在工作区。源会话本身不动——分支非破坏性,与回退互补。
async fn fork_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<ForkBody>>,
) -> Result<impl IntoResponse, ApiError> {
    let body = body.map(|Json(body)| body).unwrap_or_default();
    let live = state
        .live
        .get_or_load(&state.sessions, &id)
        .map_err(ApiError::from_session)?;
    if live.running.load(Ordering::SeqCst) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "session/running",
            "cancel the running turn before forking",
        ));
    }
    let (source_events, header) = {
        let session = live.session.clone();
        (session.events(), session.header().clone())
    };
    let cut = denia_core::session::fork_cut_index(&source_events, body.at_seq).ok_or_else(|| {
        ApiError::bad_request(
            "session/fork-unavailable",
            "仅可从已完成的轮次分支:该会话还没有已完成的轮次(或锚点所在轮次尚未完成)",
        )
    })?;
    let cwd = std::path::PathBuf::from(&header.cwd);
    let child = state
        .sessions
        .create_forked(&source_events, cut, &cwd, header.sandbox, &id)
        .map_err(ApiError::from_session)?;
    // 挂到源会话所在工作区;工作区刚被删时回滚子会话,不留孤儿。
    let workspace = state.workspaces.resolve_by_path(&cwd);
    if let Some(ws) = &workspace {
        if !state.workspaces.attach(&ws.id, child.id()) {
            let _ = state.sessions.delete(child.id());
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "workspace/not-found",
                "workspace not found",
            ));
        }
    }
    let summary = json!({
        "id": child.id(),
        "created_at": child.header().created_at,
        "excerpt": child.first_prompt_excerpt(80),
        "cwd": child.header().cwd,
        "sandbox": child.header().sandbox,
        "parent_session": child.header().parent_session,
    });
    let _ = state.events.send(ServerEvent::SessionsUpdated);
    Ok((StatusCode::CREATED, Json(json!({ "session": summary }))))
}

/// 列出该会话所有可回退的用户消息(checkpoint)。
async fn list_checkpoints(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let live = state
        .live
        .get_or_load(&state.sessions, &id)
        .map_err(ApiError::from_session)?;
    let checkpoints: Vec<serde_json::Value> = live
        .session
        .events()
        .iter()
        .filter_map(|envelope| match &envelope.event {
            SessionEvent::UserMessage {
                text,
                injected: false,
                ..
            } => Some(json!({
                "seq": envelope.seq,
                "time": envelope.time,
                "text": text,
            })),
            _ => None,
        })
        .collect();
    Ok(Json(json!({ "checkpoints": checkpoints })))
}

/// 预览回退到某个 checkpoint 时会变化的文件。
async fn checkpoint_diff(
    State(state): State<Arc<AppState>>,
    Path((id, seq)): Path<(String, u64)>,
) -> Result<impl IntoResponse, ApiError> {
    let live = state
        .live
        .get_or_load(&state.sessions, &id)
        .map_err(ApiError::from_session)?;
    let cwd = std::path::PathBuf::from(live.session.header().cwd.clone());
    let changes = state
        .file_history
        .diff(&id, &cwd, seq)
        .await
        .map_err(|error| ApiError::bad_request("file-history/diff", error))?;
    Ok(Json(json!({ "changes": changes })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RewindBody {
    to_seq: u64,
}

/// 回退到某个用户消息之前:先恢复文件,再物理截断会话。
async fn rewind_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<RewindBody>,
) -> Result<impl IntoResponse, ApiError> {
    let live = state
        .live
        .get_or_load(&state.sessions, &id)
        .map_err(ApiError::from_session)?;
    if live.running.load(Ordering::SeqCst) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "session/running",
            "cancel the running turn before rewinding",
        ));
    }
    let cwd = std::path::PathBuf::from(live.session.header().cwd.clone());
    // 先恢复文件;文件回退失败就不动会话,避免“对话已截断但文件没还原”。
    let changed_files = state
        .file_history
        .rewind(&id, &cwd, body.to_seq)
        .await
        .map_err(|error| ApiError::bad_request("file-history/rewind", error))?;
    let outcome = live
        .session
        .rewind(body.to_seq)
        .map_err(ApiError::from_session)?;
    let _ = state.events.send(ServerEvent::SessionsUpdated);
    Ok(Json(json!({
        "ok": true,
        "toSeq": outcome.to_seq,
        "toMessage": outcome.to_message,
        "removedEvents": outcome.removed_events,
        "changedFiles": changed_files,
    })))
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

/// 日志中最后一次出现的轮次号(手动压缩事件归属的轮次;空日志为 0)。
fn last_turn_of(session: &denia_session::Session) -> u32 {
    session
        .events()
        .iter()
        .rev()
        .find_map(|item| match item.event {
            SessionEvent::TurnStart { turn } => Some(turn),
            _ => None,
        })
        .unwrap_or(0)
}

/// 手动压缩(上下文面板按钮):占用运行位(压缩期间会话不可发消息),
/// 从最近一次请求头部恢复模型/系统提示/工具集,同步等待摘要调用完成;
/// 压缩行经 followers 广播展示。失败/无物可压给可读响应,不阻塞会话。
async fn compact_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
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
            "会话正在运行中,无法手动压缩",
        ));
    }
    // RAII:函数返回(含错误)自动复位运行位、清 cancel、广播结束。
    let _guard = RunningGuard::new(live.clone(), state.events.clone());
    if !crate::state::console_settings(&state.settings)
        .compaction
        .compact_enabled
    {
        return Err(ApiError::bad_request(
            "compaction/disabled",
            "上下文压缩已在设置中关闭",
        ));
    }
    if !live
        .session
        .events()
        .iter()
        .any(|item| matches!(item.event, SessionEvent::RequestHeader { .. }))
    {
        return Err(ApiError::bad_request(
            "session/no-request",
            "该会话还没有发起过模型请求,暂无可压缩内容",
        ));
    }
    // cancel 挂到会话槽:前端取消按钮可随时中止摘要调用。
    let token = CancellationToken::new();
    *live.cancel.lock().unwrap() = Some(token.clone());
    match state.driver.compact_manually(&live.session, token).await {
        Ok(Some(outcome)) => {
            let envelope = live
                .session
                .append(SessionEvent::CompactionSummary {
                    turn: last_turn_of(&live.session),
                    step: 0,
                    summary: outcome.summary,
                    replaces_from: outcome.replaces_from,
                    replaces_to: outcome.replaces_to,
                    keep_from: outcome.keep_from,
                    pre_tokens: outcome.pre_tokens,
                    post_tokens: outcome.post_tokens,
                })
                .map_err(ApiError::from_session)?;
            let _ = live.followers.send(envelope);
            Ok(Json(json!({
                "ok": true,
                "outcome": {
                    "replacesFrom": outcome.replaces_from,
                    "replacesTo": outcome.replaces_to,
                    "keepFrom": outcome.keep_from,
                    "preTokens": outcome.pre_tokens,
                    "postTokens": outcome.post_tokens,
                    "savedTokens": outcome.pre_tokens.saturating_sub(outcome.post_tokens),
                },
            })))
        }
        Ok(None) => Ok(Json(json!({ "ok": false, "reason": "nothing-to-compact" }))),
        Err(failure) => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            failure.code,
            failure.message,
        )),
    }
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
