//! 「用应用打开」端点:已探测应用表、应用图标、启动目录。
//!
//! 启动路由的校验落在网线上:应用必须是目录里已解析到的 id,路径必须是
//! 指向已存在目录的绝对路径。

use std::sync::Arc;

use axum::extract::{Path as AxumPath, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::error::ApiError;
use crate::open_in_app::OpenOutcome;
use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/open-in-app/apps", get(apps))
        .route("/api/open-in-app/icon/{id}", get(icon))
        .route("/api/open-in-app/open", post(open))
}

/// 探测到的应用 id,保持菜单顺序;可用性是活事实,不缓存到浏览器。
async fn apps(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let apps = state.open_in_app.available_ids().await;
    Json(json!({ "apps": apps }))
}

async fn icon(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, ApiError> {
    let Some(icon) = state.open_in_app.icon(&id).await else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "open-in-app/no-icon",
            format!("no icon for app '{id}'"),
        ));
    };
    let headers = [
        (header::CONTENT_TYPE, icon.content_type.to_string()),
        (header::CACHE_CONTROL, "public, max-age=3600".to_string()),
    ];
    Ok((headers, icon.bytes).into_response())
}

#[derive(Debug, Deserialize)]
struct OpenBody {
    app: String,
    path: String,
}

async fn open(
    State(state): State<Arc<AppState>>,
    Json(body): Json<OpenBody>,
) -> Result<impl IntoResponse, ApiError> {
    let path = std::path::PathBuf::from(&body.path);
    if body.path.is_empty() || !path.is_absolute() {
        return Err(ApiError::bad_request(
            "open-in-app/bad-path",
            "path must be an absolute directory path",
        ));
    }
    let is_directory = tokio::fs::metadata(&path)
        .await
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false);
    if !is_directory {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "open-in-app/dir-not-found",
            format!("directory does not exist: {}", body.path),
        ));
    }
    match state.open_in_app.launch(&body.app, &path).await {
        OpenOutcome::Launched => Ok(Json(json!({ "ok": true }))),
        OpenOutcome::UnknownApp => Err(ApiError::bad_request(
            "open-in-app/unknown-app",
            format!("unknown or unavailable app: {}", body.app),
        )),
        OpenOutcome::Failed => Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "open-in-app/launch-failed",
            format!("failed to launch {}", body.app),
        )),
    }
}
