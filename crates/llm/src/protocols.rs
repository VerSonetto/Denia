//! Wire protocols a custom gateway route may speak, and their translation
//! into the harness chunk protocol.
//!
//! Three protocols, one adapter: `openai-completions` (POST {base}/chat/
//! completions), `openai-responses` (POST {base}/responses), and
//! `anthropic-messages` (POST {root}/v1/messages). Each side has a request
//! body builder and an SSE event translator; the adapter picks a pair by the
//! route's configured protocol and lets the shared SSE driver run it.

use std::collections::BTreeMap;

use denia_core::error::{LlmFailure, codes};
use denia_core::message::ChatRole;
use denia_core::stream::{BlockType, ContentBlock, FinishReason, StreamChunk, TokenUsage};
use serde::{Deserialize, Serialize};

use crate::GenerateRequest;
use crate::wire::{
    StreamTranslator, UsageStyle, WireChunk, build_wire_messages, build_wire_tools, wire_arguments,
};

pub const DONE_MARKER: &str = "[DONE]";
/// Anthropic 的模型列表接口固定要求的版本头(官方文档 stable 版)。
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Anthropic 模型列表单页上限:探测只读一页,不跟 has_more。
const ANTHROPIC_LIST_LIMIT: u64 = 1000;

/// The wire protocol one configured route speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum WireProtocol {
    /// OpenAI Chat Completions(`POST {base}/chat/completions`)。
    #[default]
    #[serde(rename = "openai-completions")]
    ChatCompletions,
    /// OpenAI Responses(`POST {base}/responses`)。
    #[serde(rename = "openai-responses")]
    Responses,
    /// Anthropic Messages(`POST {root}/v1/messages`)。
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
}

impl WireProtocol {
    /// The settings/wire identifier.
    pub fn as_id(self) -> &'static str {
        match self {
            WireProtocol::ChatCompletions => "openai-completions",
            WireProtocol::Responses => "openai-responses",
            WireProtocol::AnthropicMessages => "anthropic-messages",
        }
    }

    /// Parses one identifier, rejecting unknown protocols.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "openai-completions" => Some(Self::ChatCompletions),
            "openai-responses" => Some(Self::Responses),
            "anthropic-messages" => Some(Self::AnthropicMessages),
            _ => None,
        }
    }

    /// Every protocol, in configuration-surface order (most-used first).
    pub const fn all() -> [WireProtocol; 3] {
        [
            WireProtocol::ChatCompletions,
            WireProtocol::Responses,
            WireProtocol::AnthropicMessages,
        ]
    }
}

/// The chat-completions request URL for one route base.
pub fn chat_completions_url(base_url: &str) -> String {
    format!("{}/chat/completions", base_url.trim_end_matches('/'))
}

/// The Responses request URL for one route base.
pub fn responses_url(base_url: &str) -> String {
    format!("{}/responses", base_url.trim_end_matches('/'))
}

/// The Messages request URL for one route base. Both published spellings of
/// the same root are accepted: a base ending in `/v1` keeps it, otherwise
/// `/v1` is inserted — mirroring the listing URL's normalization.
pub fn anthropic_messages_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

/// The model-listing URL for one protocol and route base. OpenAI protocols
/// list at `{base}/models`; Anthropic lists at `{root}/v1/models`, where the
/// root is the base without one trailing `/v1` segment.
pub fn listing_url(protocol: WireProtocol, base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    match protocol {
        WireProtocol::AnthropicMessages => {
            let root = base.strip_suffix("/v1").unwrap_or(base);
            format!("{root}/v1/models?limit={ANTHROPIC_LIST_LIMIT}")
        }
        _ => format!("{base}/models"),
    }
}

/// True when the protocol's listing endpoint authenticates with `x-api-key`
/// + `anthropic-version` instead of a bearer token.
pub fn listing_is_anthropic(protocol: WireProtocol) -> bool {
    matches!(protocol, WireProtocol::AnthropicMessages)
}

/* ---- request bodies ---- */

/// The chat-completions request body for one OpenAI-compatible route.
///
/// OpenAI-family gateways accept `reasoning_effort` when they support thinking
/// levels. The DeepSeek `thinking: { type }` switch is DeepSeek-direct wire
/// vocabulary only (`build_deepseek_body`); sending it here breaks gateways
/// such as MiniMax that reject unknown parameters.
pub(crate) fn build_openai_body(request: &GenerateRequest) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": request.model,
        "messages": build_wire_messages(request),
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if let Some(effort) = request
        .reasoning_effort
        .as_deref()
        .filter(|effort| *effort != "off")
    {
        body["reasoning_effort"] = serde_json::json!(effort);
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = serde_json::json!(temperature);
    }
    if let Some(max_tokens) = request.max_tokens {
        body["max_tokens"] = serde_json::json!(max_tokens);
    }
    if !request.stop.is_empty() {
        body["stop"] = serde_json::json!(request.stop);
    }
    let tools = build_wire_tools(&request.tools);
    if !tools.is_empty() {
        body["tools"] = serde_json::Value::Array(tools);
    }
    body
}

/// 回传清洗:复用 chat-completions 的畸形 arguments 抢救逻辑。
fn json_arguments(raw: &str) -> serde_json::Value {
    serde_json::from_str(&wire_arguments(raw)).unwrap_or(serde_json::json!({}))
}

/// The Responses request body: `input` items replay the conversation, the
/// system prompt travels as top-level `instructions`.
pub(crate) fn build_responses_body(request: &GenerateRequest) -> serde_json::Value {
    let mut input = Vec::new();
    for message in &request.messages {
        match message.role {
            // 系统提示由顶层 instructions 携带;消息里的 system 行不重放。
            ChatRole::System => {}
            ChatRole::User => {
                let mut parts = vec![serde_json::json!({
                    "type": "input_text",
                    "text": message.content,
                })];
                for image in &message.images {
                    parts.push(serde_json::json!({
                        "type": "input_image",
                        "image_url": format!("data:{};base64,{}", image.mime, image.data),
                    }));
                }
                input.push(serde_json::json!({ "role": "user", "content": parts }));
            }
            ChatRole::Assistant => {
                if !message.content.is_empty() {
                    input.push(serde_json::json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": message.content }],
                    }));
                }
                for call in &message.tool_calls {
                    input.push(serde_json::json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": wire_arguments(&call.arguments),
                    }));
                }
            }
            ChatRole::Tool => {
                let Some(call_id) = &message.tool_call_id else {
                    debug_assert!(false, "tool message without tool_call_id dropped");
                    continue;
                };
                input.push(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": message.content,
                }));
            }
        }
    }
    let mut body = serde_json::json!({
        "model": request.model,
        "input": input,
        "stream": true,
        // Responses 默认存储对话;网关/自托管多无存储,显式关闭。
        "store": false,
    });
    if let Some(system) = request.system.as_deref().filter(|s| !s.is_empty()) {
        body["instructions"] = serde_json::json!(system);
    }
    if let Some(effort) = request
        .reasoning_effort
        .as_deref()
        .filter(|effort| *effort != "off")
    {
        body["reasoning"] = serde_json::json!({ "effort": effort });
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = serde_json::json!(temperature);
    }
    if let Some(max_tokens) = request.max_tokens {
        body["max_output_tokens"] = serde_json::json!(max_tokens);
    }
    if !request.tools.is_empty() {
        // Responses 的 tools 是扁平形状(无 function 包裹)。
        let tools: Vec<serde_json::Value> = request
            .tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                })
            })
            .collect();
        body["tools"] = serde_json::Value::Array(tools);
    }
    body
}

/// Anthropic thinking 的 effort → budget 映射(budget_tokens,官方下限 1024)。
fn thinking_budget(effort: &str) -> Option<u64> {
    match effort {
        "low" => Some(4_096),
        "medium" => Some(8_192),
        "high" => Some(16_384),
        "xhigh" => Some(24_576),
        "max" => Some(32_768),
        _ => None,
    }
}

/// The Messages request body: system as a top-level field, tool results as
/// `tool_result` blocks inside user messages, adjacent same-role messages
/// merged (Anthropic requires alternating roles).
pub(crate) fn build_anthropic_body(
    request: &GenerateRequest,
    max_tokens: u64,
) -> serde_json::Value {
    let mut messages: Vec<(ChatRole, Vec<serde_json::Value>)> = Vec::new();
    let push_block = |role: ChatRole,
                      block: serde_json::Value,
                      messages: &mut Vec<(ChatRole, Vec<serde_json::Value>)>| {
        match messages.last_mut() {
            Some((last_role, blocks)) if *last_role == role => blocks.push(block),
            _ => messages.push((role, vec![block])),
        }
    };
    for message in &request.messages {
        match message.role {
            // 系统提示走顶层 system 字段。
            ChatRole::System => {}
            ChatRole::User => {
                if !message.content.is_empty() {
                    push_block(
                        ChatRole::User,
                        serde_json::json!({ "type": "text", "text": message.content }),
                        &mut messages,
                    );
                }
                for image in &message.images {
                    push_block(
                        ChatRole::User,
                        serde_json::json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": image.mime,
                                "data": image.data,
                            },
                        }),
                        &mut messages,
                    );
                }
            }
            ChatRole::Assistant => {
                if !message.content.is_empty() {
                    push_block(
                        ChatRole::Assistant,
                        serde_json::json!({ "type": "text", "text": message.content }),
                        &mut messages,
                    );
                }
                for call in &message.tool_calls {
                    push_block(
                        ChatRole::Assistant,
                        serde_json::json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": call.name,
                            "input": json_arguments(&call.arguments),
                        }),
                        &mut messages,
                    );
                }
            }
            ChatRole::Tool => {
                let Some(call_id) = &message.tool_call_id else {
                    debug_assert!(false, "tool message without tool_call_id dropped");
                    continue;
                };
                push_block(
                    ChatRole::User,
                    serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": call_id,
                        "content": message.content,
                    }),
                    &mut messages,
                );
            }
        }
    }
    let wire_messages: Vec<serde_json::Value> = messages
        .into_iter()
        .map(|(role, content)| {
            let role = if role == ChatRole::User {
                "user"
            } else {
                "assistant"
            };
            serde_json::json!({ "role": role, "content": content })
        })
        .collect();

    // thinking 开启时 Anthropic 要求 temperature = 1,干脆不带 temperature。
    let effort = request
        .reasoning_effort
        .as_deref()
        .filter(|effort| *effort != "off")
        .and_then(thinking_budget);
    let mut max_tokens = max_tokens.max(1);
    let mut body = serde_json::json!({
        "model": request.model,
        "messages": wire_messages,
        "max_tokens": max_tokens,
        "stream": true,
    });
    if let Some(system) = request.system.as_deref().filter(|s| !s.is_empty()) {
        body["system"] = serde_json::json!(system);
    }
    if let Some(budget) = effort {
        // budget_tokens 必须小于 max_tokens;不够就把上限抬到预算之上。
        if budget >= max_tokens {
            max_tokens = budget + 1_024;
            body["max_tokens"] = serde_json::json!(max_tokens);
        }
        body["thinking"] = serde_json::json!({ "type": "enabled", "budget_tokens": budget });
    } else if let Some(temperature) = request.temperature {
        body["temperature"] = serde_json::json!(temperature);
    }
    if !request.stop.is_empty() {
        body["stop_sequences"] = serde_json::json!(request.stop);
    }
    if !request.tools.is_empty() {
        let tools: Vec<serde_json::Value> = request
            .tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters,
                })
            })
            .collect();
        body["tools"] = serde_json::Value::Array(tools);
    }
    body
}

/* ---- SSE event translators ---- */

/// One protocol's SSE vocabulary → harness chunk translation. The shared
/// driver feeds every event's JSON payload; `finish` runs exactly once at EOF.
pub trait EventTranslator: Send {
    fn feed(&mut self, data: &str) -> Result<Vec<StreamChunk>, LlmFailure>;
    /// EOF: emit closing chunks, or fail (unfinished stream).
    fn finish(&mut self) -> Result<Vec<StreamChunk>, LlmFailure>;
}

/// chat-completions: `data: {...choices...}` frames plus a `[DONE]` sentinel.
pub struct CompletionsStream {
    inner: StreamTranslator,
    style: UsageStyle,
    saw_done: bool,
}

impl CompletionsStream {
    pub fn new(style: UsageStyle) -> Self {
        Self {
            inner: StreamTranslator::default(),
            style,
            saw_done: false,
        }
    }
}

impl EventTranslator for CompletionsStream {
    fn feed(&mut self, data: &str) -> Result<Vec<StreamChunk>, LlmFailure> {
        let trimmed = data.trim();
        if trimmed == DONE_MARKER {
            self.saw_done = true;
            return Ok(Vec::new());
        }
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        let wire: WireChunk = serde_json::from_str(trimmed).map_err(|error| {
            LlmFailure::new(
                codes::MALFORMED_RESPONSE,
                format!("malformed SSE payload: {error}"),
            )
        })?;
        Ok(self.inner.feed(&wire, self.style))
    }

    fn finish(&mut self) -> Result<Vec<StreamChunk>, LlmFailure> {
        if !self.saw_done {
            return Err(LlmFailure::new(
                codes::STREAM_CLOSED,
                "stream ended before the [DONE] marker",
            ));
        }
        Ok(self.inner.finalize())
    }
}

/// Responses / Messages events carry their type inside the JSON payload, so
/// both translators key off `data["type"]` rather than the SSE event name
/// (some proxies drop event names).

#[derive(Debug, Default)]
struct OpenBlock {
    index: u32,
    text: String,
}

#[derive(Debug)]
struct CallState {
    index: u32,
    call_id: String,
    name: String,
    arguments: String,
}

fn malformed(detail: impl std::fmt::Display) -> LlmFailure {
    LlmFailure::new(
        codes::MALFORMED_RESPONSE,
        format!("malformed SSE payload: {detail}"),
    )
}

/// OpenAI Responses SSE → chunks. Text/reasoning blocks stream via delta
/// events and close on their item's `done`; function calls open on item
/// `added`, accumulate arguments, and close calibrated to the done item.
#[derive(Debug, Default)]
pub struct ResponsesStream {
    next_index: u32,
    text: Option<OpenBlock>,
    reasoning: Option<OpenBlock>,
    calls: BTreeMap<String, CallState>,
    usage: Option<TokenUsage>,
    finish: Option<FinishReason>,
    saw_block: bool,
    /// 流里出现过 function_call 输出:收尾按 ToolCalls 报告(与
    /// chat-completions 的 finish_reason 语义对齐)。
    had_tool_call: bool,
    completed: bool,
}

impl ResponsesStream {
    fn take_index(&mut self) -> u32 {
        let index = self.next_index;
        self.next_index += 1;
        index
    }

    fn open_text(&mut self, out: &mut Vec<StreamChunk>) -> u32 {
        if let Some(block) = &self.text {
            return block.index;
        }
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

    fn open_reasoning(&mut self, out: &mut Vec<StreamChunk>) -> u32 {
        if let Some(block) = &self.reasoning {
            return block.index;
        }
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

    fn close_text(&mut self, out: &mut Vec<StreamChunk>) {
        if let Some(block) = self.text.take() {
            out.push(StreamChunk::BlockEnd {
                index: block.index,
                block: ContentBlock::Text { text: block.text },
            });
        }
    }

    fn close_reasoning(&mut self, out: &mut Vec<StreamChunk>) {
        if let Some(block) = self.reasoning.take() {
            out.push(StreamChunk::BlockEnd {
                index: block.index,
                block: ContentBlock::Reasoning { text: block.text },
            });
        }
    }

    fn map_usage(response: &serde_json::Value) -> TokenUsage {
        let usage = &response["usage"];
        TokenUsage {
            input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
            output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
            cache_read_tokens: usage["input_tokens_details"]["cached_tokens"].as_u64(),
            reasoning_tokens: usage["output_tokens_details"]["reasoning_tokens"].as_u64(),
        }
    }
}

impl EventTranslator for ResponsesStream {
    fn feed(&mut self, data: &str) -> Result<Vec<StreamChunk>, LlmFailure> {
        let value: serde_json::Value = serde_json::from_str(data.trim()).map_err(malformed)?;
        let event_type = value["type"].as_str().unwrap_or_default().to_string();
        let mut out = Vec::new();
        match event_type.as_str() {
            "response.output_text.delta" => {
                let Some(delta) = value["delta"].as_str() else {
                    return Ok(out);
                };
                if delta.is_empty() {
                    return Ok(out);
                }
                let index = self.open_text(&mut out);
                self.text.as_mut().unwrap().text.push_str(delta);
                out.push(StreamChunk::TextDelta {
                    index,
                    text: delta.to_string(),
                });
            }
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                let Some(delta) = value["delta"].as_str() else {
                    return Ok(out);
                };
                if delta.is_empty() {
                    return Ok(out);
                }
                let index = self.open_reasoning(&mut out);
                self.reasoning.as_mut().unwrap().text.push_str(delta);
                out.push(StreamChunk::ReasoningDelta {
                    index,
                    text: delta.to_string(),
                });
            }
            "response.output_item.added" => {
                let item = &value["item"];
                if item["type"].as_str() == Some("function_call") {
                    let item_id = item["id"].as_str().unwrap_or_default().to_string();
                    let index = self.take_index();
                    self.saw_block = true;
                    out.push(StreamChunk::BlockStart {
                        index,
                        block_type: BlockType::ToolCall,
                    });
                    self.calls.insert(
                        item_id,
                        CallState {
                            index,
                            call_id: item["call_id"]
                                .as_str()
                                .or_else(|| item["id"].as_str())
                                .unwrap_or_default()
                                .to_string(),
                            name: item["name"].as_str().unwrap_or_default().to_string(),
                            arguments: String::new(),
                        },
                    );
                }
            }
            "response.function_call_arguments.delta" => {
                let item_id = value["item_id"].as_str().unwrap_or_default();
                let Some(delta) = value["delta"].as_str() else {
                    return Ok(out);
                };
                let Some(call) = self.calls.get_mut(item_id) else {
                    return Ok(out);
                };
                call.arguments.push_str(delta);
                out.push(StreamChunk::ToolCallDelta {
                    index: call.index,
                    id: call.call_id.clone(),
                    name: (!call.name.is_empty()).then(|| call.name.clone()),
                    arguments_delta: delta.to_string(),
                });
            }
            "response.output_item.done" => {
                let item = &value["item"];
                match item["type"].as_str() {
                    Some("function_call") => {
                        self.had_tool_call = true;
                        let item_id = item["id"].as_str().unwrap_or_default();
                        let Some(call) = self.calls.remove(item_id) else {
                            return Ok(out);
                        };
                        // done 里的最终值校准 delta 流(有的网关不发 delta)。
                        let call_id = item["call_id"]
                            .as_str()
                            .or_else(|| item["id"].as_str())
                            .unwrap_or(&call.call_id)
                            .to_string();
                        let name = item["name"].as_str().unwrap_or(&call.name).to_string();
                        let arguments = item["arguments"]
                            .as_str()
                            .map(str::to_string)
                            .filter(|s| !s.is_empty())
                            .unwrap_or(call.arguments);
                        out.push(StreamChunk::ToolCallDelta {
                            index: call.index,
                            id: call_id.clone(),
                            name: Some(name.clone()),
                            arguments_delta: arguments.clone(),
                        });
                        out.push(StreamChunk::BlockEnd {
                            index: call.index,
                            block: ContentBlock::ToolCall {
                                id: call_id,
                                name,
                                arguments,
                            },
                        });
                    }
                    Some("message") => self.close_text(&mut out),
                    Some("reasoning") => self.close_reasoning(&mut out),
                    _ => {}
                }
            }
            "response.completed" | "response.incomplete" => {
                self.completed = true;
                self.close_text(&mut out);
                self.close_reasoning(&mut out);
                let response = &value["response"];
                self.usage = Some(Self::map_usage(response));
                let finish = match (event_type.as_str(), response["status"].as_str()) {
                    ("response.incomplete", _) | (_, Some("incomplete")) => FinishReason::MaxTokens,
                    _ if self.had_tool_call => FinishReason::ToolCalls,
                    _ => FinishReason::Stop,
                };
                self.finish = Some(finish);
            }
            "response.failed" => {
                self.completed = true;
                self.close_text(&mut out);
                self.close_reasoning(&mut out);
                let error = &value["response"]["error"];
                let code = error["code"]
                    .as_str()
                    .unwrap_or("RESPONSE_FAILED")
                    .to_string();
                let message = error["message"]
                    .as_str()
                    .unwrap_or("the provider failed the response")
                    .to_string();
                self.finish = Some(FinishReason::Error {
                    failure: LlmFailure::new(code, message),
                });
            }
            "error" => {
                let code = value["code"]
                    .as_str()
                    .unwrap_or("PROVIDER_ERROR")
                    .to_string();
                let message = value["message"]
                    .as_str()
                    .unwrap_or("provider error")
                    .to_string();
                return Err(LlmFailure::new(code, message));
            }
            _ => {}
        }
        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<StreamChunk>, LlmFailure> {
        if !self.completed {
            return Err(LlmFailure::new(
                codes::STREAM_CLOSED,
                "stream ended before the response completed",
            ));
        }
        let mut out = Vec::new();
        self.close_text(&mut out);
        self.close_reasoning(&mut out);
        if let Some(usage) = self.usage.take() {
            out.push(StreamChunk::Usage { usage });
        }
        let mut reason = self.finish.take().unwrap_or(FinishReason::Stop);
        if matches!(reason, FinishReason::Stop) && !self.saw_block {
            reason = FinishReason::Error {
                failure: LlmFailure::new(
                    codes::EMPTY_RESPONSE,
                    "the model produced no output blocks",
                ),
            };
        }
        out.push(StreamChunk::Finish { reason });
        Ok(out)
    }
}

/// Anthropic Messages SSE → chunks. Blocks open on `content_block_start`,
/// stream via `content_block_delta`, close on `content_block_stop`; usage
/// spans `message_start` (input) and `message_delta` (output).
#[derive(Debug, Default)]
pub struct AnthropicStream {
    next_index: u32,
    /// Anthropic content-block index → open block state.
    blocks: BTreeMap<u64, OpenBlock>,
    block_types: BTreeMap<u64, BlockType>,
    usage: Option<TokenUsage>,
    finish: Option<FinishReason>,
    saw_block: bool,
    stopped: bool,
}

impl AnthropicStream {
    fn take_index(&mut self) -> u32 {
        let index = self.next_index;
        self.next_index += 1;
        index
    }

    fn open_block(
        &mut self,
        anthropic_index: u64,
        block_type: BlockType,
        out: &mut Vec<StreamChunk>,
    ) {
        if self.blocks.contains_key(&anthropic_index) {
            return;
        }
        let index = self.take_index();
        self.saw_block = true;
        out.push(StreamChunk::BlockStart { index, block_type });
        self.blocks.insert(
            anthropic_index,
            OpenBlock {
                index,
                text: String::new(),
            },
        );
        self.block_types.insert(anthropic_index, block_type);
    }

    /// Closes one block; without a caller-supplied final block the closed
    /// block is reconstructed from the block's registered type.
    fn close_block(
        &mut self,
        anthropic_index: u64,
        final_block: Option<ContentBlock>,
        out: &mut Vec<StreamChunk>,
    ) {
        let Some(state) = self.blocks.remove(&anthropic_index) else {
            return;
        };
        let block_type = self.block_types.remove(&anthropic_index);
        let block = final_block.unwrap_or_else(|| match block_type {
            Some(BlockType::Reasoning) => ContentBlock::Reasoning {
                text: state.text.clone(),
            },
            Some(BlockType::ToolCall) => ContentBlock::ToolCall {
                id: String::new(),
                name: String::new(),
                arguments: state.text.clone(),
            },
            _ => ContentBlock::Text {
                text: state.text.clone(),
            },
        });
        out.push(StreamChunk::BlockEnd {
            index: state.index,
            block,
        });
    }

    fn flush_all(&mut self, out: &mut Vec<StreamChunk>) {
        let indexes: Vec<u64> = self.blocks.keys().copied().collect();
        for index in indexes {
            self.close_block(index, None, out);
        }
    }
}

impl EventTranslator for AnthropicStream {
    fn feed(&mut self, data: &str) -> Result<Vec<StreamChunk>, LlmFailure> {
        let value: serde_json::Value = serde_json::from_str(data.trim()).map_err(malformed)?;
        let event_type = value["type"].as_str().unwrap_or_default().to_string();
        let mut out = Vec::new();
        match event_type.as_str() {
            "message_start" => {
                let usage = &value["message"]["usage"];
                self.usage = Some(TokenUsage {
                    input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
                    output_tokens: 0,
                    cache_read_tokens: usage["cache_read_input_tokens"].as_u64(),
                    reasoning_tokens: None,
                });
            }
            "content_block_start" => {
                let anthropic_index = value["index"].as_u64().unwrap_or(0);
                let block = &value["content_block"];
                match block["type"].as_str() {
                    Some("text") => self.open_block(anthropic_index, BlockType::Text, &mut out),
                    Some("thinking") => {
                        self.open_block(anthropic_index, BlockType::Reasoning, &mut out)
                    }
                    Some("tool_use") => {
                        self.open_block(anthropic_index, BlockType::ToolCall, &mut out);
                        // 初始参数通常是 {} 或空:作为首个 delta 透传。
                        if let Some(input) = block["input"].as_str()
                            && !input.is_empty()
                            && let Some(state) = self.blocks.get_mut(&anthropic_index)
                        {
                            state.text.push_str(input);
                        }
                    }
                    // redacted_thinking 等不可流式块:不产生 chunk。
                    _ => {}
                }
            }
            "content_block_delta" => {
                let anthropic_index = value["index"].as_u64().unwrap_or(0);
                let delta = &value["delta"];
                let block_type = self.block_types.get(&anthropic_index).copied();
                match (block_type, delta["type"].as_str()) {
                    (Some(BlockType::Text), Some("text_delta")) => {
                        let Some(text) = delta["text"].as_str() else {
                            return Ok(out);
                        };
                        if let Some(state) = self.blocks.get_mut(&anthropic_index) {
                            state.text.push_str(text);
                            out.push(StreamChunk::TextDelta {
                                index: state.index,
                                text: text.to_string(),
                            });
                        }
                    }
                    (Some(BlockType::Reasoning), Some("thinking_delta")) => {
                        let Some(text) = delta["thinking"].as_str() else {
                            return Ok(out);
                        };
                        if let Some(state) = self.blocks.get_mut(&anthropic_index) {
                            state.text.push_str(text);
                            out.push(StreamChunk::ReasoningDelta {
                                index: state.index,
                                text: text.to_string(),
                            });
                        }
                    }
                    (Some(BlockType::ToolCall), Some("input_json_delta")) => {
                        let Some(partial) = delta["partial_json"].as_str() else {
                            return Ok(out);
                        };
                        let Some(state) = self.blocks.get_mut(&anthropic_index) else {
                            return Ok(out);
                        };
                        state.text.push_str(partial);
                        out.push(StreamChunk::ToolCallDelta {
                            index: state.index,
                            id: String::new(),
                            name: None,
                            arguments_delta: partial.to_string(),
                        });
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let anthropic_index = value["index"].as_u64().unwrap_or(0);
                self.close_block(anthropic_index, None, &mut out);
            }
            "message_delta" => {
                if let Some(usage) = &mut self.usage
                    && let Some(output) = value["usage"]["output_tokens"].as_u64()
                {
                    usage.output_tokens = output;
                }
                match value["delta"]["stop_reason"].as_str() {
                    Some("tool_use") => self.finish = Some(FinishReason::ToolCalls),
                    Some("max_tokens") => self.finish = Some(FinishReason::MaxTokens),
                    Some("refusal") => {
                        self.finish = Some(FinishReason::Error {
                            failure: LlmFailure::new("REFUSED", "the model refused to continue"),
                        })
                    }
                    Some("end_turn") | Some("stop_sequence") | None => {}
                    Some(other) => {
                        self.finish = Some(FinishReason::Error {
                            failure: LlmFailure::new(
                                other.to_ascii_uppercase(),
                                format!("model stopped with unknown reason: {other}"),
                            ),
                        })
                    }
                }
            }
            "message_stop" => {
                self.stopped = true;
            }
            "error" => {
                let code = value["error"]["type"]
                    .as_str()
                    .unwrap_or("PROVIDER_ERROR")
                    .to_string();
                let message = value["error"]["message"]
                    .as_str()
                    .unwrap_or("provider error")
                    .to_string();
                return Err(LlmFailure::new(code, message));
            }
            _ => {}
        }
        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<StreamChunk>, LlmFailure> {
        if !self.stopped {
            return Err(LlmFailure::new(
                codes::STREAM_CLOSED,
                "stream ended before message_stop",
            ));
        }
        let mut out = Vec::new();
        self.flush_all(&mut out);
        if let Some(usage) = self.usage.take() {
            out.push(StreamChunk::Usage { usage });
        }
        let mut reason = self.finish.take().unwrap_or(FinishReason::Stop);
        if matches!(reason, FinishReason::Stop) && !self.saw_block {
            reason = FinishReason::Error {
                failure: LlmFailure::new(
                    codes::EMPTY_RESPONSE,
                    "the model produced no output blocks",
                ),
            };
        }
        out.push(StreamChunk::Finish { reason });
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::message::{ChatMessage, ImageData, ToolCallRef};
    use denia_core::tool::ToolSchema;

    fn feed_all(translator: &mut dyn EventTranslator, frames: &[&str]) -> Vec<StreamChunk> {
        let mut out = Vec::new();
        for frame in frames {
            out.extend(translator.feed(frame).unwrap());
        }
        out.extend(translator.finish().unwrap());
        out
    }

    fn request() -> GenerateRequest {
        GenerateRequest {
            model: "test-model".to_string(),
            reasoning_effort: None,
            messages: vec![
                ChatMessage::user("你好"),
                ChatMessage::assistant(
                    "调用工具",
                    None,
                    vec![ToolCallRef {
                        id: "call_1".to_string(),
                        name: "bash".to_string(),
                        arguments: r#"{"command":"ls"}"#.to_string(),
                    }],
                ),
                ChatMessage::tool_result("call_1", "exit 0"),
                ChatMessage::user("继续"),
            ],
            system: Some("系统提示".to_string()),
            tools: vec![ToolSchema {
                name: "bash".to_string(),
                description: "run".to_string(),
                parameters: serde_json::json!({ "type": "object" }),
            }],
            temperature: None,
            max_tokens: None,
            stop: Vec::new(),
        }
    }

    #[test]
    fn responses_body_replays_conversation() {
        let body = build_responses_body(&request());
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["instructions"], "系统提示");
        assert_eq!(body["store"], false);
        let input = body["input"].as_array().unwrap();
        // user / assistant message / function_call / function_call_output / user
        assert_eq!(input.len(), 5);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["content"][0]["type"], "output_text");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["name"], "bash");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["output"], "exit 0");
        // tools 是扁平 function 形状。
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "bash");
        assert!(body["tools"][0].get("function").is_none());
    }

    #[test]
    fn responses_body_images_and_reasoning() {
        let mut request = request();
        request.messages.insert(
            0,
            ChatMessage::user_with_images(
                "看图",
                vec![ImageData {
                    mime: "image/png".to_string(),
                    data: "QUJD".to_string(),
                }],
            ),
        );
        request.reasoning_effort = Some("high".to_string());
        request.max_tokens = Some(4_096);
        let body = build_responses_body(&request);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["content"][1]["type"], "input_image");
        assert_eq!(
            input[0]["content"][1]["image_url"],
            "data:image/png;base64,QUJD"
        );
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["max_output_tokens"], 4_096);
    }

    #[test]
    fn anthropic_body_merges_roles_and_uses_tool_result_blocks() {
        let body = build_anthropic_body(&request(), 8_192);
        assert_eq!(body["system"], "系统提示");
        assert_eq!(body["max_tokens"], 8_192);
        let messages = body["messages"].as_array().unwrap();
        // user / assistant(text+tool_use) / user(tool_result + "继续" 合并)
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][1]["type"], "tool_use");
        assert_eq!(messages[1]["content"][1]["input"]["command"], "ls");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "call_1");
        assert_eq!(messages[2]["content"][1]["type"], "text");
        assert_eq!(messages[2]["content"][1]["text"], "继续");
        // tools 是 input_schema 形状。
        assert_eq!(body["tools"][0]["name"], "bash");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        // 无 effort:不带 thinking,可带 temperature。
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn anthropic_body_maps_effort_to_thinking_budget() {
        let mut request = request();
        request.reasoning_effort = Some("high".to_string());
        request.temperature = Some(0.7);
        let body = build_anthropic_body(&request, 8_192);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 16_384);
        // 预算必须小于 max_tokens:不足时上限被抬高。
        assert_eq!(body["max_tokens"], 16_384 + 1_024);
        // thinking 开启时不带 temperature(Anthropic 要求 1)。
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn anthropic_urls_accept_both_v1_spellings() {
        assert_eq!(
            anthropic_messages_url("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://gateway.example/v1/"),
            "https://gateway.example/v1/messages"
        );
        assert_eq!(
            listing_url(
                WireProtocol::AnthropicMessages,
                "https://gateway.example/v1"
            ),
            "https://gateway.example/v1/models?limit=1000"
        );
        assert_eq!(
            listing_url(WireProtocol::ChatCompletions, "https://gw.example/v1/"),
            "https://gw.example/v1/models"
        );
        assert_eq!(
            responses_url("https://gw.example/v1"),
            "https://gw.example/v1/responses"
        );
        assert_eq!(
            chat_completions_url("https://gw.example/v1"),
            "https://gw.example/v1/chat/completions"
        );
    }

    #[test]
    fn responses_stream_text_reasoning_usage() {
        let mut translator = ResponsesStream::default();
        let chunks = feed_all(
            &mut translator,
            &[
                r#"{"type":"response.output_item.added","item":{"type":"reasoning","id":"r1"}}"#,
                r#"{"type":"response.reasoning_summary_text.delta","delta":"想一下"}"#,
                r#"{"type":"response.output_item.done","item":{"type":"reasoning","id":"r1"}}"#,
                r#"{"type":"response.output_text.delta","delta":"你好"}"#,
                r#"{"type":"response.output_text.delta","delta":"!"}"#,
                r#"{"type":"response.output_item.done","item":{"type":"message","id":"m1"}}"#,
                r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":10,"output_tokens":3,"input_tokens_details":{"cached_tokens":4},"output_tokens_details":{"reasoning_tokens":2}}}}"#,
            ],
        );
        let kinds: Vec<&str> = chunks
            .iter()
            .map(|chunk| match chunk {
                StreamChunk::BlockStart { .. } => "start",
                StreamChunk::ReasoningDelta { .. } => "reasoning",
                StreamChunk::TextDelta { .. } => "text",
                StreamChunk::BlockEnd { .. } => "end",
                StreamChunk::Usage { .. } => "usage",
                StreamChunk::Finish { .. } => "finish",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "start",
                "reasoning",
                "end",
                "start",
                "text",
                "text",
                "end",
                "usage",
                "finish"
            ]
        );
        let finish = chunks.last().unwrap();
        assert!(matches!(
            finish,
            StreamChunk::Finish {
                reason: FinishReason::Stop
            }
        ));
        let usage = chunks
            .iter()
            .find_map(|chunk| match chunk {
                StreamChunk::Usage { usage } => Some(*usage),
                _ => None,
            })
            .unwrap();
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.cache_read_tokens, Some(4));
        assert_eq!(usage.reasoning_tokens, Some(2));
    }

    #[test]
    fn responses_stream_tool_call_calibrates_on_done() {
        let mut translator = ResponsesStream::default();
        let chunks = feed_all(
            &mut translator,
            &[
                r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc1","call_id":"call_1","name":"bash"}}"#,
                r#"{"type":"response.function_call_arguments.delta","item_id":"fc1","delta":"{\"com"}"#,
                r#"{"type":"response.function_call_arguments.delta","item_id":"fc1","delta":"mand\":\"ls\"}"}"#,
                r#"{"type":"response.output_item.done","item":{"type":"function_call","id":"fc1","call_id":"call_1","name":"bash","arguments":"{\"command\":\"ls -la\"}"}}"#,
                r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":5,"output_tokens":7}}}"#,
            ],
        );
        let end = chunks
            .iter()
            .find_map(|chunk| match chunk {
                StreamChunk::BlockEnd { block, .. } => Some(block.clone()),
                _ => None,
            })
            .unwrap();
        match end {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "bash");
                // done 的最终 arguments 校准 delta 累积值。
                assert_eq!(arguments, r#"{"command":"ls -la"}"#);
            }
            other => panic!("expected tool call block, got {other:?}"),
        }
        let finish = chunks.last().unwrap();
        assert!(matches!(
            finish,
            StreamChunk::Finish {
                reason: FinishReason::ToolCalls
            }
        ));
    }

    #[test]
    fn responses_stream_failed_response_yields_error_finish() {
        let mut translator = ResponsesStream::default();
        let chunks = feed_all(
            &mut translator,
            &[
                r#"{"type":"response.output_text.delta","delta":"部分"}"#,
                r#"{"type":"response.failed","response":{"status":"failed","error":{"code":"server_error","message":"boom"}}}"#,
            ],
        );
        let finish = chunks.last().unwrap();
        match finish {
            StreamChunk::Finish {
                reason: FinishReason::Error { failure },
            } => {
                assert_eq!(failure.code, "server_error");
                assert_eq!(failure.message, "boom");
            }
            other => panic!("expected error finish, got {other:?}"),
        }
    }

    #[test]
    fn responses_stream_eof_without_completed_fails() {
        let mut translator = ResponsesStream::default();
        translator
            .feed(r#"{"type":"response.output_text.delta","delta":"hi"}"#)
            .unwrap();
        let error = translator.finish().unwrap_err();
        assert_eq!(error.code, codes::STREAM_CLOSED);
    }

    #[test]
    fn anthropic_stream_text_and_usage() {
        let mut translator = AnthropicStream::default();
        let chunks = feed_all(
            &mut translator,
            &[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":12,"cache_read_input_tokens":8}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"推理中"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text"}}"#,
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"回答"}}"#,
                r#"{"type":"content_block_stop","index":1}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":6}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        let kinds: Vec<&str> = chunks
            .iter()
            .map(|chunk| match chunk {
                StreamChunk::BlockStart { .. } => "start",
                StreamChunk::ReasoningDelta { .. } => "reasoning",
                StreamChunk::TextDelta { .. } => "text",
                StreamChunk::BlockEnd { .. } => "end",
                StreamChunk::Usage { .. } => "usage",
                StreamChunk::Finish { .. } => "finish",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "start",
                "reasoning",
                "end",
                "start",
                "text",
                "end",
                "usage",
                "finish"
            ]
        );
        let reasoning_end = &chunks[2];
        match reasoning_end {
            StreamChunk::BlockEnd { block, .. } => match block {
                ContentBlock::Reasoning { text } => assert_eq!(text, "推理中"),
                other => panic!("expected reasoning block, got {other:?}"),
            },
            other => panic!("expected block end, got {other:?}"),
        }
        let usage = chunks
            .iter()
            .find_map(|chunk| match chunk {
                StreamChunk::Usage { usage } => Some(*usage),
                _ => None,
            })
            .unwrap();
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.cache_read_tokens, Some(8));
        assert_eq!(usage.output_tokens, 6);
        assert!(matches!(
            chunks.last().unwrap(),
            StreamChunk::Finish {
                reason: FinishReason::Stop
            }
        ));
    }

    #[test]
    fn anthropic_stream_tool_call() {
        let mut translator = AnthropicStream::default();
        let chunks = feed_all(
            &mut translator,
            &[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":9}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"bash","input":{}}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"ls\"}"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        let end = chunks
            .iter()
            .find_map(|chunk| match chunk {
                StreamChunk::BlockEnd { block, .. } => Some(block.clone()),
                _ => None,
            })
            .unwrap();
        match end {
            ContentBlock::ToolCall { arguments, .. } => {
                assert_eq!(arguments, r#"{"command":"ls"}"#);
            }
            other => panic!("expected tool call, got {other:?}"),
        }
        assert!(matches!(
            chunks.last().unwrap(),
            StreamChunk::Finish {
                reason: FinishReason::ToolCalls
            }
        ));
    }

    #[test]
    fn anthropic_stream_error_event_fails() {
        let mut translator = AnthropicStream::default();
        let error = translator
            .feed(r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#)
            .unwrap_err();
        assert_eq!(error.code, "overloaded_error");
    }

    #[test]
    fn completions_stream_keeps_done_semantics() {
        let mut translator = CompletionsStream::new(UsageStyle::OpenAi);
        translator
            .feed(r#"{"choices":[{"delta":{"content":"hi"}}]}"#)
            .unwrap();
        // 未到 [DONE] 就 EOF:STREAM_CLOSED(保留原语义)。
        let error = translator.finish().unwrap_err();
        assert_eq!(error.code, codes::STREAM_CLOSED);
        translator
            .feed(r#"{"choices":[{"delta":{"content":"hi"}}]}"#)
            .unwrap();
        translator.feed(DONE_MARKER).unwrap();
        let chunks = translator.finish().unwrap();
        assert!(matches!(
            chunks.last().unwrap(),
            StreamChunk::Finish {
                reason: FinishReason::Stop
            }
        ));
    }
}
