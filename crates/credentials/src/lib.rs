//! Credential store: API-key references resolved through a layered chain.
//!
//! A `CredentialRef` is a POSIX environment-variable name. Resolution order:
//! inherited process environment (read-only, wins) > `.credentials.yaml`
//! (writable) > project `.env` > home `.env`. Empty stored values are absent.
//! Values only cross the wire into `set`; nothing ever reads them out.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::broadcast;

const DOCUMENT_VERSION: u32 = 1;

/// Credential-store failures, surfaced verbatim to API callers.
#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("invalid credential reference name: {0}")]
    InvalidRefName(String),
    #[error("credential value rejected: {0}")]
    InvalidValue(String),
    #[error("credential reference {0} is shadowed by the process environment; unset it in the shell that starts the server instead")]
    Shadowed(String),
    #[error("unsupported credentials document version: {0}")]
    Version(u32),
    #[error("credentials document parse error: {0}")]
    Parse(String),
    #[error("credentials I/O: {0}")]
    Io(#[from] std::io::Error),
}

impl CredentialError {
    /// Stable wire code for API error envelopes.
    pub fn code(&self) -> &'static str {
        match self {
            CredentialError::InvalidRefName(_) => "credential/invalid-name",
            CredentialError::InvalidValue(_) => "credential/invalid-value",
            CredentialError::Shadowed(_) => "credential/shadowed",
            CredentialError::Version(_) => "credential/version",
            CredentialError::Parse(_) => "credential/parse",
            CredentialError::Io(_) => "credential/io",
        }
    }
}

/// A resolved credential value plus the layer that supplied it.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedCredential {
    pub value: String,
    pub source: &'static str,
}

/// Wire-facing description of one reference; never carries the value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialInfo {
    pub configured: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<&'static str>,
    /// False when the process environment shadows file writes.
    pub writable: bool,
}

/// A stored-reference write landed or was removed.
#[derive(Debug, Clone, PartialEq)]
pub enum CredentialEvent {
    ReferenceUpdated { reference: String },
}

#[derive(Debug, Serialize, Deserialize)]
struct CredentialDoc {
    version: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    refs: BTreeMap<String, String>,
}

impl Default for CredentialDoc {
    fn default() -> Self {
        Self {
            version: DOCUMENT_VERSION,
            refs: BTreeMap::new(),
        }
    }
}

/// The credential authority over one `.credentials.yaml`.
pub struct CredentialStore {
    path: PathBuf,
    home: PathBuf,
    state: RwLock<CredentialDoc>,
    events: Option<broadcast::Sender<CredentialEvent>>,
}

impl CredentialStore {
    /// Opens (or creates) `<home>/.credentials.yaml`. A malformed or
    /// wrong-version document fails the open loudly.
    pub fn open(home: &Path) -> Result<Self, CredentialError> {
        std::fs::create_dir_all(home)?;
        let path = home.join(".credentials.yaml");
        let doc = if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            parse_document(&text)?
        } else {
            CredentialDoc::default()
        };
        Ok(Self {
            path,
            home: home.to_path_buf(),
            state: RwLock::new(doc),
            events: None,
        })
    }

    /// Attaches the push channel used for wire invalidation.
    pub fn with_events(mut self, sender: broadcast::Sender<CredentialEvent>) -> Self {
        self.events = Some(sender);
        self
    }

    /// Resolves one reference through the full chain, per call. Consumers
    /// must not cache the value: re-resolution is how key changes reach the
    /// next request.
    pub fn resolve(&self, reference: &str) -> Result<Option<ResolvedCredential>, CredentialError> {
        validate_ref_name(reference)?;
        if let Ok(value) = std::env::var(reference) {
            if !value.is_empty() {
                return Ok(Some(ResolvedCredential { value, source: "env" }));
            }
        }
        let stored = self.state.read().unwrap().refs.get(reference).cloned();
        if let Some(value) = stored.filter(|v| !v.is_empty()) {
            return Ok(Some(ResolvedCredential { value, source: "file" }));
        }
        let fallbacks: [(Option<PathBuf>, &str); 2] = [
            (std::env::current_dir().ok(), "project-env"),
            (Some(self.home.clone()), "user-env"),
        ];
        for (dir, source) in fallbacks {
            if let Some(dir) = dir {
                if let Some(value) = read_dotenv_key(&dir.join(".env"), reference)? {
                    if !value.is_empty() {
                        return Ok(Some(ResolvedCredential { value, source }));
                    }
                }
            }
        }
        Ok(None)
    }

    /// Describes one reference without revealing its value.
    pub fn describe(&self, reference: &str) -> Result<CredentialInfo, CredentialError> {
        match self.resolve(reference)? {
            Some(resolved) => Ok(CredentialInfo {
                configured: true,
                source: Some(resolved.source),
                writable: resolved.source != "env",
            }),
            None => Ok(CredentialInfo {
                configured: false,
                source: None,
                writable: true,
            }),
        }
    }

    /// Stores a non-empty key value under `reference`.
    pub fn set(&self, reference: &str, value: &str) -> Result<(), CredentialError> {
        validate_ref_name(reference)?;
        let value = value.trim();
        if value.is_empty() {
            return Err(CredentialError::InvalidValue(
                "an API key must not be empty".to_string(),
            ));
        }
        if !value.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
            return Err(CredentialError::InvalidValue(
                "an API key must be printable ASCII without spaces".to_string(),
            ));
        }
        if std::env::var(reference).is_ok_and(|v| !v.is_empty()) {
            return Err(CredentialError::Shadowed(reference.to_string()));
        }
        {
            let mut doc = self.state.write().unwrap();
            doc.refs.insert(reference.to_string(), value.to_string());
            persist(&self.path, &doc)?;
        }
        self.announce(reference);
        Ok(())
    }

    /// Removes a stored key; absent references succeed idempotently.
    pub fn unset(&self, reference: &str) -> Result<(), CredentialError> {
        validate_ref_name(reference)?;
        if std::env::var(reference).is_ok_and(|v| !v.is_empty()) {
            return Err(CredentialError::Shadowed(reference.to_string()));
        }
        let removed = {
            let mut doc = self.state.write().unwrap();
            let removed = doc.refs.remove(reference).is_some();
            if removed {
                persist(&self.path, &doc)?;
            }
            removed
        };
        if removed {
            self.announce(reference);
        }
        Ok(())
    }

    fn announce(&self, reference: &str) {
        if let Some(sender) = &self.events {
            let _ = sender.send(CredentialEvent::ReferenceUpdated {
                reference: reference.to_string(),
            });
        }
    }
}

/// POSIX environment-variable grammar: `^[A-Za-z_][A-Za-z0-9_]*$`.
fn validate_ref_name(reference: &str) -> Result<(), CredentialError> {
    let bytes = reference.as_bytes();
    let valid = !bytes.is_empty()
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_');
    if valid {
        Ok(())
    } else {
        Err(CredentialError::InvalidRefName(reference.to_string()))
    }
}

fn parse_document(text: &str) -> Result<CredentialDoc, CredentialError> {
    if text.trim().is_empty() {
        return Ok(CredentialDoc::default());
    }
    let doc: CredentialDoc =
        serde_yaml::from_str(text).map_err(|e| CredentialError::Parse(e.to_string()))?;
    if doc.version != DOCUMENT_VERSION {
        return Err(CredentialError::Version(doc.version));
    }
    for (reference, value) in &doc.refs {
        validate_ref_name(reference)?;
        if value.is_empty() {
            return Err(CredentialError::Parse(format!(
                "credential '{reference}' has an empty value; empty means absent"
            )));
        }
    }
    Ok(doc)
}

fn persist(path: &Path, doc: &CredentialDoc) -> Result<(), CredentialError> {
    let mut text =
        serde_yaml::to_string(doc).map_err(|e| CredentialError::Parse(e.to_string()))?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, text.as_bytes())?;
    set_owner_only_permissions(&tmp);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_owner_only_permissions(_path: &Path) {}

/// Reads one key from a `.env` file: `KEY=value` lines, `#` comments,
/// optional `export` prefix and single/double quoting.
fn read_dotenv_key(path: &Path, key: &str) -> Result<Option<String>, CredentialError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, raw)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim().strip_prefix("export ").unwrap_or(name.trim());
        if name != key {
            continue;
        }
        let mut value = raw.trim();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = &value[1..value.len() - 1];
        }
        return Ok(Some(value.to_string()));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::{read_dotenv_key, validate_ref_name};
    use std::io::Write;

    #[test]
    fn ref_name_grammar() {
        assert!(validate_ref_name("DEEPSEEK_API_KEY").is_ok());
        assert!(validate_ref_name("_X1").is_ok());
        assert!(validate_ref_name("1KEY").is_err());
        assert!(validate_ref_name("has-dash").is_err());
        assert!(validate_ref_name("").is_err());
    }

    #[test]
    fn dotenv_parsing() {
        let dir = std::env::temp_dir().join(format!("dshrs-dotenv-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let env_path = dir.join(".env");
        let mut file = std::fs::File::create(&env_path).unwrap();
        writeln!(file, "# comment\nFOO=bar\nexport BAZ=\"quoted\"\nEMPTY=\nQUOTED='single'").unwrap();
        assert_eq!(
            read_dotenv_key(&env_path, "FOO").unwrap().as_deref(),
            Some("bar")
        );
        assert_eq!(
            read_dotenv_key(&env_path, "BAZ").unwrap().as_deref(),
            Some("quoted")
        );
        assert_eq!(
            read_dotenv_key(&env_path, "EMPTY").unwrap().as_deref(),
            Some("")
        );
        assert_eq!(
            read_dotenv_key(&env_path, "QUOTED").unwrap().as_deref(),
            Some("single")
        );
        assert_eq!(read_dotenv_key(&env_path, "ABSENT").unwrap(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn uuid_like() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            .to_string()
    }
}
