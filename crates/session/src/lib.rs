//! Event-sourced session storage: one JSONL file per session.
//!
//! Layout: `<home>/sessions/<id>/session.jsonl`; line 1 is the header, every
//! following line one envelope. Loads repair torn tails (truncating at the
//! first unparseable line) and close crash-orphaned turns with a synthetic
//! `turn-end { aborted }` — events are never dropped.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use denia_core::message::ChatMessage;
use denia_core::session::{
    SessionEnvelope, SessionEvent, SessionHeader, SessionHeaderKind, TurnEndReason,
    SESSION_FORMAT_VERSION, derive_messages,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("session id is not usable: {0}")]
    InvalidId(String),
    #[error("session document is corrupt: {0}")]
    Corrupt(String),
    #[error("session serialization: {0}")]
    Json(#[from] serde_json::Error),
    #[error("session I/O: {0}")]
    Io(#[from] std::io::Error),
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Mutable log state behind the per-session lock.
struct SessionInner {
    events: Vec<SessionEnvelope>,
    handle: File,
}

/// One live session: header, in-memory log, and its append handle. The log is
/// internally synchronized, so readers (snapshot, SSE replay) never wait for
/// a running turn to finish.
pub struct Session {
    header: SessionHeader,
    file: PathBuf,
    inner: Mutex<SessionInner>,
}

impl Session {
    /// Creates a fresh session file (header line only) inside `dir`.
    /// `id` becomes both the header id and, by store convention, the
    /// directory name. `sandbox` confines file tools to `cwd`.
    pub fn create(dir: &Path, id: String, cwd: &Path, sandbox: bool) -> Result<Session, SessionError> {
        std::fs::create_dir_all(dir)?;
        let file = dir.join("session.jsonl");
        let header = SessionHeader {
            kind: SessionHeaderKind::Session,
            version: SESSION_FORMAT_VERSION,
            id,
            created_at: now_millis(),
            cwd: cwd.to_string_lossy().to_string(),
            sandbox,
        };
        let mut handle = File::create(&file)?;
        writeln!(handle, "{}", serde_json::to_string(&header)?)?;
        handle.flush()?;
        let handle = open_append(&file)?;
        Ok(Session {
            header,
            file,
            inner: Mutex::new(SessionInner {
                events: Vec::new(),
                handle,
            }),
        })
    }

    /// Loads one session, repairing torn tails and orphaned turns.
    pub fn load(file: &Path) -> Result<Session, SessionError> {
        let mut raw = String::new();
        File::open(file)?.read_to_string(&mut raw)?;

        let mut lines = raw.split('\n');
        let header_line = lines.next().unwrap_or_default();
        let header: SessionHeader = serde_json::from_str(header_line)
            .map_err(|e| SessionError::Corrupt(format!("bad header: {e}")))?;
        if header.version != SESSION_FORMAT_VERSION {
            return Err(SessionError::Corrupt(format!(
                "unsupported session format version {}",
                header.version
            )));
        }

        // Byte offsets track the committed prefix for torn-tail truncation.
        let mut consumed = header_line.len() + 1;
        let mut events: Vec<SessionEnvelope> = Vec::new();
        let mut torn_at: Option<usize> = None;
        for line in lines {
            let with_newline = line.len() + 1;
            if line.trim().is_empty() {
                consumed += with_newline;
                continue;
            }
            match serde_json::from_str::<SessionEnvelope>(line) {
                Ok(envelope) => events.push(envelope),
                Err(_) => {
                    if line.trim_end().ends_with('}') {
                        // Retired or unknown event types are skipped so older
                        // logs keep loading.
                        consumed += with_newline;
                        continue;
                    }
                    torn_at = Some(consumed);
                    break;
                }
            }
            consumed += with_newline;
        }

        if let Some(torn) = torn_at {
            // Truncate to the last good boundary; the torn tail is discarded.
            let file_handle = OpenOptions::new().write(true).open(file)?;
            file_handle.set_len(torn as u64)?;
        }

        let session = Session {
            header,
            file: file.to_path_buf(),
            inner: Mutex::new(SessionInner {
                events,
                handle: open_append(file)?,
            }),
        };
        session.close_orphaned_turn()?;
        Ok(session)
    }

    pub fn id(&self) -> &str {
        &self.header.id
    }

    pub fn header(&self) -> &SessionHeader {
        &self.header
    }

    /// A detached copy of the log; readers never borrow through the lock.
    pub fn events(&self) -> Vec<SessionEnvelope> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .events
            .clone()
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    /// Appends one event, assigning contiguous seq and wall-clock time. The
    /// log lock is held only for this write, so concurrent readers observe
    /// live progress instead of waiting for the whole turn.
    pub fn append(&self, event: SessionEvent) -> Result<SessionEnvelope, SessionError> {
        let mut inner = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
        let envelope = SessionEnvelope {
            seq: inner.events.len() as u64 + 1,
            time: now_millis(),
            event,
        };
        let line = serde_json::to_string(&envelope)?;
        writeln!(inner.handle, "{line}")?;
        inner.handle.flush()?;
        inner.events.push(envelope.clone());
        Ok(envelope)
    }

    /// The model-facing history projected from the log.
    pub fn derive_messages(&self) -> Vec<ChatMessage> {
        let inner = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
        derive_messages(&inner.events)
    }

    /// The first user prompt, trimmed, for list views.
    pub fn first_prompt_excerpt(&self, max_chars: usize) -> Option<String> {
        let inner = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
        let text = inner.events.iter().find_map(|envelope| match &envelope.event {
            SessionEvent::UserMessage { text, .. } => Some(text),
            _ => None,
        })?;
        let trimmed = text.trim();
        let mut chars = trimmed.chars();
        let head: String = chars.by_ref().take(max_chars).collect();
        Some(if chars.next().is_some() {
            format!("{head}…")
        } else {
            head
        })
    }

    /// Crash recovery: a `turn-start` without `turn/end` gets a synthetic
    /// aborted close, persisted like any other event.
    fn close_orphaned_turn(&self) -> Result<(), SessionError> {
        let open_turn = {
            let inner = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
            let mut open_turn: Option<u32> = None;
            for envelope in &inner.events {
                match &envelope.event {
                    SessionEvent::TurnStart { turn } => open_turn = Some(*turn),
                    SessionEvent::TurnEnd { .. } => open_turn = None,
                    _ => {}
                }
            }
            open_turn
        };
        if let Some(turn) = open_turn {
            self.append(SessionEvent::TurnEnd {
                turn,
                reason: TurnEndReason::Aborted,
            })?;
        }
        Ok(())
    }
}

fn open_append(file: &Path) -> Result<File, SessionError> {
    Ok(OpenOptions::new().append(true).create(true).open(file)?)
}

/// One row of the session list.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub created_at: u64,
    pub excerpt: Option<String>,
    pub cwd: Option<String>,
    pub sandbox: Option<bool>,
    /// Whether the recorded working directory still exists; sessions with a
    /// dead cwd refuse new prompts.
    pub cwd_alive: bool,
}

/// The sessions root: `<home>/sessions`.
pub struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    pub fn open(home: &Path) -> Result<Self, SessionError> {
        let root = home.join("sessions");
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn create(&self, cwd: &Path, sandbox: bool) -> Result<Session, SessionError> {
        let id = uuid::Uuid::new_v4().to_string();
        let dir = self.root.join(&id);
        Session::create(&dir, id, cwd, sandbox)
    }

    pub fn load(&self, id: &str) -> Result<Session, SessionError> {
        let file = self.file_for(id)?;
        if !file.exists() {
            return Err(SessionError::NotFound(id.to_string()));
        }
        Session::load(&file)
    }

    /// Newest-first summaries; header + first prompt only, no repair.
    pub fn list(&self) -> Result<Vec<SessionSummary>, SessionError> {
        let mut summaries = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            let file = entry.path().join("session.jsonl");
            if !file.is_file() {
                continue;
            }
            if let Some(summary) = read_summary(&file) {
                summaries.push(summary);
            }
        }
        summaries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(summaries)
    }

    pub fn delete(&self, id: &str) -> Result<(), SessionError> {
        let dir = self.root.join(validate_id(id)?);
        if !dir.exists() {
            return Err(SessionError::NotFound(id.to_string()));
        }
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    fn file_for(&self, id: &str) -> Result<PathBuf, SessionError> {
        Ok(self.root.join(validate_id(id)?).join("session.jsonl"))
    }
}

fn validate_id(id: &str) -> Result<&str, SessionError> {
    let valid = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == '-');
    if valid {
        Ok(id)
    } else {
        Err(SessionError::InvalidId(id.to_string()))
    }
}

fn read_summary(file: &Path) -> Option<SessionSummary> {
    let mut raw = String::new();
    File::open(file).ok()?.read_to_string(&mut raw).ok()?;
    let lines: Vec<&str> = raw.lines().collect();
    let header: SessionHeader = serde_json::from_str(lines.first()?).ok()?;
    let id = file
        .parent()?
        .file_name()?
        .to_string_lossy()
        .to_string();
    let excerpt = lines.iter().find_map(|line| {
        let envelope = serde_json::from_str::<SessionEnvelope>(line).ok()?;
        match envelope.event {
            SessionEvent::UserMessage { text, injected: false, .. } => Some(text),
            _ => None,
        }
    });
    let excerpt = excerpt.as_ref().map(|text| excerpt_text(text, 80));
    Some(SessionSummary {
        id,
        created_at: header.created_at,
        excerpt,
        cwd: Some(header.cwd.clone()),
        sandbox: Some(header.sandbox),
        cwd_alive: std::path::Path::new(&header.cwd).is_dir(),
    })
}

fn excerpt_text(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    let mut chars = trimmed.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "denia-session-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn create_append_load_round_trip() {
        let root = temp_root();
        let store = SessionStore::open(&root).unwrap();
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();

        let session = store.create(&cwd, true).unwrap();
        let id = session.id().to_string();
        session
            .append(SessionEvent::TurnStart { turn: 1 })
            .unwrap();
        session
            .append(SessionEvent::UserMessage { text: "hello world, this is a prompt".into(), injected: false })
            .unwrap();
        session
            .append(SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            })
            .unwrap();
        drop(session);

        let loaded = store.load(&id).unwrap();
        assert_eq!(loaded.events().len(), 3);
        assert_eq!(loaded.events()[0].seq, 1);
        assert_eq!(loaded.events()[2].seq, 3);
        assert_eq!(loaded.header().cwd, cwd.to_string_lossy().to_string());

        let summaries = store.list().unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, id);
        assert!(summaries[0].excerpt.as_deref().unwrap().starts_with("hello world"));

        store.delete(&id).unwrap();
        assert!(matches!(
            store.load(&id),
            Err(SessionError::NotFound(_))
        ));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn torn_tail_is_truncated_and_open_turn_closed() {
        let root = temp_root();
        let store = SessionStore::open(&root).unwrap();
        let session = store.create(&root, true).unwrap();
        let id = session.id().to_string();
        session
            .append(SessionEvent::TurnStart { turn: 1 })
            .unwrap();
        session
            .append(SessionEvent::UserMessage { text: "hi".into(), injected: false })
            .unwrap();
        let file = session.file().to_path_buf();
        drop(session);

        // Simulate a crash mid-append: garbage after the committed prefix.
        {
            use std::io::Write as _;
            let mut f = OpenOptions::new().append(true).open(&file).unwrap();
            f.write_all(b"{\"seq\":3,\"time\":1,\"type\":\"user-mess").unwrap();
            f.flush().unwrap();
        }

        let loaded = store.load(&id).unwrap();
        // Two good events + the synthetic turn-end close.
        assert_eq!(loaded.events().len(), 3);
        match &loaded.events()[2].event {
            SessionEvent::TurnEnd { turn, reason } => {
                assert_eq!(*turn, 1);
                assert_eq!(*reason, TurnEndReason::Aborted);
            }
            other => panic!("expected turn-end, got {other:?}"),
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn ids_are_validated_before_path_use() {
        let root = temp_root();
        let store = SessionStore::open(&root).unwrap();
        assert!(matches!(
            store.load("../escape"),
            Err(SessionError::InvalidId(_))
        ));
        assert!(matches!(store.delete(""), Err(SessionError::InvalidId(_))));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn derive_messages_delegates() {
        let root = temp_root();
        let store = SessionStore::open(&root).unwrap();
        let session = store.create(&root, true).unwrap();
        session
            .append(SessionEvent::UserMessage { text: "ping".into(), injected: false })
            .unwrap();
        let messages = session.derive_messages();
        assert_eq!(messages, vec![ChatMessage::user("ping")]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
