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
    PermissionMode, SESSION_FORMAT_VERSION, SessionEnvelope, SessionEvent, SessionHeader,
    SessionHeaderKind, TurnEndReason, derive_messages,
};
use denia_core::stream::ContentBlock;
use denia_token_meter::{ContextBreakdown, ContextMeter, ContextPressure, TurnTokenUsage};
use thiserror::Error;

/// 摘要重读上限:防止异常的超长行拖垮列表。
const SUMMARY_SCAN_LIMIT: usize = 64 * 1024;

/// 崩溃孤儿轮闭合时,给"已落库但未答复"的工具调用补的合成结果
/// (对齐 dsh `TOOL_OUTCOME_UNKNOWN` 文案:结果未知,谨慎重试,不盲目重放)。
pub const ORPHAN_TOOL_RESULT: &str = "该工具调用在落库后被中断,没有持久化的结果记录,其结果未知。\
    请从工具语义判断是否重试:仅当操作只读或幂等时才重试;如果可能产生副作用,\
    先核实外部状态或询问用户。不要盲目重试。";

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
    /// 当前权限模式(由 permission-mode 事件 fold;新会话默认 auto-edit)。
    permission_mode: PermissionMode,
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
        Self::create_described(dir, id, cwd, sandbox, parent_session, None)
    }

    fn create_described(
        dir: &Path,
        id: String,
        cwd: &Path,
        sandbox: bool,
        parent_session: Option<String>,
        subagent: Option<denia_core::session::SubagentDescriptor>,
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
            subagent,
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
                    permission_mode: PermissionMode::AutoEdit,
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
        let mut permission_mode = PermissionMode::AutoEdit;
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
                        SessionEvent::PermissionMode { mode } => permission_mode = *mode,
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
                permission_mode,
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
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
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

    /// 把缓冲的日志前缀刷到磁盘(对齐 dsh checkpoint-policy 的
    /// "模型请求前"检查点):agent-loop 在模型请求 dispatched 之前调用,
    /// 保证请求前缀(step-start/system-prompt/request-header/context)已
    /// 落盘,崩溃后能完整重建请求上下文;fail-closed——flush 失败则
    /// 不派发请求。
    pub fn flush(&self) -> Result<(), SessionError> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        inner.writer.flush()?;
        Ok(())
    }

    /// [`Session::append`] 的时间显式版:分支种子回放保留源事件时间戳,
    /// 轨迹时间轴在子会话里保持真实。
    fn append_with_time(
        &self,
        event: SessionEvent,
        time: u64,
    ) -> Result<SessionEnvelope, SessionError> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
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
            SessionEvent::PermissionMode { mode } => {
                inner.permission_mode = *mode;
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
        let next_offset =
            inner.offsets.last().copied().unwrap_or(inner.base_offset) + line.len() as u64 + 1;
        inner.offsets.push(next_offset);
        // 落盘策略:步骤边界/工具结果/todo 快照/权限切换立即 flush(耐久性
        // 边界——权限档位是安全语义,必须立即可被磁盘读者看到),流式 chunk
        // 只进 buffer,超 16KB 自动落盘(高频帧零系统调用)。
        match &envelope.event {
            SessionEvent::TurnEnd { .. }
            | SessionEvent::StepEnd { .. }
            | SessionEvent::ToolResult { .. }
            | SessionEvent::TodoWrite { .. }
            | SessionEvent::PermissionMode { .. } => {
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
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
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
        inner.permission_mode = PermissionMode::AutoEdit;
        inner.meter = ContextMeter::new();
        inner.pending_turn.clear();
        let kept = inner.events.clone();
        for envelope in &kept {
            match &envelope.event {
                SessionEvent::TurnStart { turn } => inner.last_turn = inner.last_turn.max(*turn),
                SessionEvent::SystemPrompt { text, .. } => {
                    inner.last_system_prompt = Some(text.clone())
                }
                SessionEvent::PermissionMode { mode } => {
                    inner.permission_mode = *mode;
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
            let mut fh = OpenOptions::new()
                .create(true)
                .append(true)
                .open(rewind_file)?;
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
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
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

    /// 当前权限模式(由事件 fold,O(1),不遍历日志)。
    pub fn permission_mode(&self) -> PermissionMode {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .permission_mode
    }

    /// 切换会话权限模式:追加 permission-mode 事件(事件源折叠,O(1) 生效)。
    pub fn set_permission_mode(
        &self,
        mode: PermissionMode,
    ) -> Result<SessionEnvelope, SessionError> {
        self.append(SessionEvent::PermissionMode { mode })
    }

    /// The model-facing history projected from the log.
    pub fn derive_messages(&self) -> Vec<ChatMessage> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        derive_messages(&inner.events)
    }

    /// The first user prompt, trimmed, for list views.
    pub fn first_prompt_excerpt(&self, max_chars: usize) -> Option<String> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let text = inner
            .events
            .iter()
            .find_map(|envelope| match &envelope.event {
                SessionEvent::UserMessage { text, .. } => Some(text),
                _ => None,
            })?;
        Some(excerpt_text(text, max_chars))
    }

    /// Crash recovery: a `turn-start` without `turn/end` gets a synthetic
    /// interrupted close(对齐 dsh `interruptedTurnClosers`):先为已落库但
    /// 未答复的工具调用补合成错误结果,再补 `step/end`,最后补
    /// `turn/end { interrupted }`;时间戳复用最后真实事件时间戳(确定性,
    /// 不发明未来时间)。用户取消的 `aborted` 与崩溃闭合区分开。
    fn close_orphaned_turn(&self) -> Result<(), SessionError> {
        let (open_turn, last_step, pending_calls, last_time) = {
            let inner = self
                .inner
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let mut open_turn: Option<u32> = None;
            let mut last_step: Option<u32> = None;
            let mut answered: Vec<String> = Vec::new();
            let mut announced: Vec<(String, u32, u32)> = Vec::new();
            let mut last_time: u64 = 0;
            for envelope in &inner.events {
                last_time = envelope.time;
                match &envelope.event {
                    SessionEvent::TurnStart { turn } => {
                        open_turn = Some(*turn);
                        last_step = None;
                    }
                    SessionEvent::TurnEnd { .. } => {
                        open_turn = None;
                        last_step = None;
                    }
                    SessionEvent::StepStart { turn, step } => {
                        if Some(*turn) == open_turn {
                            last_step = Some(*step);
                        }
                    }
                    SessionEvent::AssistantMessage {
                        blocks, turn, step, ..
                    } => {
                        if Some(*turn) == open_turn {
                            for block in blocks {
                                if let ContentBlock::ToolCall { id, .. } = block {
                                    if !announced.iter().any(|(c, _, _)| c == id) {
                                        announced.push((id.clone(), *turn, *step));
                                    }
                                }
                            }
                        }
                    }
                    SessionEvent::ToolResult { call_id, .. } => answered.push(call_id.clone()),
                    _ => {}
                }
            }
            let pending_calls: Vec<(String, u32, u32)> = announced
                .into_iter()
                .filter(|(call, _, _)| !answered.contains(call))
                .collect();
            (open_turn, last_step, pending_calls, last_time)
        };
        let Some(turn) = open_turn else {
            return Ok(());
        };
        for (call_id, call_turn, call_step) in &pending_calls {
            self.append_with_time(
                SessionEvent::ToolResult {
                    turn: *call_turn,
                    step: *call_step,
                    call_id: call_id.clone(),
                    content: ORPHAN_TOOL_RESULT.to_string(),
                    is_error: true,
                    error: None,
                    error_identity: None,
                    meta: None,
                    replaces: None,
                    truncation: None,
                },
                last_time,
            )?;
        }
        if let Some(step) = last_step {
            self.append_with_time(SessionEvent::StepEnd { turn, step }, last_time)?;
        }
        self.append_with_time(
            SessionEvent::TurnEnd {
                turn,
                reason: TurnEndReason::Interrupted,
            },
            last_time,
        )?;
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<denia_core::session::SubagentDescriptor>,
}

/// 轮次轴锚点:一条非注入 user-message 的定位与预览文本。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionAnchor {
    pub seq: u64,
    pub text: String,
}

/// 锚点预览截断长度:轮次轴悬浮预览够用即可,不随正文长度膨胀响应。
const ANCHOR_PREVIEW_CHARS: usize = 240;

fn anchor_preview(text: &str) -> String {
    let collapsed: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let trimmed = collapsed.trim();
    if trimmed.chars().count() <= ANCHOR_PREVIEW_CHARS {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(ANCHOR_PREVIEW_CHARS).collect();
    format!("{cut}…")
}

/// 逐行读下一条可解析的日志事件:首个非空行产出会话头(存入 `header`,
/// 不作为事件返回),空行跳过,损坏行跳过(torn-tail 容忍),文件读尽返回
/// `None`。分页的两遍扫描共用,保证对同一文件产出一致的事件序列。
fn next_log_event(
    reader: &mut BufReader<File>,
    header: &mut Option<SessionHeader>,
    line: &mut String,
    line_no: &mut usize,
) -> Result<Option<SessionEnvelope>, SessionError> {
    loop {
        line.clear();
        let read = reader.read_line(line)?;
        if read == 0 {
            return Ok(None);
        }
        *line_no += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if *line_no == 1 {
            *header = Some(
                serde_json::from_str(trimmed)
                    .map_err(|e| SessionError::Corrupt(format!("bad header: {e}")))?,
            );
            continue;
        }
        return Ok(serde_json::from_str(trimmed).ok());
    }
}

/// 一次分页读取的会话事件窗口:只保留 `limit` 条,后端不驻留全量历史。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionPage {
    pub header: SessionHeader,
    pub events: Vec<SessionEnvelope>,
    pub total: u64,
    pub has_more_before: bool,
    /// 全会话非注入 user-message 锚点(与 `before` 无关,恒为全量)。
    pub anchors: Vec<SessionAnchor>,
}

/// 索引条目:启动/失效时从文件摘要得到,list 只读它。
#[derive(Debug, Clone)]
struct IndexEntry {
    meta: SessionMeta,
    stamp: FileStamp,
    /// 该条目何时进入内存索引;列表节流后用于对“刚创建/刚加载”的会话
    /// 仍做一次 stat,避免创建后立刻刷新列表时摘要滞后。
    indexed_at: u64,
}

#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub id: String,
    pub created_at: u64,
    pub excerpt: Option<String>,
    pub cwd: String,
    pub sandbox: bool,
    pub parent_session: Option<String>,
    pub subagent: Option<denia_core::session::SubagentDescriptor>,
}

/// The sessions root: `<home>/sessions` + 内存索引。
pub struct SessionStore {
    root: PathBuf,
    index: Mutex<std::collections::HashMap<String, IndexEntry>>,
    /// 活跃(已加载)会话的弱引用:list 优先从内存读摘要,
    /// 不依赖文件落盘状态,首条用户消息即刻可见。
    active: Mutex<std::collections::HashMap<String, std::sync::Weak<Session>>>,
    /// 上次对目录做完整校验的 epoch ms;列表高频刷新时避免 O(会话数) stat。
    last_scan_ms: Mutex<u64>,
}

impl SessionStore {
    pub fn open(home: &Path) -> Result<Self, SessionError> {
        let root = home.join("sessions");
        std::fs::create_dir_all(&root)?;
        let store = Self {
            root,
            index: Mutex::new(std::collections::HashMap::new()),
            active: Mutex::new(std::collections::HashMap::new()),
            last_scan_ms: Mutex::new(0),
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
                fresh.insert(
                    meta.id.clone(),
                    IndexEntry {
                        meta,
                        stamp,
                        indexed_at: now_millis(),
                    },
                );
            }
        }
        *self.index.lock().unwrap_or_else(|p| p.into_inner()) = fresh;
        *self.last_scan_ms.lock().unwrap_or_else(|p| p.into_inner()) = now_millis();
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
        let session = self.create_with_parent(cwd, sandbox, Some(parent_id.to_string()))?;
        session.seed_from(&source_events[..cut])?;
        Ok(session)
    }

    pub fn create_subagent(
        &self,
        cwd: &Path,
        sandbox: bool,
        parent_id: &str,
        descriptor: denia_core::session::SubagentDescriptor,
    ) -> Result<Session, SessionError> {
        let id = uuid::Uuid::new_v4().to_string();
        let dir = self.root.join(&id);
        let session = Session::create_described(
            &dir,
            id,
            cwd,
            sandbox,
            Some(parent_id.into()),
            Some(descriptor),
        )?;
        let meta = meta_of(&session);
        self.index.lock().unwrap_or_else(|p| p.into_inner()).insert(
            meta.id.clone(),
            IndexEntry {
                meta,
                stamp: file_stamp(session.file()).unwrap_or(FileStamp {
                    size: 0,
                    mtime_ms: 0,
                }),
                indexed_at: now_millis(),
            },
        );
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
        self.index.lock().unwrap_or_else(|p| p.into_inner()).insert(
            meta.id.clone(),
            IndexEntry {
                meta,
                stamp: file_stamp(session.file()).unwrap_or(FileStamp {
                    size: 0,
                    mtime_ms: 0,
                }),
                indexed_at: now_millis(),
            },
        );
        Ok(session)
    }

    pub fn load(&self, id: &str) -> Result<Session, SessionError> {
        let file = self.file_for(id)?;
        if !file.exists() {
            return Err(SessionError::NotFound(id.to_string()));
        }
        let session = Session::load(&file)?;
        let meta = meta_of(&session);
        self.index.lock().unwrap_or_else(|p| p.into_inner()).insert(
            meta.id.clone(),
            IndexEntry {
                meta,
                stamp: file_stamp(session.file()).unwrap_or(FileStamp {
                    size: 0,
                    mtime_ms: 0,
                }),
                indexed_at: now_millis(),
            },
        );
        Ok(session)
    }

    /// 直接按文件流式读取一个「展示粒度」事件窗口,不构造/驻留完整 `Session`。
    ///
    /// - `before = None`:取日志尾部最近 `limit` 条。
    /// - `before = Some(seq)`:取 `seq` 之前最近 `limit` 条(供前端向上翻页)。
    /// - 分页单位是展示事件:流式 `assistant-chunk` 不计入 `total` 也不进入
    ///   窗口——历史重建只依赖结算的 `assistant-message`(dsh 语义:失败
    ///   尝试不进派生历史),否则 chunk 洪流会让窗口在两三个轮次内触底。
    /// - 窗口起点对齐轮次组:展示边界(total-limit)回退到其前最近一条非
    ///   注入 user-message——保证窗口内第一个轮次完整,前端折叠概览依赖
    ///   成对的 turn-start/turn-end;边界落在首条锚点之前时直接用边界。
    /// - `anchors` 恒为全会话非注入 user-message(不受 `before` 限制),供
    ///   前端轮次轴渲染全部刻度。
    /// - 两遍顺序扫描:窗口起点依赖文件末尾才能确定的边界,单遍需无界缓存;
    ///   两遍只引入一次顺序 IO,内存 O(锚点数 + 窗口)。
    pub fn read_page(
        &self,
        id: &str,
        before: Option<u64>,
        limit: usize,
    ) -> Result<SessionPage, SessionError> {
        let file = self.file_for(id)?;
        if !file.exists() {
            return Err(SessionError::NotFound(id.to_string()));
        }
        let limit = limit.clamp(1, 1000);
        let eligible = |seq: u64| before.map_or(true, |cut| seq < cut);

        // 第一遍:统计展示事件总数、收集全会话锚点,并记录每个 eligible 锚点
        // 的展示位次(供起点回退二分)。
        let mut reader = BufReader::new(File::open(&file)?);
        let mut header: Option<SessionHeader> = None;
        let mut line = String::new();
        let mut line_no = 0usize;
        let mut total = 0u64;
        let mut anchors: Vec<SessionAnchor> = Vec::new();
        // (展示位次, seq):展示位次按 eligible 展示事件序号计。
        let mut anchor_at: Vec<(u64, u64)> = Vec::new();
        while let Some(envelope) = next_log_event(&mut reader, &mut header, &mut line, &mut line_no)? {
            if matches!(envelope.event, SessionEvent::AssistantChunk { .. }) {
                continue;
            }
            if let SessionEvent::UserMessage { text, injected: false, .. } = &envelope.event {
                anchors.push(SessionAnchor {
                    seq: envelope.seq,
                    text: anchor_preview(text),
                });
                if eligible(envelope.seq) {
                    anchor_at.push((total, envelope.seq));
                }
            }
            if eligible(envelope.seq) {
                total += 1;
            }
        }
        let header = header.ok_or_else(|| SessionError::Corrupt("missing header".into()))?;

        // 窗口起点:边界回退到其前最近锚点;无锚点可用则直接用边界。
        let boundary = total.saturating_sub(limit as u64);
        let start_index = match anchor_at.binary_search_by(|(index, _)| index.cmp(&boundary)) {
            Ok(pos) => anchor_at[pos].0,
            Err(0) => boundary,
            Err(insert) => anchor_at[insert - 1].0,
        };

        // 第二遍:收集 eligible 展示事件中位次 ≥ 起点的窗口。
        let mut reader = BufReader::new(File::open(&file)?);
        let mut header2: Option<SessionHeader> = None;
        let mut line_no = 0usize;
        let mut events: Vec<SessionEnvelope> = Vec::new();
        let mut index = 0u64;
        while let Some(envelope) = next_log_event(&mut reader, &mut header2, &mut line, &mut line_no)? {
            if matches!(envelope.event, SessionEvent::AssistantChunk { .. }) {
                continue;
            }
            if !eligible(envelope.seq) {
                continue;
            }
            if index >= start_index {
                events.push(envelope);
            }
            index += 1;
        }
        Ok(SessionPage {
            header,
            events,
            total,
            has_more_before: start_index > 0,
            anchors,
        })
    }

    /// 登记一个被 `Arc` 持有的会话:list 优先从内存读摘要,
    /// 不依赖文件落盘状态,首条用户消息即刻可见。弱引用不阻止淘汰。
    pub fn track_session(&self, session: &std::sync::Arc<Session>) {
        let meta = meta_of(session);
        self.index.lock().unwrap_or_else(|p| p.into_inner()).insert(
            meta.id.clone(),
            IndexEntry {
                meta,
                stamp: FileStamp {
                    size: 0,
                    mtime_ms: 0,
                },
                indexed_at: now_millis(),
            },
        );
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
                .filter_map(|(id, weak)| match weak.upgrade() {
                    Some(session) => {
                        summaries.push(summary_of(&session));
                        None
                    }
                    None => Some(id.clone()),
                })
                .collect();
            for id in stale {
                active.remove(&id);
            }
        }

        // 2) 目录校验节流:进程内 create/load/delete 都会直接维护索引,
        //    列表高频刷新不需要每次都对全部文件做 stat;外部直接写文件时,
        //    最多延迟 2 秒被下次全量扫描发现。
        let last_scan_value = {
            let mut last = self.last_scan_ms.lock().unwrap_or_else(|p| p.into_inner());
            let now = now_millis();
            let value = *last;
            if now.saturating_sub(value) >= 2000 {
                *last = now;
                value
            } else {
                value
            }
        };
        let should_scan = now_millis().saturating_sub(last_scan_value) >= 2000;

        {
            let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            if should_scan {
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
                        discovered.push(IndexEntry {
                            meta,
                            stamp,
                            indexed_at: now_millis(),
                        });
                    }
                }
                for entry in discovered {
                    index.insert(entry.meta.id.clone(), entry);
                }

                // 索引条目 stat 失效检测。
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
                            index.insert(
                                id,
                                IndexEntry {
                                    meta,
                                    stamp,
                                    indexed_at: now_millis(),
                                },
                            );
                        } else {
                            index.remove(&id);
                        }
                    }
                }
            } else {
                // 刚创建/加载的条目还没经过一次全量校验:单独 stat 一次,
                // 保证“创建后立刻 append 再刷新列表”也能看到最新摘要。
                let recent_ids: Vec<String> = index
                    .iter()
                    .filter(|(_, entry)| entry.indexed_at > last_scan_value)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in recent_ids {
                    let file = self.root.join(&id).join("session.jsonl");
                    let current = file_stamp(&file);
                    let dirty = match (index.get(&id), current) {
                        (Some(entry), Some(current)) => entry.stamp != current,
                        (_, None) => true,
                        (None, _) => false,
                    };
                    if dirty {
                        if let Some((meta, stamp)) = read_summary(&file) {
                            index.insert(
                                id,
                                IndexEntry {
                                    meta,
                                    stamp,
                                    indexed_at: now_millis(),
                                },
                            );
                        } else {
                            index.remove(&id);
                        }
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

    /// 一次性清扫"从未发过消息的残留空白会话"(浏览器直接关闭、进程被杀
    /// 留下的空壳)。只在**进程启动时**调用一次,不在运行期反复执行。
    ///
    /// 清理职责属于数据层,不属于某个视图:前端每个页面只知道自己的焦点
    /// (`activeId`),却能看到全局会话列表;在那里做"非活跃空白即删"会误删
    /// 别的页面(或共用同一数据目录的另一实例)刚创建、正在使用的会话。
    ///
    /// `older_than_ms` 是给并发实例留的缓冲:只删足够老的空白,避免删掉
    /// 另一个实例里用户正停着的草稿。单个删除失败(Windows 上文件被占用)
    /// 只跳过,不让一次清理挡住启动。
    pub fn sweep_stale_blanks(&self, older_than_ms: u64) -> Vec<String> {
        let cutoff = now_millis().saturating_sub(older_than_ms);
        let candidates: Vec<String> = self
            .list()
            .unwrap_or_default()
            .into_iter()
            // 有内容的、分支血缘的(fork 子会话/委派)都保留,只动纯空白壳。
            .filter(|summary| {
                summary.excerpt.is_none()
                    && summary.parent_session.is_none()
                    && summary.created_at < cutoff
            })
            .map(|summary| summary.id)
            .collect();
        candidates
            .into_iter()
            .filter(|id| self.delete(id).is_ok())
            .collect()
    }
}

fn validate_id(id: &str) -> Result<&str, SessionError> {
    let valid = !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
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
        excerpt: session
            .header()
            .subagent
            .as_ref()
            .map(|s| s.label.clone())
            .or_else(|| session.first_prompt_excerpt(80)),
        cwd: session.header().cwd.clone(),
        sandbox: session.header().sandbox,
        parent_session: session.header().parent_session.clone(),
        subagent: session.header().subagent.clone(),
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
        subagent: meta.subagent.clone(),
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
        if let SessionEvent::UserMessage {
            text,
            injected: false,
            ..
        } = envelope.event
        {
            excerpt = Some(excerpt_text(&text, 80));
            break;
        }
    }
    let header: SessionHeader = serde_json::from_str(first_line.as_deref()?).ok()?;
    let id = file.parent()?.file_name()?.to_string_lossy().to_string();
    Some((
        SessionMeta {
            id,
            created_at: header.created_at,
            excerpt: header
                .subagent
                .as_ref()
                .map(|s| s.label.clone())
                .or(excerpt),
            cwd: header.cwd,
            sandbox: header.sandbox,
            parent_session: header.parent_session,
            subagent: header.subagent,
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
        session.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
        session
            .append(SessionEvent::UserMessage {
                text: "hello world, this is a prompt".into(),
                injected: false,
                images: Vec::new(),
                channel: None,
            })
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
        assert!(
            summaries[0]
                .excerpt
                .as_deref()
                .unwrap()
                .starts_with("hello world")
        );

        store.delete(&id).unwrap();
        assert!(matches!(store.load(&id), Err(SessionError::NotFound(_))));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn read_page_streams_tail_and_before_without_full_load() {
        let root = temp_root();
        let store = SessionStore::open(&root).unwrap();
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();

        let session = store.create(&cwd, true).unwrap();
        let id = session.id().to_string();
        for i in 1..=12u64 {
            session
                .append(SessionEvent::UserMessage {
                    text: format!("message {i}"),
                    injected: false,
                    images: Vec::new(),
                    channel: None,
                })
                .unwrap();
        }
        drop(session);

        let tail = store.read_page(&id, None, 5).unwrap();
        assert_eq!(tail.total, 12);
        assert_eq!(tail.events.len(), 5);
        assert_eq!(tail.events.last().unwrap().seq, 12);
        assert!(tail.has_more_before);

        let before = store.read_page(&id, Some(7), 3).unwrap();
        assert_eq!(before.events.len(), 3);
        assert_eq!(before.events.first().unwrap().seq, 4);
        assert_eq!(before.events.last().unwrap().seq, 6);
        assert!(before.has_more_before);

        let head = store.read_page(&id, Some(4), 5).unwrap();
        assert_eq!(head.events.len(), 3);
        assert!(!head.has_more_before);

        assert!(matches!(
            store.read_page("00000000-0000-4000-8000-000000000000", None, 5),
            Err(SessionError::NotFound(_))
        ));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn read_page_display_granularity_ignores_chunks_and_aligns_turn_group() {
        let root = temp_root();
        let store = SessionStore::open(&root).unwrap();
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();

        let session = store.create(&cwd, true).unwrap();
        let id = session.id().to_string();
        let chunk = |text: &str| SessionEvent::AssistantChunk {
            turn: 1,
            step: 1,
            chunk: denia_core::stream::StreamChunk::TextDelta {
                index: 0,
                text: text.into(),
            },
        };
        // 轮次 1:用户 + 3 条 chunk + 结算消息 + 收尾。
        session
            .append(SessionEvent::UserMessage {
                text: "u1\nsecond line".into(),
                injected: false,
                images: Vec::new(),
                channel: None,
            })
            .unwrap();
        session.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
        for _ in 0..3 {
            session.append(chunk("x")).unwrap();
        }
        session
            .append(SessionEvent::AssistantMessage {
                turn: 1,
                step: 1,
                blocks: Vec::new(),
                usage: None,
                interrupted: false,
                source_event_seqs: Vec::new(),
            })
            .unwrap();
        session
            .append(SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            })
            .unwrap();
        // 轮次 2:用户 + 工具 + 2 条 chunk + 结算消息 + 收尾。
        session
            .append(SessionEvent::UserMessage {
                text: "u2".into(),
                injected: false,
                images: Vec::new(),
                channel: None,
            })
            .unwrap();
        session.append(SessionEvent::TurnStart { turn: 2 }).unwrap();
        session
            .append(SessionEvent::ToolCall {
                turn: 2,
                step: 1,
                call_id: "c1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            })
            .unwrap();
        session
            .append(SessionEvent::ToolResult {
                turn: 2,
                step: 1,
                call_id: "c1".into(),
                content: "ok".into(),
                is_error: false,
                error: None,
                error_identity: None,
                meta: None,
                replaces: None,
                truncation: None,
            })
            .unwrap();
        session.append(chunk("y")).unwrap();
        session.append(chunk("z")).unwrap();
        session
            .append(SessionEvent::AssistantMessage {
                turn: 2,
                step: 1,
                blocks: Vec::new(),
                usage: None,
                interrupted: false,
                source_event_seqs: Vec::new(),
            })
            .unwrap();
        session
            .append(SessionEvent::TurnEnd {
                turn: 2,
                reason: TurnEndReason::Completed,
            })
            .unwrap();
        drop(session);

        // 展示粒度:chunk 不计入 total,也不出现在窗口。
        let page = store.read_page(&id, None, 100).unwrap();
        assert_eq!(page.total, 10);
        assert!(page
            .events
            .iter()
            .all(|envelope| !matches!(envelope.event, SessionEvent::AssistantChunk { .. })));
        assert!(!page.has_more_before);
        // 锚点为全会话非注入用户消息,预览压平换行。
        assert_eq!(page.anchors.len(), 2);
        assert_eq!(page.anchors[0].text, "u1 second line");
        assert_eq!(page.anchors[1].text, "u2");

        // 小窗口:最近锚点(u2)距尾部已达 limit,窗口回退到 u2——
        // 轮次 2 完整在窗口内(前端折叠依赖成对的 turn-start/turn-end)。
        let tail = store.read_page(&id, None, 4).unwrap();
        assert_eq!(tail.total, 10);
        assert_eq!(tail.events.len(), 6);
        assert_eq!(tail.events.first().unwrap().seq, page.anchors[1].seq);
        assert!(tail.has_more_before);
        assert!(tail.anchors.len() == 2, "anchors 不受窗口限制");

        // before 查询:eligible 区按展示粒度计数;最近锚点(u1)起的轮次组
        // 已达 limit,窗口回退到 u1 整组返回,锚点仍为全量。
        let before = store.read_page(&id, Some(page.anchors[1].seq), 2).unwrap();
        assert_eq!(before.total, 4);
        assert_eq!(before.events.len(), 4);
        assert_eq!(before.events.first().unwrap().seq, page.anchors[0].seq);
        assert!(!before.has_more_before);
        assert_eq!(before.anchors.len(), 2);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn read_page_aligns_window_to_earlier_anchor_behind_small_tail_turns() {
        let root = temp_root();
        let store = SessionStore::open(&root).unwrap();
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();

        let session = store.create(&cwd, true).unwrap();
        let id = session.id().to_string();
        let user = |text: &str| SessionEvent::UserMessage {
            text: text.into(),
            injected: false,
            images: Vec::new(),
            channel: None,
        };
        // 轮次 1:u1 + 工具×3 对 + 结算 + 收尾(展示位次 0..9);
        // 轮次 2/3:各 4 条展示事件的小轮次。尾部小轮次合计不足 limit 时,
        // 边界会落在轮次 1 的工具段中间——窗口必须回退到 u1,否则切出
        // 无 turn-start 的残轮次,前端折叠失效。
        let turn = |turn: u32, tools: usize, session: &Session| {
            session.append(user(&format!("u{turn}"))).unwrap();
            session
                .append(SessionEvent::TurnStart { turn })
                .unwrap();
            for i in 0..tools {
                session
                    .append(SessionEvent::ToolCall {
                        turn,
                        step: 1,
                        call_id: format!("c{turn}-{i}"),
                        name: "bash".into(),
                        arguments: "{}".into(),
                    })
                    .unwrap();
                session
                    .append(SessionEvent::ToolResult {
                        turn,
                        step: 1,
                        call_id: format!("c{turn}-{i}"),
                        content: "ok".into(),
                        is_error: false,
                        error: None,
                        error_identity: None,
                        meta: None,
                        replaces: None,
                        truncation: None,
                    })
                    .unwrap();
            }
            session
                .append(SessionEvent::AssistantMessage {
                    turn,
                    step: 1,
                    blocks: Vec::new(),
                    usage: None,
                    interrupted: false,
                    source_event_seqs: Vec::new(),
                })
                .unwrap();
            session
                .append(SessionEvent::TurnEnd {
                    turn,
                    reason: TurnEndReason::Completed,
                })
                .unwrap();
        };
        turn(1, 3, &session);
        turn(2, 0, &session);
        turn(3, 0, &session);
        drop(session);

        // total=18;limit=12 → 边界=6,正是轮次 1 第三个 toolcall(工具段中)。
        let page = store.read_page(&id, None, 12).unwrap();
        assert_eq!(page.total, 18);
        // 窗口回退到 u1:第一个轮次完整,而不是从边界切出残轮次。
        assert_eq!(page.events.len(), 18);
        assert_eq!(page.anchors[0].seq, page.events[0].seq);
        assert!(matches!(page.events[0].event, SessionEvent::UserMessage { .. }));
        assert!(!page.has_more_before);

        // 尾部小窗口:边界落在轮次 2/3 之间,起点对齐 u2,完整返回尾两个轮次。
        let tail = store.read_page(&id, None, 7).unwrap();
        assert_eq!(tail.total, 18);
        assert_eq!(tail.events.len(), 8);
        assert_eq!(tail.events[0].seq, page.anchors[1].seq);
        assert!(tail.has_more_before);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn permission_mode_folds_and_persists() {
        let root = temp_root();
        let store = SessionStore::open(&root).unwrap();
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();

        let session = store.create(&cwd, true).unwrap();
        assert_eq!(session.permission_mode(), PermissionMode::AutoEdit);

        session
            .set_permission_mode(PermissionMode::ReadOnly)
            .unwrap();
        assert_eq!(session.permission_mode(), PermissionMode::ReadOnly);
        let id = session.id().to_string();
        drop(session);

        let loaded = store.load(&id).unwrap();
        assert_eq!(loaded.permission_mode(), PermissionMode::ReadOnly);

        loaded.set_permission_mode(PermissionMode::Full).unwrap();
        assert_eq!(loaded.permission_mode(), PermissionMode::Full);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn legacy_permission_values_map_to_new_modes() {
        // 旧日志三档值反序列化时落到语义等价的新档位(serde alias)。
        use denia_core::session::PermissionMode as PM;
        let cases = [
            ("read-only", PM::ReadOnly),
            ("workspace-write", PM::AutoEdit),
            ("auto-edit", PM::AutoEdit),
            ("plan", PM::Plan),
            ("danger-full-access", PM::Full),
            ("full", PM::Full),
        ];
        for (raw, expected) in cases {
            assert_eq!(
                serde_json::from_str::<PM>(&format!("\"{raw}\"")).unwrap(),
                expected,
                "legacy value {raw} must map"
            );
        }
        assert!(PM::parse("plan").is_some());
        assert!(PM::parse("nonsense").is_none());
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
            .append(SessionEvent::UserMessage {
                text: "fresh excerpt".into(),
                injected: false,
                images: Vec::new(),
                channel: None,
            })
            .unwrap();
        let summary = store.list().unwrap();
        assert!(
            summary[0]
                .excerpt
                .as_deref()
                .unwrap()
                .contains("fresh excerpt"),
            "excerpt must refresh: {:?}",
            summary[0].excerpt
        );

        // 未登记会话(值语义):落盘边界后走文件 stat 失效检测路径。
        let session2 = store.create(&root, false).unwrap();
        session2
            .append(SessionEvent::UserMessage {
                text: "file-based excerpt".into(),
                injected: false,
                images: Vec::new(),
                channel: None,
            })
            .unwrap();
        // 强制落盘:TurnEnd 边界会 flush。
        session2
            .append(SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            })
            .unwrap();
        drop(session2);
        let summaries = store.list().unwrap();
        assert!(
            summaries.iter().any(|s| s
                .excerpt
                .as_deref()
                .unwrap_or("")
                .contains("file-based excerpt")),
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
        session.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
        session
            .append(SessionEvent::UserMessage {
                text: "hi".into(),
                injected: false,
                images: Vec::new(),
                channel: None,
            })
            .unwrap();
        let file = session.file().to_path_buf();
        drop(session);

        // Simulate a crash mid-append: garbage after the committed prefix.
        {
            use std::io::Write as _;
            let mut f = OpenOptions::new().append(true).open(&file).unwrap();
            f.write_all(b"{\"seq\":3,\"time\":1,\"type\":\"user-mess")
                .unwrap();
            f.flush().unwrap();
        }

        let loaded = store.load(&id).unwrap();
        // Two good events + the synthetic turn-end close.
        assert_eq!(loaded.events().len(), 3);
        match &loaded.events()[2].event {
            SessionEvent::TurnEnd { turn, reason } => {
                assert_eq!(*turn, 1);
                assert_eq!(*reason, TurnEndReason::Interrupted);
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
            .append(SessionEvent::UserMessage {
                text: "ping".into(),
                injected: false,
                images: Vec::new(),
                channel: None,
            })
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
        session.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
        assert_eq!(session.next_turn_number(), 2);
        session
            .append(SessionEvent::SystemPrompt {
                turn: 1,
                step: 1,
                text: "sys-a".into(),
            })
            .unwrap();
        assert_eq!(session.last_system_prompt().as_deref(), Some("sys-a"));
        session
            .append(SessionEvent::SystemPrompt {
                turn: 1,
                step: 2,
                text: "sys-b".into(),
            })
            .unwrap();
        assert_eq!(session.last_system_prompt().as_deref(), Some("sys-b"));

        session
            .append(SessionEvent::UserMessage {
                text: "uno".into(),
                injected: false,
                images: Vec::new(),
                channel: None,
            })
            .unwrap();
        session
            .append(SessionEvent::UserMessage {
                text: "dos".into(),
                injected: false,
                images: Vec::new(),
                channel: None,
            })
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
            .append(SessionEvent::UserMessage {
                text: "first".into(),
                injected: false,
                images: Vec::new(),
                channel: None,
            })
            .unwrap();
        session.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
        session
            .append(SessionEvent::StepStart { turn: 1, step: 1 })
            .unwrap();
        session
            .append(SessionEvent::UserMessage {
                text: "second".into(),
                injected: false,
                images: Vec::new(),
                channel: None,
            })
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
        source.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
        source
            .append(SessionEvent::UserMessage {
                text: "first".into(),
                injected: false,
                images: Vec::new(),
                channel: None,
            })
            .unwrap();
        source
            .append(SessionEvent::AssistantMessage {
                turn: 1,
                step: 1,
                blocks: vec![denia_core::stream::ContentBlock::Text { text: "hi".into() }],
                usage: None,
                interrupted: false,
                source_event_seqs: Vec::new(),
            })
            .unwrap();
        source
            .append(SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            })
            .unwrap();
        // 未完成轮次:不应进入种子。
        source.append(SessionEvent::TurnStart { turn: 2 }).unwrap();
        source
            .append(SessionEvent::UserMessage {
                text: "running".into(),
                injected: false,
                images: Vec::new(),
                channel: None,
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
        assert_eq!(
            reloaded.header().parent_session.as_deref(),
            Some(source_id.as_str())
        );
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
