//! The API error envelope shared by every endpoint.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// Stable wire error: `{ code, message }` plus optional details.
pub struct ApiError {
    pub status: StatusCode,
    pub code: String,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn bad_request(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    pub fn from_llm(error: denia_core::error::LlmError) -> Self {
        Self::new(StatusCode::BAD_REQUEST, error.code, error.message)
    }

    pub fn from_settings(error: denia_settings::SettingsError) -> Self {
        let status = match &error {
            denia_settings::SettingsError::UnknownNamespace(_) => StatusCode::NOT_FOUND,
            denia_settings::SettingsError::Conflict { .. } => StatusCode::CONFLICT,
            denia_settings::SettingsError::Rejected(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, error.code(), error.to_string())
    }

    pub fn from_credential(error: denia_credentials::CredentialError) -> Self {
        Self::new(StatusCode::BAD_REQUEST, error.code(), error.to_string())
    }

    pub fn from_session(error: denia_session::SessionError) -> Self {
        match &error {
            denia_session::SessionError::NotFound(id) => Self::new(
                StatusCode::NOT_FOUND,
                "session/not-found",
                format!("session not found: {id}"),
            ),
            denia_session::SessionError::InvalidId(id) => Self::new(
                StatusCode::BAD_REQUEST,
                "session/invalid-id",
                format!("session id is not usable: {id}"),
            ),
            denia_session::SessionError::Corrupt(message) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "session/corrupt",
                message.clone(),
            ),
            denia_session::SessionError::Json(error) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "session/serialize",
                error.to_string(),
            ),
            denia_session::SessionError::Io(io) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "session/io",
                io.to_string(),
            ),
            denia_session::SessionError::NotARewindPoint(seq) => Self::new(
                StatusCode::BAD_REQUEST,
                "session/not-rewind-point",
                format!("seq {seq} is not a rewindable user message"),
            ),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({
            "error": {
                "code": self.code,
                "message": self.message,
            }
        });
        (self.status, Json(body)).into_response()
    }
}
