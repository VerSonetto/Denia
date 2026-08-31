use serde_json::Value;
use thiserror::Error;

/// Settings-store failures, surfaced verbatim to API callers.
#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("unknown settings namespace: {0}")]
    UnknownNamespace(String),
    #[error("settings namespace already registered: {0}")]
    DuplicateNamespace(String),
    #[error("invalid settings namespace name: {0}")]
    InvalidName(String),
    #[error("settings revision conflict: expected {expected}, actual {actual}")]
    Conflict { expected: u64, actual: u64 },
    #[error("settings write rejected: {0}")]
    Rejected(String),
    #[error("settings document parse error: {0}")]
    Parse(String),
    #[error("settings I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("settings value is not JSON-shaped: {0}")]
    NotJsonShaped(String),
}

impl SettingsError {
    /// Stable wire code for API error envelopes.
    pub fn code(&self) -> &'static str {
        match self {
            SettingsError::UnknownNamespace(_) => "settings/unknown-namespace",
            SettingsError::DuplicateNamespace(_) => "settings/duplicate-namespace",
            SettingsError::InvalidName(_) => "settings/invalid-name",
            SettingsError::Conflict { .. } => "settings/conflict",
            SettingsError::Rejected(_) => "settings/rejected",
            SettingsError::Parse(_) => "settings/parse",
            SettingsError::Io(_) => "settings/io",
            SettingsError::NotJsonShaped(_) => "settings/not-json-shaped",
        }
    }

    /// Conflict details, when this is a revision conflict.
    pub fn conflict_details(&self) -> Option<(u64, u64)> {
        match self {
            SettingsError::Conflict { expected, actual } => Some((*expected, *actual)),
            _ => None,
        }
    }
}

/// Rejects values that cannot round-trip through the YAML/JSON document:
/// the store only persists plain objects, arrays, strings, finite numbers,
/// booleans, and null.
pub fn assert_json_shaped(value: &Value, path: &str) -> Result<(), SettingsError> {
    match value {
        Value::Null | Value::Bool(_) | Value::String(_) | Value::Number(_) => Ok(()),
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                assert_json_shaped(item, &format!("{path}[{i}]"))?;
            }
            Ok(())
        }
        Value::Object(map) => {
            for (key, item) in map {
                assert_json_shaped(item, &format!("{path}.{key}"))?;
            }
            Ok(())
        }
    }
}
