//! MCP 管理端点:读取快照、增删改服务器、工具级开关、重连。
//!
//! 写路径统一两步:先写 settings(带 OCC revision,fail loud),
//! 成功后重连并同步工具面,最后广播 `mcp-updated` 让其它页面刷新。
//! 配置非法(重名 id / 空 command / 非 stdio 传输…)由 settings 的
//! validate 拒绝,这里只负责把错误原样回给 UI。

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use denia_mcp::McpServerConfig;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::mcp_runtime::McpRuntime;
use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/mcp", get(describe))
        .route("/api/mcp/servers", put(upsert_server))
        .route(
            "/api/mcp/servers/{id}",
            delete(remove_server).post(action_server),
        )
        .route("/api/mcp/tools", post(toggle_tool))
}

/// 全量快照:服务器状态 + 工具清单(env 值已脱敏,只回传 key)。
async fn describe(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let snapshot = state.mcp.manager().snapshot();
    Json(json!({
        "servers": &snapshot.servers,
        "toolCount": snapshot.tool_defs().len(),
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerBody {
    /// 目标服务器(整条替换;id 决定身份)。
    server: McpServerConfig,
    /// OCC:写入 settings 用。省略时按当前 revision 写入。
    #[serde(default)]
    expected_revision: Option<u64>,
}

async fn upsert_server(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ServerBody>,
) -> Result<impl IntoResponse, ApiError> {
    let revision = state
        .settings
        .describe()
        .into_iter()
        .find(|view| view.ns == crate::mcp_settings::MCP_NS)
        .map(|view| view.revision);
    let expected = body.expected_revision.or(revision);
    let mut config = state.mcp.config();
    // 同名替换,否则追加;顺序保持不变(配置顺序 = UI 展示顺序)。
    match config
        .servers
        .iter_mut()
        .find(|server| server.id == body.server.id)
    {
        Some(existing) => *existing = body.server,
        None => config.servers.push(body.server),
    }
    write_settings(&state, &config, expected).await?;
    state.mcp.apply().await;
    state.announce_mcp_updated();
    Ok(Json(describe_body(&state).await))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActionBody {
    /// `enable` | `disable` | `reconnect`。
    action: String,
}

async fn action_server(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<ActionBody>,
) -> Result<impl IntoResponse, ApiError> {
    match body.action.as_str() {
        "reconnect" => {
            state.mcp.reconnect(&id).await;
        }
        "enable" | "disable" => {
            let enable = body.action == "enable";
            let mut config = state.mcp.config();
            let server = config.servers.iter_mut().find(|server| server.id == id);
            let Some(server) = server else {
                return Err(ApiError::new(
                    axum::http::StatusCode::NOT_FOUND,
                    "mcp/unknown-server",
                    format!("未知的 MCP 服务器:{id}"),
                ));
            };
            server.enabled = enable;
            let value = serde_json::to_value(&config).map_err(|error| {
                ApiError::new(
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "mcp/serialize-failed",
                    error.to_string(),
                )
            })?;
            state
                .settings
                .replace(crate::mcp_settings::MCP_NS, value, current_revision(&state))
                .map_err(ApiError::from_settings)?;
            state.mcp.apply().await;
        }
        other => {
            return Err(ApiError::bad_request(
                "mcp/unknown-action",
                format!("未知的 action:{other}(可用:enable / disable / reconnect)"),
            ));
        }
    }
    state.announce_mcp_updated();
    Ok(Json(describe_body(&state).await))
}

async fn remove_server(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let mut config = state.mcp.config();
    let before = config.servers.len();
    config.servers.retain(|server| server.id != id);
    if config.servers.len() == before {
        return Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "mcp/unknown-server",
            format!("未知的 MCP 服务器:{id}"),
        ));
    }
    write_settings(&state, &config, current_revision(&state)).await?;
    state.mcp.apply().await;
    state.announce_mcp_updated();
    Ok(Json(describe_body(&state).await))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolBody {
    server: String,
    /// 服务器原始工具名(不是修饰名)。
    tool: String,
    enabled: bool,
}

/// 工具级开关:关掉的工具不进模型可见面(服务器仍连着)。
async fn toggle_tool(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ToolBody>,
) -> Result<impl IntoResponse, ApiError> {
    let mut config = state.mcp.config();
    let Some(server) = config
        .servers
        .iter_mut()
        .find(|server| server.id == body.server)
    else {
        return Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "mcp/unknown-server",
            format!("未知的 MCP 服务器:{}", body.server),
        ));
    };
    if body.enabled {
        server.disabled_tools.retain(|tool| tool != &body.tool);
    } else if !server.disabled_tools.contains(&body.tool) {
        server.disabled_tools.push(body.tool);
    }
    write_settings(&state, &config, current_revision(&state)).await?;
    // 工具开关只改过滤:不重连子进程,只重建工具面。
    state.mcp.apply().await;
    state.announce_mcp_updated();
    Ok(Json(describe_body(&state).await))
}

/// 当前 `mcp` 命名空间的 revision(OCC 写入用)。
fn current_revision(state: &AppState) -> Option<u64> {
    state
        .settings
        .describe()
        .into_iter()
        .find(|view| view.ns == crate::mcp_settings::MCP_NS)
        .map(|view| view.revision)
}

/// 写 settings(带 OCC),失败原样回给 UI(点名字段的中文原因)。
async fn write_settings(
    state: &AppState,
    config: &crate::mcp_settings::McpSettings,
    expected: Option<u64>,
) -> Result<(), ApiError> {
    let value = serde_json::to_value(config).map_err(|error| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "mcp/serialize-failed",
            error.to_string(),
        )
    })?;
    state
        .settings
        .replace(crate::mcp_settings::MCP_NS, value, expected)
        .map_err(ApiError::from_settings)?;
    Ok(())
}

/// 组装 describe 响应(与 `describe` 端点同形)。
async fn describe_body(state: &AppState) -> Value {
    let snapshot = state.mcp.manager().snapshot();
    json!({
        "servers": &snapshot.servers,
        "toolCount": snapshot.tool_defs().len(),
    })
}

/// 供前端类型参考(未在 Rust 侧使用,保持 API 形状可文档化)。
#[allow(dead_code)]
#[derive(Debug, Serialize)]
struct McpDescribeResponse {
    servers: Vec<denia_mcp::McpServerState>,
    tool_count: usize,
}

/// 编译期锚定:`McpRuntime` 是本端点的唯一入口,防止重构后漏改。
#[allow(dead_code)]
fn _runtime_witness(_runtime: &McpRuntime) {}
