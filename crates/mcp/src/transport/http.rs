//! HTTP 传输(Streamable HTTP):POST 一个 JSON-RPC,响应直接回。
//!
//! 规范要点:
//! - 请求头带 `Accept: application/json, text/event-stream` —— 服务端可以
//!   回 JSON,也可以回 SSE 流(比如把一批通知塞进流里),两种都要认;
//! - 服务端可能在首个响应里给 `mcp-session-id`,后续请求必须带回,
//!   否则无状态服务端认不出会话;
//! - 通知走同样的 POST,只是报文没有 `id`,服务端回 202 且无正文。

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::client::McpClientError;
use crate::protocol::{JsonRpcRequest, PROTOCOL_VERSION};
use crate::transport::headers;
use crate::transport::{McpTransport, parse_sse_data};

/// 一个 HTTP MCP 连接(无状态或服务端自持会话)。
pub struct HttpTransport {
    name: String,
    url: String,
    headers: HashMap<String, String>,
    client: reqwest::Client,
    next_id: RwLock<u64>,
    /// 服务端下发的会话 id;首响应拿到后,后续请求原样带回。
    session_id: RwLock<Option<String>>,
}

impl HttpTransport {
    pub fn new(
        name: impl Into<String>,
        url: String,
        headers: HashMap<String, String>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            name: name.into(),
            url,
            headers,
            client,
            next_id: RwLock::new(1),
            session_id: RwLock::new(None),
        }
    }

    /// 发一条报文并解析响应;`is_notify` 为真时不期待正文。
    async fn post(
        &self,
        frame: &str,
        timeout: Duration,
        is_notify: bool,
    ) -> Result<Option<Value>, McpClientError> {
        let mut request = self
            .client
            .post(&self.url)
            .timeout(timeout)
            .header(reqwest::header::CONTENT_TYPE, headers::CONTENT_TYPE_JSON)
            .header(reqwest::header::ACCEPT, headers::ACCEPT)
            .header(headers::PROTOCOL_VERSION, PROTOCOL_VERSION);
        for (key, value) in &self.headers {
            request = request.header(key, value);
        }
        if let Some(session) = self.session_id.read().unwrap().as_deref() {
            request = request.header(headers::SESSION_ID, session);
        }
        let response = request
            .body(frame.to_string())
            .send()
            .await
            .map_err(|error| {
                McpClientError::Disconnected(format!("请求 {}: {error}", self.url))
            })?;
        // 会话 id:首响应下发,之后每次带回(无状态服务端可能不给)。
        if let Some(session) = response.headers().get(headers::SESSION_ID)
            && let Ok(value) = session.to_str()
            && !value.trim().is_empty()
        {
            *self.session_id.write().unwrap() = Some(value.to_string());
        }
        let status = response.status();
        if !status.is_success() {
            return Err(McpClientError::Rpc {
                message: format!(
                    "MCP 服务器 {} 返回 HTTP {}{}",
                    self.name,
                    status.as_u16(),
                    status
                        .canonical_reason()
                        .map(|text| format!(" ({text})"))
                        .unwrap_or_default()
                ),
            });
        }
        if is_notify {
            return Ok(None);
        }
        let is_event_stream = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("text/event-stream"));
        let body = response.text().await.map_err(|error| {
            McpClientError::Disconnected(format!("读取响应失败:{error}"))
        })?;
        // 两种响应体都要认:SSE 时取 data 行,否则整段当 JSON。
        let json_text = if is_event_stream {
            match parse_sse_data(&body) {
                Some(text) => text,
                None => {
                    return Err(McpClientError::Disconnected(
                        "SSE 响应里没有 data 报文".to_string(),
                    ))
                }
            }
        } else {
            body
        };
        let value: Value = serde_json::from_str(json_text.trim()).map_err(|error| {
            McpClientError::Disconnected(format!("响应不是合法 JSON:{error}"))
        })?;
        Ok(Some(value))
    }
}

#[async_trait]
impl McpTransport for HttpTransport {
    async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, McpClientError> {
        let id = {
            let mut next = self.next_id.write().unwrap();
            let id = *next;
            *next += 1;
            id
        };
        let frame = serde_json::to_string(&JsonRpcRequest::new(id, method, params))
            .map_err(|error| McpClientError::Disconnected(error.to_string()))?;
        let value = tokio::time::timeout(timeout, self.post(&frame, timeout, false))
            .await
            .map_err(|_| McpClientError::Timeout {
                method: method.to_string(),
            })?
            .and_then(|value| value.ok_or_else(|| McpClientError::Disconnected("空响应".into())))?;
        crate::client::decode_response_value(value)
    }

    async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpClientError> {
        let frame = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .map_err(|error| McpClientError::Disconnected(error.to_string()))?;
        self.post(&frame, Duration::from_secs(10), true).await?;
        Ok(())
    }

    async fn shutdown(&self) {
        // HTTP 无状态:没有要关闭的长连接。若有会话,尽力通知服务端。
        if self.session_id.read().unwrap().is_some() {
            let _ = self
                .client
                .delete(&self.url)
                .timeout(Duration::from_secs(5))
                .send()
                .await;
        }
    }
}
