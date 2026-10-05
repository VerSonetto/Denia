use denia_session::SessionError;

#[derive(Debug, Clone, Copy)]
pub enum FailureKind {
    BadRequest,
    Conflict,
    NotFound,
    Internal,
}

#[derive(Debug)]
pub struct CommandError {
    pub kind: FailureKind,
    pub code: String,
    pub message: String,
}

impl CommandError {
    pub fn new(kind: FailureKind, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind,
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn bad_request(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(FailureKind::BadRequest, code, message)
    }

    pub fn from_llm(error: denia_core::error::LlmError) -> Self {
        Self::bad_request(error.code, error.message)
    }

    pub fn from_session(error: SessionError) -> Self {
        let (kind, code) = match &error {
            SessionError::NotFound(_) => (FailureKind::NotFound, "session/not-found"),
            SessionError::InvalidId(_) => (FailureKind::BadRequest, "session/invalid-id"),
            SessionError::Corrupt(_) => (FailureKind::Internal, "session/corrupt"),
            SessionError::Json(_) => (FailureKind::Internal, "session/serialize"),
            SessionError::Io(_) => (FailureKind::Internal, "session/io"),
            SessionError::NotARewindPoint(_) => {
                (FailureKind::BadRequest, "session/not-rewind-point")
            }
        };
        Self::new(kind, code, error.to_string())
    }
}
