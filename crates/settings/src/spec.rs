use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Whether a namespace change takes effect live or on restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Applies {
    Live,
    Restart,
}

/// One registered namespace's schema authority.
///
/// `defaults` anchors the layering and supplies types; `validate` accepts the
/// fully merged section and returns a normalized copy (or a rejection reason
/// that names the offending field); `secrets` lists leaf paths redacted on
/// every wire-facing describe.
pub struct NamespaceSpec {
    pub defaults: Value,
    pub validate: fn(Value) -> Result<Value, String>,
    pub secrets: &'static [&'static [&'static str]],
    pub applies: Applies,
}

/// One secret's wire-facing state: its path and whether it is set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretInfo {
    pub path: Vec<String>,
    pub set: bool,
}

/// The redacted, wire-facing view of one namespace.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceView {
    pub ns: String,
    /// Resolved value (defaults + base + user), secrets redacted.
    pub value: Value,
    /// The composition base layer, when non-null, secrets redacted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<Value>,
    /// The stored user section, when non-null, secrets redacted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<Value>,
    pub revision: u64,
    pub applies: Applies,
    pub secrets: Vec<SecretInfo>,
}

/// Reads a leaf value at `path` inside `value`, if present and non-null.
pub fn leaf_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        current = current.get(key)?;
    }
    match current {
        Value::Null => None,
        other => Some(other),
    }
}

/// Returns a copy of `value` with every secret path replaced by null.
pub fn redact_secrets(value: &Value, secrets: &[&[&str]]) -> Value {
    let mut out = value.clone();
    for path in secrets {
        redact_path(&mut out, path);
    }
    out
}

fn redact_path(value: &mut Value, path: &[&str]) {
    if path.is_empty() {
        return;
    }
    if path.len() == 1 {
        if let Value::Object(map) = value {
            if map.contains_key(path[0]) {
                map.insert(path[0].to_string(), Value::Null);
            }
        }
        return;
    }
    if let Some(next) = value.get_mut(path[0]) {
        redact_path(next, &path[1..]);
    }
}
