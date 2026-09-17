//! MCP 工具的模型可见入口(`mcp__<server>__<tool>`)。
//!
//! 一个 `McpTool` 实例 = 一个 MCP 工具。它自己是薄的:名字/描述/参数
//! schema 来自服务器声明,执行转发给共享的 [`McpManager`]。
//!
//! 两条硬约束:
//! - **结果必须分页截断**:MCP 工具返回的是任意外部文本,可能几十 MB。
//!   这里按字符分页(默认一页 8000 字符),尾部给出翻页提示;翻页复用
//!   上一次的完整结果(缓存),不会把工具跑第二遍——外部工具常有副作用;
//! - **失败是可执行文本**:服务器未连接/超时/协议错误都变成
//!   `is_error` 工具结果,模型据此自纠(等用户重连,或换别的手段),
//!   不让一次外呼失败吞掉整个轮次。

use std::sync::Arc;

use async_trait::async_trait;
use denia_core::tool::ToolSchema;
use denia_mcp::{McpManager, PAGE_CHARS};
use serde_json::{Value, json};

use crate::support::{parse_tool_args, tool_error};
use crate::{Tool, ToolContext, ToolOutput};

/// 分页参数(工具私有,不进 MCP 服务器的 arguments)。
#[derive(serde::Deserialize)]
struct McpArgs {
    /// 0-based 起始字符;默认 0。
    #[serde(default)]
    offset: usize,
    /// 本页字符数;默认 8000,上限 32000。
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    PAGE_CHARS
}

/// 一个 MCP 工具:转发到对应服务器进程。
pub struct McpTool {
    name: String,
    schema: ToolSchema,
    manager: Arc<McpManager>,
}

impl McpTool {
    /// `qualified` 是模型可见名(`mcp__<server>__<tool>`),与快照路由一致。
    pub fn new(
        qualified: impl Into<String>,
        description: String,
        input_schema: Value,
        manager: Arc<McpManager>,
    ) -> Self {
        let name = qualified.into();
        let description = decorate_description(&description);
        let parameters = decorate_schema(input_schema);
        Self {
            schema: ToolSchema {
                name: name.clone(),
                description,
                parameters,
            },
            name,
            manager,
        }
    }
}

/// 描述尾部补一行分页说明:模型第一次调用就知道长结果怎么翻。
fn decorate_description(description: &str) -> String {
    let base = if description.trim().is_empty() {
        "MCP 外部工具。".to_string()
    } else {
        description.trim().to_string()
    };
    format!(
        "{base}\n(结果较长时按字符分页返回;要继续看后面的内容,用更大的 offset 再调一次,limit 可指定本页字符数)",
    )
}

/// 在服务器给的 inputSchema 上追加 `offset`/`limit`(不改原有字段)。
///
/// 服务器 schema 可能不是 object(少见但合法),此时整段包一层并保留原
/// schema 于 `properties`,至少不让模型收到非法 JSON Schema。
fn decorate_schema(input_schema: Value) -> Value {
    let mut schema = input_schema;
    if !schema.is_object() {
        schema = json!({ "type": "object", "properties": {} });
    }
    let object = schema.as_object_mut().expect("object");
    if object.get("type").and_then(Value::as_str) != Some("object") {
        object.insert("type".to_string(), json!("object"));
    }
    let properties = object
        .entry("properties")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("properties object");
    // 只在没被服务器占用的情况下注入分页字段。
    if !properties.contains_key("offset") {
        properties.insert(
            "offset".to_string(),
            json!({
                "type": "integer",
                "minimum": 0,
                "default": 0,
                "description": "0-based 起始字符偏移;结果被分页时用它翻页。默认 0。"
            }),
        );
    }
    if !properties.contains_key("limit") {
        properties.insert(
            "limit".to_string(),
            json!({
                "type": "integer",
                "minimum": 1,
                "maximum": 32_000,
                "default": PAGE_CHARS,
                "description": "本页返回的字符数;默认 8000,最大 32000。"
            }),
        );
    }
    // 分页参数都是可选的:required 里若出现它们,无参工具会被判非法。
    if let Some(required) = object.get_mut("required").and_then(Value::as_array_mut) {
        required.retain(|item| {
            !matches!(item.as_str(), Some("offset" | "limit"))
        });
    }
    schema
}

#[async_trait]
impl Tool for McpTool {
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn execute(&self, arguments: &str, _ctx: &ToolContext) -> ToolOutput {
        // 先按整体值解析(参数结构由服务器定,不能套固定结构体),
        // 再单独取分页字段——服务器参数与分页参数混在同一层。
        let value: Value = match parse_tool_args(arguments) {
            Ok(value) => value,
            Err(error) => {
                return tool_error(
                    format!("参数解析失败:{error}"),
                    format!("参数必须是 JSON 对象;{} 的参数结构见工具声明", self.name),
                );
            }
        };
        let page: McpArgs = serde_json::from_value(value.clone()).unwrap_or(McpArgs {
            offset: 0,
            limit: PAGE_CHARS,
        });
        // 分页字段是 denia 自己加的,不能透传给服务器。
        let mut forwarded = value;
        if let Some(object) = forwarded.as_object_mut() {
            object.remove("offset");
            object.remove("limit");
        }
        match self
            .manager
            .call_paged(&self.name, page.offset, page.limit, &forwarded)
            .await
        {
            Ok(paged) => {
                let end = paged.offset + paged.shown_chars;
                let mut content = if paged.total_chars > paged.shown_chars {
                    format!(
                        "{}\n\n[第 {}-{} 字符 / 共 {} 字符{}]",
                        paged.text,
                        paged.offset,
                        end,
                        paged.total_chars,
                        if paged.has_more {
                            format!(";还有后续内容,用 offset={end} 再调一次本工具")
                        } else {
                            String::new()
                        }
                    )
                } else {
                    paged.text
                };
                if paged.uncached {
                    content.push_str(
                        "\n\n(结果超过 1MB,未缓存翻页;请缩小查询范围,例如收窄关键词或分页参数)",
                    );
                }
                if paged.is_error {
                    // MCP 侧声明的业务失败:原样作为错误结果回给模型自纠。
                    return ToolOutput::error(content);
                }
                ToolOutput::text(content)
            }
            Err(error) => tool_error(
                error,
                "这是 MCP 外部服务器的调用失败;可到设置 → MCP 检查该服务器状态,或换用其它工具完成任务",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pagination_fields_are_injected() {
        let schema = decorate_schema(json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"]
        }));
        let properties = schema["properties"].as_object().unwrap();
        assert!(properties.contains_key("offset"));
        assert!(properties.contains_key("limit"));
        assert!(properties.contains_key("query"));
        // 分页字段是可选的,不能被塞进 required。
        let required = schema["required"].as_array().unwrap();
        assert_eq!(required, &vec![json!("query")]);
    }

    #[test]
    fn server_owned_pagination_fields_are_kept() {
        let schema = decorate_schema(json!({
            "type": "object",
            "properties": { "offset": { "type": "string" } }
        }));
        // 服务器自己声明的 offset 不被覆盖(它的语义由它定义)。
        assert_eq!(schema["properties"]["offset"]["type"], json!("string"));
    }

    #[test]
    fn non_object_schema_is_repaired() {
        let schema = decorate_schema(json!("not an object"));
        assert_eq!(schema["type"], json!("object"));
        assert!(schema["properties"]["offset"].is_object());
    }

    #[test]
    fn description_carries_pagination_hint() {
        let text = decorate_description("搜索网页");
        assert!(text.starts_with("搜索网页"));
        assert!(text.contains("offset"));
        // 空描述也有兜底文案,不会让模型看到一个没说明的工具。
        assert!(decorate_description("").contains("MCP 外部工具"));
    }

    #[test]
    fn tool_schema_exposes_qualified_name() {
        let manager = Arc::new(McpManager::new(std::collections::HashSet::new()));
        let tool = McpTool::new(
            "mcp__fs__read",
            "读取文件".to_string(),
            json!({ "type": "object" }),
            manager,
        );
        assert_eq!(tool.schema().name, "mcp__fs__read");
        assert!(tool.schema().description.contains("读取文件"));
    }
}

// —— 完全目录化工具面:mcp_list 发现入口 ——

/// `mcp_list` 工具名。
pub const MCP_LIST_TOOL: &str = "mcp_list";

/// `mcp_list` 的 schema(注册表与提示词 provider 共用同一份)。
pub fn mcp_list_schema() -> ToolSchema {
    ToolSchema {
        name: MCP_LIST_TOOL.to_string(),
        description: "列出已接入的 MCP 外部工具清单(名称与用途)。需要 denia 内置能力之外的外部能力时,先用本工具查看有哪些 MCP 工具可用,再按清单里的名称直接调用。".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "description": "只列出这个服务器的工具;省略则列出全部已连接服务器的工具。"
                }
            }
        }),
    }
}

/// 目录文本渲染(纯函数,便于测试):按服务器分组列出已连接服务器的
/// 模型可见工具。被关闭/冲突的工具不出现——它们本就不进路由。
fn render_tool_catalog(servers: &[denia_mcp::McpServerState], only: Option<&str>) -> String {
    let visible: Vec<&denia_mcp::McpServerState> = servers
        .iter()
        .filter(|server| {
            server.enabled
                && server.status == denia_mcp::McpServerStatus::Connected
                && only.is_none_or(|id| server.id == id)
        })
        .collect();
    if visible.is_empty() {
        return match only {
            Some(id) => format!("没有名为 {id} 的已连接 MCP 服务器;不带 server 参数再调一次可查看全部。"),
            None => "当前没有已连接的 MCP 服务器;可到设置 → MCP 检查服务器状态或重新连接。".to_string(),
        };
    }
    let mut body = String::from("已接入的 MCP 外部工具(按名称直接调用;首次调用只返回参数定义,不会执行):\n");
    for server in visible {
        body.push_str(&format!("\n[服务器 {}]\n", server.id));
        let mut listed = 0usize;
        for tool in &server.tools {
            if !tool.enabled {
                continue;
            }
            let description = tool.description.trim();
            let description = if description.is_empty() {
                "(无描述)"
            } else {
                description
            };
            body.push_str(&format!("- {} — {description}\n", tool.qualified));
            listed += 1;
        }
        if listed == 0 {
            body.push_str("(该服务器没有可用的工具)\n");
        }
    }
    body
}

/// MCP 工具目录(`mcp_list`):完全目录化工具面的发现入口。
///
/// 默认装配里 `mcp__*` 工具不进请求(tools 数组只有已装载的那些),
/// 模型靠本工具按需查看可用清单;看中的工具按清单名称直接调用,首次
/// 调用由 agent-loop 拦截返回参数定义并装载,之后正常执行。本工具自身
/// 是内置工具,只要有已连接的 MCP 服务器就常驻注册表与请求工具面。
pub struct McpListTool {
    manager: Arc<McpManager>,
    schema: ToolSchema,
}

impl McpListTool {
    pub fn new(manager: Arc<McpManager>) -> Self {
        Self {
            manager,
            schema: mcp_list_schema(),
        }
    }
}

#[async_trait]
impl Tool for McpListTool {
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn execute(&self, arguments: &str, _ctx: &ToolContext) -> ToolOutput {
        let only: Option<String> = match parse_tool_args::<Value>(arguments) {
            Ok(value) => value
                .get("server")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string),
            Err(_) => None,
        };
        let snapshot = self.manager.snapshot();
        ToolOutput::text(render_tool_catalog(&snapshot.servers, only.as_deref()))
    }
}

#[cfg(test)]
mod catalog_tests {
    use super::*;
    use denia_mcp::{McpServerState, McpServerStatus, McpToolState};

    fn server(id: &str, status: McpServerStatus, enabled: bool, tools: Vec<McpToolState>) -> McpServerState {
        McpServerState {
            id: id.to_string(),
            transport: "stdio".to_string(),
            command: "cmd".to_string(),
            args: Vec::new(),
            url: None,
            env_keys: Vec::new(),
            header_keys: Vec::new(),
            cwd: None,
            enabled,
            status,
            error: None,
            tools,
        }
    }

    fn tool(name: &str, qualified: &str, enabled: bool) -> McpToolState {
        McpToolState {
            name: name.to_string(),
            qualified: qualified.to_string(),
            description: format!("{name} 的用途说明"),
            enabled,
            disabled_reason: None,
        }
    }

    #[test]
    fn catalog_lists_connected_servers_grouped() {
        let servers = vec![
            server("fx", McpServerStatus::Connected, true, vec![
                tool("echo", "mcp__fx__echo", true),
                tool("off", "mcp__fx__off", false),
            ]),
            server("dead", McpServerStatus::Error, true, vec![]),
        ];
        let text = render_tool_catalog(&servers, None);
        assert!(text.contains("mcp__fx__echo — echo 的用途说明"));
        assert!(!text.contains("mcp__fx__off"), "被关闭的工具不得出现在目录");
        assert!(!text.contains("[服务器 dead]"), "连接失败的服务器不得出现在目录");
    }

    #[test]
    fn catalog_filters_by_server_and_reports_missing() {
        let servers = vec![server(
            "fx",
            McpServerStatus::Connected,
            true,
            vec![tool("echo", "mcp__fx__echo", true)],
        )];
        assert!(render_tool_catalog(&servers, Some("fx")).contains("mcp__fx__echo"));
        let miss = render_tool_catalog(&servers, Some("nope"));
        assert!(miss.contains("nope") && miss.contains("没有"));
        let disabled = vec![server("off", McpServerStatus::Connected, false, vec![])];
        assert!(render_tool_catalog(&disabled, None).contains("没有已连接"));
    }
}
