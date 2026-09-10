//! MCP 客户端:握手指派 + 请求编排,与传输方式无关。
//!
//! 三种传输(stdio / http / sse)都实现 [`McpTransport`],这里的握手、
//! `tools/list`、`tools/call` 只有一份实现——新增传输不必复制协议逻辑。
//!
//! 生命周期:
//! - `connect` 按配置挑传输 → `initialize` 握手 → `notifications/initialized`;
//! - `list_tools` / `call_tool` 串行(一条连接上并发请求需要 id 配对与读
//!   任务分派,收益不抵复杂度;工具调用本身在 agent-loop 的并行池里已经
//!   并发过了);
//! - `shutdown` 关闭传输(杀子进程 / 关事件流),`Drop` 兜底。

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::McpServerConfig;
use crate::protocol::{
    JsonRpcResponse, METHOD_INITIALIZE, METHOD_INITIALIZED, METHOD_TOOLS_CALL, METHOD_TOOLS_LIST,
    McpCallResult, McpToolDef, InitializeParams, call_result_from, parse_tools_list,
};
use crate::transport::McpTransport;
use crate::transport::http::HttpTransport;
use crate::transport::sse::SseTransport;
use crate::transport::stdio::StdioTransport;

/// 单次请求超时(握手与工具调用共用)。
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// 工具调用超时比握手宽松:外部工具可能慢。
pub const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// SSE 握手里等 `endpoint` 事件的上限;超时则退回建立流时的 URL。
const ENDPOINT_WAIT: Duration = Duration::from_secs(10);

/// 客户端错误;文本面向模型与 UI,必须可执行。
#[derive(Debug, thiserror::Error)]
pub enum McpClientError {
    #[error("启动 MCP 服务器进程失败:{0}")]
    Spawn(String),
    #[error("MCP 服务器握手失败:{0}")]
    Handshake(String),
    #[error("MCP 请求超时({method})")]
    Timeout { method: String },
    #[error("MCP 服务器返回错误:{message}")]
    Rpc { message: String },
    #[error("MCP 服务器连接已断开:{0}")]
    Disconnected(String),
}

/// 一个已连接的 MCP 服务器句柄(传输方式已被擦除)。
pub struct McpClient {
    name: String,
    transport: Box<dyn McpTransport>,
    /// 单条调用的超时覆盖(来自配置的 `callTimeoutMs`)。
    call_timeout: Option<Duration>,
}

impl McpClient {
    /// 按配置建立连接并完成 `initialize` 握手。
    pub async fn connect(config: &McpServerConfig) -> Result<Self, McpClientError> {
        let name = config.id.clone();
        let transport: Box<dyn McpTransport> = match config.transport.as_str() {
            "stdio" => Box::new(
                StdioTransport::connect(
                    name.clone(),
                    &config.command,
                    &config.args,
                    &config.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                    config.cwd.as_deref(),
                )
                .await?,
            ),
            "http" => Box::new(HttpTransport::new(
                name.clone(),
                config
                    .url
                    .clone()
                    .ok_or_else(|| {
                        McpClientError::Spawn("http 传输缺少 url".to_string())
                    })?,
                request_headers(config),
                http_client()?,
            )),
            "sse" => Box::new(
                SseTransport::connect(
                    name.clone(),
                    config.url.clone().ok_or_else(|| {
                        McpClientError::Spawn("sse 传输缺少 url".to_string())
                    })?,
                    request_headers(config),
                    http_client()?,
                    ENDPOINT_WAIT,
                )
                .await?,
            ),
            other => {
                return Err(McpClientError::Spawn(format!(
                    "不支持的传输方式:{other}(可用:stdio / http / sse)"
                )))
            }
        };
        let client = Self {
            name,
            transport,
            call_timeout: config
                .call_timeout_ms
                .map(|ms| Duration::from_millis(ms.max(1))),
        };
        client.handshake().await?;
        Ok(client)
    }

    async fn handshake(&self) -> Result<(), McpClientError> {
        let params = serde_json::to_value(InitializeParams::default())
            .map_err(|error| McpClientError::Handshake(error.to_string()))?;
        let _result: Value = self
            .transport
            .request(METHOD_INITIALIZE, Some(params), REQUEST_TIMEOUT)
            .await
            .map_err(|error| McpClientError::Handshake(error.to_string()))?;
        // `notifications/initialized` 是通知:无 id、无响应。stdio 与 http
        // 都可能因为服务器实现差异而失败,失败只记日志不阻断——握手已成功。
        if let Err(error) = self
            .transport
            .notify(METHOD_INITIALIZED, None)
            .await
        {
            tracing::debug!(server = %self.name, %error, "initialized 通知未送达(忽略)");
        }
        Ok(())
    }

    /// 拉取工具清单。
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>, McpClientError> {
        let result = self
            .transport
            .request(METHOD_TOOLS_LIST, None, self.call_timeout.unwrap_or(REQUEST_TIMEOUT))
            .await?;
        Ok(parse_tools_list(&result))
    }

    /// 调用一个工具;`arguments` 原样透传给服务器(结构由它的 inputSchema 定)。
    pub async fn call_tool(
        &self,
        tool: &str,
        arguments: Value,
    ) -> Result<McpCallResult, McpClientError> {
        let params = json!({ "name": tool, "arguments": arguments });
        let result = self
            .transport
            .request(
                METHOD_TOOLS_CALL,
                Some(params),
                self.call_timeout.unwrap_or(CALL_TIMEOUT),
            )
            .await?;
        Ok(call_result_from(&result))
    }

    /// 主动关闭:关传输(杀子进程 / 关事件流)。
    pub async fn shutdown(&self) {
        self.transport.shutdown().await;
    }
}

/// 附加请求头(Authorization 等);`headers` 配置里逐条透传。
fn request_headers(config: &McpServerConfig) -> HashMap<String, String> {
    config
        .headers
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// HTTP 类传输共用的 reqwest 客户端(连接超时短、请求超时由调用方给)。
fn http_client() -> Result<reqwest::Client, McpClientError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| McpClientError::Spawn(format!("创建 HTTP 客户端失败:{error}")))
}

/// 把一段响应文本解成 result(共用:错误 → Err)。
pub(crate) fn decode_response(raw: &str) -> Result<Value, McpClientError> {
    let response: JsonRpcResponse = serde_json::from_str(raw)
        .map_err(|error| McpClientError::Disconnected(format!("响应不是合法 JSON:{error}")))?;
    if let Some(error) = response.error {
        return Err(McpClientError::Rpc {
            message: format!("{} (code {})", error.message, error.code),
        });
    }
    Ok(response.result.unwrap_or(Value::Null))
}

/// 把一条响应报文解成 result(共用:错误 → Err)。
pub(crate) fn decode_response_value(response: Value) -> Result<Value, McpClientError> {
    let parsed: JsonRpcResponse = serde_json::from_value(response)
        .map_err(|error| McpClientError::Disconnected(format!("响应不是合法 JSON:{error}")))?;
    if let Some(error) = parsed.error {
        return Err(McpClientError::Rpc {
            message: format!("{} (code {})", error.message, error.code),
        });
    }
    Ok(parsed.result.unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_are_actionable_chinese() {
        let error = McpClientError::Spawn("npx: 找不到文件".to_string());
        assert!(error.to_string().contains("启动 MCP 服务器进程失败"));
        let rpc = McpClientError::Rpc {
            message: "Method not found (code -32601)".to_string(),
        };
        assert!(rpc.to_string().contains("Method not found"));
    }

    #[test]
    fn decode_response_value_extracts_result_and_error() {
        let ok = decode_response_value(json!({ "jsonrpc": "2.0", "id": 1, "result": { "a": 1 } }))
            .expect("ok");
        assert_eq!(ok, json!({ "a": 1 }));

        let err = decode_response_value(json!({
            "jsonrpc": "2.0", "id": 1, "error": { "code": -32601, "message": "nope" }
        }));
        assert!(err.is_err());
    }
}
