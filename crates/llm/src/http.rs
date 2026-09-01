//! Shared HTTP error mapping for chat-completions endpoints.

use denia_core::error::{LlmFailure, codes};
use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::StatusCode;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct WireErrorBody {
    #[serde(default)]
    error: Option<WireErrorDetail>,
}

#[derive(Debug, Deserialize)]
struct WireErrorDetail {
    #[serde(default)]
    message: Option<String>,
}

const MESSAGE_CAP: usize = 500;

/// Maps one failed HTTP response onto the failure taxonomy.
pub fn http_error_failure(status: StatusCode, body: &str, headers: &HeaderMap) -> LlmFailure {
    let message = error_message(body);
    let code = match status.as_u16() {
        401 | 403 => codes::AUTH,
        429 => {
            if message.to_lowercase().contains("quota") {
                codes::QUOTA
            } else {
                codes::RATE_LIMIT
            }
        }
        400 => {
            let lower = message.to_lowercase();
            if lower.contains("context") || lower.contains("length") || lower.contains("tokens") {
                codes::CONTEXT_WINDOW_EXCEEDED
            } else {
                codes::INVALID_REQUEST
            }
        }
        408 => codes::TIMEOUT,
        _ if status.is_server_error() => codes::SERVER,
        _ => codes::INVALID_REQUEST,
    };
    let mut failure = LlmFailure::new(code, message).with_status(status.as_u16());
    if let Some(ms) = retry_after_ms(headers) {
        failure = failure.with_retry_after_ms(ms);
    }
    if let Some(id) = headers
        .get("x-request-id")
        .or_else(|| headers.get("x-deepseek-request-id"))
        .and_then(|v| v.to_str().ok())
    {
        failure = failure.with_request_id(id);
    }
    failure
}

fn error_message(body: &str) -> String {
    let message = serde_json::from_str::<WireErrorBody>(body)
        .ok()
        .and_then(|parsed| parsed.error)
        .and_then(|detail| detail.message)
        .unwrap_or_else(|| {
            let trimmed = body.trim();
            if trimmed.is_empty() {
                "the provider returned no error body".to_string()
            } else {
                trimmed.to_string()
            }
        });
    let mut chars: Vec<char> = message.chars().take(MESSAGE_CAP).collect();
    if message.chars().count() > MESSAGE_CAP {
        chars.push('…');
    }
    chars.into_iter().collect()
}

/// Parses `Retry-After` as delta seconds or an HTTP date.
fn retry_after_ms(headers: &HeaderMap) -> Option<u64> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?;
    if let Some(seconds) = value.trim().parse::<u64>().ok() {
        return Some(seconds.saturating_mul(1000));
    }
    None
}

/// Maps a reqwest send/transport error onto the taxonomy.
pub fn transport_failure(error: reqwest::Error) -> LlmFailure {
    if error.is_timeout() {
        LlmFailure::new(codes::TIMEOUT, format!("request timed out: {error}"))
    } else {
        LlmFailure::new(codes::TRANSPORT, format!("transport error: {error}"))
    }
}

/// The [`transport_failure`] snapshot wrapped as the live error form.
pub fn transport_error(error: reqwest::Error) -> denia_core::error::LlmError {
    denia_core::error::LlmError::from_failure(transport_failure(error))
}
