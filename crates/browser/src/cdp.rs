//! CDP WebSocket 客户端:单条连接多路复用所有 target 会话。
//!
//! 协议与 ZCode 的 `WebSocketCdpTransport`+flatten sessions 一致:
//! - 请求:`{id, method, params, sessionId?}`
//! - 响应:`{id, result?/error?}`
//! - 事件:`{method, params, sessionId?}`

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

/// 一条 CDP 事件(method + params + 可选 sessionId)。
#[derive(Debug, Clone)]
pub struct CdpEvent {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

struct Pending {
    sender: oneshot::Sender<Result<Value, String>>,
}

#[derive(Clone)]
pub struct CdpHandle {
    next_id: Arc<AtomicU64>,
    pending: Arc<Mutex<HashMap<u64, Pending>>>,
    request_tx: mpsc::UnboundedSender<String>,
    closed: Arc<tokio::sync::Notify>,
    closed_flag: Arc<std::sync::atomic::AtomicBool>,
}

impl CdpHandle {
    /// 连接并启动读循环。连接断开后所有在途请求立即失败。
    pub async fn connect(url: &str) -> Result<(Self, mpsc::UnboundedReceiver<CdpEvent>), String> {
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|error| format!("CDP WebSocket 连接失败({url}): {error}"))?;
        let (mut sink, mut stream) = ws.split();

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (request_tx, mut request_rx) = mpsc::unbounded_channel::<String>();

        let next_id = Arc::new(AtomicU64::new(1));
        let pending: Arc<Mutex<HashMap<u64, Pending>>> = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(tokio::sync::Notify::new());
        let closed_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // 写任务:把序列化好的 JSON 文本发进 socket。
        tokio::spawn(async move {
            while let Some(text) = request_rx.recv().await {
                if sink.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        // 读任务:分发响应/事件。
        let pending_reader = pending.clone();
        let closed_reader = closed.clone();
        let closed_flag_reader = closed_flag.clone();
        let event_tx_reader = event_tx.clone();
        tokio::spawn(async move {
            while let Some(item) = stream.next().await {
                let message = match item {
                    Ok(message) => message,
                    Err(_) => break,
                };
                let text = match message {
                    Message::Text(text) => text,
                    Message::Ping(_) | Message::Pong(_) | Message::Binary(_) => continue,
                    Message::Close(_) => break,
                    Message::Frame(_) => continue,
                };
                let value: Value = match serde_json::from_str(&text) {
                    Ok(value) => value,
                    Err(_) => continue,
                };
                if let Some(id) = value.get("id").and_then(Value::as_u64) {
                    let pending_item = pending_reader.lock().unwrap().remove(&id);
                    if let Some(Pending { sender }) = pending_item {
                        let outcome = if let Some(error) = value.get("error") {
                            let message = error
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("cdp_error")
                                .to_string();
                            Err(message)
                        } else {
                            Ok(value.get("result").cloned().unwrap_or(Value::Null))
                        };
                        let _ = sender.send(outcome);
                    }
                    continue;
                }
                if let Some(method) = value.get("method").and_then(Value::as_str) {
                    let event = CdpEvent {
                        method: method.to_string(),
                        params: value.get("params").cloned().unwrap_or(Value::Null),
                        session_id: value
                            .get("sessionId")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    };
                    let _ = event_tx_reader.send(event);
                }
            }
            closed_flag_reader.store(true, Ordering::SeqCst);
            // 冲掉所有在途请求。
            let mut guard = pending_reader.lock().unwrap();
            for (_, Pending { sender }) in guard.drain() {
                let _ = sender.send(Err("cdp_disconnected".to_string()));
            }
            drop(guard);
            closed_reader.notify_waiters();
        });

        Ok((
            Self {
                next_id,
                pending,
                request_tx,
                closed,
                closed_flag,
            },
            event_rx,
        ))
    }

    /// 发送一条 CDP 命令,等待结果(默认 120s 超时)。
    pub async fn send(&self, method: &str, params: Value) -> Result<Value, String> {
        self.send_with_session(method, params, None).await
    }

    /// 发送一条挂在指定 target 会话上的 CDP 命令。
    pub async fn send_with_session(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut request = json!({ "id": id, "method": method, "params": params });
        if let Some(session_id) = session_id {
            request["sessionId"] = Value::String(session_id.to_string());
        }
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(id, Pending { sender: tx });
        let text = serde_json::to_string(&request).map_err(|e| e.to_string())?;
        self.request_tx
            .send(text)
            .map_err(|_| "cdp_writer_closed".to_string())?;
        match tokio::time::timeout(Duration::from_secs(120), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("cdp_caller_dropped".to_string()),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(format!("cdp_timeout:{method}"))
            }
        }
    }

    /// 连接是否已断开。
    pub fn is_closed(&self) -> bool {
        self.closed_flag.load(Ordering::SeqCst)
    }

    /// 等待连接断开。
    pub async fn closed(&self) {
        if self.closed_flag.load(Ordering::SeqCst) {
            return;
        }
        self.closed.notified().await;
    }
}
