//! SSE 传输(HTTP + SSE 双通道):POST 发请求,响应从事件流上回来。
//!
//! 握手流程(MCP 规范的 http+sse):
//! 1. `GET <url>`(Accept: text/event-stream)建立事件流;
//! 2. 服务端先发一个 `event: endpoint`,`data:` 里给出 POST 地址
//!    (可能是绝对路径,也可能是相对路径,需要按 base 解析);
//! 3. 客户端 POST JSON-RPC 到该 endpoint,服务端回 202(无正文);
//! 4. 真正的响应从事件流上以 `event: message` 回来,按 `id` 配对。
//!
//! 配对实现:后台任务把流上每条带 `id` 的报文写进一张表并广播唤醒,
//! 请求方在自己的 id 出现前阻塞等待(带超时)。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::Notify;

use crate::client::McpClientError;
use crate::protocol::{JsonRpcRequest, PROTOCOL_VERSION};
use crate::transport::headers;
use crate::transport::{McpTransport, parse_sse_data};

/// 一个 SSE MCP 连接。
pub struct SseTransport {
    name: String,
    /// POST 端点(来自 `endpoint` 事件;未收到时退回建立流用的 URL)。
    post_url: Mutex<String>,
    headers: HashMap<String, String>,
    client: reqwest::Client,
    next_id: Mutex<u64>,
    /// id → 响应报文(流上收到的全部带 id 报文)。
    responses: Arc<Mutex<HashMap<u64, Value>>>,
    /// 新报文到达时唤醒所有等待者。
    arrived: Arc<Notify>,
    /// 读流任务的取消句柄;shutdown 时 abort。
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl SseTransport {
    /// 建立事件流;等到 `endpoint` 事件(或超时)后再用 POST 发请求。
    pub async fn connect(
        name: impl Into<String>,
        url: String,
        headers: HashMap<String, String>,
        client: reqwest::Client,
        wait_timeout: Duration,
    ) -> Result<Self, McpClientError> {
        let name = name.into();
        let (endpoint_tx, mut endpoint_rx) = tokio::sync::oneshot::channel::<String>();
        // sender 只能发一次:包成 Option 交给读流循环 take(重连时可能
        // 再来 endpoint 事件,重复投递没有意义)。
        let endpoint_tx = std::sync::Mutex::new(Some(endpoint_tx));
        let responses: Arc<Mutex<HashMap<u64, Value>>> = Arc::new(Mutex::new(HashMap::new()));
        let arrived = Arc::new(Notify::new());

        let mut request = client
            .get(&url)
            .header(reqwest::header::ACCEPT, headers::ACCEPT_SSE)
            .header(headers::PROTOCOL_VERSION, PROTOCOL_VERSION);
        for (key, value) in &headers {
            request = request.header(key, value);
        }
        let response = request.send().await.map_err(|error| {
            McpClientError::Disconnected(format!("建立 SSE 流失败({url}): {error}"))
        })?;
        if !response.status().is_success() {
            return Err(McpClientError::Rpc {
                message: format!(
                    "MCP 服务器 {} 的 SSE 端点返回 HTTP {}",
                    name,
                    response.status().as_u16()
                ),
            });
        }
        let stream = response.bytes_stream();

        let reader_responses = responses.clone();
        let reader_arrived = arrived.clone();
        let reader_base = url.clone();
        let reader_name = name.clone();
        let reader = tokio::spawn(async move {
            // 逐块累积字节,按 SSE 的空行切事件(块边界可能落在事件中间)。
            let mut buffer = String::new();
            let mut stream = std::pin::pin!(stream);
            let mut endpoint_tx = endpoint_tx;
            while let Some(chunk) = stream.next().await {
                let Ok(chunk) = chunk else { break };
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                // 只处理完整事件(以空行结尾);残留留在 buffer 里等下一块。
                while let Some(split) = buffer.find("\n\n") {
                    let event = buffer[..split].to_string();
                    buffer.drain(..split + 2);
                    if handle_event(&event, &reader_responses, &reader_arrived) {
                        // `endpoint` 事件:把 POST 地址交给握手方。
                        // sender 是 Option:take 走即表示已投递过,后续
                        // endpoint 事件(重连时可能再来)不再重复发。
                        if let Some(endpoint) = endpoint_of(&event, &reader_base)
                            && let Some(tx) = endpoint_tx.lock().unwrap().take()
                        {
                            let _ = tx.send(endpoint);
                        }
                    }
                }
            }
        });

        // 等 endpoint(服务端没给就退回原 URL,有些实现直接复用同一地址)。
        let post_url = match tokio::time::timeout(wait_timeout, &mut endpoint_rx).await {
            Ok(Ok(url)) => url,
            _ => url.clone(),
        };
        Ok(Self {
            name,
            post_url: Mutex::new(post_url),
            headers,
            client,
            next_id: Mutex::new(1),
            responses,
            arrived,
            reader: Mutex::new(Some(reader)),
        })
    }

    async fn post(&self, frame: &str, timeout: Duration) -> Result<(), McpClientError> {
        let url = self.post_url.lock().unwrap().clone();
        let mut request = self
            .client
            .post(&url)
            .timeout(timeout)
            .header(reqwest::header::CONTENT_TYPE, headers::CONTENT_TYPE_JSON)
            .header(reqwest::header::ACCEPT, headers::ACCEPT_SSE);
        for (key, value) in &self.headers {
            request = request.header(key, value);
        }
        let response = request
            .body(frame.to_string())
            .send()
            .await
            .map_err(|error| {
                McpClientError::Disconnected(format!("POST {url} 失败: {error}"))
            })?;
        if !response.status().is_success() {
            return Err(McpClientError::Rpc {
                message: format!(
                    "MCP 服务器 {} 的 POST 端点返回 HTTP {}",
                    self.name,
                    response.status().as_u16()
                ),
            });
        }
        Ok(())
    }
}

#[async_trait]
impl McpTransport for SseTransport {
    async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, McpClientError> {
        let id = {
            let mut next = self.next_id.lock().unwrap();
            let id = *next;
            *next += 1;
            id
        };
        let frame = serde_json::to_string(&JsonRpcRequest::new(id, method, params))
            .map_err(|error| McpClientError::Disconnected(error.to_string()))?;
        // 先登记等待,再发请求:响应可能比"开始等待"更早到达。
        {
            let mut map = self.responses.lock().unwrap();
            map.remove(&id);
        }
        self.post(&frame, timeout).await?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // 先查表:响应可能已在(或竞态下刚到)。
            let hit = {
                let mut map = self.responses.lock().unwrap();
                map.remove(&id)
            };
            if let Some(value) = hit {
                return crate::client::decode_response_value(value);
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(McpClientError::Timeout {
                    method: method.to_string(),
                });
            }
            // 等新报文;Notify 必须先注册再查表,否则会漏掉刚到的那条。
            let notified = self.arrived.notified();
            if tokio::time::timeout(remaining, notified).await.is_err() {
                return Err(McpClientError::Timeout {
                    method: method.to_string(),
                });
            }
        }
    }

    async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpClientError> {
        let frame = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .map_err(|error| McpClientError::Disconnected(error.to_string()))?;
        self.post(&frame, Duration::from_secs(10)).await
    }

    async fn shutdown(&self) {
        if let Some(reader) = self.reader.lock().unwrap().take() {
            reader.abort();
        }
    }
}

impl Drop for SseTransport {
    fn drop(&mut self) {
        if let Ok(guard) = self.reader.lock()
            && let Some(reader) = guard.as_ref()
        {
            reader.abort();
        }
    }
}

/// 处理一个完整的 SSE 事件:把带 id 的 JSON-RPC 报文存进表并唤醒等待者。
/// 返回 `true` 表示这是一个 `endpoint` 事件。
fn handle_event(
    event: &str,
    responses: &Arc<Mutex<HashMap<u64, Value>>>,
    arrived: &Arc<Notify>,
) -> bool {
    let is_endpoint = event.lines().any(|line| line.trim() == "event: endpoint");
    if is_endpoint {
        return true;
    }
    let Some(data) = parse_sse_data(event) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(data.trim()) else {
        return false;
    };
    let Some(id) = value.get("id").and_then(Value::as_u64) else {
        // 通知(无 id):MCP 的服务器→客户端通知,本期只需不阻塞。
        return false;
    };
    responses.lock().unwrap().insert(id, value);
    // notify_one 只唤醒一个等待者,可能正是等别的 id 的那个;用 waiters
    // 让所有等待者都重查一遍表。
    arrived.notify_waiters();
    false
}

/// 从 `endpoint` 事件里解析 POST 地址;相对路径按建立流的 URL 解析。
fn endpoint_of(event: &str, base: &str) -> Option<String> {
    let data = parse_sse_data(event)?;
    let endpoint = data.trim().trim_matches('"').to_string();
    if endpoint.is_empty() {
        return None;
    }
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return Some(endpoint);
    }
    // 相对路径:以 base 的 origin + 路径解析(支持 `?sessionId=` 查询串)。
    let base_url = reqwest::Url::parse(base).ok()?;
    let joined = base_url.join(&endpoint).ok()?;
    Some(joined.to_string())
}

/// 仅供测试:暴露 SSE 事件的解析判定。
#[cfg(test)]
pub(crate) fn endpoint_of_for_test(event: &str, base: &str) -> Option<String> {
    endpoint_of(event, base)
}

/// 未使用但保留:便于将来按事件名分流(如 `event: ping`)。
#[allow(dead_code)]
fn event_name_of(event: &str) -> Option<String> {
    event
        .lines()
        .find_map(|line| line.strip_prefix("event:"))
        .map(|name| name.trim().to_string())
}

#[allow(dead_code)]
fn _unused_set(_: HashSet<u64>) {}
