//! The model-facing declaration of one tool.

use serde::{Deserialize, Serialize};

/// What the model sees: name, description, and a JSON Schema for args.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// A JSON Schema object literal; core does not validate it.
    #[cfg_attr(feature = "bindings", ts(type = "unknown"))]
    pub parameters: serde_json::Value,
}
