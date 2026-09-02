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

#[derive(Debug, Deserialize)]
pub struct WireChunk {
    #[serde(default)]
    pub choices: Vec<WireChoice>,
    #[serde(default)]
    pub usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
pub struct WireChoice {
    #[serde(default)]
    pub delta: Option<WireDelta>,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WireDelta {
    /// Present on first chunks; the translator infers roles from fields.
    #[serde(default)]
    #[allow(dead_code)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<WireToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
pub struct WireToolCallDelta {
    pub index: u32,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<WireFunctionDelta>,
}

#[derive(Debug, Deserialize)]
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

/// How `prompt_tokens` accounts cache hits on one wire flavor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageStyle {
    /// DeepSeek: `prompt_tokens` includes cache hits; subtract them.
    DeepSeek,
    /// OpenAI-compatible: `prompt_tokens` is the uncached count.
    OpenAi,
}

pub fn map_usage(wire: &WireUsage, style: UsageStyle) -> TokenUsage {
    let prompt = wire.prompt_tokens.unwrap_or(0);
    let cache_read = match style {
        UsageStyle::DeepSeek => wire.prompt_cache_hit_tokens,
        UsageStyle::OpenAi => wire
            .prompt_tokens_details
            .as_ref()
            .and_then(|details| details.cached_tokens),
    };
    let input_tokens = match style {
        UsageStyle::DeepSeek => prompt.saturating_sub(cache_read.unwrap_or(0)),
        UsageStyle::OpenAi => prompt,
    };
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
fn wire_arguments(raw: &str) -> String {
    if serde_json::from_str::<serde_json::Value>(raw).is_ok() {
        return raw.to_string();
    }
    let mut iter = serde_json::Deserializer::from_str(raw.trim())
        .into_iter::<serde_json::Value>();
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
    if let Some(system) = &request.system {
        if !system.is_empty() {
            out.push(plain("system", system.clone()));
        }
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
                out.push(WireMessage {
                    role: "tool",
                    content: Some(serde_json::Value::String(message.content.clone())),
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
}

impl StreamTranslator {
    fn take_index(&mut self) -> u32 {
        let index = self.next_index;
        self.next_index += 1;
        index
    }

    /// Feeds one wire chunk, returning the delta chunks emitted immediately.
    pub fn feed(&mut self, chunk: &WireChunk, style: UsageStyle) -> Vec<StreamChunk> {
        let mut out = Vec::new();
        for choice in &chunk.choices {
            if let Some(delta) = &choice.delta {
                self.feed_delta(delta, &mut out);
            }
            if let Some(raw) = choice
                .finish_reason
                .as_deref()
                .filter(|reason| !reason.is_empty())
            {
                if self.finish.is_none() {
                    self.finish = Some(map_finish_reason(raw));
                }
            }
        }
        if let Some(usage) = &chunk.usage {
            self.usage = Some(map_usage(usage, style));
        }
        out
    }

    fn feed_delta(&mut self, delta: &WireDelta, out: &mut Vec<StreamChunk>) {
        if let Some(text) = delta.reasoning_content.as_deref().filter(|t| !t.is_empty()) {
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
        if let Some(text) = delta.content.as_deref().filter(|t| !t.is_empty()) {
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
        if let Some(tool_calls) = &delta.tool_calls {
            for wire_call in tool_calls {
                if !self.tool_calls.contains_key(&wire_call.index) {
                    let index = self.take_index();
                    self.saw_block = true;
                    out.push(StreamChunk::BlockStart {
                        index,
                        block_type: BlockType::ToolCall,
                    });
                    self.tool_calls.insert(
                        wire_call.index,
                        ToolCallState {
                            index,
                            id: String::new(),
                            name: String::new(),
                            arguments: String::new(),
                        },
                    );
                }
                let entry = self.tool_calls.get_mut(&wire_call.index).unwrap();
                if let Some(id) = wire_call.id.as_deref().filter(|id| !id.is_empty()) {
                    if entry.id.is_empty() {
                        entry.id = id.to_string();
                    }
                }
                if let Some(function) = &wire_call.function {
                    let mut first_name: Option<String> = None;
                    if let Some(name) = function.name.as_deref().filter(|n| !n.is_empty()) {
                        if entry.name.is_empty() {
                            entry.name = name.to_string();
                            first_name = Some(name.to_string());
                        }
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
        }
    }

    /// Closes every open block, then emits usage and finish. A `stop` finish
    /// with zero blocks is the `EMPTY_RESPONSE` error.
    pub fn finalize(&mut self) -> Vec<StreamChunk> {
        let mut out = Vec::new();
        if let Some(block) = self.reasoning.take() {
            out.push(StreamChunk::BlockEnd {
                index: block.index,
                block: ContentBlock::Reasoning { text: block.text },
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
                },
            });
        }
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
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{StreamTranslator, UsageStyle, WireChunk, build_wire_messages};

    fn chunk(json: &str) -> WireChunk {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn reasoning_then_text_then_finish() {
        let mut translator = StreamTranslator::default();
        let mut all = Vec::new();
        all.extend(translator.feed(&chunk(
            r#"{"choices":[{"delta":{"reasoning_content":"thinking..."}}]}"#,
        ), UsageStyle::DeepSeek));
        all.extend(translator.feed(&chunk(
            r#"{"choices":[{"delta":{"content":"hello"}}]}"#,
        ), UsageStyle::DeepSeek));
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
        let usage = all.iter().find_map(|c| match c {
            denia_core::stream::StreamChunk::Usage { usage } => Some(*usage),
            _ => None,
        }).unwrap();
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.cache_read_tokens, Some(2));
    }

    #[test]
    fn empty_stop_is_empty_response() {
        let mut translator = StreamTranslator::default();
        translator.feed(&chunk(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#), UsageStyle::OpenAi);
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
        let emitted = translator.feed(&chunk(
            r#"{"choices":[{"delta":{"reasoning_content":""}}]}"#,
        ), UsageStyle::DeepSeek);
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

        let body = crate::openai::build_openai_body(&request);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "bash");

        let without_tools = GenerateRequest {
            tools: Vec::new(),
            ..request
        };
        let body = crate::openai::build_openai_body(&without_tools);
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
        };
        let body = crate::openai::build_openai_body(&request);
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
        };
        let body = crate::openai::build_openai_body(&request);
        assert!(body.get("thinking").is_none());
        assert_eq!(body["reasoning_effort"], "high");
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
                }],
            )],
            system: None,
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
            stop: Vec::new(),
        };
        let messages = build_wire_messages(&request);
        assert_eq!(messages.len(), 1);
        let content = messages[0].content.as_ref().unwrap();
        let parts = content.as_array().expect("multimodal content is an array");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "看这张图");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(
            parts[1]["image_url"]["url"],
            "data:image/png;base64,QUJD"
        );
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
        };
        let messages = build_wire_messages(&request);
        assert_eq!(messages[0].content.as_ref().unwrap(), "plain");
    }
}
