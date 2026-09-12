//! 项目记忆端点(设置页只读 viewer):列工作区记忆清单、读单个记忆文件。
//!
//! 只读,不做增删改——改记忆走模型或手改文件。安全口径:文件名按组件
//! 校验(拒绝 `..`/前缀/根)、拒绝符号链接、单文件 5MiB 上限。

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::error::ApiError;
use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/memories/projects", get(list_projects))
        .route("/api/memories/projects/{id}/{*file}", get(read_memory_file))
}

/// 列出全部工作区的记忆清单(未启用过记忆的工作区 files 为空,不报错)。
async fn list_projects(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let workspaces = state.workspaces.list();
    let mut projects = Vec::with_capacity(workspaces.len());
    for workspace in workspaces {
        let home = state.home.clone();
        let path = std::path::PathBuf::from(workspace.path.clone());
        // fs 阻塞调用走 spawn_blocking(设计哲学:不挡异步运行时)。
        let manifest = tokio::task::spawn_blocking(move || {
            crate::project_memory::scan_manifest(&crate::project_memory::memory_root(
                &home, &path,
            ))
        })
        .await
        .unwrap_or_else(|_| crate::project_memory::ProjectMemoryManifest {
            root: String::new(),
            index: None,
            files: Vec::new(),
            updated_at: 0,
        });
        projects.push(json!({
            "id": workspace.id,
            "title": workspace.title,
            "path": workspace.path,
            "updatedAt": manifest.updated_at,
            "root": manifest.root,
            "index": manifest.index,
            "files": manifest.files,
        }));
    }
    Json(json!({ "projects": projects }))
}

/// 读取一个记忆文件。路径段里的 `/` 分隔的相对路径(axum 通配捕获);
/// 三种失败形态分别给码:非法路径 400、不存在/读取期被删 404、超限 413。
async fn read_memory_file(
    State(state): State<Arc<AppState>>,
    Path((id, file)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    let workspace = state.workspaces.get(&id).ok_or_else(|| {
        ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "memory/workspace-not-found",
            "workspace not found",
        )
    })?;
    let root =
        crate::project_memory::memory_root(&state.home, std::path::Path::new(&workspace.path));
    let raw = file.trim().to_string();
    let root_for_resolve = root.clone();
    let path = tokio::task::spawn_blocking(move || {
        crate::project_memory::resolve_manifest_file(&root_for_resolve, &raw)
    })
    .await
    .map_err(|e| ApiError::bad_request("memory/read-failed", e.to_string()))?
    .map_err(|message| {
        if message.contains("不存在") {
            ApiError::new(
                axum::http::StatusCode::NOT_FOUND,
                "memory/file-not-found",
                message,
            )
        } else {
            ApiError::bad_request("memory/bad-path", message)
        }
    })?;
    let metadata = tokio::task::spawn_blocking({
        let path = path.clone();
        move || std::fs::metadata(&path)
    })
    .await
    .map_err(|e| ApiError::bad_request("memory/read-failed", e.to_string()))?
    .map_err(io_error)?;
    if metadata.len() > crate::project_memory::MAX_FILE_BYTES {
        return Err(ApiError::new(
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            "memory/file-too-large",
            format!(
                "记忆文件超过 {} MiB 读取上限,请在磁盘上直接查看",
                crate::project_memory::MAX_FILE_BYTES / (1024 * 1024)
            ),
        ));
    }
    let content = tokio::task::spawn_blocking({
        let path = path.clone();
        move || std::fs::read_to_string(&path)
    })
    .await
    .map_err(|e| ApiError::bad_request("memory/read-failed", e.to_string()))?
    .map_err(io_error)?;
    let name = path
        .strip_prefix(&root)
        .unwrap_or(&path)
        .to_string_lossy()
        .replace('\\', "/");
    Ok(Json(json!({
        "name": name,
        "content": content,
        "size": metadata.len(),
        "modifiedMs": metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    })))
}

/// 读取期的 fs 错误:文件消失按 404(读取期变更提示的素材),其余 500。
fn io_error(error: std::io::Error) -> ApiError {
    if error.kind() == std::io::ErrorKind::NotFound {
        ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "memory/file-not-found",
            "读取期间文件已被删除或移动,请刷新列表",
        )
    } else {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "memory/read-failed",
            error.to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::sync::Arc;

    /// 只读记忆 API:清单按工作区分桶、非 md 不进清单、单文件可读、
    /// 缺失 404、非 md 400。越界与符号链接拒绝由 resolve_manifest_file
    /// 的单测覆盖。
    #[tokio::test]
    async fn memory_api_lists_projects_and_reads_files() {
        let home = std::env::temp_dir().join(format!("denia-mem-api-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let state = Arc::new(crate::state::build_state(&home, false).await.unwrap());
        let ws_dir = home.join("ws");
        std::fs::create_dir_all(&ws_dir).unwrap();
        let record = state
            .workspaces
            .create(&ws_dir, None)
            .map_err(|e| e.to_string())
            .unwrap();
        let root = crate::project_memory::memory_root(&home, &ws_dir);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("MEMORY.md"),
            "- [偏好](user-prefs.md) — 用户偏好 pnpm\n",
        )
        .unwrap();
        std::fs::write(
            root.join("user-prefs.md"),
            "---\nname: user-prefs\ndescription: 用户偏好\n---\n正文\n",
        )
        .unwrap();
        std::fs::write(root.join("notes.txt"), "非 md 不进清单").unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = crate::api::router().with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let client = reqwest::Client::new();

        let projects: Value = client
            .get(format!("{base}/api/memories/projects"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let mine = projects["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|project| project["id"] == record.id.as_str())
            .expect("新建工作区必须出现在记忆清单里");
        let files = mine["files"].as_array().unwrap();
        assert_eq!(files.len(), 1, "非 md 文件不进清单:{files:?}");
        assert_eq!(files[0]["name"], "user-prefs.md");
        assert!(mine["index"].is_object(), "索引条目应带 size/mtime");
        assert_eq!(mine["index"]["name"], "MEMORY.md");
        assert!(mine["updatedAt"].as_u64().unwrap() > 0);

        let file: Value = client
            .get(format!(
                "{base}/api/memories/projects/{}/user-prefs.md",
                record.id
            ))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(file["name"], "user-prefs.md");
        assert!(file["content"].as_str().unwrap().contains("正文"));
        assert_eq!(file["modifiedMs"], files[0]["modifiedMs"]);

        // 缺失 404;非 md 400。
        let status = client
            .get(format!(
                "{base}/api/memories/projects/{}/missing.md",
                record.id
            ))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
        let status = client
            .get(format!(
                "{base}/api/memories/projects/{}/notes.txt",
                record.id
            ))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        let status = client
            .get(format!("{base}/api/memories/projects/missing-id/a.md"))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
        server.abort();
        std::fs::remove_dir_all(home).unwrap();
    }
}
