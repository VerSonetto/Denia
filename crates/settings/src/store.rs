use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde_json::{Map, Value};
use tokio::sync::broadcast;

use crate::error::{SettingsError, assert_json_shaped};
use crate::is_valid_namespace;
use crate::merge::deep_merge;
use crate::spec::{NamespaceSpec, NamespaceView, SecretInfo, leaf_at, redact_secrets};

/// Topology and write notifications for wire push.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingsEvent {
    /// The resolved value of `ns` may have changed.
    Updated { ns: String },
    /// The stored user section of `ns` changed; `revision` is the new value.
    DocumentUpdated { ns: String, revision: u64 },
}

struct NamespaceState {
    spec: NamespaceSpec,
    base: Value,
    user: Value,
    revision: u64,
}

struct Inner {
    namespaces: HashMap<String, NamespaceState>,
    document: SettingsStoreDocument,
}

/// The in-memory settings authority over one `settings.yaml`.
///
/// Writes are serialized by the store lock, re-validated against the
/// namespace spec, and persisted atomically. The document maps namespace
/// names to user sections; unregistered sections stay on disk and are
/// surfaced again if their namespace registers later.
pub struct SettingsStore {
    inner: RwLock<Inner>,
    path: PathBuf,
    events: Option<broadcast::Sender<SettingsEvent>>,
}

impl SettingsStore {
    /// Opens (or creates) `<home>/settings.yaml` and loads any existing
    /// document. A malformed document fails the open loudly.
    pub fn open(home: &Path) -> Result<Self, SettingsError> {
        std::fs::create_dir_all(home)?;
        let path = home.join("settings.yaml");
        let document = if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            parse_document(&text)?
        } else {
            SettingsStoreDocument::default()
        };
        Ok(Self {
            inner: RwLock::new(Inner {
                namespaces: HashMap::new(),
                document,
            }),
            path,
            events: None,
        })
    }

    /// Attaches the push channel used for wire invalidation.
    pub fn with_events(mut self, sender: broadcast::Sender<SettingsEvent>) -> Self {
        self.events = Some(sender);
        self
    }

    /// The document path this store persists to.
    pub fn document_path(&self) -> &Path {
        &self.path
    }

    /// Registers one namespace with its spec and composition base layer.
    /// The stored user section (if any) is re-validated at registration.
    pub fn register(
        &self,
        ns: &str,
        spec: NamespaceSpec,
        base: Value,
    ) -> Result<(), SettingsError> {
        if !is_valid_namespace(ns) {
            return Err(SettingsError::InvalidName(ns.to_string()));
        }
        let mut inner = self.inner.write().unwrap();
        if inner.namespaces.contains_key(ns) {
            return Err(SettingsError::DuplicateNamespace(ns.to_string()));
        }
        let user = inner
            .document
            .sections
            .get(ns)
            .cloned()
            .unwrap_or(Value::Null);
        if !user.is_null() {
            let merged = deep_merge(&deep_merge(&spec.defaults, &base), &user);
            (spec.validate)(merged).map_err(SettingsError::Rejected)?;
        }
        inner.namespaces.insert(
            ns.to_string(),
            NamespaceState {
                spec,
                base,
                user,
                revision: 0,
            },
        );
        Ok(())
    }

    /// The resolved value of one namespace (defaults + base + user).
    pub fn resolved(&self, ns: &str) -> Result<Value, SettingsError> {
        let inner = self.inner.read().unwrap();
        let state = inner
            .namespaces
            .get(ns)
            .ok_or_else(|| SettingsError::UnknownNamespace(ns.to_string()))?;
        Ok(resolve(state))
    }

    /// Redacted views of every registered namespace.
    pub fn describe(&self) -> Vec<NamespaceView> {
        let inner = self.inner.read().unwrap();
        let mut names: Vec<&String> = inner.namespaces.keys().collect();
        names.sort();
        names.iter().map(|ns| view_of(ns, &inner.namespaces[*ns])).collect()
    }

    /// Merges `patch` into the user section with optimistic concurrency.
    pub fn update(
        &self,
        ns: &str,
        patch: Value,
        expected_revision: Option<u64>,
    ) -> Result<NamespaceView, SettingsError> {
        assert_json_shaped(&patch, ns)?;
        let mut inner = self.inner.write().unwrap();
        let next_user = {
            let state = inner
                .namespaces
                .get_mut(ns)
                .ok_or_else(|| SettingsError::UnknownNamespace(ns.to_string()))?;
            check_revision(state, expected_revision)?;
            let base_section = if state.user.is_null() {
                Value::Object(Map::new())
            } else {
                state.user.clone()
            };
            deep_merge(&base_section, &patch)
        };
        self.commit_write(&mut inner, ns, next_user)
    }

    /// Replaces the user section wholesale; absent keys re-inherit from the
    /// base layers. An empty object resets the namespace.
    pub fn replace(
        &self,
        ns: &str,
        section: Value,
        expected_revision: Option<u64>,
    ) -> Result<NamespaceView, SettingsError> {
        assert_json_shaped(&section, ns)?;
        let section = if section.is_null() {
            Value::Object(Map::new())
        } else {
            section
        };
        if !section.is_object() {
            return Err(SettingsError::Rejected(
                "a settings section must be an object".to_string(),
            ));
        }
        let mut inner = self.inner.write().unwrap();
        {
            let state = inner
                .namespaces
                .get_mut(ns)
                .ok_or_else(|| SettingsError::UnknownNamespace(ns.to_string()))?;
            check_revision(state, expected_revision)?;
        }
        self.commit_write(&mut inner, ns, section)
    }
}

fn resolve(state: &NamespaceState) -> Value {
    let merged = deep_merge(&state.spec.defaults, &state.base);
    if state.user.is_null() {
        merged
    } else {
        deep_merge(&merged, &state.user)
    }
}

fn view_of(ns: &String, state: &NamespaceState) -> NamespaceView {
    let secrets = state.spec.secrets;
    let secret_infos = secrets
        .iter()
        .map(|path| SecretInfo {
            path: path.iter().map(|s| s.to_string()).collect(),
            set: leaf_at(&resolve(state), path).is_some(),
        })
        .collect();
    NamespaceView {
        ns: ns.clone(),
        value: redact_secrets(&resolve(state), secrets),
        base: if state.base.is_null() {
            None
        } else {
            Some(redact_secrets(&state.base, secrets))
        },
        user: if state.user.is_null() {
            None
        } else {
            Some(redact_secrets(&state.user, secrets))
        },
        revision: state.revision,
        applies: state.spec.applies,
        secrets: secret_infos,
    }
}

fn check_revision(state: &NamespaceState, expected: Option<u64>) -> Result<(), SettingsError> {
    match expected {
        Some(expected) if expected != state.revision => Err(SettingsError::Conflict {
            expected,
            actual: state.revision,
        }),
        _ => Ok(()),
    }
}

/// Validates, installs, persists, and announces one user-section write.
/// Borrows of `inner.namespaces` and `inner.document` stay sequential so
/// the disjoint fields never overlap.
impl SettingsStore {
    fn commit_write(
        &self,
        inner: &mut Inner,
        ns: &str,
        next_user: Value,
    ) -> Result<NamespaceView, SettingsError> {
        let revision = {
            let state = inner
                .namespaces
                .get_mut(ns)
                .ok_or_else(|| SettingsError::UnknownNamespace(ns.to_string()))?;
            let merged = deep_merge(&deep_merge(&state.spec.defaults, &state.base), &next_user);
            (state.spec.validate)(merged).map_err(SettingsError::Rejected)?;
            state.user = next_user;
            state.revision += 1;
            state.revision
        };
        let stored = inner
            .namespaces
            .get(ns)
            .map(|state| state.user.clone())
            .unwrap_or(Value::Null);
        if stored.is_null() || stored.as_object().is_some_and(|map| map.is_empty()) {
            inner.document.sections.remove(ns);
        } else {
            inner.document.sections.insert(ns.to_string(), stored);
        }
        persist(self, inner)?;
        let state = inner
            .namespaces
            .get(ns)
            .ok_or_else(|| SettingsError::UnknownNamespace(ns.to_string()))?;
        let view = view_of(&ns.to_string(), state);
        if let Some(sender) = &self.events {
            let _ = sender.send(SettingsEvent::Updated { ns: ns.to_string() });
            let _ = sender.send(SettingsEvent::DocumentUpdated {
                ns: ns.to_string(),
                revision,
            });
        }
        Ok(view)
    }
}

fn persist(store: &SettingsStore, inner: &Inner) -> Result<(), SettingsError> {
    let mut doc = serde_yaml::to_string(&inner.document)
        .map_err(|e| SettingsError::Parse(e.to_string()))?;
    if !doc.ends_with('\n') {
        doc.push('\n');
    }
    atomic_write(&store.path, doc.as_bytes())?;
    Ok(())
}

/// Writes via temp file + rename so readers never observe a torn document.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, bytes)?;
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

fn parse_document(text: &str) -> Result<SettingsStoreDocument, SettingsError> {
    if text.trim().is_empty() {
        return Ok(SettingsStoreDocument::default());
    }
    let value: serde_yaml::Value =
        serde_yaml::from_str(text).map_err(|e| SettingsError::Parse(e.to_string()))?;
    let mapping = match value {
        serde_yaml::Value::Mapping(map) => map,
        _ => {
            return Err(SettingsError::Parse(
                "settings root must be a mapping".to_string(),
            ))
        }
    };
    let mut sections: Map<String, Value> = Map::new();
    for (key, section) in mapping {
        let key = yaml_string_key(key)?;
        let section = json_from_yaml(section)?;
        if !section.is_object() && !section.is_null() {
            return Err(SettingsError::Parse(format!(
                "settings section '{key}' must be a mapping"
            )));
        }
        sections.insert(key, section);
    }
    Ok(SettingsStoreDocument { sections })
}

fn yaml_string_key(key: serde_yaml::Value) -> Result<String, SettingsError> {
    match key {
        serde_yaml::Value::String(s) => Ok(s),
        other => Err(SettingsError::Parse(format!(
            "settings namespace key must be a string, got: {other:?}"
        ))),
    }
}

fn json_from_yaml(value: serde_yaml::Value) -> Result<Value, SettingsError> {
    serde_json::to_value(value).map_err(|e| SettingsError::Parse(e.to_string()))
}

/// On-disk document shape: a bare map of namespace name -> user section.
#[derive(Debug, Clone, Default)]
pub struct SettingsStoreDocument {
    pub sections: Map<String, Value>,
}

impl serde::Serialize for SettingsStoreDocument {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.sections.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for SettingsStoreDocument {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self {
            sections: Map::deserialize(deserializer)?,
        })
    }
}
