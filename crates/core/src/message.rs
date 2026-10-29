//! The chat vocabulary used by request construction and session projection.

pub fn assistant_from_blocks(blocks: &[crate::stream::ContentBlock]) -> Option<ChatMessage> {
    use crate::stream::ContentBlock;
    let mut message = ChatMessage::assistant("", None, Vec::new());
    for block in blocks {
        match block {
            ContentBlock::Text { text } => message.content.push_str(text),
            ContentBlock::Reasoning { text, replay } => {
                message
                    .reasoning_content
                    .get_or_insert_with(String::new)
                    .push_str(text);
                if let Some(replay) = replay {
                    message.reasoning_replay.push(replay.clone());
                }
            }
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
                incomplete,
            } => {
                if !incomplete
                    && !id.is_empty()
                    && !name.is_empty()
                    && serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(arguments)
                        .is_ok()
                {
                    message.tool_calls.push(ToolCallRef {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    });
                }
            }
        }
    }
    if message.content.is_empty()
        && message.tool_calls.is_empty()
        && message.reasoning_content.is_none()
        && message.reasoning_replay.is_empty()
    {
        None
    } else {
        Some(message)
    }
}

use serde::{Deserialize, Serialize};

/// Wire role of a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "kebab-case")]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

/// One tool call carried by an assistant message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ToolCallRef {
    /// Provider-assigned call id, correlated with the tool result.
    pub id: String,
    pub name: String,
    /// Raw JSON arguments exactly as the model emitted them.
    pub arguments: String,
}

/// One inline image attached to a user message (base64 data URL payload).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ImageData {
    /// MIME type, e.g. `image/png`.
    pub mime: String,
    /// Base64-encoded image bytes (raw, no data-URL prefix).
    pub data: String,
    /// 落盘后的绝对路径(用户粘贴的图会写进 uploads 目录)。
    ///
    /// 与 `data` 互为备份:内联 data URL 是首选视觉通道,路径让模型在收不到
    /// 图片时仍能用 `read_file` 自己取回内容。不参与 wire 编码。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    pub path: Option<String>,
}

/// Prompt-cache provenance for one message.
///
/// Runtime-only: never serialized to the wire, never persisted, and never
/// exposed to the UI. It tells the provider encoder which messages can anchor
/// a reusable prefix, so a rebuild-per-request block is not mistaken for
/// history.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessageCache {
    /// Rebuilt for every request; a prefix spanning it cannot match next turn.
    pub per_call: bool,
    /// A real user turn, not an injected reminder/context block.
    pub conversational_user: bool,
}

/// One wire-adjacent chat message.
///
/// `tool_calls` is assistant-only and `tool_call_id` is tool-role-only; the
/// wire builders and the projection both enforce those invariants. `images`
/// is user-role-only and converts to a multimodal content array on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
    /// Prompt-cache provenance (runtime-only, never serialized).
    #[serde(skip)]
    #[cfg_attr(feature = "bindings", ts(skip))]
    pub cache: MessageCache,
    /// Inline images for vision-capable models (user-role only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageData>,
    /// Passes the model's chain-of-thought back on reasoning-capable routes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_replay: Vec<crate::stream::ReasoningReplay>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
            cache: MessageCache::default(),
            images: Vec::new(),
            reasoning_content: None,
            reasoning_replay: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn user_with_images(content: impl Into<String>, images: Vec<ImageData>) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
            cache: MessageCache::default(),
            images,
            reasoning_content: None,
            reasoning_replay: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant(
        content: impl Into<String>,
        reasoning_content: Option<String>,
        tool_calls: Vec<ToolCallRef>,
    ) -> Self {
        Self {
            role: ChatRole::Assistant,
            content: content.into(),
            cache: MessageCache::default(),
            images: Vec::new(),
            reasoning_content,
            reasoning_replay: Vec::new(),
            tool_calls,
            tool_call_id: None,
        }
    }

    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::Tool,
            content: content.into(),
            cache: MessageCache::default(),
            images: Vec::new(),
            reasoning_content: None,
            reasoning_replay: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }

    /// assistant 消息在模型面上是否已无任何内容。
    ///
    /// 超长参数打桩会把一个工具调用从消息里摘掉;摘掉后如果正文、推理、
    /// 其余调用都不剩,这条消息就不能再进 wire(空 assistant 消息既浪费
    /// 计价,部分 provider 也会拒收)。
    pub fn is_empty_assistant(&self) -> bool {
        self.role == ChatRole::Assistant
            && self.content.is_empty()
            && self.images.is_empty()
            && self.tool_calls.is_empty()
            && self.reasoning_content.is_none()
            && self.reasoning_replay.is_empty()
    }
}
