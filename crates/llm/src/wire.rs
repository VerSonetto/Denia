//! The shared OpenAI-family wire vocabulary (chat-completions SSE) and the
//! state machine translating wire chunks into [`StreamChunk`]s.
//!
//! Delta chunks emit immediately; block ends, usage, and finish are deferred
//! to [`StreamTranslator::finalize`], which runs at `[DONE]`.

use std::collections::BTreeMap;

use denia_core::error::{LlmFailure, codes};
use denia_core::message::ChatRole;
use denia_core::stream::{BlockType, ContentBlock, FinishReason, StreamChunk, TokenUsage};
use serde::{Deserialize, Serialize};

use crate::GenerateRequest;

/// `#[serde(default)]` 只兜"字段缺失",兜不住显式 `null`;而 usage/收尾帧
/// 发 `"choices": null` 的网关实测存在。统一按空值兜住,不让一个 null 炸掉整步。
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Deserialize)]
pub struct WireChunk {
    #[serde(default, deserialize_with = "null_as_default")]
    pub choices: Vec<WireChoice>,
    #[serde(default)]
    pub usage: Option<WireUsage>,
    /// 帧级错误(OpenAI 的 `chat.completion.chunk` 会在流中带 `error` 对象)。
    /// 带 error 的帧不是正文帧,必须让整步失败而不是当成空帧跳过。
    #[serde(default)]
    pub error: Option<WireError>,
}

#[derive(Debug, Deserialize)]
pub struct WireError {
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WireChoice {
    #[serde(default)]
    pub delta: Option<WireDelta>,
    #[serde(default)]
    pub finish_reason: Option<String>,
    /// `choices[].index`:候选序号。`n>1` 时同一帧会有多条 choice,
    /// 只认第一条(规范里 index 0 是主候选),避免把多个候选的正文拼在一起。
    #[serde(default)]
    pub index: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct WireDelta {
    /// Present on first chunks; the translator infers roles from fields.
    #[serde(default)]
    #[allow(dead_code)]
    pub role: Option<String>,
    /// 正文。规范是字符串,但实测也有网关发数组(多模态 parts)或对象,
    /// 所以这里收成原始 JSON,再由 [`WireDelta::content_text`] 归一成文本。
    #[serde(default)]
    pub content: Option<serde_json::Value>,
    /// 标准的推理通道(DeepSeek 及部分网关用)。
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// 模型的拒答正文(OpenAI 的 `delta.refusal`)。它不是正文,是拒答,
    /// 单独取出来按拒答处理,不能当正常回答往下走。
    #[serde(default)]
    pub refusal: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<WireToolCallDelta>>,
    /// 旧式的单函数调用(OpenAI 早于 tool_calls 的形状),部分网关仍在发。
    #[serde(default)]
    pub function_call: Option<WireFunctionDelta>,
    /// 供应商自造的其余字段。
    ///
    /// 承载推理的字段名没有统一标准:除标准的 `reasoning_content` 外,各家
    /// 网关自造的名字都见过(`reasoning`、`thinking`、`reasoning_details` …)。
    /// 所以不按供应商列白名单,而是按语义在 [`WireDelta::reasoning_text`]
    /// 里识别——新供应商换了字段名也照样能解析。
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// 承载推理的键名里,出现这些词根即视为推理通道(驼峰/下划线/连字符均覆盖)。
const REASONING_KEY_ROOTS: &[&str] = &["reasoning", "thinking", "thought"];

/// 承载语义的词根。只有推理词根 + 承载语义同时命中才认定,避免把
/// `thought_of`（“想法归属”之类的普通字段）误判成推理通道。
const REASONING_PAYLOAD_ROOTS: &[&str] = &["content", "text", "delta", "detail", "message"];

/// 归一化键名:小写 + 非字母数字一律换成空格(驼峰也会被切开)。
fn normalize_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut previous_lower = false;
    for ch in key.chars() {
        if ch.is_ascii_alphanumeric() {
            let lower = ch.to_ascii_lowercase();
            // 驼峰边界:大写字母前插空格,让 ReasoningText 切成两个词元。
            if ch.is_ascii_uppercase() && previous_lower {
                out.push(' ');
            }
            out.push(lower);
            previous_lower = lower.is_ascii_lowercase() || lower.is_ascii_digit();
        } else {
            out.push(' ');
            previous_lower = false;
        }
    }
    out
}

/// 键名是否属于推理族。
///
/// 判定式:键名里出现推理词根,且键名还带一个“承载内容”的语义词根。
/// - `reasoning` / `reasoning_content` / `reasoning_details` / `ThinkingText`
///   → 命中(推理词根 + 承载词根);
/// - `reasoning` 单独一个键 → 命中(推理词根本身即承载);
/// - `thought_of` / `thinking_about` → 不命中(无承载语义,不是推理正文)。
fn is_reasoning_key(key: &str) -> bool {
    let normalized = normalize_key(key);
    let has_reasoning_root = REASONING_KEY_ROOTS
        .iter()
        .any(|root| normalized.contains(root));
    if !has_reasoning_root {
        return false;
    }
    // 键名归一化后只剩推理词根本身(如 `reasoning`)时,本身就是承载字段。
    let tokens: Vec<&str> = normalized.split_whitespace().collect();
    let only_reasoning_root = tokens
        .iter()
        .all(|token| REASONING_KEY_ROOTS.contains(token) || token.is_empty());
    if only_reasoning_root {
        return true;
    }
    REASONING_PAYLOAD_ROOTS
        .iter()
        .any(|root| normalized.contains(root))
}

/// 条目类型是否属于「只有密文/签名、没有可读正文」的一类。
fn is_opaque_reasoning_kind(kind: &str) -> bool {
    const OPAQUE_TOKENS: &[&str] = &[
        "signature",
        "redacted",
        "encrypted",
        "binary",
        "hash",
        "cipher",
    ];
    normalize_key(kind)
        .split_whitespace()
        .any(|token| OPAQUE_TOKENS.contains(&token))
}

/// 结构化推理条目里的正文可能落在哪个键。
///
/// 实测各家不统一:OpenRouter 的 `reasoning.text` 条目用 `text`,而
/// `reasoning.summary` 条目用 `summary`;别的网关还用 `content`/`value`。
/// 按顺序挨个找,第一个非空的即正文。
const DETAIL_TEXT_KEYS: &[&str] = &["text", "summary", "content", "value", "delta"];

/// 从结构化推理数组里取第一条可读正文。
fn detail_text(items: &[serde_json::Value]) -> Option<&str> {
    items.iter().find_map(|item| {
        let object = item.as_object()?;
        if let Some(kind) = object.get("type").and_then(|value| value.as_str())
            && is_opaque_reasoning_kind(kind)
        {
            // 签名/密文类条目没有正文,不产出增量。
            return None;
        }
        DETAIL_TEXT_KEYS
            .iter()
            .find_map(|key| object.get(*key).and_then(|value| value.as_str()))
            .filter(|text| !text.is_empty())
    })
}

impl WireDelta {
    /// 本帧的正文(增量语义),没有则 `None`。
    ///
    /// 规范里 `content` 是字符串,但实测有网关发数组(多模态 parts:
    /// `[{"type":"text","text":"..."}]`)或对象(`{"text":"..."}`)。三种形态
    /// 归一成同一段文本,不让形态差异变成“正文丢失”。
    ///
    /// 数组形态需要拼接多处文本,故返回 `Cow`:字符串/对象形态不分配。
    fn content_text(&self) -> Option<std::borrow::Cow<'_, str>> {
        match self.content.as_ref()? {
            serde_json::Value::String(text) => Some(std::borrow::Cow::Borrowed(text)),
            serde_json::Value::Array(parts) => {
                // 多模态 parts:只取文本类型的部分,图片等非文本 part 跳过。
                let mut joined = String::new();
                for part in parts {
                    let Some(object) = part.as_object() else {
                        continue;
                    };
                    let is_text = object
                        .get("type")
                        .and_then(|value| value.as_str())
                        .is_none_or(|kind| kind == "text" || kind == "output_text");
                    if !is_text {
                        continue;
                    }
                    if let Some(text) = object.get("text").and_then(|value| value.as_str()) {
                        joined.push_str(text);
                    }
                }
                (!joined.is_empty()).then_some(std::borrow::Cow::Owned(joined))
            }
            serde_json::Value::Object(object) => object
                .get("text")
                .and_then(|value| value.as_str())
                .filter(|text| !text.is_empty())
                .map(std::borrow::Cow::Borrowed),
            _ => None,
        }
    }

    /// 本帧的推理正文(增量语义),没有则 `None`。
    ///
    /// 按语义识别而非供应商白名单:
    /// - 键名属推理族(`reasoning` / `thinking` / `thought` 词根)且值是字符串 → 取之;
    /// - 键名属推理族且值是数组 → 取条目里的正文(`text`/`summary`/…,
    ///   跳过签名/密文类条目);
    /// - 扁平字符串优先于数组:两者常携带同一段内容,取其一才不重复记录。
    fn reasoning_text(&self) -> Option<&str> {
        if let Some(text) = self.reasoning_content.as_deref().filter(|t| !t.is_empty()) {
            return Some(text);
        }
        let mut from_array = None;
        for (key, value) in &self.extra {
            if !is_reasoning_key(key) {
                continue;
            }
            match value {
                serde_json::Value::String(text) if !text.is_empty() => return Some(text),
                serde_json::Value::Array(items) if from_array.is_none() => {
                    from_array = detail_text(items);
                }
                _ => {}
            }
        }
        from_array
    }
}

#[derive(Debug, Deserialize)]
pub struct WireToolCallDelta {
    /// 规范要求每个 tool-call 增量帧都带 `index`,但实测有网关只在首帧带、
    /// 续帧省略,甚至整条调用都不带。缺省交给 [`StreamTranslator::resolve_wire_index`]
    /// 归类,不在此处硬失败。
    #[serde(default)]
    pub index: Option<u32>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<WireFunctionDelta>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct WireFunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WireUsage {
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    #[serde(default)]
    pub completion_tokens: Option<u64>,
    /// DeepSeek-only: cache hits included in `prompt_tokens`.
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<u64>,
    /// DeepSeek 也报未命中数。与命中数**同时**存在时以命中数为准(命中数是
    /// 权威口径);只有未命中数时,缓存读 = 总 prompt − 未命中数。
    #[serde(default)]
    pub prompt_cache_miss_tokens: Option<u64>,
    /// OpenAI-style cache accounting.
    #[serde(default)]
    pub prompt_tokens_details: Option<WirePromptTokensDetails>,
    #[serde(default)]
    pub completion_tokens_details: Option<WireCompletionTokensDetails>,
}

#[derive(Debug, Deserialize)]
pub struct WirePromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct WireCompletionTokensDetails {
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
}

/// 一种 wire 风格的缓存记账方式。
///
/// 两种风格**都**在总 prompt 里含缓存命中(即 `cache_read` 是总 prompt 的
/// **子集**),区别只在命中数放在哪个字段。归一后的契约由
/// `denia_core::stream::TokenUsage` 定义:`input_tokens` 是**未缓存输入**,
/// 与 `cache_read_tokens` 互斥。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageStyle {
    /// DeepSeek 原生:命中数在 `prompt_cache_hit_tokens`。
    DeepSeek,
    /// OpenAI 系(`chat.completions` 与 `responses` 同构):命中数在
    /// `prompt_tokens_details.cached_tokens` / `input_tokens_details.cached_tokens`。
    OpenAi,
}

pub fn map_usage(wire: &WireUsage, style: UsageStyle) -> TokenUsage {
    let prompt = wire.prompt_tokens.unwrap_or(0);
    let cache_read = match style {
        UsageStyle::DeepSeek => wire.prompt_cache_hit_tokens.or_else(|| {
            // 只有未命中数时反推命中数:命中 = 总 prompt − 未命中。
            wire.prompt_cache_miss_tokens
                .map(|miss| prompt.saturating_sub(miss))
        }),
        UsageStyle::OpenAi => wire
            .prompt_tokens_details
            .as_ref()
            .and_then(|details| details.cached_tokens)
            // DeepSeek-compatible endpoints on the generic route still report
            // native cache fields; parsing them is wire compatibility, not a
            // dedicated adapter.
            .or(wire.prompt_cache_hit_tokens)
            .or_else(|| {
                wire.prompt_cache_miss_tokens
                    .map(|miss| prompt.saturating_sub(miss))
            }),
    };
    // 两种风格都要减:`prompt_tokens` 报的是**含缓存的总 prompt**。
    // 不减的后果是 `input_tokens` 变成总 prompt,而下游(core 的契约、
    // token-meter 的 `uncached_input_tokens`、前端的命中率与上下文环)
    // 一律按"未缓存"消费 —— 命中率会被算成 cache/(cache+未缓存),腰斩一半。
    let input_tokens = prompt.saturating_sub(cache_read.unwrap_or(0));
    TokenUsage {
        input_tokens,
        output_tokens: wire.completion_tokens.unwrap_or(0),
        cache_read_tokens: cache_read,
        reasoning_tokens: wire
            .completion_tokens_details
            .as_ref()
            .and_then(|details| details.reasoning_tokens),
    }
}

/// Wire `finish_reason` to the chunk protocol; unknown reasons are errors.
pub fn map_finish_reason(raw: &str) -> FinishReason {
    match raw {
        "stop" => FinishReason::Stop,
        "tool_calls" => FinishReason::ToolCalls,
        "length" => FinishReason::MaxTokens,
        other => FinishReason::Error {
            failure: LlmFailure::new(
                other.to_ascii_uppercase(),
                format!("model stopped with unknown reason: {other}"),
            ),
        },
    }
}

#[derive(Debug, Serialize)]
pub struct WireMessage {
    pub role: &'static str,
    /// Always present for system/user/assistant (empty string when the
    /// assistant turn is tool-call-only); tool messages carry their output.
    /// 多模态:带图片时是 OpenAI content-parts 数组,否则是普通字符串。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<WireAssistantToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// One assistant tool call on the wire (OpenAI shape; DeepSeek speaks it too).
#[derive(Debug, Serialize)]
pub struct WireAssistantToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: &'static str,
    pub function: WireFunctionRef,
}

#[derive(Debug, Serialize)]
pub struct WireFunctionRef {
    pub name: String,
    pub arguments: String,
}

/// 回传清洗:部分网关(如 MiniMax)会把 arguments 再解析成 dict,
/// 畸形原始串会毒化下一轮请求。抢救第一个 JSON 值,否则 {}。
pub(crate) fn wire_arguments(raw: &str) -> String {
    if serde_json::from_str::<serde_json::Value>(raw).is_ok() {
        return raw.to_string();
    }
    let mut iter = serde_json::Deserializer::from_str(raw.trim()).into_iter::<serde_json::Value>();
    match iter.next() {
        Some(Ok(value)) => value.to_string(),
        _ => "{}".to_string(),
    }
}
/// Serializes the request history into wire messages.
pub fn build_wire_messages(request: &GenerateRequest) -> Vec<WireMessage> {
    let plain = |role: &'static str, content: String| WireMessage {
        role,
        content: Some(serde_json::Value::String(content)),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
    };
    let mut out = Vec::new();
    if let Some(system) = &request.system
        && !system.is_empty()
    {
        out.push(plain("system", system.clone()));
    }
    for message in &request.messages {
        match message.role {
            ChatRole::System => out.push(plain("system", message.content.clone())),
            ChatRole::User => {
                if message.images.is_empty() {
                    out.push(plain("user", message.content.clone()));
                } else {
                    // 多模态 content-parts:文本 + image_url(data URL)。
                    let mut parts = vec![serde_json::json!({
                        "type": "text",
                        "text": message.content,
                    })];
                    for image in &message.images {
                        parts.push(serde_json::json!({
                            "type": "image_url",
                            "image_url": {
                                "url": format!("data:{};base64,{}", image.mime, image.data),
                            },
                        }));
                    }
                    out.push(WireMessage {
                        role: "user",
                        content: Some(serde_json::Value::Array(parts)),
                        reasoning_content: None,
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
            }
            ChatRole::Assistant => {
                let calls = (!message.tool_calls.is_empty()).then(|| {
                    message
                        .tool_calls
                        .iter()
                        .map(|call| WireAssistantToolCall {
                            id: call.id.clone(),
                            call_type: "function",
                            function: WireFunctionRef {
                                name: call.name.clone(),
                                arguments: wire_arguments(&call.arguments),
                            },
                        })
                        .collect()
                });
                out.push(WireMessage {
                    role: "assistant",
                    content: Some(serde_json::Value::String(message.content.clone())),
                    reasoning_content: message.reasoning_content.clone(),
                    tool_calls: calls,
                    tool_call_id: None,
                });
            }
            ChatRole::Tool => {
                // A tool result without its call id would break provider
                // correlation; the projection always sets it.
                let Some(call_id) = &message.tool_call_id else {
                    debug_assert!(false, "tool message without tool_call_id dropped");
                    continue;
                };
                // 带图工具结果(如 browser 截图):content 用多模态数组,
                // 与 user 消息的图片编码同一格式;纯文本保持字符串(最大兼容)。
                let content = if message.images.is_empty() {
                    serde_json::Value::String(message.content.clone())
                } else {
                    let mut parts = vec![serde_json::json!({
                        "type": "text",
                        "text": message.content,
                    })];
                    for image in &message.images {
                        parts.push(serde_json::json!({
                            "type": "image_url",
                            "image_url": {
                                "url": format!("data:{};base64,{}", image.mime, image.data)
                            },
                        }));
                    }
                    serde_json::Value::Array(parts)
                };
                out.push(WireMessage {
                    role: "tool",
                    content: Some(content),
                    reasoning_content: None,
                    tool_calls: None,
                    tool_call_id: Some(call_id.clone()),
                });
            }
        }
    }
    out
}

/// The OpenAI-family `tools` array; both adapters share the shape.
pub fn build_wire_tools(tools: &[denia_core::tool::ToolSchema]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                }
            })
        })
        .collect()
}

#[derive(Debug, Default)]
struct OpenBlock {
    index: u32,
    text: String,
}

#[derive(Debug, Default)]
struct ToolCallState {
    index: u32,
    id: String,
    name: String,
    arguments: String,
}

/// The wire-delta state machine. One open reasoning block, one open text
/// block, and one tool-call state per wire index; block indices are assigned
/// in open order and are unique across block types.
#[derive(Debug, Default)]
pub struct StreamTranslator {
    next_index: u32,
    reasoning: Option<OpenBlock>,
    text: Option<OpenBlock>,
    tool_calls: BTreeMap<u32, ToolCallState>,
    usage: Option<TokenUsage>,
    finish: Option<FinishReason>,
    saw_block: bool,
    /// 未决的 content 前缀。`Some` = 内联 think 兼容开启且还在攒(等 think 收尾标记
    /// 或封顶);`None` = 未开启或已定性。见 [`Self::with_inline_think_tags`]。
    lead: Option<String>,
    /// 流中出现的帧级错误(OpenAI 的流会在 chunk 里带 `error` 对象)。
    /// 记下来,在收尾时转成 Error finish,不让错误被静默吞掉。
    pending_error: Option<LlmFailure>,
}

/// 上游把推理内联进 content 时,答案前遗留的 think 收尾标记(opencode2api/zen 免费档
/// 实测:开头的 `<think>` 缺失,只剩 `</think>`)。JSON 里该串会被转义成 `\u003c/think\u003e`,
/// 但 SSE 帧解码后就是普通文本。
const INLINE_THINK_CLOSE: &str = "</think>";

/// 未决前缀的封顶。超过仍未见收尾标记,就认定「没有内联推理」,原样按正文放行——
/// 避免给正常 provider 的流式首字带来无上限的缓冲延迟。
const INLINE_THINK_LEAD_CAP: usize = 32 * 1024;

impl StreamTranslator {
    /// 开启内联 think 兼容:有的 chat-completions 网关把推理内联进 `content`
    /// (以 `</think>` 收尾、开头的 `<think>` 被上游模板吃掉),而不是走标准
    /// `reasoning_content`。开启后先把 content 前缀攒进 `lead`,遇到首个收尾标记
    /// 再拆成推理块 + 正文块。定向兼容,默认关闭。
    pub fn with_inline_think_tags(inline_think_tags: bool) -> Self {
        Self {
            lead: inline_think_tags.then(String::new),
            ..Self::default()
        }
    }

    fn take_index(&mut self) -> u32 {
        let index = self.next_index;
        self.next_index += 1;
        index
    }

    /// 把一帧 tool-call 增量归到它的 wire index。帧带 `index` 时原样采用;
    /// 缺 `index`(非规范网关)时按下述规则归类,而不是让整步失败:
    /// - 帧里带非空 `id` 或 `function.name` → 这是"一次新调用"的首帧,取一个未被占用的 index;
    /// - 只剩 `function.arguments` → 该调用的续帧,并入已打开的最大 index;
    /// - 连一个已打开的调用都没有 → 落到 0(尽力归位,后续由参数校验兜底)。
    fn resolve_wire_index(&self, call: &WireToolCallDelta) -> u32 {
        if let Some(index) = call.index {
            return index;
        }
        let opens_new_call = call.id.as_deref().is_some_and(|id| !id.is_empty())
            || call
                .function
                .as_ref()
                .and_then(|function| function.name.as_deref())
                .is_some_and(|name| !name.is_empty());
        if opens_new_call {
            return self.tool_calls.keys().next_back().map_or(0, |k| k + 1);
        }
        self.tool_calls.keys().next_back().copied().unwrap_or(0)
    }

    /// Feeds one wire chunk, returning the delta chunks emitted immediately.
    pub fn feed(&mut self, chunk: &WireChunk, style: UsageStyle) -> Vec<StreamChunk> {
        let mut out = Vec::new();
        // `n>1` 时同一帧会有多条 choice(每个候选一条)。denia 只跑一条流,
        // 所以只取主候选(index 0);没有 index 0 时退回第一条。
        // 不做这个收敛的话,多个候选的正文会被拼成一段乱码。
        let primary = chunk
            .choices
            .iter()
            .find(|choice| choice.index == Some(0))
            .or_else(|| chunk.choices.first());
        for choice in primary.into_iter() {
            if let Some(delta) = &choice.delta {
                self.feed_delta(delta, &mut out);
            }
            if let Some(raw) = choice
                .finish_reason
                .as_deref()
                .filter(|reason| !reason.is_empty())
                && self.finish.is_none()
            {
                self.finish = Some(map_finish_reason(raw));
            }
        }
        if let Some(usage) = &chunk.usage {
            self.usage = Some(map_usage(usage, style));
        }
        if let Some(error) = &chunk.error {
            // 流中带 error 的帧:错误必须让整步失败,不能当空帧跳过。
            self.pending_error = Some(LlmFailure::new(
                error
                    .code
                    .as_deref()
                    .or(error.r#type.as_deref())
                    .unwrap_or("PROVIDER_ERROR")
                    .to_string(),
                error
                    .message
                    .as_deref()
                    .unwrap_or("the provider reported an error mid-stream")
                    .to_string(),
            ));
        }
        out
    }

    /// 取走并清空流中累积的帧级错误(若有)。
    pub fn take_error(&mut self) -> Option<LlmFailure> {
        self.pending_error.take()
    }

    fn feed_delta(&mut self, delta: &WireDelta, out: &mut Vec<StreamChunk>) {
        if let Some(text) = delta.reasoning_text() {
            // 推理走了某个通道:此前若在攒内联前缀,那说明这个网关并不内联推理,
            // 把它原样当正文放行并退出内联模式(避免误把正文当推理)。
            self.flush_pending_lead_as_text(out);
            self.push_reasoning(text, out);
        }
        if let Some(text) = delta.content_text().filter(|t| !t.is_empty()) {
            if self.lead.is_some() {
                self.push_lead(&text, out);
            } else {
                self.push_text(&text, out);
            }
        }
        if let Some(text) = delta.refusal.as_deref().filter(|t| !t.is_empty()) {
            // 拒答不是正文:单独开一个文本块,让上层能识别“模型拒答”而不是
            // 把拒答当正常回答交付出去。
            self.flush_pending_lead_as_text(out);
            self.push_text(text, out);
        }
        if let Some(tool_calls) = &delta.tool_calls {
            self.flush_pending_lead_as_text(out);
            for wire_call in tool_calls {
                self.push_tool_call(wire_call, out);
            }
        }
        if let Some(function_call) = &delta.function_call {
            // 旧式单函数调用(OpenAI 早于 tool_calls 的形状)。归成一条工具调用,
            // 走与 tool_calls 完全相同的路径,不复用已有状态。
            self.flush_pending_lead_as_text(out);
            self.push_tool_call(
                &WireToolCallDelta {
                    index: None,
                    id: None,
                    function: Some(function_call.clone()),
                },
                out,
            );
        }
    }

    /// 归入并累积一条工具调用增量。
    fn push_tool_call(&mut self, wire_call: &WireToolCallDelta, out: &mut Vec<StreamChunk>) {
        let wire_index = self.resolve_wire_index(wire_call);
        if !self.tool_calls.contains_key(&wire_index) {
            let index = self.take_index();
            self.saw_block = true;
            out.push(StreamChunk::BlockStart {
                index,
                block_type: BlockType::ToolCall,
            });
            self.tool_calls.insert(
                wire_index,
                ToolCallState {
                    index,
                    id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                },
            );
        }
        let entry = self.tool_calls.get_mut(&wire_index).unwrap();
        if let Some(id) = wire_call.id.as_deref().filter(|id| !id.is_empty())
            && entry.id.is_empty()
        {
            entry.id = id.to_string();
        }
        if let Some(function) = &wire_call.function {
            let mut first_name: Option<String> = None;
            if let Some(name) = function.name.as_deref().filter(|n| !n.is_empty())
                && entry.name.is_empty()
            {
                entry.name = name.to_string();
                first_name = Some(name.to_string());
            }
            if let Some(arguments) = &function.arguments {
                entry.arguments.push_str(arguments);
                out.push(StreamChunk::ToolCallDelta {
                    index: entry.index,
                    id: entry.id.clone(),
                    name: first_name,
                    arguments_delta: arguments.clone(),
                });
            }
        }
    }

    /// 打开(或复用)推理块并追加一段推理文本。
    fn push_reasoning(&mut self, text: &str, out: &mut Vec<StreamChunk>) {
        let index = match &mut self.reasoning {
            Some(block) => block.index,
            None => {
                let index = self.take_index();
                self.saw_block = true;
                out.push(StreamChunk::BlockStart {
                    index,
                    block_type: BlockType::Reasoning,
                });
                self.reasoning = Some(OpenBlock {
                    index,
                    text: String::new(),
                });
                index
            }
        };
        self.reasoning.as_mut().unwrap().text.push_str(text);
        out.push(StreamChunk::ReasoningDelta {
            index,
            text: text.to_string(),
        });
    }

    /// 打开(或复用)正文块并追加一段文本。
    fn push_text(&mut self, text: &str, out: &mut Vec<StreamChunk>) {
        let index = match &mut self.text {
            Some(block) => block.index,
            None => {
                let index = self.take_index();
                self.saw_block = true;
                out.push(StreamChunk::BlockStart {
                    index,
                    block_type: BlockType::Text,
                });
                self.text = Some(OpenBlock {
                    index,
                    text: String::new(),
                });
                index
            }
        };
        self.text.as_mut().unwrap().text.push_str(text);
        out.push(StreamChunk::TextDelta {
            index,
            text: text.to_string(),
        });
    }

    /// 内联 think 兼容:把 content 先攒进 `lead`,直到看见 think 收尾标记
    /// (标记前是推理、标记后是正文),或超过封顶/始终没有标记(整段就是正文)。
    /// 标记可能被切在两帧之间,所以是按累计缓冲查找,而非逐帧匹配。
    fn push_lead(&mut self, text: &str, out: &mut Vec<StreamChunk>) {
        let buffer = match self.lead.as_mut() {
            Some(buffer) => buffer,
            None => return,
        };
        buffer.push_str(text);
        let outcome = match buffer.find(INLINE_THINK_CLOSE) {
            Some(pos) => Some((
                buffer[..pos].to_string(),
                buffer[pos + INLINE_THINK_CLOSE.len()..].to_string(),
            )),
            None if buffer.len() > INLINE_THINK_LEAD_CAP => {
                Some((String::new(), std::mem::take(buffer)))
            }
            None => None,
        };
        if let Some((head, rest)) = outcome {
            self.lead = None;
            if !head.is_empty() {
                self.push_reasoning(&head, out);
            }
            if !rest.is_empty() {
                self.push_text(&rest, out);
            }
        }
    }

    /// 结束内联判定:把还没定性的前缀原样当正文放行。
    fn flush_pending_lead_as_text(&mut self, out: &mut Vec<StreamChunk>) {
        if let Some(pending) = self.lead.take().filter(|pending| !pending.is_empty()) {
            self.push_text(&pending, out);
        }
    }

    /// Closes every open block, then emits usage and finish. A `stop` finish
    /// with zero blocks is the `EMPTY_RESPONSE` error.
    pub fn finalize(&mut self) -> Vec<StreamChunk> {
        let mut out = Vec::new();
        self.flush_pending_lead_as_text(&mut out);
        if let Some(block) = self.reasoning.take() {
            out.push(StreamChunk::BlockEnd {
                index: block.index,
                block: ContentBlock::Reasoning {
                    text: block.text,
                    replay: None,
                },
            });
        }
        if let Some(block) = self.text.take() {
            out.push(StreamChunk::BlockEnd {
                index: block.index,
                block: ContentBlock::Text { text: block.text },
            });
        }
        for (_, call) in std::mem::take(&mut self.tool_calls) {
            out.push(StreamChunk::BlockEnd {
                index: call.index,
                block: ContentBlock::ToolCall {
                    id: call.id,
                    name: call.name,
                    arguments: call.arguments,
                    incomplete: false,
                },
            });
        }
        if let Some(usage) = self.usage.take() {
            out.push(StreamChunk::Usage { usage });
        }
        let mut reason = self.finish.take().unwrap_or(FinishReason::Stop);
        if let Some(failure) = self.pending_error.take() {
            // 流中报错优先于一切:即使已经收到正文,也必须让整步失败。
            reason = FinishReason::Error { failure };
        } else if matches!(reason, FinishReason::Stop) && !self.saw_block {
            reason = FinishReason::Error {
                failure: LlmFailure::new(
                    codes::EMPTY_RESPONSE,
                    "the model produced no output blocks",
                ),
            };
        }
        out.push(StreamChunk::Finish { reason });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{
        StreamTranslator, UsageStyle, WireChunk, WireUsage, build_wire_messages, map_usage,
    };

    fn chunk(json: &str) -> WireChunk {
        serde_json::from_str(json).unwrap()
    }

    fn collect_reasoning(all: &[denia_core::stream::StreamChunk]) -> String {
        all.iter()
            .filter_map(|c| match c {
                denia_core::stream::StreamChunk::ReasoningDelta { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn collect_text(all: &[denia_core::stream::StreamChunk]) -> String {
        all.iter()
            .filter_map(|c| match c {
                denia_core::stream::StreamChunk::TextDelta { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// 非标准推理字段(实测:OpenRouter 的 `minimax/minimax-m3.1-flash:free` 把
    /// 推理放在 `reasoning` / `reasoning_details`)原本被整体丢弃,导致首字延迟
    /// 期间界面全静默。
    #[test]
    fn nonstandard_reasoning_fields_become_reasoning_block() {
        let mut translator = StreamTranslator::default();
        let mut all = Vec::new();
        all.extend(translator.feed(
            &chunk(
                r#"{"choices":[{"delta":{"content":"","role":"assistant","reasoning":"We need answer likely","reasoning_details":[{"type":"reasoning.text","text":"We need answer likely","format":"unknown","index":0}]},"finish_reason":null}]}"#,
            ),
            UsageStyle::OpenAi,
        ));
        all.extend(translator.feed(
            &chunk(
                r#"{"choices":[{"delta":{"content":"","role":"assistant","reasoning":" create HTML file in repo."}}]}"#,
            ),
            UsageStyle::OpenAi,
        ));
        all.extend(translator.feed(
            &chunk(r#"{"choices":[{"delta":{"content":"好的，我来创建"}}]}"#),
            UsageStyle::OpenAi,
        ));
        all.extend(translator.finalize());

        // 两个字段内容相同,只能记一遍。
        assert_eq!(
            collect_reasoning(&all),
            "We need answer likely create HTML file in repo."
        );
        assert_eq!(collect_text(&all), "好的，我来创建");
    }

    /// 推理字段的名字没有统一标准:只要键名属推理族,就该被认出来。
    /// 换供应商换字段名,不该又要改解析器。
    #[test]
    fn any_reasoning_family_key_is_recognized() {
        for key in [
            "reasoning",
            "reasoning_content",
            "thinking",
            "thought",
            "reasoning_text",
            "ReasoningText",
        ] {
            let mut translator = StreamTranslator::default();
            let frame = format!(r#"{{"choices":[{{"delta":{{"{key}":"片段-{key}"}}}}]}}"#);
            let emitted = translator.feed(&chunk(&frame), UsageStyle::OpenAi);
            assert_eq!(
                collect_reasoning(&emitted),
                format!("片段-{key}"),
                "键名 {key} 应被识别为推理通道"
            );
        }
    }

    /// 推理族字段里,只有数组形态时也要能取出正文。
    #[test]
    fn reasoning_array_field_yields_text() {
        let mut translator = StreamTranslator::default();
        let emitted = translator.feed(
            &chunk(
                r#"{"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","text":"only details","index":0}]}}]}"#,
            ),
            UsageStyle::OpenAi,
        );
        assert_eq!(collect_reasoning(&emitted), "only details");
    }

    /// 签名/密文类条目没有可读正文,不得产出空增量。
    #[test]
    fn opaque_reasoning_entries_yield_no_text() {
        let mut translator = StreamTranslator::default();
        let emitted = translator.feed(
            &chunk(
                r#"{"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.signature","signature":"abc"},{"type":"reasoning.redacted","data":"xxx"}]}}]}"#,
            ),
            UsageStyle::OpenAi,
        );
        assert!(emitted.is_empty());
    }

    /// 非推理族字段不得被误判成推理。
    #[test]
    fn unrelated_fields_are_not_reasoning() {
        let mut translator = StreamTranslator::default();
        let emitted = translator.feed(
            &chunk(
                r#"{"choices":[{"delta":{"content":"正文","source":"x","thought_of":"no","metadata":{"a":1}}}]}"#,
            ),
            UsageStyle::OpenAi,
        );
        assert!(collect_reasoning(&emitted).is_empty());
        assert_eq!(collect_text(&emitted), "正文");
    }

    /// 标准 `reasoning_content` 仍然优先,不被其它推理族字段抢走。
    #[test]
    fn standard_reasoning_content_wins_over_other_fields() {
        let mut translator = StreamTranslator::default();
        let emitted = translator.feed(
            &chunk(
                r#"{"choices":[{"delta":{"reasoning_content":"standard","reasoning":"other","reasoning_details":[{"type":"reasoning.text","text":"other"}]}}]}"#,
            ),
            UsageStyle::OpenAi,
        );
        assert_eq!(collect_reasoning(&emitted), "standard");
    }

    /// 内联 think 兼容:网关把推理内联进 content(`</think>` 收尾、开头 `<think>` 缺失)
    /// 时,首个收尾标记之前归推理块、之后归正文块;标记可能被切在两帧之间。
    #[test]
    fn inline_think_tags_split_reasoning_from_text() {
        let mut translator = StreamTranslator::with_inline_think_tags(true);
        let mut all = Vec::new();
        all.extend(translator.feed(
            &chunk(r#"{"choices":[{"delta":{"content":"The user just said hi.</"}}]}"#),
            UsageStyle::OpenAi,
        ));
        all.extend(translator.feed(
            &chunk(r#"{"choices":[{"delta":{"content":"think>Hi! How can I help?"}}]}"#),
            UsageStyle::OpenAi,
        ));
        all.extend(translator.finalize());

        assert_eq!(collect_reasoning(&all), "The user just said hi.");
        assert_eq!(collect_text(&all), "Hi! How can I help?");
        // 收尾标记是上游噪音,绝不能泄漏进任何块。
        let leaked = format!("{}{}", collect_reasoning(&all), collect_text(&all));
        assert!(!leaked.contains("</think>"), "{leaked}");
    }

    /// 默认关闭:同一个内联 content 原样作为正文(不认识该约定的 provider 行为不变)。
    #[test]
    fn inline_think_tags_off_keeps_content_verbatim() {
        let mut translator = StreamTranslator::default();
        let mut all = translator.feed(
            &chunk(r#"{"choices":[{"delta":{"content":"reasoning</think>answer"}}]}"#),
            UsageStyle::OpenAi,
        );
        all.extend(translator.finalize());

        assert!(collect_reasoning(&all).is_empty());
        assert_eq!(collect_text(&all), "reasoning</think>answer");
    }

    /// 开启兼容但没有收尾标记:整段就是正文(短流由 finalize 兜底放行)。
    #[test]
    fn inline_think_tags_without_marker_flush_as_text() {
        let mut translator = StreamTranslator::with_inline_think_tags(true);
        let mut all = translator.feed(
            &chunk(r#"{"choices":[{"delta":{"content":"just a normal answer"}}]}"#),
            UsageStyle::OpenAi,
        );
        all.extend(translator.finalize());

        assert!(collect_reasoning(&all).is_empty());
        assert_eq!(collect_text(&all), "just a normal answer");
    }

    /// 网关若走规范 reasoning_content 通道,内联缓冲必须让位,不得把正文误判为推理。
    #[test]
    fn inline_think_tags_yield_to_real_reasoning_channel() {
        let mut translator = StreamTranslator::with_inline_think_tags(true);
        let mut all = translator.feed(
            &chunk(r#"{"choices":[{"delta":{"reasoning_content":"proper reasoning"}}]}"#),
            UsageStyle::OpenAi,
        );
        all.extend(translator.feed(
            &chunk(r#"{"choices":[{"delta":{"content":"answer"}}]}"#),
            UsageStyle::OpenAi,
        ));
        all.extend(translator.finalize());

        assert_eq!(collect_reasoning(&all), "proper reasoning");
        assert_eq!(collect_text(&all), "answer");
    }

    /// 封顶兜底:未决前缀超过 32KB 仍未见到收尾标记时,整段按正文放行——
    /// 不能为了等标记把正常流式的首字无限缓冲下去。
    #[test]
    fn inline_think_tags_lead_cap_flushes_as_text() {
        let mut translator = StreamTranslator::with_inline_think_tags(true);
        let first = "a".repeat(20 * 1024);
        let second = "b".repeat(20 * 1024);
        let frame = serde_json::json!({"choices":[{"delta":{"content":&first}}]}).to_string();
        let mut all = translator.feed(&chunk(&frame), UsageStyle::OpenAi);
        // 未超封顶:还在内联判定,什么都不发。
        assert!(all.is_empty());
        let frame = serde_json::json!({"choices":[{"delta":{"content":&second}}]}).to_string();
        all.extend(translator.feed(&chunk(&frame), UsageStyle::OpenAi));
        all.extend(translator.finalize());

        assert!(collect_reasoning(&all).is_empty());
        assert_eq!(collect_text(&all), format!("{first}{second}"));
    }

    /// 内联判定期间出现 tool_calls:未决前缀按正文放行,不阻塞工具调用。
    #[test]
    fn inline_think_tags_flush_on_tool_calls() {
        let mut translator = StreamTranslator::with_inline_think_tags(true);
        let mut all = translator.feed(
            &chunk(r#"{"choices":[{"delta":{"content":"leftover prose"}}]}"#),
            UsageStyle::OpenAi,
        );
        assert!(all.is_empty());
        all.extend(translator.feed(
            &chunk(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"edit","arguments":"{}"}}]}}]}"#,
            ),
            UsageStyle::OpenAi,
        ));
        all.extend(translator.finalize());

        assert!(collect_reasoning(&all).is_empty());
        assert_eq!(collect_text(&all), "leftover prose");
        assert!(all.iter().any(|c| matches!(
            c,
            denia_core::stream::StreamChunk::BlockEnd {
                block: denia_core::stream::ContentBlock::ToolCall { name, .. },
                ..
            } if name == "edit"
        )));
    }

    #[test]
    fn reasoning_then_text_then_finish() {
        let mut translator = StreamTranslator::default();
        let mut all = Vec::new();
        all.extend(translator.feed(
            &chunk(r#"{"choices":[{"delta":{"reasoning_content":"thinking..."}}]}"#),
            UsageStyle::DeepSeek,
        ));
        all.extend(translator.feed(
            &chunk(r#"{"choices":[{"delta":{"content":"hello"}}]}"#),
            UsageStyle::DeepSeek,
        ));
        all.extend(translator.feed(&chunk(
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":3,"prompt_cache_hit_tokens":2}}"#,
        ), UsageStyle::DeepSeek));
        all.extend(translator.finalize());

        let kinds: Vec<&str> = all
            .iter()
            .map(|c| match c {
                denia_core::stream::StreamChunk::BlockStart { .. } => "block-start",
                denia_core::stream::StreamChunk::ReasoningDelta { .. } => "reasoning-delta",
                denia_core::stream::StreamChunk::TextDelta { .. } => "text-delta",
                denia_core::stream::StreamChunk::BlockEnd { .. } => "block-end",
                denia_core::stream::StreamChunk::Usage { .. } => "usage",
                denia_core::stream::StreamChunk::Finish { .. } => "finish",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "block-start",
                "reasoning-delta",
                "block-start",
                "text-delta",
                "block-end",
                "block-end",
                "usage",
                "finish"
            ]
        );
        // Cache hits are subtracted from DeepSeek prompt tokens.
        let usage = all
            .iter()
            .find_map(|c| match c {
                denia_core::stream::StreamChunk::Usage { usage } => Some(*usage),
                _ => None,
            })
            .unwrap();
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.cache_read_tokens, Some(2));
    }

    /// 口径契约:两种 wire 风格归一到同一个 `TokenUsage` 语义 ——
    /// `input_tokens` 是**未缓存**输入,与 `cache_read_tokens` 互斥。
    ///
    /// 这条守的是一个曾经真实发生的 bug:OpenAI 系分支直接把
    /// `prompt_tokens`(含缓存的总 prompt)当成了 `input_tokens`,于是下游
    /// (前端的命中率公式、token-meter 的 `uncached_input_tokens`、上下文环)
    /// 全部按"未缓存"消费时把分母算成 cache+未缓存,命中率显示成真值的一半。
    /// 两家的差别只在缓存字段叫什么,减法两边都要做。
    #[test]
    fn both_wire_flavors_normalize_to_uncached_input() {
        // DeepSeek 原生:命中数在 prompt_cache_hit_tokens。
        let deepseek: WireUsage = serde_json::from_str(
            r#"{"prompt_tokens":100,"completion_tokens":7,"prompt_cache_hit_tokens":80}"#,
        )
        .unwrap();
        let mapped = map_usage(&deepseek, UsageStyle::DeepSeek);
        assert_eq!(mapped.input_tokens, 20, "DeepSeek:未缓存 = 100 - 80");
        assert_eq!(mapped.cache_read_tokens, Some(80));

        // OpenAI 系:命中数在 prompt_tokens_details.cached_tokens。
        let openai: WireUsage = serde_json::from_str(
            r#"{"prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":80}}"#,
        )
        .unwrap();
        let mapped = map_usage(&openai, UsageStyle::OpenAi);
        assert_eq!(mapped.input_tokens, 20, "OpenAI:未缓存 = 100 - 80");
        assert_eq!(mapped.cache_read_tokens, Some(80));

        // 没有任何缓存字段:整个 prompt 都是未缓存的(命中为 0)。
        let plain: WireUsage =
            serde_json::from_str(r#"{"prompt_tokens":100,"completion_tokens":7}"#).unwrap();
        let mapped = map_usage(&plain, UsageStyle::OpenAi);
        assert_eq!(mapped.input_tokens, 100);
        assert_eq!(mapped.cache_read_tokens, None);
    }

    #[test]
    fn empty_stop_is_empty_response() {
        let mut translator = StreamTranslator::default();
        translator.feed(
            &chunk(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#),
            UsageStyle::OpenAi,
        );
        let final_chunks = translator.finalize();
        let finish = final_chunks.last().unwrap();
        match finish {
            denia_core::stream::StreamChunk::Finish { reason } => {
                let denia_core::stream::FinishReason::Error { failure } = reason else {
                    panic!("expected error finish, got {reason:?}")
                };
                assert_eq!(failure.code, denia_core::error::codes::EMPTY_RESPONSE);
            }
            other => panic!("expected finish, got {other:?}"),
        }
    }

    #[test]
    fn empty_first_reasoning_delta_opens_no_block() {
        let mut translator = StreamTranslator::default();
        let emitted = translator.feed(
            &chunk(r#"{"choices":[{"delta":{"reasoning_content":""}}]}"#),
            UsageStyle::DeepSeek,
        );
        assert!(emitted.is_empty());
    }

    #[test]
    fn assistant_tool_calls_and_tool_role_serialize() {
        use crate::request::GenerateRequest;
        use denia_core::message::{ChatMessage, ToolCallRef};
        use denia_core::tool::ToolSchema;

        let request = GenerateRequest {
            model: "m".to_string(),
            reasoning_effort: None,
            messages: vec![
                ChatMessage::assistant(
                    "calling",
                    None,
                    vec![ToolCallRef {
                        id: "c1".to_string(),
                        name: "bash".to_string(),
                        arguments: r#"{"command":"ls"}"#.to_string(),
                    }],
                ),
                ChatMessage::tool_result("c1", "exit code: 0"),
            ],
            system: None,
            tools: vec![ToolSchema {
                name: "bash".to_string(),
                description: "run a shell command".to_string(),
                parameters: serde_json::json!({ "type": "object" }),
            }],
            temperature: None,
            max_tokens: None,
            stop: Vec::new(),
            cache_key: None,
        };
        let messages = build_wire_messages(&request);
        let json = serde_json::to_value(&messages).unwrap();
        assert_eq!(json[0]["role"], "assistant");
        assert_eq!(json[0]["content"], "calling");
        assert_eq!(json[0]["tool_calls"][0]["id"], "c1");
        assert_eq!(json[0]["tool_calls"][0]["type"], "function");
        assert_eq!(json[0]["tool_calls"][0]["function"]["name"], "bash");
        assert_eq!(json[1]["role"], "tool");
        assert_eq!(json[1]["tool_call_id"], "c1");
        assert_eq!(json[1]["content"], "exit code: 0");
        assert!(json[1].get("tool_calls").is_none());

        let body = crate::protocols::build_openai_body(&request);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "bash");

        let without_tools = GenerateRequest {
            tools: Vec::new(),
            ..request
        };
        let body = crate::protocols::build_openai_body(&without_tools);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn openai_off_effort_omits_reasoning_fields() {
        use crate::request::GenerateRequest;

        let request = GenerateRequest {
            model: "MiniMax-M3".to_string(),
            reasoning_effort: Some("off".to_string()),
            messages: vec![denia_core::message::ChatMessage::user("title")],
            system: Some("system".to_string()),
            tools: Vec::new(),
            temperature: None,
            max_tokens: Some(64),
            stop: Vec::new(),
            cache_key: None,
        };
        let body = crate::protocols::build_openai_body(&request);
        assert!(body.get("thinking").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["max_tokens"], 64);
    }

    #[test]
    fn openai_effort_sends_reasoning_effort_only() {
        use crate::request::GenerateRequest;

        let request = GenerateRequest {
            model: "gpt-5".to_string(),
            reasoning_effort: Some("high".to_string()),
            messages: vec![denia_core::message::ChatMessage::user("hello")],
            system: None,
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
            stop: Vec::new(),
            cache_key: None,
        };
        let body = crate::protocols::build_openai_body(&request);
        assert!(body.get("thinking").is_none());
        assert_eq!(body["reasoning_effort"], "high");
    }
    #[test]
    fn null_choices_and_null_usage_are_tolerated() {
        // 显式 null 不是"字段缺失",`#[serde(default)]` 兜不住,必须单独宽容。
        let parsed = chunk(r#"{"choices":null,"usage":null}"#);
        assert!(parsed.choices.is_empty());
        assert!(parsed.usage.is_none());
    }

    fn tool_call_ends(
        chunks: &[denia_core::stream::StreamChunk],
    ) -> Vec<denia_core::stream::ContentBlock> {
        chunks
            .iter()
            .filter_map(|c| match c {
                denia_core::stream::StreamChunk::BlockEnd { block, .. } => Some(block.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn tool_call_without_index_is_merged_into_one_call() {
        // 非规范网关:首帧带 id/name、续帧只带 arguments,整条调用一个 index 都不给。
        // 缺 index 不能炸整步,且两次增量必须归并成同一次调用。
        let mut translator = StreamTranslator::default();
        let mut all = translator.feed(
            &chunk(
                r#"{"choices":[{"delta":{"tool_calls":[{"id":"call_1","type":"function","function":{"name":"bash","arguments":"{\"command\":"}}]}}]}"#,
            ),
            UsageStyle::OpenAi,
        );
        all.extend(translator.feed(
            &chunk(
                r#"{"choices":[{"delta":{"tool_calls":[{"function":{"arguments":"\"ls\"}"}}]}}]}"#,
            ),
            UsageStyle::OpenAi,
        ));
        all.extend(translator.finalize());

        let ends = tool_call_ends(&all);
        assert_eq!(ends.len(), 1, "缺 index 的两段增量应归并成一次调用");
        match &ends[0] {
            denia_core::stream::ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "bash");
                assert_eq!(arguments, r#"{"command":"ls"}"#);
            }
            other => panic!("expected tool call, got {other:?}"),
        }
    }

    #[test]
    fn indexless_continuation_without_open_call_is_not_dropped() {
        // 极端形态:只有 arguments 续帧、没有任何首帧。尽力归位到 0,不丢帧。
        let mut translator = StreamTranslator::default();
        let mut all = translator.feed(
            &chunk(r#"{"choices":[{"delta":{"tool_calls":[{"function":{"arguments":"{}"}}]}}]}"#),
            UsageStyle::OpenAi,
        );
        all.extend(translator.finalize());
        assert_eq!(tool_call_ends(&all).len(), 1);
    }

    #[test]
    fn present_index_still_drives_parallel_calls() {
        // 带 index 时保持原语义:两个 index 归成两次独立调用。
        let mut translator = StreamTranslator::default();
        let mut all = translator.feed(
            &chunk(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c0","function":{"name":"bash"}},{"index":1,"id":"c1","function":{"name":"grep"}}]}}]}"#,
            ),
            UsageStyle::OpenAi,
        );
        all.extend(translator.finalize());
        let ends = tool_call_ends(&all);
        assert_eq!(ends.len(), 2);
        let names: Vec<String> = ends
            .iter()
            .map(|block| match block {
                denia_core::stream::ContentBlock::ToolCall { name, .. } => name.clone(),
                other => panic!("expected tool call, got {other:?}"),
            })
            .collect();
        assert_eq!(names, vec!["bash", "grep"]);
    }
}

#[cfg(test)]
mod wire_tests {
    use super::{build_wire_messages, wire_arguments};

    #[test]
    fn wire_arguments_salvages_malformed() {
        assert_eq!(wire_arguments(r#"{"path": "x"}"#), r#"{"path": "x"}"#);
        assert_eq!(
            wire_arguments(r#"{"path": "x"} trailing junk"#),
            r#"{"path":"x"}"#
        );
        assert_eq!(wire_arguments("complete garbage"), "{}");
    }

    #[test]
    fn images_become_multimodal_content_parts() {
        use crate::request::GenerateRequest;
        use denia_core::message::{ChatMessage, ImageData};

        let request = GenerateRequest {
            model: "vision-model".to_string(),
            reasoning_effort: None,
            messages: vec![ChatMessage::user_with_images(
                "看这张图",
                vec![ImageData {
                    mime: "image/png".to_string(),
                    data: "QUJD".to_string(),
                    path: None,
                }],
            )],
            system: None,
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
            stop: Vec::new(),
            cache_key: None,
        };
        let messages = build_wire_messages(&request);
        assert_eq!(messages.len(), 1);
        let content = messages[0].content.as_ref().unwrap();
        let parts = content.as_array().expect("multimodal content is an array");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "看这张图");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,QUJD");
    }

    #[test]
    fn plain_user_stays_a_string() {
        use crate::request::GenerateRequest;
        let request = GenerateRequest {
            model: "m".to_string(),
            reasoning_effort: None,
            messages: vec![denia_core::message::ChatMessage::user("plain")],
            system: None,
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
            stop: Vec::new(),
            cache_key: None,
        };
        let messages = build_wire_messages(&request);
        assert_eq!(messages[0].content.as_ref().unwrap(), "plain");
    }
}
