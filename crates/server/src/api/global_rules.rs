//! 全局规则读写:`<home>/AGENTS.md`。
//!
//! 与工作区内的 `AGENTS.md` 是同一套发现机制里的两份文件(见
//! `crate::workspace_instructions`):全局那份作用域最宽、最先注入,
//! 模型侧标签为 `~/AGENTS.md`。这里只负责把它暴露成可编辑的资源,
//! 不参与注入逻辑。

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::state::AppState;

/// 全局规则文件名(与工作区指令发现逻辑共用固定名)。
pub const FILE_NAME: &str = "AGENTS.md";

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route(
        "/api/global-rules",
        get(get_global_rules).put(put_global_rules),
    )
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GlobalRulesView {
    text: String,
    /// `file` = 已自定义;`default` = 未设置(文件缺失或为空)。
    source: &'static str,
    /// 模型可见的来源标签(符号路径,不暴露真实 home)。
    display_path: String,
    path: String,
}

#[derive(Debug, Deserialize)]
struct PutBody {
    text: String,
}

fn file_path(state: &AppState) -> PathBuf {
    state.home.join(FILE_NAME)
}

fn view_of(state: &AppState) -> Result<GlobalRulesView, ApiError> {
    let path = file_path(state);
    // 与注入侧一致:空白文件等同未设置。
    let text = match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => text,
        _ => String::new(),
    };
    Ok(GlobalRulesView {
        source: if text.is_empty() { "default" } else { "file" },
        text,
        display_path: "~/AGENTS.md".to_string(),
        path: path.display().to_string(),
    })
}

async fn get_global_rules(State(state): State<Arc<AppState>>) -> Result<impl axum::response::IntoResponse, ApiError> {
    Ok(Json(view_of(&state)?))
}

async fn put_global_rules(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PutBody>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    let path = file_path(&state);
    // 清空 = 删除文件,回到"未设置",不留空文件占位。
    let result = if body.text.trim().is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            // 文件本就不存在:幂等成功。
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    } else {
        std::fs::write(&path, &body.text)
    };
    result.map_err(|error| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "global-rules/io",
            error.to_string(),
        )
    })?;
    Ok(Json(view_of(&state)?))
}
