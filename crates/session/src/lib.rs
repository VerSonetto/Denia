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
use denia_token_meter::{ContextBreakdown, ContextMeter, ContextPressure, TurnTokenUsage};
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
    #[error("seq {0} is not a rewindable user message")]
    NotARewindPoint(u64),
}

/// 物理回退的结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct RewindOutcome {
    pub to_seq: u64,
    pub to_message: Option<String>,
    pub removed_events: usize,
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
    /// 每个事件行结束的字节偏移(含换行);物理回退截断文件用。
    /// 与 `events` 一一对应。
    offsets: Vec<u64>,
    /// session.jsonl 首行(header)结束后的字节偏移。
    base_offset: u64,
    /// 最新 turn 号(append 时维护),next_turn_number O(1)。
    last_turn: u32,
    /// 最近一次落日志的系统提示词;should_log_system_prompt 的 O(1) 依据。
    last_system_prompt: Option<String>,
    /// token-meter 增量 fold:上下文 token 组成的 O(1) 投影。
    meter: ContextMeter,
    /// 当前正在进行的 turn 的 envelope 缓冲;`turn-end` 到来时交给
    /// `meter.fold_turn`,fold 成功则并入精确 usage 累计并更新 anchor。
    pending_turn: Vec<SessionEnvelope>,
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
    /// `parent_session` 记录分支血缘(非分支会话为 `None`)。
    pub fn create(
        dir: &Path,
        id: String,
        cwd: &Path,
        sandbox: bool,
        parent_session: Option<String>,
    ) -> Result<Session, SessionError> {
        std::fs::create_dir_all(dir)?;
        let file = dir.join("session.jsonl");
        let header = SessionHeader {
            kind: SessionHeaderKind::Session,
            version: SESSION_FORMAT_VERSION,
            id,
            created_at: now_millis(),
            cwd: cwd.to_string_lossy().to_string(),
            sandbox,
            parent_session,
        };
        {
            let mut handle = File::create(&file)?;
            let header_line = serde_json::to_string(&header)?;
            writeln!(handle, "{}", header_line)?;
            handle.flush()?;
            let base_offset = header_line.len() as u64 + 1;
            let writer = open_append_writer(&file)?;
            return Ok(Session {
                header,
                file,
                inner: Mutex::new(SessionInner {
                    events: Vec::new(),
                    writer,
                    offsets: Vec::new(),
                    base_offset,
                    last_turn: 0,
                    last_system_prompt: None,
                    meter: ContextMeter::new(),
                    pending_turn: Vec::new(),
                }),
            });
        }
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
        let consumed_base = header_line.len() + 1;
        let mut consumed = consumed_base;
        let mut offsets: Vec<u64> = Vec::new();
        let mut events: Vec<SessionEnvelope> = Vec::new();
        let mut torn_at: Option<usize> = None;
        let mut last_turn = 0u32;
        let mut last_system_prompt: Option<String> = None;
        let mut meter = ContextMeter::new();
        for line in lines {
            let with_newline = line.len() + 1;
            if line.trim().is_empty() {
                consumed += with_newline;
                continue;
            }
            match serde_json::from_str::<SessionEnvelope>(line) {
                Ok(envelope) => {
                    offsets.push(consumed as u64 + with_newline as u64);
                    match &envelope.event {
                        SessionEvent::TurnStart { turn } => last_turn = last_turn.max(*turn),
                        SessionEvent::SystemPrompt { text, .. } => {
                            last_system_prompt = Some(text.clone())
                        }
                        _ => {}
                    }
                    meter.apply_one(&envelope);
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
                offsets,
                base_offset: consumed_base as u64,
                last_turn,
                last_system_prompt,
                meter,
                pending_turn: Vec::new(),
            }),
        };
        session.close_orphaned_turn()?;
        session.refresh_meter_from_log()?;
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
        self.append_with_time(event, now_millis())
    }

    /// [`Session::append`] 的时间显式版:分支种子回放保留源事件时间戳,
    /// 轨迹时间轴在子会话里保持真实。
    fn append_with_time(
        &self,
        event: SessionEvent,
        time: u64,
    ) -> Result<SessionEnvelope, SessionError> {
        let mut inner = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
        let envelope = SessionEnvelope {
            seq: inner.events.len() as u64 + 1,
            time,
            event,
        };
        // 维护 last_turn / last_system_prompt 的 O(1) 投影。
        match &envelope.event {
            SessionEvent::TurnStart { turn } => {
                inner.last_turn = inner.last_turn.max(*turn);
            }
            SessionEvent::SystemPrompt { text, .. } => {
                inner.last_system_prompt = Some(text.clone());
            }
            _ => {}
        }
        // 维护 token-meter:
        // 1) 每个事件都贡献 message/system 启发式 fold(apply_one 内部按角色累计)。
        // 2) `TurnStart` 重置本轮 envelope 缓冲;`TurnEnd` 闭合时把整段
        //    喂给 `meter.fold_turn`,成功则并入精确 usage 与 anchor。
        if matches!(&envelope.event, SessionEvent::TurnStart { .. }) {
            inner.pending_turn.clear();
        }
        inner.pending_turn.push(envelope.clone());
        if matches!(&envelope.event, SessionEvent::TurnEnd { .. }) {
            let slice: Vec<SessionEnvelope> = inner.pending_turn.drain(..).collect();
            inner.meter.fold_turn(&slice);
        }
        inner.meter.apply_one(&envelope);

        let line = serde_json::to_string(&envelope)?;
        writeln!(inner.writer, "{line}")?;
        let next_offset = inner.offsets.last().copied().unwrap_or(inner.base_offset) + line.len() as u64 + 1;
        inner.offsets.push(next_offset);
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

    /// 分支种子回放:把源日志前缀原样写入新会话(重新从 1 连续编号,
    /// 保留源事件时间戳),last_turn/last_system_prompt/token-meter 随
    /// `append_with_time` 一致重建。回放后新会话 `derive_messages` 与
    /// 源会话前缀逐字一致——model-visible == logged 的不变量不破坏。
    pub fn seed_from(&self, source: &[SessionEnvelope]) -> Result<(), SessionError> {
        for envelope in source {
            self.append_with_time(envelope.event.clone(), envelope.time)?;
        }
        Ok(())
    }

    /// 物理回退:截断会话日志到目标用户消息**之前**,并把回退审计追加到
    /// `rewinds.jsonl`。内存事件/token-meter/计数器同步重建。
    ///
    /// 这是破坏性操作:目标 seq 之后的事件从磁盘移除,不可恢复。
    pub fn rewind(&self, to_seq: u64) -> Result<RewindOutcome, SessionError> {
        let mut inner = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
        if to_seq == 0 || to_seq as usize > inner.events.len() {
            return Err(SessionError::NotARewindPoint(to_seq));
        }
        let target_idx = to_seq as usize - 1;
        let target = &inner.events[target_idx];
        let to_message = match &target.event {
            SessionEvent::UserMessage {
                text,
                injected: false,
                ..
            } => Some(text.clone()),
            _ => return Err(SessionError::NotARewindPoint(to_seq)),
        };

        let removed_events = inner.events.len() - target_idx;
        // 先 flush,确保所有已写事件落盘;再按最后一个保留事件的字节偏移截断。
        inner.writer.flush()?;
        let truncate_offset = if target_idx == 0 {
            inner.base_offset
        } else {
            *inner
                .offsets
                .get(target_idx - 1)
                .ok_or(SessionError::NotARewindPoint(to_seq))?
        };
        {
            let fh = OpenOptions::new().write(true).open(&self.file)?;
            fh.set_len(truncate_offset)?;
        }

        // 内存与派生状态回退。
        inner.events.truncate(target_idx);
        inner.offsets.truncate(target_idx);
        inner.last_turn = 0;
        inner.last_system_prompt = None;
        inner.meter = ContextMeter::new();
        inner.pending_turn.clear();
        let kept = inner.events.clone();
        for envelope in &kept {
            match &envelope.event {
                SessionEvent::TurnStart { turn } => inner.last_turn = inner.last_turn.max(*turn),
                SessionEvent::SystemPrompt { text, .. } => {
                    inner.last_system_prompt = Some(text.clone())
                }
                _ => {}
            }
            if matches!(&envelope.event, SessionEvent::TurnStart { .. }) {
                inner.pending_turn.clear();
            }
            inner.pending_turn.push(envelope.clone());
            if matches!(&envelope.event, SessionEvent::TurnEnd { .. }) {
                let slice: Vec<SessionEnvelope> = inner.pending_turn.drain(..).collect();
                inner.meter.fold_turn(&slice);
            }
            inner.meter.apply_one(envelope);
        }

        // 回退审计:独立于 session.jsonl 追加,物理截断不会抹掉这段记录。
        let rewind_file = self.file.with_file_name("rewinds.jsonl");
        let record = serde_json::json!({
            "time": now_millis(),
            "to_seq": to_seq,
            "to_message": to_message,
            "removed_events": removed_events,
        });
        {
            let mut fh = OpenOptions::new().create(true).append(true).open(rewind_file)?;
            writeln!(fh, "{record}")?;
        }

        Ok(RewindOutcome {
            to_seq,
            to_message,
            removed_events,
        })
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

    /// 从内存事件重建 token-meter(load 后调用一次;之后由 `append`
    /// 增量维护)。
    fn refresh_meter_from_log(&self) -> Result<(), SessionError> {
        let mut inner = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
        let events = inner.events.clone();
        let mut pending: Vec<SessionEnvelope> = Vec::new();
        for envelope in &events {
            if matches!(&envelope.event, SessionEvent::TurnStart { .. }) {
                pending.clear();
            }
            pending.push(envelope.clone());
            if matches!(&envelope.event, SessionEvent::TurnEnd { .. }) {
                let slice: Vec<SessionEnvelope> = pending.drain(..).collect();
                inner.meter.fold_turn(&slice);
            }
            inner.meter.apply_one(envelope);
        }
        Ok(())
    }

    /// 更新工具声明 token(供 token-meter;server 在请求启动后通知 fold)。
    pub fn set_tools_tokens(&self, tokens: u64) {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .meter
            .set_tools_tokens(tokens);
    }

    /// 更新系统提示词 token(driver 喂 framed 版;同 provider 所见)。
    pub fn set_system_tokens(&self, tokens: u64) {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .meter
            .set_system_tokens(tokens);
    }

    /// 当前上下文 token 拆分快照(纯启发式)。
    pub fn context_breakdown(&self) -> ContextBreakdown {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .meter
            .breakdown()
    }

    /// 当前 provider 精确 usage 累计。
    pub fn turn_token_usage(&self) -> TurnTokenUsage {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .meter
            .turn_usage()
    }

    /// 当前上下文压力(锚点 + 启发式;圆环面板用此值除以窗口得到百分比)。
    pub fn context_pressure(&self) -> ContextPressure {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .meter
            .context_pressure()
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
    /// 分支血缘:父会话 id(非分支会话为 `None`)。侧栏据此把子会话
    /// 嵌套在源会话之下(抄 dsh parentSessionId)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
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
    pub parent_session: Option<String>,
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
        self.create_with_parent(cwd, sandbox, None)
    }

    /// 分支创建:新会话继承父会话的 cwd/sandbox,血缘写进 header,
    /// 并把源日志前缀(`source[..cut]`)作为种子回放。
    pub fn create_forked(
        &self,
        source_events: &[SessionEnvelope],
        cut: usize,
        cwd: &Path,
        sandbox: bool,
        parent_id: &str,
    ) -> Result<Session, SessionError> {
        let session =
            self.create_with_parent(cwd, sandbox, Some(parent_id.to_string()))?;
        session.seed_from(&source_events[..cut])?;
        Ok(session)
    }

    fn create_with_parent(
        &self,
        cwd: &Path,
        sandbox: bool,
        parent_session: Option<String>,
    ) -> Result<Session, SessionError> {
        let id = uuid::Uuid::new_v4().to_string();
        let dir = self.root.join(&id);
        let session = Session::create(&dir, id, cwd, sandbox, parent_session)?;
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
            // 活跃会话已在步骤 1 产出摘要,索引并入时跳过,避免同一会话出现两行。
            let listed: std::collections::HashSet<String> =
                summaries.iter().map(|s| s.id.clone()).collect();
            summaries.extend(
                index
                    .values()
                    .filter(|entry| !listed.contains(&entry.meta.id))
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
        parent_session: session.header().parent_session.clone(),
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
        parent_session: meta.parent_session.clone(),
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
            parent_session: header.parent_session,
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

    #[test]
    fn rewind_truncates_log_and_writes_audit() {
        let root = temp_root();
        let dir = root.join("s");
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let session = Session::create(&dir, "s".to_string(), &cwd, true, None).unwrap();
        session
            .append(SessionEvent::UserMessage { text: "first".into(), injected: false, images: Vec::new() })
            .unwrap();
        session.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
        session.append(SessionEvent::StepStart { turn: 1, step: 1 }).unwrap();
        session
            .append(SessionEvent::UserMessage { text: "second".into(), injected: false, images: Vec::new() })
            .unwrap();
        session.append(SessionEvent::TurnStart { turn: 2 }).unwrap();

        let outcome = session.rewind(4).unwrap();
        assert_eq!(outcome.removed_events, 2);
        assert_eq!(outcome.to_message.as_deref(), Some("second"));
        assert_eq!(session.events().len(), 3);
        assert_eq!(session.events()[0].seq, 1);
        assert_eq!(session.events()[2].seq, 3);
        assert_eq!(session.next_turn_number(), 2);

        let audit = std::fs::read_to_string(dir.join("rewinds.jsonl")).unwrap();
        assert!(audit.contains("\"to_seq\":4"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn forked_session_replays_seed_with_lineage() {
        use denia_core::session::fork_cut_index;

        let root = temp_root();
        let store = SessionStore::open(&root).unwrap();
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();

        let source = store.create(&cwd, true).unwrap();
        let source_id = source.id().to_string();
        source
            .append(SessionEvent::TurnStart { turn: 1 })
            .unwrap();
        source
            .append(SessionEvent::UserMessage {
                text: "first".into(),
                injected: false,
                images: Vec::new(),
            })
            .unwrap();
        source
            .append(SessionEvent::AssistantMessage {
                turn: 1,
                step: 1,
                blocks: vec![denia_core::stream::ContentBlock::Text { text: "hi".into() }],
                usage: None,
                interrupted: false,
            })
            .unwrap();
        source
            .append(SessionEvent::TurnEnd { turn: 1, reason: TurnEndReason::Completed })
            .unwrap();
        // 未完成轮次:不应进入种子。
        source
            .append(SessionEvent::TurnStart { turn: 2 })
            .unwrap();
        source
            .append(SessionEvent::UserMessage {
                text: "running".into(),
                injected: false,
                images: Vec::new(),
            })
            .unwrap();
        let source_events = source.events();

        let cut = fork_cut_index(&source_events, None).unwrap();
        assert_eq!(cut, 4, "无锚点切到最后一个 turn-end(含)");
        let child = store
            .create_forked(&source_events, cut, &cwd, true, &source_id)
            .unwrap();

        // 种子回放:seq 重排连续、时间戳保留、派生历史与前缀逐字一致。
        let child_events = child.events();
        assert_eq!(child_events.len(), 4);
        for (index, envelope) in child_events.iter().enumerate() {
            assert_eq!(envelope.seq, index as u64 + 1);
            assert_eq!(envelope.time, source_events[index].time, "种子保留源时间戳");
        }
        assert_eq!(
            child.derive_messages(),
            derive_messages(&source_events[..cut]),
        );
        // 未完成轮次的消息不属于子会话。
        assert!(child.first_prompt_excerpt(80).unwrap().contains("first"));
        let child_id = child.id().to_string();
        drop(child);

        // 血缘:header 与摘要都带 parent_session;重载后仍在。
        let reloaded = store.load(&child_id).unwrap();
        assert_eq!(reloaded.header().parent_session.as_deref(), Some(source_id.as_str()));
        let summary = store
            .list()
            .unwrap()
            .into_iter()
            .find(|s| s.id == child_id)
            .unwrap();
        assert_eq!(summary.parent_session.as_deref(), Some(source_id.as_str()));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
