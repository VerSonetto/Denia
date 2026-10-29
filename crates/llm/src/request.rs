//! Request vocabulary handed to adapters.

use denia_core::message::ChatMessage;
use denia_core::tool::ToolSchema;

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
    /// Stable provider cache identity (session-scoped), used where the wire
    /// accepts `prompt_cache_key`. `None` leaves the provider's automatic
    /// prefix cache without an explicit affinity hint.
    pub cache_key: Option<String>,
}
