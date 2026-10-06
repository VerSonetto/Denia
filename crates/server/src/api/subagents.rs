//! 子代理定义管理端点：名册、创建/更新/删除、恢复默认、工具目录与派遣预览。
//!
//! 写入统一经过 [`SubagentProfileStore`]：revision 校验、路径穿越拒绝与
//! 原子替换都在那一层，HTTP 层只做身份解析（会话 → 项目根）与错误码映射。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use denia_core::subagent::{SubagentProfile, SubagentProfileSource, ToolChoice};
use serde::Deserialize;
use serde_json::json;

use crate::error::ApiError;
use crate::state::AppState;
use crate::subagents::profiles::SubagentError;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/subagent-profiles",
            get(list_profiles).post(create_profile),
        )
        .route(
            "/api/subagent-profiles/{qualified_id}",
            axum::routing::put(update_profile).delete(delete_profile),
        )
        .route(
            "/api/subagent-profiles/{qualified_id}/reset",
            post(reset_profile),
        )
        .route("/api/subagent-profiles/preview", post(preview))
        .route("/api/subagent-tools", get(list_tools))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScopeQuery {
    /// 会话 id：项目级定义与真实授权都要从它推出的项目根算。
    #[serde(default)]
    session_id: Option<String>,
}

/// 会话 → 项目根的解析（不接受调用方直接给路径）。
fn project_root_for(state: &AppState, session_id: Option<&str>) -> Option<std::path::PathBuf> {
    let cwd = session_id.and_then(|id| {
        state
            .live
            .get(id)
            .map(|live| std::path::PathBuf::from(live.session.header().cwd.clone()))
    });
    cwd.and_then(|cwd| crate::subagents::project_root_of(&cwd))
}

fn map_error(error: SubagentError) -> ApiError {
    let status = match error.code.as_str() {
        "subagent/profile-not-found" => StatusCode::NOT_FOUND,
        "subagent/revision-conflict" => StatusCode::CONFLICT,
        "subagent/builtin-read-only" => StatusCode::FORBIDDEN,
        "subagent/profile-broken" | "subagent/profile-invalid" => StatusCode::UNPROCESSABLE_ENTITY,
        _ if error.code.contains("not-found") => StatusCode::NOT_FOUND,
        _ if error.code.contains("conflict") || error.code.contains("exists") => {
            StatusCode::CONFLICT
        }
        _ => StatusCode::BAD_REQUEST,
    };
    ApiError::new(status, error.code.clone(), error.to_string())
}

/// 名册快照：按来源分组、带 revision 与诊断。
async fn list_profiles(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ScopeQuery>,
) -> impl IntoResponse {
    let project_root = project_root_for(&state, query.session_id.as_deref());
    let rows = state.subagents.rows_for(project_root.as_deref());
    Json(json!({
        "userRoot": state.subagents.user_root().display().to_string(),
        "projectRoot": state.subagents
            .project_root(project_root.as_deref())
            .map(|root| root.display().to_string()),
        "profiles": rows.as_ref(),
        "deprecations": state.subagent_migration_note.clone(),
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WriteBody {
    /// `user` | `project`；缺省 `user`。
    #[serde(default)]
    scope: Option<String>,
    profile: SubagentProfile,
    #[serde(default)]
    expected_revision: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
}

fn parse_scope(raw: Option<&str>) -> Result<SubagentProfileSource, ApiError> {
    match raw.unwrap_or("user") {
        "user" => Ok(SubagentProfileSource::User),
        "project" => Ok(SubagentProfileSource::Project),
        other => Err(ApiError::bad_request(
            "subagent/invalid-scope",
            format!("作用域只能是 user 或 project，收到：{other}"),
        )),
    }
}

async fn create_profile(
    State(state): State<Arc<AppState>>,
    Json(body): Json<WriteBody>,
) -> Result<impl IntoResponse, ApiError> {
    let scope = parse_scope(body.scope.as_deref())?;
    let project_root = project_root_for(&state, body.session_id.as_deref());
    let store = state.subagents.clone();
    let profile = body.profile.clone();
    let expected = body.expected_revision.clone();
    let row = tokio::task::spawn_blocking(move || {
        store.write(scope, &profile, project_root.as_deref(), expected.as_deref())
    })
    .await
    .map_err(|error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "api/join", error.to_string()))?
    .map_err(map_error)?;
    let _ = state.events.send(crate::state::ServerEvent::SessionsUpdated);
    Ok((StatusCode::CREATED, Json(json!({ "profile": row }))))
}

async fn update_profile(
    State(state): State<Arc<AppState>>,
    Path(qualified_id): Path<String>,
    Json(body): Json<WriteBody>,
) -> Result<impl IntoResponse, ApiError> {
    let project_root = project_root_for(&state, body.session_id.as_deref());
    // 内置定义：写的是同 id 的用户覆盖（"编辑内置"不改程序资源）。
    let (scope, id) = split_qualified(&qualified_id).map_err(map_error)?;
    let scope = if scope == SubagentProfileSource::Builtin {
        SubagentProfileSource::User
    } else {
        scope
    };
    let mut profile = body.profile.clone();
    profile.id = id;
    // 只读上限的保护：把 explore 的工具改成含写/命令的能力时，拒绝偷偷
    // 解除只读——要么显式改成 inherit，要么另存自定义定义。
    if scope == SubagentProfileSource::User
        && profile.id == "explore"
        && let ToolChoice::Allowlist { names } = &profile.tools
        && names.iter().any(|name| {
            matches!(name.as_str(), "write_file" | "edit" | "bash" | "job_start")
        })
        && profile.permission_ceiling.is_read_only()
    {
        return Err(ApiError::bad_request(
            "subagent/read-only-ceiling-conflict",
            "只读上限的定义不能同时授予写/命令类工具：请改为 permissionCeiling=inherit，或另存为自定义定义",
        ));
    }
    let store = state.subagents.clone();
    let expected = body.expected_revision.clone();
    let row = tokio::task::spawn_blocking(move || {
        store.write(scope, &profile, project_root.as_deref(), expected.as_deref())
    })
    .await
    .map_err(|error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "api/join", error.to_string()))?
    .map_err(map_error)?;
    Ok(Json(json!({ "profile": row })))
}

async fn delete_profile(
    State(state): State<Arc<AppState>>,
    Path(qualified_id): Path<String>,
    Query(query): Query<ScopeQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let project_root = project_root_for(&state, query.session_id.as_deref());
    let (scope, id) = split_qualified(&qualified_id).map_err(map_error)?;
    let store = state.subagents.clone();
    let restored_root = project_root.clone();
    let id_for_task = id.clone();
    let effective = tokio::task::spawn_blocking(move || {
        store.remove(scope, &id_for_task, project_root.as_deref(), None)
    })
    .await
    .map_err(|error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "api/join", error.to_string()))?
    .map_err(map_error)?;
    // 明确回报"删除后生效的是哪一份"，避免把删覆盖显示成永久消失。
    let effective = effective.or_else(|| {
        state
            .subagents
            .resolve(&id, restored_root.as_deref())
            .ok()
            .and_then(|resolved| {
                state
                    .subagents
                    .rows_for(restored_root.as_deref())
                    .iter()
                    .find(|row| row.qualified_id == resolved.qualified_id)
                    .cloned()
            })
    });
    Ok(Json(json!({ "effective": effective })))
}

/// 恢复默认：删掉该 id 在指定作用域的覆盖（内置 = 恢复程序提供的定义）。
async fn reset_profile(
    State(state): State<Arc<AppState>>,
    Path(qualified_id): Path<String>,
    Query(query): Query<ScopeQuery>,
) -> Result<impl IntoResponse, ApiError> {
    delete_profile(State(state), Path(qualified_id), Query(query)).await
}

fn split_qualified(target: &str) -> Result<(SubagentProfileSource, String), SubagentError> {
    match target.split_once(':') {
        Some(("builtin", id)) if !id.is_empty() => {
            Ok((SubagentProfileSource::Builtin, id.to_string()))
        }
        Some(("user", id)) if !id.is_empty() => Ok((SubagentProfileSource::User, id.to_string())),
        Some(("project", id)) if !id.is_empty() => {
            Ok((SubagentProfileSource::Project, id.to_string()))
        }
        _ => Err(SubagentError::new(
            "subagent/invalid-qualified-id",
            format!("需要限定 id（builtin:/user:/project:），收到：{target}"),
        )),
    }
}

/// 后端真实工具目录：来源、分类、可授予性与原因。
async fn list_tools(
    State(state): State<Arc<AppState>>,
    Query(_query): Query<ScopeQuery>,
) -> impl IntoResponse {
    let registered: Vec<(String, String)> = state
        .driver
        .tools()
        .schemas()
        .into_iter()
        .map(|schema| (schema.name, schema.description))
        .collect();
    Json(json!({ "tools": crate::subagents::catalog(&registered) }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreviewBody {
    /// 必须给会话：全局页面只能说"定义请求了什么"，只有具体父会话才能说
    /// "最终实际拿到什么"。
    session_id: String,
    #[serde(default)]
    profile_id: Option<String>,
    #[serde(default)]
    inline: Option<denia_core::subagent::InlineSubagentSpec>,
}

/// 派遣预览：不启动模型，返回该父会话下的有效工具/模型/权限与诊断。
async fn preview(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PreviewBody>,
) -> Result<impl IntoResponse, ApiError> {
    let live = state.live.get(&body.session_id).ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "session/not-found",
            "预览需要指定一个在运行中的父会话",
        )
    })?;
    let project_root = crate::subagents::project_root_of(&std::path::PathBuf::from(
        live.session.header().cwd.clone(),
    ));
    let target = match (&body.profile_id, &body.inline) {
        (Some(_), Some(_)) => {
            return Err(ApiError::bad_request(
                "subagent/ambiguous-target",
                "profileId 与 inline 只能给一个",
            ));
        }
        (Some(id), None) => denia_core::subagent::DelegateTarget::Profile(id.clone()),
        (None, Some(spec)) => denia_core::subagent::DelegateTarget::Inline(spec.clone()),
        (None, None) => denia_core::subagent::DelegateTarget::Default,
    };
    let args = denia_core::subagent::DelegateArgs {
        prompt: "preview".into(),
        description: None,
        profile_id: body.profile_id.clone(),
        inline: body.inline.clone(),
        allowed_tools: None,
        persona: None,
        max_depth: None,
        run_in_background: None,
        provider: None,
        model: None,
        reasoning_effort: None,
    };
    let definition = crate::subagents::resolve_definition(
        &state.subagents,
        &target,
        project_root.as_deref(),
        &args,
    )
    .map_err(|reason| ApiError::bad_request("subagent/preview-rejected", reason))?;
    let preset = state
        .driver
        .preset_for(live.session.agent_preset().as_deref());
    let excluded = preset
        .as_ref()
        .map(|preset| preset.features.excluded_tools())
        .unwrap_or_default();
    let parent_grant = crate::subagents::policy::parent_grant(
        &state.driver.tools().names(),
        &excluded,
        preset.as_ref().and_then(|preset| preset.tools.as_deref()),
    );
    let grant = crate::subagents::policy_grant(
        &definition.profile,
        definition.narrowing.as_deref(),
        &parent_grant,
    )
    .map_err(|reason| ApiError::bad_request("subagent/preview-rejected", reason))?;
    Ok(Json(json!({
        "profile": definition.summary(),
        "name": definition.profile.name,
        "description": definition.profile.description,
        "tools": grant.tools,
        "parentGrant": parent_grant,
        "permissionCeiling": grant.permission_ceiling,
        "model": definition.profile.explicit_model(),
        "legacy": definition.legacy,
        "diagnostics": definition.note,
    })))
}
