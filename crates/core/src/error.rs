//! The failure taxonomy shared across adapters and the wire.
//!
//! An [`LlmFailure`] is a frozen, serializable fact snapshot taken at the
//! adapter boundary; [`LlmError`] is the live error carrying that snapshot.

use serde::{Deserialize, Serialize};

/// Canonical failure codes, kept as strings for wire stability.
pub mod codes {
    pub const NO_ADAPTER: &str = "NO_ADAPTER";
    pub const MISSING_CREDENTIAL: &str = "MISSING_CREDENTIAL";
    pub const INVALID_CREDENTIAL: &str = "INVALID_CREDENTIAL";
    pub const AUTH: &str = "AUTH";
    pub const RATE_LIMIT: &str = "RATE_LIMIT";
    pub const QUOTA: &str = "QUOTA";
    pub const SERVER: &str = "SERVER";
    pub const TIMEOUT: &str = "TIMEOUT";
    pub const TRANSPORT: &str = "TRANSPORT";
    pub const ABORTED: &str = "ABORTED";
    pub const CONTEXT_WINDOW_EXCEEDED: &str = "CONTEXT_WINDOW_EXCEEDED";
    pub const INVALID_REQUEST: &str = "INVALID_REQUEST";
    pub const EMPTY_RESPONSE: &str = "EMPTY_RESPONSE";
    pub const UNSUPPORTED_CONTENT: &str = "UNSUPPORTED_CONTENT";
    pub const UNSUPPORTED_REASONING_EFFORT: &str = "UNSUPPORTED_REASONING_EFFORT";
    pub const MALFORMED_RESPONSE: &str = "MALFORMED_RESPONSE";
    pub const STREAM_CLOSED: &str = "STREAM_CLOSED";
    pub const STEP_LIMIT: &str = "STEP_LIMIT";
    pub const SETTINGS: &str = "SETTINGS";
    pub const UNKNOWN: &str = "UNKNOWN";
}

/// Serializable failure facts, frozen at the adapter boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmFailure {
    pub message: String,
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_retry_after_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

impl LlmFailure {
    /// Build a failure under a canonical [`codes`] entry.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: code.into(),
            status: None,
            provider_retry_after_ms: None,
            request_id: None,
        }
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    pub fn with_retry_after_ms(mut self, ms: u64) -> Self {
        self.provider_retry_after_ms = Some(ms);
        self
    }

    pub fn with_request_id(mut self, id: impl Into<String>) -> Self {
        self.request_id = Some(id.into());
        self
    }
}

/// The live error form carrying the frozen failure snapshot.
#[derive(Debug, Clone, thiserror::Error)]
#[error("[{code}] {message}")]
pub struct LlmError {
    pub message: String,
    pub code: String,
    pub failure: LlmFailure,
}

impl LlmError {
    /// Build an error whose snapshot matches a canonical [`codes`] entry.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        let failure = LlmFailure::new(code, message);
        Self::from_failure(failure)
    }

    pub fn from_failure(failure: LlmFailure) -> Self {
        Self {
            message: failure.message.clone(),
            code: failure.code.clone(),
            failure,
        }
    }
}
