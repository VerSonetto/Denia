//! The chat vocabulary used by request construction and session projection.

use serde::{Deserialize, Serialize};

/// Wire role of a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

/// One tool call carried by an assistant message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[serde(rename_all = "camelCase")]
pub struct ImageData {
    /// MIME type, e.g. `image/png`.
    pub mime: String,
    /// Base64-encoded image bytes (raw, no data-URL prefix).
    pub data: String,
}

/// One wire-adjacent chat message.
///
/// `tool_calls` is assistant-only and `tool_call_id` is tool-role-only; the
/// wire builders and the projection both enforce those invariants. `images`
/// is user-role-only and converts to a multimodal content array on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
    /// Inline images for vision-capable models (user-role only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageData>,
    /// Passes the model's chain-of-thought back on reasoning-capable routes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
            images: Vec::new(),
            reasoning_content: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn user_with_images(content: impl Into<String>, images: Vec<ImageData>) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
            images,
            reasoning_content: None,
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
            images: Vec::new(),
            reasoning_content,
            tool_calls,
            tool_call_id: None,
        }
    }

    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::Tool,
            content: content.into(),
            images: Vec::new(),
            reasoning_content: None,
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }
}
