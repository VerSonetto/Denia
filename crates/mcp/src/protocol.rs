//! JSON-RPC 2.0 报文与 MCP 方法常量。
//!
//! MCP 用 JSON-RPC 2.0,`initialize` → `notifications/initialized` →
//! `tools/list` / `tools/call` 是最小可用面(resources/prompts 本期不做,
//! 配置里出现即被拒绝)。帧格式是 **newline-delimited JSON**:一行一条
//! 报文,不含裸换行(字符串内的换行已被 JSON 转义)。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// MCP 协议版本(`initialize` 时上报与协商)。
pub const PROTOCOL_VERSION: &str = "2024-11-05";
/// 客户端身份:denia 自己。
pub const CLIENT_NAME: &str = "denia";
pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub const METHOD_INITIALIZE: &str = "initialize";
pub const METHOD_INITIALIZED: &str = "notifications/initialized";
pub const METHOD_TOOLS_LIST: &str = "tools/list";
pub const METHOD_TOOLS_CALL: &str = "tools/call";

/// 单条 JSON-RPC 2.0 请求。
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    pub fn new(id: u64, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method: method.into(),
            params,
        }
    }
}

/// 单条 JSON-RPC 2.0 响应(错误与结果二选一)。
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcResponse {
    #[allow(dead_code)]
    pub jsonrpc: Option<String>,
    pub id: Option<Value>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC 错误对象。
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[allow(dead_code)]
    #[serde(default)]
    pub data: Option<Value>,
}

/// `initialize` 的入参。
#[derive(Debug, Clone, Serialize)]
pub struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: Value,
    #[serde(rename = "clientInfo")]
    pub client_info: ClientInfo,
}

/// 客户端身份信息。
#[derive(Debug, Clone, Serialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

impl Default for InitializeParams {
    fn default() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION.to_string(),
            capabilities: serde_json::json!({}),
            client_info: ClientInfo {
                name: CLIENT_NAME.to_string(),
                version: CLIENT_VERSION.to_string(),
            },
        }
    }
}

/// 一个 MCP 工具声明(来自 `tools/list`)。
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolDef {
    /// 服务器侧的原始工具名(未加前缀)。
    pub name: String,
    pub description: String,
    /// JSON Schema;缺失时由调用方补 `{"type":"object"}`。
    pub input_schema: Value,
}

impl McpToolDef {
    pub fn new(name: String, description: String, input_schema: Value) -> Self {
        let input_schema = if input_schema.is_null() || !input_schema.is_object() {
            serde_json::json!({ "type": "object" })
        } else {
            input_schema
        };
        Self {
            name,
            description,
            input_schema,
        }
    }
}

/// `tools/call` 的结果:文本正文 + MCP 侧错误标记。
#[derive(Debug, Clone, PartialEq)]
pub struct McpCallResult {
    /// 已折成纯文本的 content(reason:非文本 content 无法进模型请求)。
    pub text: String,
    /// MCP 显式声明 `isError` —— 这是业务失败,不是协议失败,要作为
    /// `is_error` 工具结果回给模型让它自纠(而不是当异常抛出)。
    pub is_error: bool,
}

/// 把 MCP 的 `content` 数组折成一段纯文本。
///
/// 元素形态:
/// - `{ "type": "text", "text": "…" }` → 正文;
/// - 其它类型(image/resource…):转成一行占位说明,模型知道有内容但
///   拿不到(本期不支持二进制回传),不会静默丢失。
pub fn flatten_content(content: Option<&Value>) -> String {
    let Some(items) = content.and_then(Value::as_array) else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "text" => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    parts.push(text.to_string());
                }
            }
            "image" => parts.push("[该工具返回了一张图片,denia 暂不支持回传图片内容]".to_string()),
            "resource" => {
                let uri = item
                    .get("resource")
                    .and_then(|r| r.get("uri"))
                    .and_then(Value::as_str)
                    .unwrap_or("(unknown)");
                parts.push(format!("[该工具返回了一个资源:{uri}]"));
            }
            other => parts.push(format!("[该工具返回了不支持的内容类型:{other}]")),
        }
    }
    parts.join("\n")
}

/// 从 `tools/call` 的 result 里取出文本与错误标记。
pub fn call_result_from(value: &Value) -> McpCallResult {
    McpCallResult {
        text: flatten_content(value.get("content")),
        is_error: value
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

/// 从 `tools/list` 的 result 里解析工具清单;缺字段的条目整条跳过
/// (不带 name 的工具无法调用,静塞进列表只会让模型调到空)。
pub fn parse_tools_list(value: &Value) -> Vec<McpToolDef> {
    let Some(items) = value.get("tools").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let name = item.get("name").and_then(Value::as_str)?;
            if name.trim().is_empty() {
                return None;
            }
            let description = item
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let schema = item.get("inputSchema").cloned().unwrap_or(Value::Null);
            Some(McpToolDef::new(name.to_string(), description, schema))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_serializes_to_jsonrpc_envelope() {
        let request = JsonRpcRequest::new(1, METHOD_INITIALIZE, Some(json!({"a": 1})));
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["jsonrpc"], json!("2.0"));
        assert_eq!(value["id"], json!(1));
        assert_eq!(value["method"], json!("initialize"));
        assert_eq!(value["params"], json!({"a": 1}));
    }

    #[test]
    fn request_omits_absent_params() {
        let value = serde_json::to_value(JsonRpcRequest::new(2, METHOD_TOOLS_LIST, None)).unwrap();
        assert!(value.get("params").is_none());
    }

    #[test]
    fn parses_tools_list_and_skips_nameless() {
        let result = json!({
            "tools": [
                { "name": "read", "description": "读", "inputSchema": { "type": "object" } },
                { "description": "没有名字的工具" },
                { "name": "   " },
                { "name": "no_schema" }
            ]
        });
        let tools = parse_tools_list(&result);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "read");
        assert_eq!(tools[0].description, "读");
        // 缺 inputSchema 的补默认 object schema,而不是把 null 交给模型。
        assert_eq!(tools[1].input_schema, json!({ "type": "object" }));
    }

    #[test]
    fn flatten_content_handles_text_image_and_resource() {
        let value = json!({
            "content": [
                { "type": "text", "text": "第一行" },
                { "type": "text", "text": "第二行" },
                { "type": "image", "data": "AAA" },
                { "type": "resource", "resource": { "uri": "file:///a.txt" } },
                { "type": "audio", "data": "x" }
            ]
        });
        let text = flatten_content(value.get("content"));
        assert!(text.starts_with("第一行\n第二行"));
        assert!(text.contains("不支持回传图片"));
        assert!(text.contains("file:///a.txt"));
        assert!(text.contains("audio"));
    }

    #[test]
    fn call_result_reads_is_error_flag() {
        let ok = call_result_from(&json!({ "content": [{ "type": "text", "text": "done" }] }));
        assert_eq!(ok.text, "done");
        assert!(!ok.is_error);

        let failed = call_result_from(&json!({
            "content": [{ "type": "text", "text": "boom" }],
            "isError": true
        }));
        assert!(failed.is_error);
    }

    #[test]
    fn parses_error_response() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}}"#;
        let response: JsonRpcResponse = serde_json::from_str(raw).unwrap();
        let error = response.error.expect("has error");
        assert_eq!(error.code, -32601);
        assert!(error.message.contains("Method not found"));
        assert!(response.result.is_none());
    }
}
