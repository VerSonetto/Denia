//! 附件上传端点:`POST /api/attachments`。
//!
//! 文件(不限格式)以 base64 JSON 上传,持久化到
//! `<home>/uploads/<session_id>/<sanitized-name>`,返回绝对路径;
//! 发送消息时以 `files` 引用,模型可通过 read_file 等工具读取。

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::error::ApiError;
use crate::state::AppState;

/// 原始文件大小上限(50MB;base64 传输约 67MB)。
const MAX_FILE_BYTES: usize = 50 * 1024 * 1024;

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/api/attachments", post(upload_attachment))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadBody {
    /// 归属会话(存储目录名;校验为可用的会话 id)。
    session_id: String,
    /// 原始文件名(仅取 basename 并消毒)。
    name: String,
    #[serde(default)]
    mime: String,
    /// base64 编码的文件内容。
    data: String,
}

async fn upload_attachment(
    State(state): State<Arc<AppState>>,
    Json(body): Json<UploadBody>,
) -> Result<impl IntoResponse, ApiError> {
    let Ok(session_id) = sanitize_id(&body.session_id) else {
        return Err(ApiError::bad_request(
            "attachment/bad-session",
            "session id is not usable",
        ));
    };
    // 会话必须已存在(上传只属于真实会话)。只看文件在不在:完整 load
    // 会把整份日志解析一遍还会写盘,与这一句判断的代价完全不成比例。
    if !state.sessions.exists(&session_id) {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "session/not-found",
            "session not found",
        ));
    }
    let raw_len_estimate = body.data.len().saturating_mul(3) / 4;
    if raw_len_estimate > MAX_FILE_BYTES {
        return Err(ApiError::bad_request(
            "attachment/too-large",
            format!("file exceeds the {}-byte upload cap", MAX_FILE_BYTES),
        ));
    }
    let bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        body.data.as_bytes(),
    )
    .map_err(|_| {
        ApiError::bad_request("attachment/bad-base64", "file payload is not valid base64")
    })?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(ApiError::bad_request(
            "attachment/too-large",
            format!("file exceeds the {}-byte upload cap", MAX_FILE_BYTES),
        ));
    }

    let (file_name, target) =
        write_upload(&state.home, &session_id, &body.name, &bytes).map_err(|error| {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "attachment/io", error)
        })?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "path": target.to_string_lossy(),
            "name": file_name,
            "bytes": bytes.len(),
        })),
    ))
}

pub(crate) use crate::infrastructure::uploads::{
    remove_session_uploads, sanitize_id, write_upload,
};
