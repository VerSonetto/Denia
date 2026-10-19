//! QQ Bot WebSocket 网关:Hello/Identify/Heartbeat/Resume 与重连。
//!
//! 与 hermes 一致的处理:
//! - Hello(op 10) 之后发 Identify 或 Resume(有 session 时);
//! - 心跳间隔按服务端 `heartbeat_interval` 的 80%;
//! - op 7(服务端要求重连)与 op 9(会话失效)都断开重连;
//! - 断线按 2/5/10/30/60 秒退避,直到取消。

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use super::bot::QqBot;
use super::client::Client;
use super::protocol::{
    GatewayPayload, heartbeat_body, identify_body, parse_inbound, resume_body,
};
use crate::weixin::SharedStatus;

const BACKOFF_SECONDS: [u64; 5] = [2, 5, 10, 30, 60];

/// 网关主循环:连接→认证→收事件;失败退避重连,取消即退出。
pub async fn run(
    client: Arc<Client>,
    bot: Arc<QqBot>,
    status: SharedStatus,
    cancel: CancellationToken,
) {
    let mut attempt = 0usize;
    loop {
        if cancel.is_cancelled() {
            break;
        }
        match connect_once(&client, &bot, &status, &cancel).await {
            Ok(()) => attempt = 0,
            Err(error) => set_error(&status, error),
        }
        if cancel.is_cancelled() {
            break;
        }
        let delay = BACKOFF_SECONDS[attempt.min(BACKOFF_SECONDS.len() - 1)];
        attempt = (attempt + 1).min(BACKOFF_SECONDS.len());
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_secs(delay)) => {}
        }
    }
    let mut state = status.lock().unwrap_or_else(|poison| poison.into_inner());
    state.connected = false;
}

async fn connect_once(
    client: &Arc<Client>,
    bot: &Arc<QqBot>,
    status: &SharedStatus,
    cancel: &CancellationToken,
) -> Result<(), String> {
    let gateway = client.gateway_url().await?;
    let (mut socket, _) = tokio_tungstenite::connect_async(gateway.as_str())
        .await
        .map_err(|error| format!("QQ WebSocket 连接失败:{error}"))?;
    let mut ticker: Option<tokio::time::Interval> = None;
    let mut last_seq: Option<i64> = None;
    let mut session_id: Option<String> = None;
    loop {
        let tick = async {
            match ticker.as_mut() {
                Some(ticker) => {
                    ticker.tick().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = socket.close(None).await;
                return Ok(());
            }
            frame = socket.next() => {
                let Some(frame) = frame else {
                    return Ok(());
                };
                let raw = match frame {
                    Ok(Message::Text(text)) => text.to_string(),
                    Ok(Message::Binary(bytes)) => String::from_utf8_lossy(&bytes).to_string(),
                    Ok(Message::Close(_)) => return Ok(()),
                    Ok(_) => continue,
                    Err(error) => return Err(format!("QQ WebSocket 读取失败:{error}")),
                };
                let Ok(payload) = serde_json::from_str::<GatewayPayload>(&raw) else {
                    continue;
                };
                if let Some(seq) = payload.s {
                    last_seq = Some(seq);
                }
                match payload.op {
                    10 => {
                        let interval = payload
                            .d
                            .get("heartbeat_interval")
                            .and_then(Value::as_u64)
                            .unwrap_or(30_000);
                        let token = client.token().await?;
                        let body = match (session_id.as_deref(), last_seq) {
                            (Some(session), Some(seq)) => resume_body(&token, session, seq),
                            _ => identify_body(&token),
                        };
                        socket
                            .send(Message::Text(body.to_string().into()))
                            .await
                            .map_err(|error| format!("QQ WebSocket 认证发送失败:{error}"))?;
                        let mut next = tokio::time::interval(Duration::from_millis(
                            (interval as f64 * 0.8) as u64,
                        ));
                        next.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                        ticker = Some(next);
                    }
                    0 => {
                        if let Some(event_type) = payload.event_type.as_deref() {
                            match event_type {
                                "READY" => {
                                    if let Some(session) =
                                        payload.d.get("session_id").and_then(Value::as_str)
                                    {
                                        session_id = Some(session.to_string());
                                    }
                                    mark_connected(status);
                                }
                                "RESUMED" => mark_connected(status),
                                "INTERACTION_CREATE" => {
                                    let bot = bot.clone();
                                    let data = payload.d.clone();
                                    tokio::spawn(async move {
                                        bot.handle_interaction(data).await;
                                    });
                                }
                                _ => {
                                    if let Some(message) = parse_inbound(event_type, &payload.d) {
                                        let bot = bot.clone();
                                        tokio::spawn(async move {
                                            bot.handle_inbound(message).await;
                                        });
                                    }
                                }
                            }
                        }
                    }
                    7 | 9 => return Ok(()),
                    _ => {}
                }
            }
            _ = tick => {
                let body = heartbeat_body(last_seq);
                socket
                    .send(Message::Text(body.to_string().into()))
                    .await
                    .map_err(|error| format!("QQ 心跳发送失败:{error}"))?;
            }
        }
    }
}

fn mark_connected(status: &SharedStatus) {
    let mut state = status.lock().unwrap_or_else(|poison| poison.into_inner());
    state.connected = true;
    state.last_error = None;
}

fn set_error(status: &SharedStatus, error: String) {
    let mut state = status.lock().unwrap_or_else(|poison| poison.into_inner());
    state.connected = false;
    state.last_error = Some(error);
}
