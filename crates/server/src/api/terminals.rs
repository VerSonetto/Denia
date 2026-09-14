//! 终端 API:PTY 会话生命周期(REST)+ 交互数据通道(WebSocket)。
//!
//! # 为什么交互数据走 WebSocket 而不是 SSE
//!
//! SSE 是**单向**的:服务端推、客户端只能另开 REST 发键盘输入。终端每敲一个
//! 键就要一次 HTTP 往返,在中文输入法、`vim` 的 `hjkl` 连按、方向键重复这些
//! 场景下延迟叠加得很明显;而 WebSocket 一条连接双向复用,按键直接进管道。
//!
//! 所以分工是:
//! - **REST** 管生命周期:创建/列出/关闭/尺寸(低频、需要明确成功失败);
//! - **WebSocket** 管数据:键盘输入、粘贴、输出流、退出通知(高频、双向)。
//!
//! 这也和 ZCode 的分工一致:它走 Electron IPC 双向通道,这里用 WebSocket
//! 承担同一角色。

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};

use denia_terminal::{TerminalEvent, base64_decode};

use crate::error::ApiError;
use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/terminals", get(list).post(create))
        .route("/api/terminals/{id}", axum::routing::delete(close))
        .route("/api/terminals/{id}/resize", post(resize))
        .route("/api/terminals/{id}/ws", get(attach))
}

fn error(message: String) -> ApiError {
    ApiError::bad_request("terminal/operation-failed", message)
}

/// GET /api/terminals — 终端列表快照。
async fn list(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(serde_json::to_value(state.terminals.snapshot()).unwrap_or_else(|_| json!({"terminals": []})))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateBody {
    /// 工作目录;缺省用当前 home。
    cwd: Option<String>,
    /// 前端 `fit()` 预算好的列数:PTY 一开始就是正确宽度,避免首屏折行。
    cols: Option<u16>,
    rows: Option<u16>,
}

/// POST /api/terminals — 新建终端。
async fn create(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateBody>,
) -> Result<Json<Value>, ApiError> {
    let cwd = match body.cwd.as_deref() {
        Some(raw) if !raw.trim().is_empty() => std::path::PathBuf::from(raw),
        // 不传 cwd 时落到 home:终端总得有个起点,不能因为前端漏传就拒绝。
        _ => state.home.clone(),
    };
    let info = state
        .terminals
        .create(&cwd, body.cols, body.rows)
        .map_err(error)?;
    Ok(Json(json!({ "terminal": info })))
}

/// DELETE /api/terminals/{id} — 关闭终端(杀进程)。
async fn close(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    state.terminals.close(&id).map_err(error)?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct ResizeBody {
    cols: u16,
    rows: u16,
}

/// POST /api/terminals/{id}/resize — 同步尺寸。
async fn resize(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<ResizeBody>,
) -> Result<Json<Value>, ApiError> {
    state
        .terminals
        .resize(&id, body.cols, body.rows)
        .await
        .map_err(error)?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct AttachQuery {
    /// `1` 时先补发服务端回滚缓冲(刷新/重连后补齐历史)。
    #[serde(default)]
    replay: Option<String>,
}

/// GET /api/terminals/{id}/ws — 交互数据通道。
///
/// ## 协议
///
/// 客户端 → 服务端(JSON 文本帧):
/// - `{"type":"input","data":"<base64>"}` — 键盘输入/粘贴,原样写进 PTY
/// - `{"type":"resize","cols":N,"rows":N}` — 尺寸变化
/// - `{"type":"ping"}` — 心跳
///
/// 服务端 → 客户端(JSON 文本帧):
/// - `{"type":"data","terminalId":"...","data":"<base64>"}` — PTY 输出
/// - `{"type":"exit","terminalId":"...","exitCode":N}` — 进程退出
/// - `{"type":"replay","data":"<base64>"}` — 回滚缓冲(仅 `replay=1` 时首发)
/// - `{"type":"error","message":"..."}`
///
/// 输出走 base64 而不是原始文本:**PTY 字节流不保证是合法 UTF-8**
/// (程序可能分两次写一个多字节字符,或直接吐二进制)。若按文本帧发,
/// 每个分片都会被强制 UTF-8 校验,半个汉字就会让整条连接断开。
async fn attach(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<AttachQuery>,
    upgrade: WebSocketUpgrade,
) -> impl IntoResponse {
    upgrade.on_upgrade(move |socket| handle_socket(state, id, query, socket))
}

async fn handle_socket(
    state: Arc<AppState>,
    id: String,
    query: AttachQuery,
    socket: WebSocket,
) {
    if state.terminals.get(&id).is_none() {
        let (mut sink, _) = socket.split();
        let _ = sink
            .send(Message::Text(
                json!({"type":"error","message":format!("终端不存在:{id}")})
                    .to_string()
                    .into(),
            ))
            .await;
        return;
    }

    let (mut sink, mut stream) = socket.split();

    // 先订阅再补发回滚缓冲:两步之间产生的输出既在缓冲里、也会走事件,
    // 顺序上"缓冲在前、增量在后",不会丢也不会重。
    let mut events = state.terminals.subscribe();

    if query.replay.as_deref() == Some("1") {
        if let Some(bytes) = state.terminals.scrollback(&id) {
            if !bytes.is_empty() {
                let payload = json!({
                    "type": "replay",
                    "data": base64_encode(&bytes),
                });
                if sink.send(Message::Text(payload.to_string().into())).await.is_err() {
                    return;
                }
            }
        }
    }

    // 输出泵:广播 → WebSocket。只转发本终端的事件。
    //
    // 注意 `expected_id` 与 match 里解构出的 `terminal_id` 是**两个**名字:
    // 若都叫 `terminal_id`,内层绑定会遮蔽外层,比较就变成"自己等于自己",
    // 所有终端的事件都会被转发给每一条连接(串台)。
    let expected_id = id.clone();
    let mut send_task = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    let relevant = match &event {
                        TerminalEvent::Data { terminal_id, .. } => terminal_id == &expected_id,
                        TerminalEvent::Exit { terminal_id, .. } => terminal_id == &expected_id,
                        TerminalEvent::Closed { terminal_id, .. } => terminal_id == &expected_id,
                        TerminalEvent::TerminalsChanged => false,
                    };
                    if !relevant {
                        continue;
                    }
                    let is_terminal = matches!(
                        event,
                        TerminalEvent::Exit { .. } | TerminalEvent::Closed { .. }
                    );
                    let text = serde_json::to_string(&event).unwrap_or_default();
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                    // 退出/关闭后连接没有继续存在的意义。
                    if is_terminal {
                        break;
                    }
                }
                // Lagged:高吞吐下丢掉一批帧。终端输出丢帧会花屏,所以这里
                // 主动断开让前端重连(重连会 replay 完整缓冲,画面自愈),
                // 而不是像普通事件流那样静默跳过。
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(terminal_id = %expected_id, skipped, "terminal ws lagged; closing for replay");
                    let _ = sink
                        .send(Message::Text(
                            json!({"type":"error","message":"输出过快,连接已重置;正在重连补齐画面"})
                                .to_string()
                                .into(),
                        ))
                        .await;
                    break;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // 输入泵:WebSocket → PTY。
    let state_for_input = state.clone();
    let terminal_id = id.clone();
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(message)) = stream.next().await {
            match message {
                Message::Text(text) => {
                    let Ok(value) = serde_json::from_str::<Value>(&text) else {
                        continue;
                    };
                    match value.get("type").and_then(Value::as_str) {
                        Some("input") => {
                            let Some(raw) = value.get("data").and_then(Value::as_str) else {
                                continue;
                            };
                            match base64_decode(raw) {
                                Ok(bytes) => {
                                    if let Err(error) =
                                        state_for_input.terminals.write(&terminal_id, bytes).await
                                    {
                                        tracing::debug!(terminal_id = %terminal_id, %error, "terminal write failed");
                                        break;
                                    }
                                }
                                Err(error) => {
                                    tracing::debug!(terminal_id = %terminal_id, %error, "bad terminal input");
                                }
                            }
                        }
                        Some("resize") => {
                            let cols = value.get("cols").and_then(Value::as_u64).unwrap_or(80) as u16;
                            let rows = value.get("rows").and_then(Value::as_u64).unwrap_or(24) as u16;
                            if let Err(error) =
                                state_for_input.terminals.resize(&terminal_id, cols, rows).await
                            {
                                tracing::debug!(terminal_id = %terminal_id, %error, "terminal resize failed");
                            }
                        }
                        Some("ping") => {}
                        _ => {}
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    // 任一方向结束就收敛另一方向:否则用户关掉标签后输入泵还挂在 PTY 上。
    tokio::select! {
        _ = &mut send_task => recv_task.abort(),
        _ = &mut recv_task => send_task.abort(),
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
