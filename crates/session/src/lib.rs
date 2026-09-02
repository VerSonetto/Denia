//! Event-sourced session storage: one JSONL file per session.
//!
//! Layout: `<home>/sessions/<id>/session.jsonl`; line 1 is the header, every
//! following line one envelope.
//!
//! ## 性能设计(长会话 / 大量会话)
//!
//! - **内存索引**:[`SessionStore`] 启动时只对每个文件流式读摘要(header +
//!   首条用户消息,找到即停),之后 `list()` 不再触碰文件内容,只做
//!   stat 校验(mtime/size 变了才重读该会话的摘要)。O(会话数) stat,
//!   零全文读取。
//! - **Buffered append**:日志写入走 [`BufWriter`],不逐事件 flush;崩溃
//!   丢失的尾部由 torn-tail 修复兜底(与现有恢复协议一致),SSE 广播走
//!   内存,不依赖文件落盘。
//! - **惰性加载**:会话事件只有显式 `load` 才进内存;空闲会话由
//!   server 侧的 LiveSessions 淘汰,这里不驻留任何全量事件。
//!
//! ## 容错
//!
//! 加载时修复 torn tail(截断到最后一个合法行边界)并用合成
//! `turn-end { aborted }` 关闭崩溃遗留的孤儿轮次——事件不会静默丢失。

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use denia_core::message::ChatMessage;
use denia_core::session::{
    SessionEnvelope, SessionEvent, SessionHeader, SessionHeaderKind, TurnEndReason,
    SESSION_FORMAT_VERSION, derive_messages,
};
use thiserror::Error;

/// 摘要重读上限:防止异常的超长行拖垮列表。
const SUMMARY_SCAN_LIMIT: usize = 64 * 1024;

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

/// 文件尺寸/修改时间戳:索引失效校验的签名。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    size: u64,
    mtime_ms: u64,
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Some(FileStamp {
        size: meta.len(),
        mtime_ms: mtime,
    })
}

/// Mutable log state behind the per-session lock.
struct SessionInner {
    events: Vec<SessionEnvelope>,
    writer: BufWriter<File>,
    /// 最新 turn 号(append 时维护),next_turn_number O(1)。
    last_turn: u32,
    /// 最近一次落日志的系统提示词;should_log_system_prompt 的 O(1) 依据。
    last_system_prompt: Option<String>,
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
        {
            let mut handle = File::create(&file)?;
            writeln!(handle, "{}", serde_json::to_string(&header)?)?;
            handle.flush()?;
        }
        let writer = open_append_writer(&file)?;
        Ok(Session {
            header,
            file,
            inner: Mutex::new(SessionInner {
                events: Vec::new(),
                writer,
                last_turn: 0,
                last_system_prompt: None,
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
        let mut last_turn = 0u32;
        let mut last_system_prompt: Option<String> = None;
        for line in lines {
            let with_newline = line.len() + 1;
            if line.trim().is_empty() {
                consumed += with_newline;
                continue;
            }
            match serde_json::from_str::<SessionEnvelope>(line) {
                Ok(envelope) => {
                    match &envelope.event {
                        SessionEvent::TurnStart { turn } => last_turn = last_turn.max(*turn),
                        SessionEvent::SystemPrompt { text, .. } => {
                            last_system_prompt = Some(text.clone())
                        }
                        _ => {}
                    }
                    events.push(envelope)
                }
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
                writer: open_append_writer(file)?,
                last_turn,
                last_system_prompt,
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
    /// 仅在断线重快照/`get_session` 使用;高频路径请用 [`Session::events_after`]。
    pub fn events(&self) -> Vec<SessionEnvelope> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .events
            .clone()
    }

    /// Envelopes with `seq > after`, in order. SSE replay 专用:不克隆全量,
    /// 只收集增量区间。
    pub fn events_after(&self, after: u64) -> Vec<SessionEnvelope> {
        let inner = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
        let events = &inner.events;
        if after >= events.len() as u64 {
            return Vec::new();
        }
        events[after as usize..].to_vec()
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    /// Appends one event, assigning contiguous seq and wall-clock time. The
    /// log lock is held only for this write, so concurrent readers observe
    /// live progress instead of waiting for the whole turn.
    ///
    /// 写入走 BufWriter:高频流式帧不逐条 flush,崩溃尾部由 torn-tail 修复。
    pub fn append(&self, event: SessionEvent) -> Result<SessionEnvelope, SessionError> {
        let mut inner = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
        let envelope = SessionEnvelope {
            seq: inner.events.len() as u64 + 1,
            time: now_millis(),
            event,
        };
        match &envelope.event {
            SessionEvent::TurnStart { turn } => inner.last_turn = inner.last_turn.max(*turn),
            SessionEvent::SystemPrompt { text, .. } => {
                inner.last_system_prompt = Some(text.clone())
            }
            _ => {}
        }
        let line = serde_json::to_string(&envelope)?;
        writeln!(inner.writer, "{line}")?;
        // 落盘策略:步骤边界/工具结果/todo 快照立即 flush(耐久性边界),
        // 流式 chunk 只进 buffer,超 16KB 自动落盘(高频帧零系统调用)。
        match &envelope.event {
            SessionEvent::TurnEnd { .. }
            | SessionEvent::StepEnd { .. }
            | SessionEvent::ToolResult { .. }
            | SessionEvent::TodoWrite { .. } => {
                inner.writer.flush()?;
            }
            _ => {
                if inner.writer.buffer().len() >= 16 * 1024 {
                    inner.writer.flush()?;
                }
            }
        }
        inner.events.push(envelope.clone());
        Ok(envelope)
    }

    /// O(1):下一个 turn 号(维护的计数器,不再遍历日志)。
    pub fn next_turn_number(&self) -> u32 {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .last_turn
            + 1
    }

    /// O(1):最近一次落库的系统提示词正文。
    pub fn last_system_prompt(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .last_system_prompt
            .clone()
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
        Some(excerpt_text(text, max_chars))
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

/// Drop 时把缓冲的日志推给 OS(不阻塞;优雅退出路径)。
impl Drop for Session {
    fn drop(&mut self) {
        if let Ok(inner) = self.inner.get_mut() {
            let _ = inner.writer.flush();
        }
    }
}

fn open_append_writer(file: &Path) -> Result<BufWriter<File>, SessionError> {
    Ok(BufWriter::new(
        OpenOptions::new().append(true).create(true).open(file)?,
    ))
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

/// 索引条目:启动/失效时从文件摘要得到,list 只读它。
#[derive(Debug, Clone)]
struct IndexEntry {
    meta: SessionMeta,
    stamp: FileStamp,
}

#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub id: String,
    pub created_at: u64,
    pub excerpt: Option<String>,
    pub cwd: String,
    pub sandbox: bool,
}

/// The sessions root: `<home>/sessions` + 内存索引。
pub struct SessionStore {
    root: PathBuf,
    index: Mutex<std::collections::HashMap<String, IndexEntry>>,
    /// 活跃(已加载)会话的弱引用:list 优先从内存读摘要,
    /// 不依赖文件落盘状态,首条用户消息即刻可见。
    active: Mutex<std::collections::HashMap<String, std::sync::Weak<Session>>>,
}

impl SessionStore {
    pub fn open(home: &Path) -> Result<Self, SessionError> {
        let root = home.join("sessions");
        std::fs::create_dir_all(&root)?;
        let store = Self {
            root,
            index: Mutex::new(std::collections::HashMap::new()),
            active: Mutex::new(std::collections::HashMap::new()),
        };
        store.rescan_index()?;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 扫描目录重建摘要索引:每个文件只读 header + 首条用户消息(找到即停),
    /// 不读全文;文件损坏的会话静默跳过(与旧行为一致)。
    pub fn rescan_index(&self) -> Result<(), SessionError> {
        let mut fresh = std::collections::HashMap::new();
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            let file = entry.path().join("session.jsonl");
            if !file.is_file() {
                continue;
            }
            if let Some((meta, stamp)) = read_summary(&file) {
                fresh.insert(meta.id.clone(), IndexEntry { meta, stamp });
            }
        }
        *self.index.lock().unwrap_or_else(|p| p.into_inner()) = fresh;
        Ok(())
    }

    pub fn create(&self, cwd: &Path, sandbox: bool) -> Result<Session, SessionError> {
        let id = uuid::Uuid::new_v4().to_string();
        let dir = self.root.join(&id);
        let session = Session::create(&dir, id, cwd, sandbox)?;
        let meta = meta_of(&session);
        self.index
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(meta.id.clone(), IndexEntry {
                meta,
                stamp: file_stamp(session.file()).unwrap_or(FileStamp { size: 0, mtime_ms: 0 }),
            });
        Ok(session)
    }

    pub fn load(&self, id: &str) -> Result<Session, SessionError> {
        let file = self.file_for(id)?;
        if !file.exists() {
            return Err(SessionError::NotFound(id.to_string()));
        }
        let session = Session::load(&file)?;
        let meta = meta_of(&session);
        self.index
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(meta.id.clone(), IndexEntry {
                meta,
                stamp: file_stamp(session.file()).unwrap_or(FileStamp { size: 0, mtime_ms: 0 }),
            });
        Ok(session)
    }

    /// 登记一个被 `Arc` 持有的会话:list 优先从内存读摘要,
    /// 不依赖文件落盘状态,首条用户消息即刻可见。弱引用不阻止淘汰。
    pub fn track_session(&self, session: &std::sync::Arc<Session>) {
        let meta = meta_of(session);
        self.index
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(meta.id.clone(), IndexEntry {
                meta,
                stamp: FileStamp { size: 0, mtime_ms: 0 },
            });
        self.active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(session.id().to_string(), std::sync::Arc::downgrade(session));
    }

    /// List summaries from the memory index and live registry.
    ///
    /// - 活跃(已加载)会话:纯内存读摘要,不碰文件、不等落盘。
    /// - 其余条目:对每个文件做 stat 校验,文件变了才重读摘要;
    ///   目录与索引的差集(新建文件)会被发现并补进索引——
    ///   不做任何全文读取。
    pub fn list(&self) -> Result<Vec<SessionSummary>, SessionError> {
        let mut summaries: Vec<SessionSummary> = Vec::new();

        // 1) 活跃会话内存优先(顺带清理失效弱引用)。
        {
            let mut active = self.active.lock().unwrap_or_else(|p| p.into_inner());
            let stale: Vec<String> = active
                .iter()
                .filter_map(|(id, weak)| {
                    match weak.upgrade() {
                        Some(session) => {
                            summaries.push(summary_of(&session));
                            None
                        }
                        None => Some(id.clone()),
                    }
                })
                .collect();
            for id in stale {
                active.remove(&id);
            }
        }

        // 2) 目录差集:新出现的会话文件补进索引(外部写入/脚本生成)。
        {
            let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            let indexed: std::collections::HashSet<String> = index.keys().cloned().collect();
            let mut discovered = Vec::new();
            for entry in std::fs::read_dir(&self.root)? {
                let entry = entry?;
                let file = entry.path().join("session.jsonl");
                if !file.is_file() {
                    continue;
                }
                let id = entry.file_name().to_string_lossy().to_string();
                if indexed.contains(&id) {
                    continue;
                }
                if summaries.iter().any(|s| s.id == id) {
                    continue;
                }
                if let Some((meta, stamp)) = read_summary(&file) {
                    discovered.push(IndexEntry { meta, stamp });
                }
            }
            for entry in discovered {
                index.insert(entry.meta.id.clone(), entry);
            }

            // 3) 索引条目 stat 失效检测。
            let ids: Vec<String> = index
                .keys()
                .filter(|id| !summaries.iter().any(|s| &s.id == *id))
                .cloned()
                .collect();
            for id in ids {
                let file = self.root.join(&id).join("session.jsonl");
                let current = file_stamp(&file);
                let entry = index.get(&id);
                let dirty = match (entry, current) {
                    (Some(entry), Some(current)) => entry.stamp != current,
                    (_, None) => true,
                    (None, _) => false,
                };
                if dirty {
                    if let Some((meta, stamp)) = read_summary(&file) {
                        index.insert(id, IndexEntry { meta, stamp });
                    } else {
                        index.remove(&id);
                    }
                }
            }
            summaries.extend(
                index
                    .values()
                    .map(|entry| summary_of_meta(&entry.meta)),
            );
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
        self.index
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id);
        self.active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id);
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

fn meta_of(session: &Session) -> SessionMeta {
    SessionMeta {
        id: session.id().to_string(),
        created_at: session.header().created_at,
        excerpt: session.first_prompt_excerpt(80),
        cwd: session.header().cwd.clone(),
        sandbox: session.header().sandbox,
    }
}

fn summary_of(session: &Session) -> SessionSummary {
    summary_of_meta(&meta_of(session))
}

fn summary_of_meta(meta: &SessionMeta) -> SessionSummary {
    SessionSummary {
        id: meta.id.clone(),
        created_at: meta.created_at,
        excerpt: meta.excerpt.clone(),
        cwd: Some(meta.cwd.clone()),
        sandbox: Some(meta.sandbox),
        cwd_alive: Path::new(&meta.cwd).is_dir(),
    }
}

/// 摘要读取:header + 首条用户消息(流式逐行,提前停止),跳过损坏行。
fn read_summary(file: &Path) -> Option<(SessionMeta, FileStamp)> {
    let stamp = file_stamp(file)?;
    let mut reader = BufReader::new(File::open(file).ok()?);
    let mut bytes_read = 0usize;
    let mut first_line: Option<String> = None;
    let mut excerpt: Option<String> = None;
    loop {
        if bytes_read >= SUMMARY_SCAN_LIMIT {
            break;
        }
        let mut line = String::new();
        let read = reader.read_line(&mut line).ok()?;
        if read == 0 {
            break;
        }
        bytes_read += read;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if first_line.is_none() {
            first_line = Some(trimmed.to_string());
            continue;
        }
        let envelope: SessionEnvelope = match serde_json::from_str(trimmed) {
            Ok(envelope) => envelope,
            Err(_) => continue,
        };
        if let SessionEvent::UserMessage { text, injected: false, .. } = envelope.event {
            excerpt = Some(excerpt_text(&text, 80));
            break;
        }
    }
    let header: SessionHeader = serde_json::from_str(first_line.as_deref()?).ok()?;
    let id = file
        .parent()?
        .file_name()?
        .to_string_lossy()
        .to_string();
    Some((
        SessionMeta {
            id,
            created_at: header.created_at,
            excerpt,
            cwd: header.cwd,
            sandbox: header.sandbox,
        },
        stamp,
    ))
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
            .append(SessionEvent::UserMessage { text: "hello world, this is a prompt".into(), injected: false, images: Vec::new() })
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

        // 摘要索引:list 不需要触碰文件内容。
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
    fn index_refreshes_on_file_change() {
        let root = temp_root();
        let store = SessionStore::open(&root).unwrap();
        // 真实路径:server 以 Arc 持有并登记,list 从内存读摘要。
        let session = std::sync::Arc::new(store.create(&root, true).unwrap());
        store.track_session(&session);
        assert!(store.list().unwrap()[0].excerpt.is_none());

        session
            .append(SessionEvent::UserMessage { text: "fresh excerpt".into(), injected: false, images: Vec::new() })
            .unwrap();
        let summary = store.list().unwrap();
        assert!(
            summary[0].excerpt.as_deref().unwrap().contains("fresh excerpt"),
            "excerpt must refresh: {:?}",
            summary[0].excerpt
        );

        // 未登记会话(值语义):落盘边界后走文件 stat 失效检测路径。
        let session2 = store.create(&root, false).unwrap();
        session2
            .append(SessionEvent::UserMessage { text: "file-based excerpt".into(), injected: false, images: Vec::new() })
            .unwrap();
        // 强制落盘:TurnEnd 边界会 flush。
        session2
            .append(SessionEvent::TurnEnd { turn: 1, reason: TurnEndReason::Completed })
            .unwrap();
        drop(session2);
        let summaries = store.list().unwrap();
        assert!(
            summaries.iter().any(|s| s.excerpt.as_deref().unwrap_or("").contains("file-based excerpt")),
            "file-based excerpt must refresh via stat invalidation"
        );
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
            .append(SessionEvent::UserMessage { text: "hi".into(), injected: false, images: Vec::new() })
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
            .append(SessionEvent::UserMessage { text: "ping".into(), injected: false, images: Vec::new() })
            .unwrap();
        let messages = session.derive_messages();
        assert_eq!(messages, vec![ChatMessage::user("ping")]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn turn_counter_and_system_prompt_tracking() {
        let root = temp_root();
        let store = SessionStore::open(&root).unwrap();
        let session = store.create(&root, true).unwrap();
        assert_eq!(session.next_turn_number(), 1);
        session
            .append(SessionEvent::TurnStart { turn: 1 })
            .unwrap();
        assert_eq!(session.next_turn_number(), 2);
        session
            .append(SessionEvent::SystemPrompt { turn: 1, step: 1, text: "sys-a".into() })
            .unwrap();
        assert_eq!(session.last_system_prompt().as_deref(), Some("sys-a"));
        session
            .append(SessionEvent::SystemPrompt { turn: 1, step: 2, text: "sys-b".into() })
            .unwrap();
        assert_eq!(session.last_system_prompt().as_deref(), Some("sys-b"));

        session
            .append(SessionEvent::UserMessage { text: "uno".into(), injected: false, images: Vec::new() })
            .unwrap();
        session
            .append(SessionEvent::UserMessage { text: "dos".into(), injected: false, images: Vec::new() })
            .unwrap();
        let after = session.events_after(1);
        assert_eq!(after.len(), 4, "seq > 1 应为 4 条,实际 {}", after.len());
        assert_eq!(after[0].seq, 2);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
