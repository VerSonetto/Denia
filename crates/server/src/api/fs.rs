//! Server-side directory browsing for the workspace picker.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::error::ApiError;
use crate::state::AppState;

const MAX_ENTRIES: usize = 500;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/fs/dirs", get(list_dirs))
        .route("/api/fs/pick", post(pick_dir))
        .route("/api/fs/mkdir", post(mkdir))
        .route("/api/fs/capability", get(capability))
}

/// 目录选择器 seam 的能力决策(抄 dsh directory-picker-auto):
/// 远程绑定/SSH → browse;win/mac → native;linux 有显示+zenity → native;
/// 其余 → browse(到处能用)。消费端遇到未知 kind 的默认行为是隐藏入口。
async fn capability(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let kind = if state.bound_remote
        || std::env::var_os("SSH_CONNECTION").is_some()
        || std::env::var_os("SSH_TTY").is_some()
    {
        "browse"
    } else if cfg!(windows) || cfg!(target_os = "macos") {
        "native"
    } else if cfg!(target_os = "linux") {
        let has_display =
            std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
        let has_tool = which("zenity") || which("kdialog");
        if has_display && has_tool {
            "native"
        } else {
            "browse"
        }
    } else {
        "browse"
    };
    Json(json!({ "kind": kind }))
}

fn which(tool: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(tool).exists()))
        .unwrap_or(false)
}

/// browse 能力的建文件夹原语(抄 dsh browse seam 的 createDirectory)。
async fn mkdir(Json(body): Json<MkdirBody>) -> Result<impl IntoResponse, ApiError> {
    let parent = std::path::PathBuf::from(body.path.trim());
    let name = body.name.trim();
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return Err(ApiError::bad_request(
            "directory-picker/create-failed",
            "invalid folder name",
        ));
    }
    let target = parent.join(name);
    if target.exists() {
        return Err(ApiError::bad_request(
            "directory-picker/exists",
            format!("'{}' 已存在", target.display()),
        ));
    }
    std::fs::create_dir_all(&target).map_err(|e| {
        ApiError::bad_request(
            "directory-picker/create-failed",
            format!("create failed: {e}"),
        )
    })?;
    Ok(Json(json!({ "path": target.to_string_lossy() })))
}

#[derive(Debug, serde::Deserialize)]
struct MkdirBody {
    path: String,
    name: String,
}

/// Opens the OS-native directory chooser on the host and returns the picked
/// path, or `null` when the user cancels. Blocks only a worker thread while
/// the dialog is open.
async fn pick_dir() -> Result<impl IntoResponse, ApiError> {
    let picked = tokio::task::spawn_blocking(|| {
        rfd::FileDialog::new()
            .set_title("denia: choose a workspace directory")
            .pick_folder()
    })
    .await
    .map_err(|e| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "fs/pick-failed",
            e.to_string(),
        )
    })?;
    Ok(Json(json!({
        "path": picked.map(|p| p.to_string_lossy().to_string()),
    })))
}

#[derive(Debug, Deserialize)]
struct BrowseQuery {
    #[serde(default)]
    path: Option<String>,
}

fn default_root() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

async fn list_dirs(Query(query): Query<BrowseQuery>) -> Result<impl IntoResponse, ApiError> {
    tokio::task::spawn_blocking(move || browse_impl(query.path))
        .await
        .map_err(|e| {
            ApiError::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "fs/browse-failed",
                e.to_string(),
            )
        })?
}

fn browse_impl(path: Option<String>) -> Result<impl IntoResponse, ApiError> {
    let base = match path.filter(|p| !p.trim().is_empty()) {
        Some(raw) => PathBuf::from(raw.trim()),
        None => default_root(),
    };
    if !base.is_dir() {
        return Err(ApiError::bad_request(
            "fs/not-a-directory",
            format!("'{}' is not a directory", base.display()),
        ));
    }
    let read = std::fs::read_dir(&base).map_err(|e| {
        ApiError::bad_request("fs/unreadable", format!("cannot read directory: {e}"))
    })?;
    let mut dirs: Vec<String> = Vec::new();
    for entry in read.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            dirs.push(entry.file_name().to_string_lossy().to_string());
        }
        if dirs.len() >= MAX_ENTRIES {
            break;
        }
    }
    // At a filesystem root on Windows, offer the other drives so the picker
    // can cross from C: to D: and friends.
    let parent = base.parent().map(|p| p.to_string_lossy().to_string());
    if parent.is_none() && cfg!(windows) {
        dirs = (b'A'..=b'Z')
            .map(|letter| format!("{}:\\", letter as char))
            .filter(|root| PathBuf::from(root.as_str()).is_dir())
            .collect();
    }
    dirs.sort_by_key(|name| name.to_lowercase());
    Ok(Json(json!({
        "path": base.to_string_lossy().to_string(),
        "parent": parent,
        "dirs": dirs,
    })))
}
