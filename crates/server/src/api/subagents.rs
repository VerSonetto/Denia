//! 子代理定义管理 API：目录、CRUD、内置恢复默认、工具目录与派遣预览。
//!
//! 设计约束（执行计划 11.1）：
//! - 启用/禁用复用 PUT，避免两个写入口；
//! - `revision` 是内容版本，外部编辑同样使旧 revision 失效；
//! - 项目作用域由**已知会话或已存在目录**推导，请求不能指定任意写入路径；
//! - 状态码：400 格式错误 / 403 身份或作用域拒绝 / 404 不存在 /
//!   409 版本或名称冲突 / 422 无法解析为可执行定义。

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use denia_core::session::PermissionMode;
use denia_core::subagent::{
    ProfileColor, ProfileSource, ProfileWriteScope, SubagentInlineSpec, SubagentProfile,
    ToolCapability, child_hard_denied, codes, tool_capability,
};
use denia_tools::runtime_command::DelegateArgs;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::state::AppState;
use crate::subagents::profiles::ProfileStore;
use crate::subagents::resolver;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/subagent-profiles",
            get(list_profiles).post(create_profile),
        )
        .route("/api/subagent-profiles/preview", post(preview_profile))
        .route("/api/subagent-tools", get(list_tools))
        .route(
            "/api/subagent-profiles/{qualified_id}",
            axum::routing::put(update_profile).delete(delete_profile),
        )
        .route(
            "/api/subagent-profiles/{qualified_id}/reset",
            post(reset_profile),
        )
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScopeQuery {
    /// 会话 id：项目作用域从该会话的 cwd 推导（首选）。
    #[serde(default)]
    session_id: Option<String>,
    /// 已存在的目录：没有会话时的回退入口；不接受不存在的路径。
    #[serde(default)]
    cwd: Option<String>,
}

impl ScopeQuery {
    fn from_map(query: &std::collections::HashMap<String, String>) -> Self {
        ScopeQuery {
            session_id: query.get("sessionId").cloned(),
            cwd: query.get("cwd").cloned(),
        }
    }
}

fn project_root_of(
    state: &AppState,
    session_id: Option<&str>,
    cwd: Option<&str>,
) -> Result<Option<PathBuf>, ApiError> {
    if let Some(id) = session_id {
        let header = state
            .sessions
            .read_log_header(id)
            .map_err(ApiError::from_session)?;
        let cwd = PathBuf::from(header.cwd);
        if !cwd.is_dir() {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "subagent/cwd-missing",
                format!("会话 {} 的工作目录已不存在：{}", id, cwd.display()),
            ));
        }
        return Ok(Some(ProfileStore::project_root(&cwd)));
    }
    if let Some(raw) = cwd {
        let path = PathBuf::from(raw);
        if !path.is_dir() {
            return Err(ApiError::bad_request(
                "subagent/cwd-missing",
                format!("项目目录不存在：{}", path.display()),
            ));
        }
        return Ok(Some(ProfileStore::project_root(&path)));
    }
    Ok(None)
}

fn map_error(error: crate::subagents::ProfileError) -> ApiError {
    let status = match error.code.as_str() {
        codes::PROFILE_NOT_FOUND => StatusCode::NOT_FOUND,
        codes::PROFILE_EXISTS | codes::REVISION_CONFLICT | codes::PROFILE_SHADOWED => {
            StatusCode::CONFLICT
        }
        codes::SCOPE_FORBIDDEN => StatusCode::FORBIDDEN,
        codes::AMBIGUOUS_SPEC | codes::DEPTH_CONFIG_REMOVED => StatusCode::BAD_REQUEST,
        codes::PROFILE_INVALID
        | codes::TOOL_UNKNOWN
        | codes::TOOL_NOT_GRANTED
        | codes::TOOL_HARD_DENIED
        | codes::PROFILE_DISABLED
        | codes::EMPTY_TOOLS => StatusCode::UNPROCESSABLE_ENTITY,
        _ => StatusCode::BAD_REQUEST,
    };
    let mut message = error.message.clone();
    if let Some(field) = &error.field {
        message.push_str(&format!("（字段：{field}）"));
    }
    if !error.candidates.is_empty() {
        message.push_str(&format!("；可用候选：{}", error.candidates.join("、")));
    }
    ApiError::new(status, error.code, message)
}

/// 目录：有效项 + 被覆盖项 + 诊断。UI 的列表区读这一份。
async fn list_profiles(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Result<impl IntoResponse, ApiError> {
    let query = ScopeQuery::from_map(&query);
    let project_root = project_root_of(&state, query.session_id.as_deref(), query.cwd.as_deref())?;
    let store = state.subagent_profiles.clone();
    let root = project_root.clone();
    let catalog = tokio::task::spawn_blocking(move || store.catalog(root.as_deref()))
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api/join",
                error.to_string(),
            )
        })?;
    Ok(Json(json!({
        "scope": project_root.as_ref().map(|_| "project").unwrap_or("user"),
        "projectRoot": catalog.project_root,
        "projectDir": catalog.project_dir,
        "userDir": catalog.user_dir,
        "revision": catalog.revision,
        "effective": catalog.effective,
        "shadowed": catalog.shadowed,
        "diagnostics": catalog.diagnostics,
        "colors": ProfileColor::ALL,
        "sources": [ProfileSource::Builtin, ProfileSource::User, ProfileSource::Project],
    })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateBody {
    scope: ProfileWriteScope,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    profile: SubagentProfile,
}

async fn create_profile(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateBody>,
) -> Result<impl IntoResponse, ApiError> {
    let project_root = project_root_of(&state, body.session_id.as_deref(), body.cwd.as_deref())?;
    let store = state.subagent_profiles.clone();
    let root = project_root.clone();
    let scope = body.scope;
    let mut profile = body.profile;
    let instructions = std::mem::take(&mut profile.instructions);
    let view = tokio::task::spawn_blocking(move || {
        store.create(scope, root.as_deref(), profile, instructions)
    })
    .await
    .map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api/join",
            error.to_string(),
        )
    })?
    .map_err(map_error)?;
    Ok((StatusCode::CREATED, Json(json!({ "profile": view }))))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateBody {
    profile: SubagentProfile,
    expected_revision: u64,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    /// 内置定义的编辑永远写用户覆盖；显式给 project 时才写项目层。
    #[serde(default)]
    scope: Option<ProfileWriteScope>,
}

async fn update_profile(
    State(state): State<Arc<AppState>>,
    Path(qualified_id): Path<String>,
    Json(body): Json<UpdateBody>,
) -> Result<impl IntoResponse, ApiError> {
    let project_root = project_root_of(&state, body.session_id.as_deref(), body.cwd.as_deref())?;
    let store = state.subagent_profiles.clone();
    let root = project_root.clone();
    let mut profile = body.profile;
    let instructions = std::mem::take(&mut profile.instructions);
    let expected = body.expected_revision;
    let view = tokio::task::spawn_blocking(move || {
        store.update(
            root.as_deref(),
            &qualified_id,
            profile,
            instructions,
            expected,
        )
    })
    .await
    .map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api/join",
            error.to_string(),
        )
    })?
    .map_err(map_error)?;
    Ok(Json(json!({ "profile": view })))
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteQuery {
    #[serde(default)]
    expected_revision: Option<u64>,
    #[serde(default)]
    scope: Option<ProfileWriteScope>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

impl DeleteQuery {
    fn from_map(query: &std::collections::HashMap<String, String>) -> Self {
        DeleteQuery {
            expected_revision: query.get("expectedRevision").and_then(|v| v.parse().ok()),
            scope: query.get("scope").and_then(|value| match value.as_str() {
                "user" => Some(ProfileWriteScope::User),
                "project" => Some(ProfileWriteScope::Project),
                _ => None,
            }),
            session_id: query.get("sessionId").cloned(),
            cwd: query.get("cwd").cloned(),
        }
    }
}

async fn delete_profile(
    State(state): State<Arc<AppState>>,
    Path(qualified_id): Path<String>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Result<impl IntoResponse, ApiError> {
    let body = DeleteQuery::from_map(&query);
    let project_root = project_root_of(&state, body.session_id.as_deref(), body.cwd.as_deref())?;
    let store = state.subagent_profiles.clone();
    let root = project_root.clone();
    let scope = match body.scope {
        Some(scope) => scope,
        None => scope_for(&store, project_root.as_deref(), &qualified_id)?,
    };
    let restored = tokio::task::spawn_blocking(move || {
        store.delete(
            root.as_deref(),
            &qualified_id,
            scope,
            body.expected_revision,
        )
    })
    .await
    .map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api/join",
            error.to_string(),
        )
    })?
    .map_err(map_error)?;
    Ok(Json(json!({ "restored": restored })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResetBody {
    scope: ProfileWriteScope,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

async fn reset_profile(
    State(state): State<Arc<AppState>>,
    Path(qualified_id): Path<String>,
    Json(body): Json<ResetBody>,
) -> Result<impl IntoResponse, ApiError> {
    let project_root = project_root_of(&state, body.session_id.as_deref(), body.cwd.as_deref())?;
    let store = state.subagent_profiles.clone();
    let root = project_root.clone();
    let scope = body.scope;
    let restored =
        tokio::task::spawn_blocking(move || store.reset(root.as_deref(), &qualified_id, scope))
            .await
            .map_err(|error| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "api/join",
                    error.to_string(),
                )
            })?
            .map_err(map_error)?;
    Ok(Json(json!({ "restored": restored })))
}

/// 删除/恢复默认时的默认作用域：定义自身所在的可写层。
fn scope_for(
    store: &ProfileStore,
    project_root: Option<&std::path::Path>,
    qualified_id: &str,
) -> Result<ProfileWriteScope, ApiError> {
    let catalog = store.catalog(project_root);
    let entry = catalog
        .effective
        .iter()
        .find(|view| view.qualified_id == qualified_id)
        .ok_or_else(|| {
            map_error(crate::subagents::ProfileError::new(
                codes::PROFILE_NOT_FOUND,
                format!("未知的子代理定义：{qualified_id}"),
            ))
        })?;
    Ok(crate::subagents::profiles::write_scope_for(entry))
}

/// 真实工具目录：来源、能力分类、可授予性与原因。
///
/// 指定会话时给出"该父会话下最终可授予"的判定；不指定会话时只公布部署
/// 注册表与硬禁事实，不宣称最终可用性（计划 11.2）。
async fn list_tools(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Result<impl IntoResponse, ApiError> {
    let query = ScopeQuery::from_map(&query);
    let registered = state.driver.tools().names();
    let context = parent_context(&state, query.session_id.as_deref(), query.cwd.as_deref())?;
    let mut tools = Vec::new();
    for name in &registered {
        let capability = tool_capability(name);
        let hard_denied = child_hard_denied(name);
        let (grantable, reason) = match &context {
            Some(context) => {
                if hard_denied {
                    (
                        false,
                        "子代理不能使用该工具（禁止派遣子代理 / 宿主配置 / 会话主控）",
                    )
                } else if context.grant.contains(name) {
                    (true, "父会话当前可授予")
                } else {
                    (false, "父会话当前未授予（preset、features 或权限档收窄）")
                }
            }
            None => {
                if hard_denied {
                    (
                        false,
                        "子代理不能使用该工具（禁止派遣子代理 / 宿主配置 / 会话主控）",
                    )
                } else {
                    (
                        true,
                        "未指定父会话：此处只表示定义可请求，最终可用性以预览为准",
                    )
                }
            }
        };
        tools.push(json!({
            "name": name,
            "source": if name.starts_with("mcp__") { "mcp" } else { "builtin" },
            "category": category_label(capability),
            "effect": effect_note(name, capability),
            "readOnlyCompatible": read_only_compatible(capability),
            "granted": context.as_ref().map(|context| context.grant.contains(name)),
            "grantable": grantable,
            "hardDenied": hard_denied,
            "reason": reason,
        }));
    }
    Ok(Json(json!({
        "sessionId": query.session_id,
        "registered": registered.len(),
        "tools": tools,
    })))
}

/// 工具的副作用说明：写/shell/MCP 的影响必须一句话说清（选工具时看得见）。
fn effect_note(name: &str, capability: ToolCapability) -> &'static str {
    if name.starts_with("mcp__") {
        return "外部 MCP 服务器操作：副作用由该服务器决定，可能改远端数据。";
    }
    match capability {
        ToolCapability::Read => "只读取工作区或网络内容，不改动文件。",
        ToolCapability::Write => "会修改工作区文件（`write_file` 覆盖整份、`edit` 局部替换）。",
        ToolCapability::Shell => "会启动进程：命令可读写文件、访问网络、承担超时与清理义务。",
        ToolCapability::Delegation => "与其他代理通信（子代理只能用 send_message 汇报）。",
        ToolCapability::HostAdministration => "改动 denia 宿主配置，属人类配置面。",
        ToolCapability::SessionControl => "改变会话主控模式或操作主会话目标。",
        ToolCapability::Interaction => "阻塞等待用户作答（有超时与结局区分）。",
        ToolCapability::Jobs => "启动后台任务：进程在子代理结束后仍需收尾。",
        ToolCapability::Browser => "驱动常驻浏览器实例，会产生 tab 资源。",
        ToolCapability::Mcp => "外部 MCP 服务器操作。",
        ToolCapability::Other => "副作用取决于工具实现。",
    }
}

/// 只读上限下是否仍然可用：写、命令、后台任务与派遣/宿主控制类都不兼容。
/// `ask`（提问）不改动任何东西，与只读兼容。
fn read_only_compatible(capability: ToolCapability) -> bool {
    !matches!(
        capability,
        ToolCapability::Write
            | ToolCapability::Shell
            | ToolCapability::Jobs
            | ToolCapability::Delegation
            | ToolCapability::HostAdministration
            | ToolCapability::SessionControl
    )
}

fn category_label(capability: ToolCapability) -> &'static str {
    match capability {
        ToolCapability::Read => "读取",
        ToolCapability::Write => "写入",
        ToolCapability::Shell => "命令与后台任务",
        ToolCapability::Delegation => "代理派遣",
        ToolCapability::HostAdministration => "宿主配置",
        ToolCapability::SessionControl => "会话主控",
        ToolCapability::Interaction => "用户交互",
        ToolCapability::Jobs => "后台任务",
        ToolCapability::Browser => "浏览器",
        ToolCapability::Mcp => "MCP",
        ToolCapability::Other => "其他",
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreviewBody {
    #[serde(default)]
    profile_id: Option<String>,
    #[serde(default)]
    inline: Option<SubagentInlineSpec>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

/// 不启动模型，返回某父会话下有效工具/模型/权限与诊断。
async fn preview_profile(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PreviewBody>,
) -> Result<impl IntoResponse, ApiError> {
    let context = parent_context(&state, body.session_id.as_deref(), body.cwd.as_deref())?;
    let Some(context) = context else {
        // 没有父会话就不宣称"最终可用工具"。
        return Ok(Json(json!({
            "sessionId": Value::Null,
            "final": false,
            "note": "未指定父会话：只能显示定义请求集合，不能判断最终可用工具。",
            "requested": body.profile_id.or_else(|| body.inline.as_ref().map(|spec| spec.name.clone())),
        })));
    };
    let store = state.subagent_profiles.clone();
    let project_root = context.project_root.clone();
    let profile_id = body.profile_id.clone();
    let resolved = match &profile_id {
        Some(raw) => {
            let raw = raw.clone();
            let root = project_root.clone();
            Some(
                tokio::task::spawn_blocking(move || store.resolve(root.as_deref(), &raw))
                    .await
                    .map_err(|error| {
                        ApiError::new(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "api/join",
                            error.to_string(),
                        )
                    })?
                    .map_err(map_error)?,
            )
        }
        None => None,
    };
    let args = DelegateArgs {
        prompt: "preview".to_string(),
        description: None,
        profile_id,
        inline: body.inline.clone(),
        provider: None,
        model: None,
        reasoning_effort: None,
        allowed_tools: None,
        run_in_background: None,
        persona: None,
        max_depth: None,
    };
    let dispatch = resolver::resolve_dispatch(
        &args,
        false,
        &context.selection,
        &context.grant,
        &context.registered,
        resolved.as_ref(),
        false,
    )
    .map_err(map_error)?;
    let ceiling = dispatch.permission_ceiling;
    let mode = resolver::effective_permission_mode(ceiling, context.mode);
    Ok(Json(json!({
        "sessionId": context.session_id,
        "final": true,
        "profile": {
            "qualifiedId": dispatch.qualified_id,
            "name": dispatch.name,
            "description": dispatch.description,
            "inline": dispatch.inline,
            "revision": dispatch.revision,
        },
        "tools": dispatch.tools,
        "toolCount": dispatch.tools.len(),
        "model": dispatch.selection,
        "modelOrigin": match dispatch.model_origin {
            resolver::ModelOrigin::CallOverride => "call-override",
            resolver::ModelOrigin::ProfileExplicit => "profile-explicit",
            resolver::ModelOrigin::Parent => "parent",
        },
        "permissionCeiling": ceiling.as_str(),
        "effectivePermissionMode": mode.as_str(),
        "delegationAllowed": false,
        "instructionScope": "project-only",
        "fingerprint": dispatch.fingerprint(),
    })))
}

/// 父会话上下文：可授予工具 + preset + 权限档 + 当前模型。
struct ParentContext {
    session_id: Option<String>,
    project_root: Option<PathBuf>,
    grant: Vec<String>,
    registered: Vec<String>,
    selection: denia_core::config::ModelSelection,
    mode: PermissionMode,
}

fn parent_context(
    state: &AppState,
    session_id: Option<&str>,
    cwd: Option<&str>,
) -> Result<Option<ParentContext>, ApiError> {
    let registered = state.driver.tools().names();
    let Some(id) = session_id else {
        // 没有会话时只做目录级说明（cwd 只用来解析项目作用域）。
        if cwd.is_some() {
            let _ = project_root_of(state, None, cwd)?;
        }
        return Ok(None);
    };
    let header = state
        .sessions
        .read_log_header(id)
        .map_err(ApiError::from_session)?;
    let live = state
        .live
        .get_or_load(&state.sessions, id)
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "subagent/session-load",
                error.to_string(),
            )
        })?;
    let mode = live.session.permission_mode();
    let preset = state
        .driver
        .preset_for(live.session.agent_preset().as_deref());
    let features = state.driver.features_for(&live.session);
    let preset_tools = preset.as_ref().and_then(|preset| preset.tools.as_deref());
    let grant = resolver::parent_grant(&registered, &features, preset_tools, mode);
    let selection = live
        .session
        .events()
        .iter()
        .rev()
        .find_map(|envelope| match &envelope.event {
            denia_core::session::SessionEvent::RequestHeader { header, .. } => {
                Some(denia_core::config::ModelSelection {
                    provider: header.config.provider.clone(),
                    model: header.config.model.clone(),
                    reasoning_effort: header.config.reasoning_effort.clone(),
                })
            }
            _ => None,
        })
        .unwrap_or(denia_core::config::ModelSelection {
            provider: String::new(),
            model: String::new(),
            reasoning_effort: None,
        });
    let project_root = {
        let cwd = PathBuf::from(header.cwd);
        cwd.is_dir().then(|| ProfileStore::project_root(&cwd))
    };
    Ok(Some(ParentContext {
        session_id: Some(id.to_string()),
        project_root,
        grant,
        registered,
        selection,
        mode,
    }))
}
