//! MCP 工具的模型可见入口(`mcp__<server>__<tool>`)。
//!
//! 一个 `McpTool` 实例 = 一个 MCP 工具。它自己是薄的:名字/描述/参数
//! schema 来自服务器声明,执行转发给共享的 [`McpManager`]。
//!
//! 三条硬约束:
//! - **结果必须分页截断**:MCP 工具返回的是任意外部文本,可能几十 MB。
//!   这里按字符分页(默认一页 8000 字符),尾部给出续读提示;
//! - **续读靠结果身份**:长结果尾部会给出 `result_id`,模型带同一条令牌
//!   再调一次才读后续页(不重跑工具——外部工具常有副作用)。参数相同但
//!   没带令牌 = 一次新调用 = 真实执行,结果绝不暗中复用;
//! - **不改写服务器原生参数**:denia 的分页字段(`offset`/`limit`/
//!   `result_id`)只在服务器没占用这些名字时用它们,被占用就退回 `denia_*`;
//!   服务器声明的参数与 `required` 一律原样转发,不删不改;
//! - **失败是可执行文本**:服务器未连接/超时/协议错误都变成
//!   `is_error` 工具结果,模型据此自纠(等用户重连,或换别的手段),
//!   不让一次外呼失败吞掉整个轮次。

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use denia_core::tool::ToolSchema;
use denia_mcp::{MAX_PAGE_CHARS, McpManager, PAGE_CHARS, PageCall, PagedError, PagedResult};
use serde_json::{Value, json};

use crate::support::{parse_tool_args, tool_error};
use crate::{Tool, ToolContext, ToolOutput};

/// denia 注入的分页字段名(工具私有,绝不透传给 MCP 服务器)。
///
/// 默认就叫 `offset` / `limit` / `result_id`;服务器 schema 里已经占了某个
/// 名字(在 `properties` 或 `required` 里出现)时退回 `denia_<名字>`——
/// 服务器自己的 `offset` 有它自己的语义,不能被 denia 的分页改写。
#[derive(Debug, Clone, PartialEq)]
struct PageFields {
    offset: String,
    limit: String,
    result_id: String,
    /// 服务器声明了 `offset`:此时 `offset>0` 是它自己的参数,不是续读请求。
    native_offset: bool,
}

/// 分页字段的解析结果。
struct PageArgs {
    offset: usize,
    /// 模型是否显式写了 offset(与「缺省 0」区分:显式 `offset>0` 是旧式续读)。
    offset_explicit: bool,
    limit: usize,
    result_id: Option<String>,
}

/// 从原始参数里取分页字段。
///
/// 字段名按服务器 schema 定(见 [`PageFields`]);数字宽容接受字符串数字,
/// 与 `support::parse_tool_args` 的口径一致。
fn parse_page_args(value: &Value, fields: &PageFields) -> Result<PageArgs, String> {
    let number = |name: &str| -> Result<Option<usize>, String> {
        let raw = match value.get(name) {
            None | Some(Value::Null) => return Ok(None),
            Some(raw) => raw,
        };
        let parsed = match raw {
            Value::Number(number) => number.as_u64().map(|value| value as usize),
            Value::String(text) => text.trim().parse::<usize>().ok(),
            _ => None,
        };
        parsed
            .map(Some)
            .ok_or_else(|| format!("{name} 必须是不小于 0 的整数"))
    };
    let offset = number(&fields.offset)?;
    let limit = number(&fields.limit)?;
    let result_id = match value.get(&fields.result_id) {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => {
            let text = text.trim();
            (!text.is_empty()).then(|| text.to_string())
        }
        Some(_) => return Err(format!("{} 必须是字符串", fields.result_id)),
    };
    Ok(PageArgs {
        offset: offset.unwrap_or(0),
        offset_explicit: offset.is_some(),
        limit: limit.unwrap_or(PAGE_CHARS).clamp(1, MAX_PAGE_CHARS),
        result_id,
    })
}

/// 剔除 denia 私有分页字段;服务器原生参数(含它自己的 `offset`/`limit`)原样保留。
fn strip_page_fields(mut value: Value, fields: &PageFields) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.remove(&fields.offset);
        object.remove(&fields.limit);
        object.remove(&fields.result_id);
    }
    value
}

/// 一个 MCP 工具:转发到对应服务器进程。
pub struct McpTool {
    name: String,
    schema: ToolSchema,
    manager: Arc<McpManager>,
    /// denia 私有分页字段名(按服务器 schema 定,避免改写原生参数)。
    fields: PageFields,
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
        let (parameters, fields) = decorate_schema(input_schema);
        Self {
            schema: ToolSchema {
                name: name.clone(),
                description,
                parameters,
            },
            name,
            manager,
            fields,
        }
    }
}

/// 描述尾部补一段分页说明:模型第一次调用就知道长结果怎么续读。
fn decorate_description(description: &str) -> String {
    let base = if description.trim().is_empty() {
        "MCP 外部工具。".to_string()
    } else {
        description.trim().to_string()
    };
    format!(
        "{base}\n(结果较长时按字符分页返回:调用会真实执行工具,尾部给出 result_id;要继续看后面的内容,带上同一个 result_id 与提示里的 offset 再调一次本工具(读同一次结果,不会重新执行)。不带 result_id 的调用都是新调用,会重新执行工具。)",
    )
}

/// 在服务器给的 inputSchema 上追加 denia 的分页字段。
///
/// 硬约束:**不改写服务器声明的东西**——
/// - 字段名被服务器占用(出现在 `properties` 或 `required` 里)时,denia 退回
///   `denia_<名字>`(服务器自己的 `offset` 有它自己的语义);
/// - `required` 一律不增不删:denia 的分页字段全是可选的,无需改写数组。
///
/// 服务器 schema 可能不是 object(少见但合法),此时整段包一层并保留原
/// schema 于 `properties`,至少不让模型收到非法 JSON Schema。
fn decorate_schema(input_schema: Value) -> (Value, PageFields) {
    let mut schema = input_schema;
    if !schema.is_object() {
        schema = json!({ "type": "object", "properties": {} });
    }
    let object = schema.as_object_mut().expect("object");
    if object.get("type").and_then(Value::as_str) != Some("object") {
        object.insert("type".to_string(), json!("object"));
    }
    // 服务器占用的名字:properties 与 required 都算。
    let mut taken: HashSet<String> = HashSet::new();
    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        taken.extend(properties.keys().cloned());
    }
    if let Some(required) = object.get("required").and_then(Value::as_array) {
        taken.extend(
            required
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string),
        );
    }
    let fields = PageFields {
        offset: pick_page_field("offset", &taken),
        limit: pick_page_field("limit", &taken),
        result_id: pick_page_field("result_id", &taken),
        native_offset: taken.contains("offset"),
    };
    let properties = object
        .entry("properties")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("properties object");
    properties.insert(
        fields.offset.clone(),
        json!({
            "type": "integer",
            "minimum": 0,
            "default": 0,
            "description": "0-based 起始字符偏移;读某次结果的后续页时用它。默认 0。"
        }),
    );
    properties.insert(
        fields.limit.clone(),
        json!({
            "type": "integer",
            "minimum": 1,
            "maximum": 32_000,
            "default": PAGE_CHARS,
            "description": "本页返回的字符数;默认 8000,最大 32000。"
        }),
    );
    properties.insert(
        fields.result_id.clone(),
        json!({
            "type": "string",
            "description": "结果身份:上次输出的尾部若给了 result_id,带上它 + 更大的 offset 就能读后续页,不会重新执行工具;省略即发起一次新的真实调用。"
        }),
    );
    (schema, fields)
}

/// 选一个不与服务器冲突的分页字段名。
fn pick_page_field(base: &str, taken: &HashSet<String>) -> String {
    if !taken.contains(base) {
        return base.to_string();
    }
    let fallback = format!("denia_{base}");
    if !taken.contains(&fallback) {
        return fallback;
    }
    format!("denia_page_{base}")
}

#[async_trait]
impl Tool for McpTool {
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn execute(&self, arguments: &str, ctx: &ToolContext) -> ToolOutput {
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
        let page = match parse_page_args(&value, &self.fields) {
            Ok(page) => page,
            Err(error) => {
                return tool_error(
                    error,
                    format!(
                        "{} / {} 是 denia 的分页字段,请传整数;{} 是结果身份令牌(字符串)",
                        self.fields.offset, self.fields.limit, self.fields.result_id
                    ),
                );
            }
        };
        // denia 自己的分页字段不能透传给服务器;服务器原生字段原样保留。
        let forwarded = strip_page_fields(value, &self.fields);
        let call = PageCall {
            qualified: &self.name,
            arguments: &forwarded,
            offset: page.offset,
            limit: page.limit,
            result_id: page.result_id.as_deref(),
            // 旧式写法(`offset>0` 但不带令牌)只在 denia 自己注入的 offset 上
            // 才可能是续读请求;服务器原生 `offset` 是它自己的参数语义。
            legacy_resume: page.offset_explicit && page.offset > 0 && !self.fields.native_offset,
            // 结果不跨会话复用:身份只在它自己的会话里有效。
            session: ctx.session_id.as_deref(),
        };
        match self.manager.call_paged(&call).await {
            Ok(paged) => {
                let content = render_paged(&paged, &self.fields);
                if paged.is_error {
                    // MCP 侧声明的业务失败:原样(含错误状态)回给模型自纠。
                    return ToolOutput::error(content);
                }
                ToolOutput::text(content)
            }
            Err(error) => paged_error_output(error),
        }
    }
}

/// 组装模型看到的一页文本:正文 + 续读说明(确实还有后续内容时才给)。
///
/// 续读提示必须带上结果身份:没有身份就分不清「新调用」与「看下一页」,
/// 而这个区别直接决定外部工具会不会被再跑一遍。
fn render_paged(paged: &PagedResult, fields: &PageFields) -> String {
    let mut content = paged.text.clone();
    if paged.total_chars > paged.shown_chars {
        let end = paged.offset + paged.shown_chars;
        let hint = match &paged.result_id {
            Some(id) if paged.has_more => format!(
                ";还有后续内容,带上 {}={id} 和 {}={end} 再调一次本工具(读同一次结果,不会重新执行)",
                fields.result_id, fields.offset
            ),
            Some(id) => format!(";这是最后一页(结果身份 {}={id})", fields.result_id),
            None => String::new(),
        };
        content.push_str(&format!(
            "\n\n[第 {}-{} 字符 / 共 {} 字符{}]",
            paged.offset, end, paged.total_chars, hint
        ));
    }
    if paged.uncached {
        content.push_str("\n\n(结果超过 1MB,未缓存翻页;请缩小查询范围,例如收窄关键词或分页参数)");
    }
    content
}

/// 分页/续读失败 → 可执行的错误结果。
///
/// 结果失效、跨会话、旧式续读找不到原结果这三种情形都**不静默重跑**外部
/// 工具:如实说明并交回选择权(要不要再发一次新调用,由模型/用户决定——
/// 新调用才意味着真实执行,可能有副作用)。
fn paged_error_output(error: PagedError) -> ToolOutput {
    match error {
        PagedError::UnknownResult { result_id } => tool_error(
            format!("无法续读结果:{result_id} 已不存在(服务器重连或缓存已淘汰)"),
            "去掉 result_id 与 offset 重新调用本工具会真实重新执行一次(外部工具可能有副作用);也可以缩小查询范围,一次读完",
        ),
        PagedError::ForeignSession { result_id } => tool_error(
            format!("无法续读结果:{result_id} 属于另一个会话"),
            "result_id 只在产生它的那次会话里有效;请在本会话里重新调用工具获取新结果",
        ),
        PagedError::LostResult => tool_error(
            "无法翻页:没有可续读的结果(用 offset>0 续读时,原结果必须还在缓存里)".to_string(),
            "续读要带上上次输出尾部的 result_id;若没有,请去掉 offset 重新调用本工具(会真实执行一次),或在参数里限定更小的范围一次读完",
        ),
        PagedError::Call(error) => tool_error(
            error,
            "这是 MCP 外部服务器的调用失败;可到设置 → MCP 检查该服务器状态,或换用其它工具完成任务",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pagination_fields_are_injected() {
        let (schema, fields) = decorate_schema(json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"]
        }));
        let properties = schema["properties"].as_object().unwrap();
        assert!(properties.contains_key("offset"));
        assert!(properties.contains_key("limit"));
        assert!(properties.contains_key("result_id"));
        assert!(properties.contains_key("query"));
        assert_eq!(fields.offset, "offset");
        assert_eq!(fields.limit, "limit");
        assert_eq!(fields.result_id, "result_id");
        assert!(!fields.native_offset);
        // 分页字段是可选的,不能被塞进 required。
        let required = schema["required"].as_array().unwrap();
        assert_eq!(required, &vec![json!("query")]);
    }

    /// 服务器声明的字段名一律不动:denia 退回 `denia_*`,原生参数照原样转发。
    #[test]
    fn server_owned_pagination_fields_are_kept() {
        let (schema, fields) = decorate_schema(json!({
            "type": "object",
            "properties": {
                "offset": { "type": "string" },
                "limit": { "type": "integer" }
            },
            "required": ["offset"]
        }));
        // 服务器自己声明的东西一字不改(包括 required)。
        assert_eq!(schema["properties"]["offset"]["type"], json!("string"));
        assert_eq!(schema["properties"]["limit"]["type"], json!("integer"));
        assert_eq!(schema["required"], json!(["offset"]));
        // denia 的分页字段退回私有名字,不与原生语义撞车。
        assert_eq!(fields.offset, "denia_offset");
        assert_eq!(fields.limit, "denia_limit");
        assert_eq!(fields.result_id, "result_id");
        assert!(fields.native_offset);
        assert!(schema["properties"]["denia_offset"].is_object());
    }

    /// 只声明在 `required` 里的名字也算被服务器占用(不能把我们注入的
    /// 可选字段搭进去变成必填)。
    #[test]
    fn required_only_names_are_still_treated_as_owned() {
        let (_, fields) = decorate_schema(json!({
            "type": "object",
            "required": ["offset"]
        }));
        assert_eq!(fields.offset, "denia_offset");
        assert!(fields.native_offset);
    }

    /// 服务器原生参数原样转发,denia 私有字段被剔除。
    #[test]
    fn native_fields_are_forwarded_untouched() {
        // 三个名字都被服务器占用:denia 的分页字段全部退回 `denia_*`。
        let (_, fields) = decorate_schema(json!({
            "type": "object",
            "properties": {
                "offset": { "type": "string" },
                "limit": { "type": "integer" },
                "result_id": { "type": "string" }
            }
        }));
        let forwarded = strip_page_fields(
            json!({
                "offset": "cursor-2",
                "limit": 3,
                "result_id": "srv-1",
                "denia_offset": 10,
                "denia_limit": 5,
                "denia_result_id": "res-1"
            }),
            &fields,
        );
        assert_eq!(
            forwarded,
            json!({ "offset": "cursor-2", "limit": 3, "result_id": "srv-1" })
        );
    }

    /// 名字没被占用时,分页字段就是普通的 `offset`/`limit`/`result_id`,
    /// 它们不进服务器参数。
    #[test]
    fn denia_owned_fields_are_stripped() {
        let (_, fields) = decorate_schema(json!({ "type": "object", "properties": {} }));
        let forwarded = strip_page_fields(
            json!({ "query": "x", "offset": 10, "limit": 5, "result_id": "res-1" }),
            &fields,
        );
        assert_eq!(forwarded, json!({ "query": "x" }));
    }

    #[test]
    fn non_object_schema_is_repaired() {
        let (schema, fields) = decorate_schema(json!("not an object"));
        assert_eq!(schema["type"], json!("object"));
        assert!(schema["properties"]["offset"].is_object());
        assert_eq!(fields.offset, "offset");
    }

    #[test]
    fn description_carries_pagination_hint() {
        let text = decorate_description("搜索网页");
        assert!(text.starts_with("搜索网页"));
        assert!(text.contains("result_id"));
        // 空描述也有兜底文案,不会让模型看到一个没说明的工具。
        assert!(decorate_description("").contains("MCP 外部工具"));
    }

    /// 显式 `offset` 与缺省 0 要分得清——前者才是旧式续读请求。
    #[test]
    fn page_args_distinguish_explicit_offset() {
        let (_, fields) = decorate_schema(json!({ "type": "object", "properties": {} }));
        let args = parse_page_args(
            &json!({ "offset": "30", "limit": 10, "result_id": " res-1 " }),
            &fields,
        )
        .expect("合法参数");
        assert_eq!(args.offset, 30);
        assert!(args.offset_explicit);
        assert_eq!(args.limit, 10);
        assert_eq!(args.result_id.as_deref(), Some("res-1"));

        let default = parse_page_args(&json!({}), &fields).expect("缺省值");
        assert_eq!(default.offset, 0);
        assert!(!default.offset_explicit);
        assert_eq!(default.limit, PAGE_CHARS);
        assert!(default.result_id.is_none());

        // 非法类型要说清楚,不静默当成缺省值。
        assert!(parse_page_args(&json!({ "offset": -1 }), &fields).is_err());
        assert!(parse_page_args(&json!({ "result_id": 7 }), &fields).is_err());
    }

    /// 续读提示必须带结果身份;没有身份(超大结果)就说清楚翻不了。
    #[test]
    fn paged_text_carries_result_identity() {
        let (_, fields) = decorate_schema(json!({ "type": "object", "properties": {} }));
        let paged = PagedResult {
            text: "AAAA".to_string(),
            is_error: false,
            total_chars: 10,
            offset: 0,
            shown_chars: 4,
            has_more: true,
            from_cache: false,
            uncached: false,
            result_id: Some("res-1".to_string()),
        };
        let rendered = render_paged(&paged, &fields);
        assert!(rendered.contains("result_id=res-1"), "{rendered}");
        assert!(rendered.contains("offset=4"), "{rendered}");

        let uncached = PagedResult {
            uncached: true,
            result_id: None,
            ..paged
        };
        let rendered = render_paged(&uncached, &fields);
        assert!(!rendered.contains("result_id="), "{rendered}");
        assert!(rendered.contains("未缓存"), "{rendered}");
    }

    /// 续读失败的错误必须可执行,并把「要不要重跑」的选择权交回模型。
    #[test]
    fn paged_errors_are_actionable() {
        let expired = paged_error_output(PagedError::UnknownResult {
            result_id: "res-9".to_string(),
        });
        assert!(expired.is_error);
        assert!(expired.content.contains("res-9"));
        assert!(expired.content.contains("重新调用"));

        let foreign = paged_error_output(PagedError::ForeignSession {
            result_id: "res-9".to_string(),
        });
        assert!(foreign.is_error);
        assert!(foreign.content.contains("会话"));

        let lost = paged_error_output(PagedError::LostResult);
        assert!(lost.is_error);
        assert!(lost.content.contains("result_id"));

        let call = paged_error_output(PagedError::Call("未连接".to_string()));
        assert!(call.is_error);
        assert!(call.content.contains("MCP"));
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
        assert_eq!(tool.fields.offset, "offset");
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
///
/// `granted` 是**当前会话**的冻结授权：`Some` 时未授权的 MCP 工具不出现在
/// 目录里（目录、首次参数加载与真实执行三处口径一致；计划 6.4）。`None`
/// 表示该会话没有被定义收窄，按部署全集列出。
fn render_tool_catalog(
    servers: &[denia_mcp::McpServerState],
    only: Option<&str>,
    granted: Option<&[String]>,
) -> String {
    let allowed = |qualified: &str| granted.is_none_or(|list| list.iter().any(|n| n == qualified));
    let visible: Vec<&denia_mcp::McpServerState> = servers
        .iter()
        .filter(|server| {
            server.enabled
                && server.status == denia_mcp::McpServerStatus::Connected
                && only.is_none_or(|id| server.id == id)
                && server
                    .tools
                    .iter()
                    .any(|tool| tool.enabled && allowed(&tool.qualified))
        })
        .collect();
    if visible.is_empty() {
        if granted.is_some() {
            return match only {
                Some(id) => format!(
                    "当前会话没有被授予服务器 {id} 的任何 MCP 工具；需要外部能力请让父代理用授予了对应工具的定义重新派遣。"
                ),
                None => {
                    "当前会话没有被授予任何 MCP 外部工具；需要外部能力请让父代理用授予了 MCP 工具的定义重新派遣。"
                        .to_string()
                }
            };
        }
        return match only {
            Some(id) => {
                format!("没有名为 {id} 的已连接 MCP 服务器;不带 server 参数再调一次可查看全部。")
            }
            None => {
                "当前没有已连接的 MCP 服务器;可到设置 → MCP 检查服务器状态或重新连接。".to_string()
            }
        };
    }
    let mut body =
        String::from("已接入的 MCP 外部工具(按名称直接调用;首次调用只返回参数定义,不会执行):\n");
    for server in visible {
        body.push_str(&format!("\n[服务器 {}]\n", server.id));
        let mut listed = 0usize;
        for tool in &server.tools {
            if !tool.enabled || !allowed(&tool.qualified) {
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
    /// 会话授权来源：绑定后目录按当前会话的冻结授权过滤。
    grants: Option<Arc<dyn crate::capabilities::AgentRuntime>>,
}

impl McpListTool {
    pub fn new(manager: Arc<McpManager>) -> Self {
        Self {
            manager,
            schema: mcp_list_schema(),
            grants: None,
        }
    }

    /// 绑定授权来源：子代理只能看到自己冻结授权里的 MCP 工具。
    pub fn with_grant_source(mut self, source: Arc<dyn crate::capabilities::AgentRuntime>) -> Self {
        self.grants = Some(source);
        self
    }
}

#[async_trait]
impl Tool for McpListTool {
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn execute(&self, arguments: &str, ctx: &ToolContext) -> ToolOutput {
        let only: Option<String> = match parse_tool_args::<Value>(arguments) {
            Ok(value) => value
                .get("server")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string),
            Err(_) => None,
        };
        let granted = match (&self.grants, ctx.session_id.as_deref()) {
            (Some(source), Some(session)) => source.granted_tools(session),
            _ => None,
        };
        let snapshot = self.manager.snapshot();
        ToolOutput::text(render_tool_catalog(
            &snapshot.servers,
            only.as_deref(),
            granted.as_deref(),
        ))
    }
}

#[cfg(test)]
mod catalog_tests {
    use super::*;
    use denia_mcp::{McpServerState, McpServerStatus, McpToolState};

    fn server(
        id: &str,
        status: McpServerStatus,
        enabled: bool,
        tools: Vec<McpToolState>,
    ) -> McpServerState {
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
            server(
                "fx",
                McpServerStatus::Connected,
                true,
                vec![
                    tool("echo", "mcp__fx__echo", true),
                    tool("off", "mcp__fx__off", false),
                ],
            ),
            server("dead", McpServerStatus::Error, true, vec![]),
        ];
        let text = render_tool_catalog(&servers, None, None);
        assert!(text.contains("mcp__fx__echo — echo 的用途说明"));
        assert!(!text.contains("mcp__fx__off"), "被关闭的工具不得出现在目录");
        assert!(
            !text.contains("[服务器 dead]"),
            "连接失败的服务器不得出现在目录"
        );
    }

    #[test]
    fn catalog_filters_by_server_and_reports_missing() {
        let servers = vec![server(
            "fx",
            McpServerStatus::Connected,
            true,
            vec![tool("echo", "mcp__fx__echo", true)],
        )];
        assert!(render_tool_catalog(&servers, Some("fx"), None).contains("mcp__fx__echo"));
        let miss = render_tool_catalog(&servers, Some("nope"), None);
        assert!(miss.contains("nope") && miss.contains("没有"));
        let disabled = vec![server("off", McpServerStatus::Connected, false, vec![])];
        assert!(render_tool_catalog(&disabled, None, None).contains("没有已连接"));
    }

    /// 子代理的 MCP 目录按**冻结授权**过滤：未授权的工具不出现在目录里，
    /// 也不把"目录根本没有"与"服务器没连上"混为一谈。
    #[test]
    fn catalog_hides_tools_outside_the_session_grant() {
        let servers = vec![server(
            "fx",
            McpServerStatus::Connected,
            true,
            vec![
                tool("echo", "mcp__fx__echo", true),
                tool("write", "mcp__fx__write", true),
            ],
        )];
        let granted = vec!["mcp__fx__echo".to_string()];
        let text = render_tool_catalog(&servers, None, Some(&granted));
        assert!(text.contains("mcp__fx__echo"));
        assert!(
            !text.contains("mcp__fx__write"),
            "未授权的 MCP 工具不得出现在目录里:{text}"
        );
        // 授权里一个 MCP 工具都没有：明确说明是授权问题，不是连接问题。
        let none: Vec<String> = vec!["read_file".to_string()];
        let text = render_tool_catalog(&servers, None, Some(&none));
        assert!(text.contains("没有被授予任何 MCP"), "{text}");
        // 未指定授权（普通会话）：按部署全集列出。
        assert!(render_tool_catalog(&servers, None, None).contains("mcp__fx__write"));
    }
}
