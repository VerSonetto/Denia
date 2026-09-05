//! 内嵌浏览器 API:命令执行 + 状态 + screencast SSE。
//!
//! 面板与 `browser` 工具共用 [`AppState::browser`] 同一实例;命令面与
//! ZCode Browser Use 一致(navigate/snapshot/click/…/viewportReset)。

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use tokio_stream::wrappers::BroadcastStream;

use denia_browser::BrowserCommand;

use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/browser/state", get(browser_state))
        .route("/api/browser/command", post(browser_command))
        .route("/api/browser/stream", get(browser_stream))
}

/// GET /api/browser/state — tabs/活跃/dialog 快照。
async fn browser_state(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(state.browser.snapshot_state().await)
}

#[derive(Deserialize)]
struct CommandBody {
    command: Value,
}

/// POST /api/browser/command {command:{method:...}} — 与工具共用同一执行路径。
async fn browser_command(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CommandBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let command: BrowserCommand = serde_json::from_value(body.command)
        .map_err(|error| (StatusCode::BAD_REQUEST, format!("非法浏览器命令: {error}")))?;
    let outcome = state.browser.execute(command).await;
    let value = serde_json::to_value(&outcome).unwrap_or_else(|_| serde_json::json!({"ok": false}));
    Ok(Json(value))
}

/// GET /api/browser/stream — 浏览器事件 SSE(tabs/frame/dialog/navigated/exited)。
async fn browser_stream(
    State(state): State<Arc<AppState>>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let receiver = state.browser.subscribe();
    let stream = BroadcastStream::new(receiver).filter_map(|result| async move {
        match result {
            Ok(event) => Some(Ok(
                Event::default().data(serde_json::to_string(&event).unwrap_or_default())
            )),
            Err(_) => None,
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
