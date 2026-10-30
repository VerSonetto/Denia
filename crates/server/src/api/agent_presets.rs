//! Agent preset 端点:名册、声明查看、创建(复制/手工)、编辑与删除。
//!
//! 创建有两条路径,最终落到同一个目录格式:复制(`copy`)只搬运既有目录,
//! 手工创建(`author`)接受设置页编辑器提交的完整规范;"文件就是编辑器"
//! 仍成立——用户随时可以用自己的编辑器改 `preset.yml`,名册监听文件变化
//! 后热刷新。

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use denia_core::preset::PresetSpec;
use serde::Deserialize;
use serde_json::json;

use crate::error::ApiError;
use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/agent-presets", get(list_presets).post(copy_preset))
        // 手工创建走独立路径:集合上的 POST 已被"复制创作"占用,两者提交的
        // 体形状不同(来源+新 id vs 完整规范)。静态段优先于 `{id}`,不会
        // 遮蔽同名的 GET/DELETE。
        .route("/api/agent-presets/new", post(create_preset))
        .route(
            "/api/agent-presets/{id}",
            get(get_preset).put(update_preset).delete(delete_preset),
        )
}

/// 名册快照:随附集合在前,用户集合在后;每一行都带健康状态。
/// `modeSelection` 决定前端是否显示预设选择器(关闭时空白会话固定用
/// 部署默认预设,用户保存的 default 一并失效);`tools` 是部署已注册的
/// 工具名,供设置页编辑器的工具多选使用。
async fn list_presets(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({
        "default": state.agent_presets.default_id(),
        "modeSelection": state.agent_presets.mode_selection_enabled(),
        "root": state.agent_presets.root().display().to_string(),
        "presets": state.agent_presets.rows().as_ref(),
        "tools": state.agent_presets.known_tool_names(),
    }))
}

/// 单个 preset 的声明文本与结构化规范(设置页查看器与编辑器用)。
///
/// `spec` 对 broken 行是 null:解析不出定义的 preset 不该开放编辑,但也不
/// 该因此让整个查看请求失败。
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
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "agent-preset/broken",
            reason,
        )
    })?;
    let spec = state.agent_presets.spec(&id).ok();
    Ok(Json(json!({ "preset": row, "text": text, "spec": spec })))
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

/// 复制创作:整个目录复制到用户根目录,声明文件重写元数据。
async fn copy_preset(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CopyBody>,
) -> Result<impl IntoResponse, ApiError> {
    let store = state.agent_presets.clone();
    let name = body.name.clone();
    let row =
        tokio::task::spawn_blocking(move || store.copy(&body.from, &body.id, name.as_deref()))
            .await
            .map_err(spawn_failed)?
            .map_err(|reason| ApiError::bad_request("agent-preset/copy-rejected", reason))?;
    broadcast(&state);
    Ok((StatusCode::CREATED, Json(json!({ "preset": row }))))
}

/// 手工创作:设置页编辑器"新建"的保存路径,与 `create_preset` 工具走
/// 同一个存储方法(同一套 id 占用与工具名校验)。
async fn create_preset(
    State(state): State<Arc<AppState>>,
    Json(spec): Json<PresetSpec>,
) -> Result<impl IntoResponse, ApiError> {
    let store = state.agent_presets.clone();
    let row = tokio::task::spawn_blocking(move || store.author(&spec))
        .await
        .map_err(spawn_failed)?
        .map_err(|reason| ApiError::bad_request("agent-preset/create-rejected", reason))?;
    broadcast(&state);
    Ok((StatusCode::CREATED, Json(json!({ "preset": row }))))
}

/// 覆盖编辑:只重写用户预设目录里的 `preset.yml`,附带文件原样保留。
/// 随附 preset 是部署对照基线,拒绝编辑。
async fn update_preset(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(spec): Json<PresetSpec>,
) -> Result<impl IntoResponse, ApiError> {
    let store = state.agent_presets.clone();
    let row = tokio::task::spawn_blocking(move || store.update(&id, &spec))
        .await
        .map_err(spawn_failed)?
        .map_err(|reason| {
            let code = if reason.contains("不可编辑") {
                StatusCode::CONFLICT
            } else if reason.contains("不存在") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            ApiError::new(code, "agent-preset/update-rejected", reason)
        })?;
    broadcast(&state);
    Ok(Json(json!({ "preset": row })))
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
        .map_err(spawn_failed)?
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
    broadcast(&state);
    Ok(StatusCode::NO_CONTENT)
}

/// 名册变化广播:设置页与选择器据此重拉。
fn broadcast(state: &AppState) {
    let _ = state
        .events
        .send(crate::state::ServerEvent::AgentPresetsUpdated);
}

fn spawn_failed(error: tokio::task::JoinError) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "api/join",
        error.to_string(),
    )
}

fn unknown_preset(id: &str) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "agent-preset/not-found",
        format!("未知的 preset:{id}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;

    /// 端到端走一遍设置页编辑器的三条请求:手工创建 → 读取规范 → 覆盖
    /// 编辑,确认路由、错误码与 JSON 形状和前端契约一致。
    #[tokio::test]
    async fn create_read_and_update_a_user_preset() {
        let home = std::env::temp_dir().join(format!("denia-presets-api-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let state = Arc::new(crate::state::build_state(&home, false, 0).await.unwrap());
        let app = router().with_state(state.clone());

        // 名册带 tools 名册(编辑器工具多选的数据源)。
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/agent-presets")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            value["tools"]
                .as_array()
                .is_some_and(|tools| tools.iter().any(|tool| tool == "bash")),
            "工具名册要含部署注册的工具:{value}"
        );

        // 手工创建:body 就是编辑器提交的规范。
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/agent-presets/new")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "id": "editor-made",
                            "name": "编辑器预设",
                            "description": "由设置页手工创建",
                            "persona": "你是手写预设。",
                            "tools": ["bash"],
                            "features": {"goal": false}
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["preset"]["id"], "editor-made");
        assert_eq!(value["preset"]["writable"], true);

        // 详情:结构化 spec 与声明文本同时下发。
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/agent-presets/editor-made")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["spec"]["name"], "编辑器预设");
        assert_eq!(value["spec"]["features"]["goal"], false);
        assert!(
            value["text"]
                .as_str()
                .is_some_and(|text| text.contains("编辑器预设"))
        );

        // 覆盖编辑:改名 + 省略 tools(恢复全量工具集);body 里的 id 被忽略。
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/agent-presets/editor-made")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"id": "ignored-on-purpose", "name": "编辑器预设 v2"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["preset"]["name"], "编辑器预设 v2");
        assert!(
            value["preset"]["tools"].is_null(),
            "省略 tools = 恢复全量工具集:{value}"
        );

        // 随附预设不可编辑:409 而不是静默成功。
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/agent-presets/standard")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"name": "冒充"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 409);

        // 非法 id 是 400,不落盘。
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/agent-presets/new")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"id": "Bad", "name": "非法"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 400);

        // broken 行:spec 为 null(编辑器不开放),但查看请求本身不失败。
        let broken_dir = home.join(crate::agent_presets::DIR_NAME).join("typo");
        std::fs::create_dir_all(&broken_dir).unwrap();
        std::fs::write(
            broken_dir.join(crate::agent_presets::FILE_NAME),
            "features:\n  typoKey: true\n",
        )
        .unwrap();
        state.agent_presets.refresh();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/agent-presets/typo")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(value["spec"].is_null(), "broken 行没有可编辑的规范:{value}");

        // 删除收尾。
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/agent-presets/editor-made")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 204);

        drop(app);
        drop(state);
        // 名册监听线程可能还持有目录句柄(Windows),清理失败不影响断言。
        let _ = std::fs::remove_dir_all(home);
    }
}
