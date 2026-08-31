//! Namespaced user-settings store.
//!
//! One YAML document (`<home>/settings.yaml`) maps namespace names to user
//! sections. Every registered namespace resolves through three layers —
//! schema defaults, composition base, user section — and carries a monotonic
//! revision for optimistic concurrency. Secret paths are redacted on every
//! wire-facing describe.

mod error;
mod merge;
mod spec;
mod store;

pub use error::SettingsError;
pub use merge::deep_merge;
pub use spec::{Applies, NamespaceSpec, NamespaceView, SecretInfo};
pub use store::{SettingsEvent, SettingsStore};

/// Validates a namespace name: lowercase alphanumeric segments led by a
/// letter, joined by single hyphens (`^[a-z][a-z0-9-]*$`).
pub fn is_valid_namespace(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    if !bytes[0].is_ascii_lowercase() {
        return false;
    }
    bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

#[cfg(test)]
mod tests {
    use super::is_valid_namespace;

    #[test]
    fn namespace_grammar() {
        assert!(is_valid_namespace("agent-default-model"));
        assert!(is_valid_namespace("llm"));
        assert!(!is_valid_namespace(""));
        assert!(!is_valid_namespace("Agent"));
        assert!(!is_valid_namespace("9lives"));
        assert!(!is_valid_namespace("has_underscore"));
    }
}
