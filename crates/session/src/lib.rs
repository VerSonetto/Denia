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

mod append;
mod creation;
mod history;
mod log_reader;
mod log_writer;
mod projection;
mod recovery;
mod store;
mod summary;
pub mod task_projection;
#[cfg(test)]
mod tests;

use log_reader::{
    FileStamp, file_stamp, is_transient_event, next_log_event, now_millis, open_append_writer,
    usage_envelope,
};
use log_writer::LogWriter;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use summary::{
    IndexEntry, anchor_preview, excerpt_text, meta_of, read_summary, summary_of, summary_of_meta,
};
pub use summary::{SessionAnchor, SessionMeta, SessionPage, SessionSummary, SessionTotals};

use denia_core::message::ChatMessage;
use denia_core::session::{
    ApprovalOutcome, AskOutcome, AskResolution, GoalOp, GoalState, PermissionMode,
    SESSION_FORMAT_VERSION, SessionEnvelope, SessionEvent, SessionHeader, SessionHeaderKind,
    TurnEndReason, apply_goal_op,
};
use denia_core::stream::ContentBlock;
use denia_core::task::{RevisionId, TaskState};
use denia_token_meter::{ContextBreakdown, ContextMeter, ContextPressure, TurnTokenUsage};
use thiserror::Error;

pub use history::{HistoryProjection, LEGACY_HISTORY_PROJECTION_VERSION, legacy_subagent_drop_set};
pub use history::{SUBAGENT_SEED_VERSION, SubagentSeed, build_subagent_seed};
pub use task_projection::{TaskFoldScope, fresh_revision_id, project_task, project_task_scoped};

/// 历史投影文件名（会话目录内，与会话日志同级）。
pub const HISTORY_PROJECTION_FILE: &str = "history-projection.json";

/// 崩溃孤儿轮闭合时,给"已落库但未答复"的工具调用补的合成结果
/// (结果未知,谨慎重试,不盲目重放)。
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

/// Mutable log state behind the per-session lock.
struct SessionInner {
    events: Vec<SessionEnvelope>,
    writer: LogWriter,
    /// 每个事件行结束的字节偏移(含换行);物理回退截断文件用。
    /// 与 `events` 一一对应。
    offsets: Vec<u64>,
    /// session.jsonl 首行(header)结束后的字节偏移。
    base_offset: u64,
    /// 下一次 append 的文件字节偏移(含 transient 行)。
    ///
    /// `offsets` 只登记驻留事件的行尾偏移,transient(chunk)事件不登记,
    /// 因此文件游标必须单独维护;回退截断后同步回退到截断点。
    next_offset: u64,
    /// 最新 turn 号(append 时维护),next_turn_number O(1)。
    last_turn: u32,
    /// 最近一次落日志的系统提示词;should_log_system_prompt 的 O(1) 依据。
    last_system_prompt: Option<String>,
    /// token-meter 增量 fold:上下文 token 组成的 O(1) 投影。
    meter: ContextMeter,
    /// 当前 turn 的轻量计费事件,不含消息正文/工具输出;`turn-end` 时
    /// 交给 `meter.fold_turn`,随后释放缓冲容量。
    pending_turn: Vec<SessionEnvelope>,
    /// 当前权限模式(由 permission-mode 事件 fold;新会话默认 auto-edit)。
    permission_mode: PermissionMode,
    /// 当前会话的 agent preset(由 agent-preset 事件 fold,latest-wins;
    /// None = 未指定,落到部署默认预设)。
    agent_preset: Option<String>,
    /// 当前会话目标(goal 事件折叠;None = 无目标)。
    goal: Option<GoalState>,
    /// 当前任务验证账本(`Task` 事件与写入类 `ToolCall` 折叠;None = 日志里
    /// 没有任务事件)。
    ///
    /// 与 goal 的 O(1) 状态机单步不同,任务折叠要重放整条日志:结局取决于
    /// 验证结论与写入类工具调用之间的先后,还取决于折叠作用域(本会话身份 +
    /// rewind 报废的 revision)。折叠**不读用户消息**,也不看模型声明 ——
    /// 进它输入的只有 `Task` 事件与 `edit` / `write_file` 的目标路径
    /// (见 [`SessionInner::refold_task`] 与 `crate::task_projection`)。
    task: Option<TaskState>,
    /// 已报废的 revision id:被 rewind **物理截断**掉的旧分支身份。
    ///
    /// 折叠层能拒绝的只有"日志里还在的 id";截断之后那些 id 在日志里再也
    /// 查不到,只有这里还记得。加载时从 `rewinds.jsonl` 并集读入、回退时
    /// 把本次截掉的身份并进来,**只增不减**。
    retired_revisions: Vec<RevisionId>,
    /// 会话标题(session-title 事件折叠,latest-wins;None = 尚未生成)。
    title: Option<String>,
    /// 派生面缓存:`events` 的每次变更都会使它失效(见 `derived_revision`)。
    ///
    /// agent 每个 step 至少派生两次(微压缩闸门 + 请求构造),每次都是全量
    /// 重建整条历史并克隆所有正文/工具参数 —— 长会话里这是每 step 数 MB 级
    /// 的搬运。事件是 append-only 的,同一份 `events` 派生结果恒等,缓存下来
    /// 即可把"每 step 两次"降为"每个新事件一次"。
    derived_surface: Option<Arc<[denia_core::session::SurfaceMessage]>>,
    /// 缓存对应的日志版本号;与 `log_revision` 不符即视为失效。
    derived_revision: u64,
    /// 日志版本号:每次 append 与回退(截断)都自增。
    ///
    /// 不能用 `events.len()` 当键:回退会把日志截断,截断后的长度可能与
    /// 某次旧派生时的长度恰好相同,那时长度键会误判为"命中",把回退掉的
    /// 内容当成仍然存在。单调递增的版本号没有这个歧义。
    log_revision: u64,
    /// 冷态:事件 Vec 不驻留(仅浏览路径的打开方式)。
    ///
    /// 冷会话只保留聚合投影(meter/goal/权限/preset/标题/last_turn 等小
    /// 状态),事件需要时要么走磁盘流式回放([`Session::events_after`]),
    /// 要么 [`Session::ensure_hot`] 升级为全量驻留。所有**写路径**
    /// (append/回退/压缩/分支)必须先 ensure_hot,保证 `seq == index+1`
    /// 的既有不变式只在热态成立。
    cold: bool,
    /// 日志中最大的事件 seq(冷热态都维护);冷态快速判断回放区间用。
    last_seq: u64,
    /// 驻留事件的内存字节估算(逐行累计 append 写入的字节数)。
    ///
    /// 与 [`Session::log_bytes`](磁盘文件大小)是两回事:轮次闭合时 chunk
    /// 即被清扫,该值随之回落。驻留预算必须用它 —— 日志里八九成是已清扫的
    /// chunk,拿文件大小当代理量会把成本高估近十倍,预算形同虚设。
    resident_bytes: u64,
    /// 其中属于 transient(chunk)的部分;`turn-end` 清扫时按此扣减。
    transient_bytes: u64,
    /// 首个未结算 chunk 的下标;此前的历史无需参与清扫。
    transient_start: Option<usize>,
    first_prompt_excerpt: Option<String>,
    /// 旧子代理的模型历史投影（计划 9.4）；非旧 child 恒为 None。
    ///
    /// 只作用于**模型面**（`derive_messages`/派生面/注入基线），审计 UI 与
    /// `with_events` 仍是完整日志。
    history_projection: Option<history::HistoryProjection>,
}

/// 任务折叠作用域:折叠方身份(决定哪些验证结论算数)+ rewind 报废的 revision。
///
/// 热/冷恢复、增量 append 与 rewind 回放共用这一个构造函数,作用域不会因为
/// 走了哪条路径而不同——子代理不继承父结论、rewind 后旧身份不复活这两条
/// 都靠它成立。
fn task_fold_scope(session_id: &str, retired: &[RevisionId]) -> TaskFoldScope {
    TaskFoldScope {
        session: Some(session_id.to_string()),
        retired_revisions: retired.to_vec(),
    }
}

impl SessionInner {
    /// 由整条驻留日志重算任务验证账本。
    ///
    /// 账本要按**折叠方身份**(本会话 id)与**已报废 revision**裁决,所以重放
    /// 必须带上作用域 —— 子代理不继承父的验收结论、rewind 之后旧身份不复活
    /// 这两条都靠它成立。成本落在写一条验证结论上(一次 run_checks 一条),
    /// 不在每 step 派生两次的热路径上。
    fn refold_task(&mut self, session_id: &str) {
        let scope = task_fold_scope(session_id, &self.retired_revisions);
        self.task = project_task_scoped(&self.events, &scope);
    }
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
    pub fn id(&self) -> &str {
        &self.header.id
    }

    pub fn directory(&self) -> &Path {
        self.file.parent().expect("session file has a directory")
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

    /// 日志中最大的事件 seq(冷热态都维护)。等待方用它作为增量游标。
    pub fn last_seq(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .last_seq
    }

    /// Envelopes with `seq > after`, in order. SSE replay 专用:不克隆全量,
    /// 只收集增量区间。
    ///
    /// 内存事件表不驻留已闭合轮次的 chunk(seq 有空洞),因此热路径按
    /// **seq** 定位而不是下标;游标落在被清扫区间内时,回放只含非 chunk
    /// 事件——闭环轮次的历史重建本就只依赖结算消息,前端断档自愈兜底。
    /// 冷态游标在尾部直接零回放(零 IO),落后才走磁盘流式扫描。
    pub fn events_after(&self, after: u64) -> Vec<SessionEnvelope> {
        {
            let inner = self
                .inner
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if !inner.cold {
                let idx = inner
                    .events
                    .partition_point(|envelope| envelope.seq <= after);
                return inner.events[idx..].to_vec();
            }
            if after >= inner.last_seq {
                return Vec::new();
            }
        }
        self.read_events_after_disk(after)
    }

    /// 冷态磁盘回放:流式扫描日志,收集 `seq > after` 的事件。
    fn read_events_after_disk(&self, after: u64) -> Vec<SessionEnvelope> {
        let Ok(file) = File::open(&self.file) else {
            return Vec::new();
        };
        let mut reader = BufReader::new(file);
        let mut header = None;
        let mut line = String::new();
        let mut line_no = 0usize;
        let mut out = Vec::new();
        while let Ok(Some(envelope)) =
            next_log_event(&mut reader, &mut header, &mut line, &mut line_no)
        {
            if envelope.seq > after {
                out.push(envelope);
            }
        }
        out
    }

    /// 是否处于热态(事件全量驻留)。
    pub fn is_hot(&self) -> bool {
        !self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .cold
    }

    /// 升级为热态(事件全量驻留):所有写路径(append/回退/压缩/分支)
    /// 前的强制步骤。幂等;首次升级执行完整的 load 修复(孤儿轮闭合 +
    /// meter 重算),保证修复总在首次写入前落地。
    pub fn ensure_hot(&self) -> Result<(), SessionError> {
        if self.is_hot() {
            return Ok(());
        }
        let hot = Session::load(&self.file)?;
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !inner.cold {
            return Ok(());
        }
        let mut hot_inner = hot
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        std::mem::swap(&mut *inner, &mut *hot_inner);
        Ok(())
    }

    pub fn cool(&self) -> Result<bool, SessionError> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if inner.cold {
            return Ok(false);
        }
        let mut open_turn = false;
        for envelope in &inner.events {
            match envelope.event {
                SessionEvent::TurnStart { .. } => open_turn = true,
                SessionEvent::TurnEnd { .. } => open_turn = false,
                _ => {}
            }
        }
        if open_turn {
            return Ok(false);
        }
        inner.writer.flush()?;
        inner.events = Vec::new();
        inner.offsets = Vec::new();
        inner.pending_turn = Vec::new();
        inner.derived_surface = None;
        inner.resident_bytes = 0;
        inner.transient_bytes = 0;
        inner.transient_start = None;
        inner.cold = true;
        Ok(true)
    }

    /// 在日志锁内只读地跑一段闭包,不克隆日志。
    ///
    /// [`Session::events`] 的语义是"给我一份日志副本",被当只读迭代器用时
    /// 就变成了"为了数 3 个 step 先克隆整条日志(含全部 chunk)"。这个入口
    /// 让读路径零拷贝;闭包内**不得**再调用任何需要本会话锁的方法(会自锁)。
    pub fn with_events<R>(&self, f: impl FnOnce(&[SessionEnvelope]) -> R) -> R {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        f(&inner.events)
    }

    /// 模型面事件视图：应用历史投影（若有）后的事件序列。
    ///
    /// UI/审计继续用 [`Session::with_events`]（完整日志）；任何"模型见过什么"
    /// 的判断（派生的对话历史、自动注入的基线）都必须走这里，否则投影会被
    /// 旁路掉——旧 child 的全局规则又会重新进模型。
    pub fn with_model_events<R>(&self, f: impl FnOnce(&[SessionEnvelope]) -> R) -> R {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match &inner.history_projection {
            None => f(&inner.events),
            Some(projection) => {
                let filtered: Vec<SessionEnvelope> = inner
                    .events
                    .iter()
                    .filter(|envelope| !projection.drops(envelope.seq))
                    .cloned()
                    .collect();
                f(&filtered)
            }
        }
    }

    /// 当前生效的历史投影（没有则为 None）。
    pub fn history_projection(&self) -> Option<history::HistoryProjection> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .history_projection
            .clone()
    }

    /// 会话日志文件的字节数(O(1) metadata)。
    ///
    /// 用磁盘日志体积作驻留内存的代理量做预算控制,避免为记账而遍历
    /// 全部事件;内存实际占用约为该值的 1.5~2.5 倍(结构体 + 分配器开销)。
    ///
    /// **注意**:这个值含已闭合轮次的 chunk(它们落盘后即从内存清扫),
    /// 与真实驻留量能差近十倍。做内存预算请用 [`Session::resident_bytes`]。
    pub fn log_bytes(&self) -> u64 {
        std::fs::metadata(&self.file).map(|m| m.len()).unwrap_or(0)
    }

    /// 驻留事件的内存字节估算(O(1))。
    ///
    /// 逐行累计 append 写入的字节数,`turn-end` 清扫 chunk 时同步扣减,
    /// 因此只反映"此刻真占着内存的事件"。冷会话恒为 0。
    /// 内存实际占用约为该值的 1.5~2.5 倍(结构体 + 分配器开销)。
    pub fn resident_bytes(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .resident_bytes
    }

    pub fn file(&self) -> &Path {
        &self.file
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

/// The sessions root: `<home>/sessions` + 内存索引。
pub struct SessionStore {
    root: PathBuf,
    index: Mutex<std::collections::HashMap<String, IndexEntry>>,
    /// 活跃(已加载)会话的弱引用:list 优先从内存读摘要,
    /// 不依赖文件落盘状态,首条用户消息即刻可见。
    active: Mutex<std::collections::HashMap<String, std::sync::Weak<Session>>>,
    /// 上次对目录做完整校验的 epoch ms;列表高频刷新时避免 O(会话数) stat。
    last_scan_ms: Mutex<u64>,
    /// 分页增量索引(见 `store::PageIndex`):只存展示事件的行偏移、
    /// 锚点与统计,不存正文;LRU 上限见 `PAGE_INDEX_MAX_ENTRIES`。
    page_indexes: Mutex<store::PageIndexCache>,
}

fn validate_id(id: &str) -> Result<&str, SessionError> {
    let valid = !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    if valid {
        Ok(id)
    } else {
        Err(SessionError::InvalidId(id.to_string()))
    }
}
