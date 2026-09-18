//! The model-facing declaration of one tool.

use serde::{Deserialize, Serialize};

/// What the model sees: name, description, and a JSON Schema for args.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// A JSON Schema object literal; core does not validate it.
    pub parameters: serde_json::Value,
}
