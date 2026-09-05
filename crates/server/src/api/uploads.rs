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
const MAX_NAME_CHARS: usize = 120;

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
    // 会话必须已存在(上传只属于真实会话)。
    if !state.sessions.load(&session_id).is_ok() {
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

    let file_name = sanitize_name(&body.name);
    let target_dir = state.home.join("uploads").join(&session_id);
    std::fs::create_dir_all(&target_dir).map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "attachment/io",
            format!("create upload dir: {error}"),
        )
    })?;

    // 同名冲突:追加数字后缀,绝不覆盖。
    let mut target = target_dir.join(&file_name);
    let mut counter = 1usize;
    while target.exists() {
        let stem = target_dir.join(format!("{file_name}.{counter}"));
        target = stem;
        counter += 1;
    }
    std::fs::write(&target, &bytes).map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "attachment/io",
            format!("write upload: {error}"),
        )
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

fn sanitize_id(id: &str) -> Result<String, String> {
    let valid = !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    if valid {
        Ok(id.to_string())
    } else {
        Err("invalid id".to_string())
    }
}

/// 取 basename、去路径分隔符与非法字符,防路径注入与目录穿越。
fn sanitize_name(name: &str) -> String {
    let base = std::path::Path::new(name)
        .file_name()
        .map(|part| part.to_string_lossy().to_string())
        .unwrap_or_else(|| "upload.bin".to_string());
    let cleaned: String = base
        .chars()
        .filter(|ch| {
            !matches!(
                ch,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '\0'
            )
        })
        .take(MAX_NAME_CHARS)
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        "upload.bin".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_sanitized_to_a_single_component() {
        assert_eq!(sanitize_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_name("a/b\\c.txt"), "c.txt");
        assert_eq!(sanitize_name(""), "upload.bin");
        assert_eq!(sanitize_name("weird:*?name.txt"), "weirdname.txt");
    }

    #[test]
    fn ids_are_validated() {
        assert!(sanitize_id("9b7c2a6e-2f2a-4c8a-8d3c-a0f4b2e7c1d2").is_ok());
        assert!(sanitize_id("../escape").is_err());
        assert!(sanitize_id("").is_err());
    }
}
