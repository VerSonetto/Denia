//! 三种 MCP 传输:stdio、HTTP(Streamable HTTP)、SSE(HTTP+SSE)。
//!
//! 统一抽象成 [`McpTransport`]:一个传输只负责"把一条 JSON-RPC 报文
//! 送进去、把对应的响应取回来"。握手、`tools/list`、`tools/call` 的编排
//! 在 [`crate::client::McpClient`] 里只有一份,三种传输共享——新增传输不
//! 需要复制一遍协议逻辑。
//!
//! 语义对照(学 dsh 的 transport 分类,与 MCP 规范一致):
//! - **stdio**:本地子进程,stdin/stdout 承载 newline-delimited JSON;
//! - **http**:Streamable HTTP,POST 一个 JSON-RPC 请求,响应是 JSON
//!   (或 `text/event-stream`,规范允许二者),服务端可无状态;
//! - **sse**:HTTP + SSE 双通道,POST 发请求(回 202 + `Accept`),
//!   响应从先前建立的 `GET …/sse` 事件流上按 id 配对取回。

pub mod http;
pub mod sse;
pub mod stdio;

use async_trait::async_trait;
use serde_json::Value;

use crate::client::McpClientError;

/// 一条 MCP 连接的传输通道。
///
/// 实现必须串行可用(同一时刻一个请求在飞):MCP 的 id 配对在此抽象下
/// 就是"发一条、等一条",并发由 agent-loop 的工具并行池负责。
#[async_trait]
pub trait McpTransport: Send + Sync {
    /// 发送一条**请求**并返回它的 result(错误以 `Err` 返回)。
    async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: std::time::Duration,
    ) -> Result<Value, McpClientError>;

    /// 发送一条**通知**(无 id、无响应);尽力而为,失败只记日志。
    async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpClientError>;

    /// 关闭这条连接(杀子进程 / 关事件流)。必须幂等。
    async fn shutdown(&self);
}

/// HTTP 类传输共用的请求头常量。
pub mod headers {
    /// MCP 协议版本头(Streamable HTTP 与 SSE 都用)。
    pub const PROTOCOL_VERSION: &str = "mcp-protocol-version";
    /// 客户端用 POST 发 JSON-RPC,同时接受 JSON 与 SSE 两种响应体。
    pub const ACCEPT: &str = "application/json, text/event-stream";
    /// SSE 事件流的专用 Accept。
    pub const ACCEPT_SSE: &str = "text/event-stream";
    pub const CONTENT_TYPE_JSON: &str = "application/json";
    /// Streamable HTTP 的会话 id(服务端返回,后续请求带回)。
    pub const SESSION_ID: &str = "mcp-session-id";
}

/// 从 `text/event-stream` 正文里取出 `data:` 行并拼成 JSON 文本。
///
/// SSE 的字段语义:`event:` 是事件名(可缺省,默认 `message`),`data:`
/// 可跨多行用多个 `data:` 行拼接,空行结束一个事件。MCP 的 JSON-RPC
/// 报文都放在 `data:` 里,所以这里只取 `data:` 行。
pub(crate) fn parse_sse_data(body: &str) -> Option<String> {
    let mut payload = String::new();
    let mut saw_data = false;
    for line in body.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            // 空行 = 事件结束;已收集到数据就停。
            if saw_data {
                break;
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            saw_data = true;
            payload.push_str(rest.trim_start());
        }
    }
    if saw_data && !payload.trim().is_empty() {
        Some(payload)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_data_line() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        assert_eq!(
            parse_sse_data(body).as_deref(),
            Some("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}")
        );
    }

    #[test]
    fn parses_multi_data_lines_and_ignores_comments() {
        let body = ": keep-alive\ndata: {\"a\":\ndata: 1}\nid: 7\n\n";
        assert_eq!(parse_sse_data(body).as_deref(), Some("{\"a\":1}"));
    }

    #[test]
    fn empty_or_comment_only_stream_yields_nothing() {
        assert_eq!(parse_sse_data(""), None);
        assert_eq!(parse_sse_data(": ping\n\n"), None);
        // 只有 data 标记但内容为空:不算有效报文。
        assert_eq!(parse_sse_data("data: \n\n"), None);
    }
}
