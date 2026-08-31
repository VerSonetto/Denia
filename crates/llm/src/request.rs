//! Request vocabulary handed to adapters.

use dshrs_core::message::ChatMessage;
use dshrs_core::tool::ToolSchema;

/// One model call's inputs, provider-agnostic.
#[derive(Debug, Clone)]
pub struct GenerateRequest {
    pub model: String,
    /// Adapter-owned effort id; `None` lets the route default apply.
    pub reasoning_effort: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub system: Option<String>,
    /// Model-facing tool declarations; empty sends no `tools` array.
    pub tools: Vec<ToolSchema>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u64>,
    pub stop: Vec<String>,
}
