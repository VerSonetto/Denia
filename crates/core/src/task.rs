//! 任务账本与"有证据的完成判定"的领域词汇:任务身份、revision、要求来源、
//! 事实 / 假设 / 失败尝试 / 未决问题、变更范围、证据与验证结论。
//!
//! ## 为什么是这些形状
//!
//! - **状态只能折出来**。本模块只定义事件负载([`TaskOp`])与折叠后的快照
//!   ([`TaskState`]);当前状态与结局由 `denia-session::task_projection` 从
//!   耐久事件折叠(与 [`crate::session::GoalOp`] / [`crate::session::apply_goal_op`]
//!   同一套路)。没有"直接改状态"的入口,也就没有第二份会漂移的真值。
//! - **revision 是身份,不是可以重数的序号**。[`RevisionId`] 由宿主从不会
//!   复用的来源分配(如 uuid),rewind 截断日志后再推进必须换新 id:旧 revision
//!   上的验证结论绑在旧 id 上,新分支不可能继承它。折叠只读取事件里的 id,
//!   从不自己造一个。
//! - **模型与宿主的分工写进类型**。`Amend` / `SetRemaining` / `Block` 是模型
//!   可声明的意图;`RecordEvidence` / `RecordValidation` 是宿主执行链的产物。
//!   [`ValidationResult`] 字段私有、只能由 [`ValidationResult::from_check_runs`]
//!   构造,结论由检查命令的退出码算出 —— 类型层没有"直接写 verified"的位置。
//!   (工具面"谁才能写 `RecordValidation`"由后续切片的工具注册与权限收口。)
//! - **事实必须能追溯**。给不出引用、或引用的事件在日志里核不到的事实主张,
//!   折叠时降级为假设,而不是记成事实。
//! - **有界**。所有集合都有上限(`MAX_*`):超限保留最新的,丢弃条数记进
//!   [`LedgerOverflow`],不静默蒸发。取舍是任务状态会随快照与上下文注入反复
//!   出现,所以这里只放结论性短句与引用(上限 [`MAX_CLAIM_CHARS`]);证据正文与
//!   命令输出留在会话日志和产物文件里。
//! - **不持久化模型内部思维链**。本模块没有任何字段承载推理全文。

use serde::{Deserialize, Serialize};

/// 单条断言文本的字符上限。任务状态要随快照与上下文注入反复出现,长正文是
/// 纯成本;完整证据留在会话日志与产物文件里,这里只放结论性短句。
pub const MAX_CLAIM_CHARS: usize = 400;
/// 证据引用条数上限。
pub const MAX_EVIDENCE: usize = 64;
/// 验证结论保留条数上限(保留最新的;当前结论只看绑在当前 revision 上的最新
/// 一条,更老的只作审计)。
pub const MAX_VALIDATIONS: usize = 16;
/// 要求 / 验收项上限(跨 revision 保留原始来源引用,不允许模型洗掉用户约束)。
pub const MAX_REQUIREMENTS: usize = 32;
pub const MAX_FACTS: usize = 64;
pub const MAX_ASSUMPTIONS: usize = 32;
pub const MAX_FAILED_ATTEMPTS: usize = 32;
pub const MAX_OPEN_QUESTIONS: usize = 32;
/// 待完成工作条目上限(整表替换,与 `todo-write` 同策)。
pub const MAX_REMAINING: usize = 32;
/// 变更范围记录条数上限。
pub const MAX_CHANGES: usize = 32;
/// revision 历史条数上限。
pub const MAX_REVISIONS: usize = 16;

/// 初版 revision 的固定理由。保留这个字段是为了让历史记录形状统一 —— 没有
/// "上一版"可以解释。
pub const OPEN_REVISION_REASON: &str = "task-open";

/// 截断到 [`MAX_CLAIM_CHARS`] 个字符;超长时补省略标记(读的人能看出这不是
/// 原文全文)。按字符而不是字节切,不产生半个汉字。
pub fn truncate_claim(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_CLAIM_CHARS {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(MAX_CLAIM_CHARS).collect();
    format!("{cut}…")
}

/// 稳定任务 id:宿主分配(如 `task-<uuid>`),不随 rewind / 分支改变;账本与
/// 证据引用只认它,不认会被重编号的日志序号。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct TaskId(String);

impl TaskId {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 任务 revision 的身份。**宿主分配,不可复用**:rewind 截断日志后再推进时
/// 必须给新的 id,否则旧分支的验证结论会被当成新版本的结论继承下来。
///
/// 折叠端另有一道防线(见 `denia-session::task_projection`):换版事件里的 id
/// 若在历史里出现过、或在宿主声明的"已报废"集合里,换版被拒且当前结论失效。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct RevisionId(String);

impl RevisionId {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 证据的稳定 id(宿主分配)。证据关联用它,不用会被 fork 重编号的 seq。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct EvidenceId(String);

impl EvidenceId {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 要求 / 验收项的稳定 id(宿主分配):验证结论用它声明"覆盖了哪些验收项"。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct RequirementId(String);

impl RequirementId {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 断言(事实 / 假设 / 失败尝试 / 未决问题)的稳定 id(宿主分配):
/// `ResolveQuestion` 等后续操作按它定位条目。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct NoteId(String);

impl NoteId {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 用户要求的来源引用:指向产生这条要求的**事件身份**,而不是复制一段文本
/// 冒充出处。
///
/// 四个字段各有分工:`session` + `time_ms` 是跨 fork 重编号依然稳定的身份
/// (分支种子回放保留源事件时间戳);`seq` 是记录时刻的日志坐标,便于回到原话
/// 所在的事件;`quote` 是有界短摘录,给人核对引用是否仍指向他说的那句话 ——
/// 它**不是**事实本身。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SourceRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    pub session: Option<String>,
    /// 记录时刻该事件的日志序号(reference 用,不是身份)。
    #[serde(default)]
    pub seq: u64,
    /// 该事件的落盘时间(epoch ms):跨 fork 重编号仍稳定的身份。
    #[serde(default)]
    pub time_ms: u64,
    /// 有界短摘录;仅供人核对,不作为事实依据。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    pub quote: Option<String>,
}

/// 要求 / 验收项的种类。分开记是因为完成裁决只看 [`RequirementKind::Acceptance`]:
/// 目标文本变了不等于验收标准变了。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "kebab-case")]
pub enum RequirementKind {
    /// 目标(用户要达成什么)。
    Goal,
    /// 任务类型/性质(修 bug、加功能、只读分析……)。
    TaskType,
    /// 约束(不能动什么、必须遵守什么)。
    Constraint,
    /// 验收项(完成判定要逐项对上的那一类)。
    Acceptance,
}

/// 事件载荷:声明一条要求 / 验收项(带来源引用)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct RequirementClaim {
    pub id: RequirementId,
    pub kind: RequirementKind,
    pub text: String,
    pub source: SourceRef,
}

/// 折叠后的要求 / 验收项。保留 `revision`:旧版要求连同原始来源引用一起留在
/// 账上,模型不能靠"重新声明"把用户约束洗掉。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Requirement {
    pub id: RequirementId,
    pub revision: RevisionId,
    pub kind: RequirementKind,
    pub text: String,
    pub source: SourceRef,
    pub declared_at: u64,
}

/// 一版 revision 的记录(审计:哪一版存在过、为什么换版)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct RevisionRecord {
    pub id: RevisionId,
    /// 展示序号(第几版)。只是界面用的计数器,不做身份:rewind 之后序号会从头
    /// 数,而 id 不会。
    pub ordinal: u32,
    /// 换版原因(解释"为什么旧验证不算数")。
    pub reason: String,
    pub at: u64,
}

/// 事实主张的引用:要么指向用户原话,要么指向宿主证据。
///
/// 折叠时会核对:引用的事件必须是日志里的非注入用户消息,引用的证据必须在
/// 账本上。核不到就降级为假设(而不是记成事实)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum FactCitation {
    /// 用户原话。
    User { reference: SourceRef },
    /// 宿主执行链产生的证据。
    Evidence { evidence: EvidenceId },
}

/// 事件载荷:一条断言的声明(模型可产生的意图)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum NoteClaim {
    /// 事实主张。必须带引用:核不到就降级为假设。
    Fact {
        id: NoteId,
        text: String,
        citation: FactCitation,
    },
    /// 明确标注的假设(与事实在类型上分开,读的人不必猜哪条是猜的)。
    Assumption {
        id: NoteId,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "bindings", ts(optional))]
        rationale: Option<String>,
    },
    /// 已失败的尝试(带着证据更好,没证据也能记 —— 它不是事实主张)。
    FailedAttempt {
        id: NoteId,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "bindings", ts(optional))]
        evidence: Option<EvidenceId>,
    },
    /// 未决问题。
    OpenQuestion { id: NoteId, text: String },
}

/// 折叠后的事实:引用一定是核对过的(用户原话能在日志里找到,或证据 id 在
/// 账本上)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Fact {
    pub id: NoteId,
    pub revision: RevisionId,
    pub text: String,
    pub citation: FactCitation,
    pub at: u64,
}

/// 折叠后的假设(明确标注,不与事实混)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Assumption {
    pub id: NoteId,
    pub revision: RevisionId,
    pub text: String,
    /// 为什么这么猜;来源核不实时这里记明原因(如"引用的用户消息不在日志里")。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    pub rationale: Option<String>,
    pub at: u64,
}

/// 折叠后的失败尝试。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct FailedAttempt {
    pub id: NoteId,
    pub revision: RevisionId,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    pub evidence: Option<EvidenceId>,
    pub at: u64,
}

/// 折叠后的未决问题。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct OpenQuestion {
    pub id: NoteId,
    pub revision: RevisionId,
    pub text: String,
    pub at: u64,
}

/// 一个文件的内容指纹。`digest` 由宿主计算(核心不引入哈希依赖):它是"验证
/// 时看到的那份字节"的凭据。要求覆盖相关脏文件、未跟踪输入或显式输入清单,
/// **不能**只绑 Git HEAD —— HEAD 看不见的改动同样必须让旧验证失效。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct FileFingerprint {
    pub path: String,
    pub digest: String,
}

/// 证据的来路(宿主执行链;`call_id` 是日志内稳定身份)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum EvidenceSource {
    /// 一次工具调用的结果。
    ToolCall { call_id: String, tool: String },
    /// 一次检查命令的运行。
    Check { command: String, exit_code: i32 },
    /// 一次文件读取 / 目录列举。
    FileRead { path: String },
}

/// 一条证据(宿主执行链的产物)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Evidence {
    pub id: EvidenceId,
    /// 产生这条证据时所属的 revision。换版后旧证据仍可被引用(它是材料),
    /// 但**不能**单独支撑新 revision 的验收结论 —— 那是 [`ValidationResult`] 的
    /// 职责。
    pub revision: RevisionId,
    pub source: EvidenceSource,
    /// 这条证据观察到的工作区状态。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fingerprints: Vec<FileFingerprint>,
    /// 有界摘要(超出 [`MAX_CLAIM_CHARS`] 由折叠截断);正文留在日志/产物里。
    pub summary: String,
    /// 产生它的会话。跨会话 seed(父 / 分支)带进来的记录靠它被识别为外来。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    pub origin_session: Option<String>,
    pub at: u64,
}

/// 一次检查命令的运行记录(宿主执行链填写)。命令与工作目录都要冻结,否则
/// "通过"指的是哪次运行无法复核。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct CheckRun {
    pub command: String,
    /// 进程退出码;`None` = 没跑起来(启动失败 / 超时 / 取消),按未通过计。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    pub exit_code: Option<i32>,
    /// 通过所需的退出码;缺省 0(非零即失败)。
    #[serde(default)]
    pub expect_exit_code: i32,
    /// 运行时的工作目录。
    #[serde(default)]
    pub workdir: String,
    /// 运行开始时冻结的输入指纹。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fingerprints: Vec<FileFingerprint>,
}

impl CheckRun {
    /// 这条检查是否通过。退出码缺失(没跑起来)与不匹配都算未通过。
    pub fn passed(&self) -> bool {
        self.exit_code == Some(self.expect_exit_code)
    }
}

/// 验证结论:通过 / 失败。它由检查运行的退出码算出,不是可以填的字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "kebab-case")]
pub enum ValidationVerdict {
    Passed,
    Failed,
}

/// 一次验证结论:绑定"验证的是哪个 revision + 哪些文件内容指纹"。
///
/// 字段私有是刻意的 —— 除 [`ValidationResult::from_check_runs`] 没有构造路径,
/// 而结论由检查运行的退出码算出,所以类型层不存在"直接写 verified"的位置。
/// 日志反序列化(serde)是回放路径;写入侧仍必须走构造函数(没有检查运行就
/// 构造不出结论)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ValidationResult {
    revision: RevisionId,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    runs: Vec<CheckRun>,
    /// 本次验证声明覆盖的验收项。检查通过只代表它声明的范围。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    covered: Vec<RequirementId>,
    /// 结论产生时的会话;与折叠方会话不一致时只作审计,不进裁决。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    origin_session: Option<String>,
    #[serde(default)]
    at: u64,
}

impl ValidationResult {
    /// 唯一构造入口(宿主执行链):至少要有一次真正跑过的检查,否则不构成
    /// 验证结论 —— 返回 `None`,调用方不得把它记成验证(没有"空报告即通过")。
    pub fn from_check_runs(
        revision: RevisionId,
        runs: Vec<CheckRun>,
        covered: Vec<RequirementId>,
        origin_session: Option<String>,
        at: u64,
    ) -> Option<Self> {
        if runs.is_empty() {
            return None;
        }
        Some(Self {
            revision,
            runs,
            covered,
            origin_session,
            at,
        })
    }

    pub fn revision(&self) -> &RevisionId {
        &self.revision
    }

    pub fn runs(&self) -> &[CheckRun] {
        &self.runs
    }

    pub fn covered(&self) -> &[RequirementId] {
        &self.covered
    }

    pub fn origin_session(&self) -> Option<&str> {
        self.origin_session.as_deref()
    }

    pub fn at(&self) -> u64 {
        self.at
    }

    /// 本次验证的结论。`None` = 报告里没有任何检查运行(回放出来的畸形记录),
    /// 不构成结论 —— 折叠据此跳过它,而不是当成通过。
    pub fn verdict(&self) -> Option<ValidationVerdict> {
        if self.runs.is_empty() {
            return None;
        }
        Some(if self.runs.iter().all(CheckRun::passed) {
            ValidationVerdict::Passed
        } else {
            ValidationVerdict::Failed
        })
    }

    /// 本次验证冻结的全部输入指纹(各次运行并集,同路径后写的覆盖先写的)。
    pub fn fingerprints(&self) -> Vec<FileFingerprint> {
        let mut merged: Vec<FileFingerprint> = Vec::new();
        for run in &self.runs {
            for fingerprint in &run.fingerprints {
                upsert_fingerprint(&mut merged, fingerprint);
            }
        }
        merged
    }
}

/// 一条变更范围记录(宿主在改完后写:改了哪些文件、改成了什么指纹)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct TaskChange {
    pub revision: RevisionId,
    pub summary: String,
    pub fingerprints: Vec<FileFingerprint>,
    pub at: u64,
}

/// 有界保存的丢弃计数:每个集合超上限时保留最新的,丢弃条数记在这里。
/// 上限造成的损失必须可见 —— "用户约束被悄悄删掉"正是要防的事。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct LedgerOverflow {
    #[serde(default)]
    pub requirements: u32,
    #[serde(default)]
    pub facts: u32,
    #[serde(default)]
    pub assumptions: u32,
    #[serde(default)]
    pub failed_attempts: u32,
    #[serde(default)]
    pub open_questions: u32,
    #[serde(default)]
    pub evidence: u32,
    #[serde(default)]
    pub validations: u32,
    #[serde(default)]
    pub changes: u32,
    #[serde(default)]
    pub remaining: u32,
    #[serde(default)]
    pub revisions: u32,
}

/// 任务的进行状态。终态只有两种:收口(`Closed`)或外部受阻(`Blocked`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "kebab-case")]
pub enum TaskStatus {
    /// 进行中。
    Active,
    /// 外部受阻(等用户、等外部系统、环境坏了)。不是"完成",也不允许被
    /// 当成完成。
    Blocked,
    /// 已收口。结局由证据裁决(见 [`TaskState::outcome`])。
    Closed,
}

/// 任务结局(与 `TurnEndReason::Completed` 的"本轮正常结束"是两件事)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "kebab-case")]
pub enum TaskOutcome {
    /// 未验证完成:收口了,但没有能支撑当前 revision 的有效验收证据。
    Unverified,
    /// 验收通过:当前 revision 的验收项全部被有效的通过证据覆盖。
    Passed,
    /// 验收失败:当前 revision 上有有效的失败结论。
    Failed,
    /// 外部受阻。
    Blocked,
}

impl TaskOutcome {
    /// 中文标签(模型侧注入与 UI 的统一口径)。
    pub fn label(self) -> &'static str {
        match self {
            Self::Unverified => "未验证完成",
            Self::Passed => "验收通过",
            Self::Failed => "验收失败",
            Self::Blocked => "外部受阻",
        }
    }
}

/// 当前 revision 拿不到可用结论的原因(诊断:说清"为什么不是 verified")。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum StaleReason {
    /// 当前 revision 上还没跑过任何验证。
    NeverRun,
    /// 只在别的 revision 上跑过:要求换了版,旧结论不自动继承。
    RevisionChanged,
    /// 相关文件在验证之后又被改过(指纹不再匹配)。
    InputsChanged { paths: Vec<String> },
    /// 有记录但没有可用的检查运行(空报告)。
    NoCheckRun,
    /// 结论来自别的会话(父 / 分支 seed 继承),不参与当前裁决。
    ForeignOrigin,
    /// 拒绝过一次复用旧 revision id 的换版:当前版本的身份不干净,结论一律
    /// 不可信(宁可判"未验证")。
    RevisionConflict,
}

/// 当前 revision 的验证状态(由 [`TaskState::verification`] 计算,不是存储字段)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum VerificationState {
    /// 拿不到可用结论。
    Unverified { reason: StaleReason },
    /// 有可用结论。`verdict` 只可能来自宿主写入的 [`ValidationResult`]。
    Verified {
        revision: RevisionId,
        verdict: ValidationVerdict,
        /// 这次结论冻结的输入指纹(可展示"验证的是这些字节")。
        fingerprints: Vec<FileFingerprint>,
        /// 它声明覆盖的验收项。
        covered: Vec<RequirementId>,
        at: u64,
    },
}

/// 折叠后的任务账本快照。
///
/// 所有字段都是**折叠的产物**;结论类信息不在字段里(见 [`Self::verification`] /
/// [`Self::outcome`]),因此重复序列化不会让结论与证据脱钩。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct TaskState {
    pub id: TaskId,
    /// 当前 revision(当前要求集的身份)。
    pub revision: RevisionId,
    /// 本账本所属会话(折叠时由作用域带入):把跨会话 seed 继承来的验证结论挡
    /// 在裁决之外 —— 子代理不继承父的验收结论。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    pub session: Option<String>,
    pub status: TaskStatus,
    pub goal: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    pub blocked_reason: Option<String>,
    /// 拒绝过复用旧 revision id 的换版:当前版本身份不干净,结论一律不可信,
    /// 直到出现合法的新 revision。rewind 之后宿主复用旧 id 的兜底。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub revision_conflict: bool,
    /// 被拒绝的换版次数(复用旧 id)。拒绝这件事本身也必须可见。
    #[serde(default)]
    pub revision_rejected: u32,
    /// 历次 revision(有界;首个是初版)。
    #[serde(default)]
    pub revisions: Vec<RevisionRecord>,
    /// 要求与验收项(跨 revision 保留原始来源引用)。
    #[serde(default)]
    pub requirements: Vec<Requirement>,
    #[serde(default)]
    pub facts: Vec<Fact>,
    #[serde(default)]
    pub assumptions: Vec<Assumption>,
    #[serde(default)]
    pub failed_attempts: Vec<FailedAttempt>,
    #[serde(default)]
    pub open_questions: Vec<OpenQuestion>,
    #[serde(default)]
    pub changes: Vec<TaskChange>,
    /// 待完成工作(整表替换,latest-wins)。
    #[serde(default)]
    pub remaining: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    #[serde(default)]
    pub validations: Vec<ValidationResult>,
    /// 当前生效的文件内容指纹基线(path → digest,后写的覆盖先写的)。
    /// 由变更范围事件维护;验证结论据此判断是否已失效。
    #[serde(default)]
    pub fingerprints: Vec<FileFingerprint>,
    #[serde(default)]
    pub overflow: LedgerOverflow,
    pub created_at: u64,
    pub updated_at: u64,
}

impl TaskState {
    /// 当前 revision 的验证状态。**纯计算**:没有任何字段能直接写"verified",
    /// 能写的只有宿主产生的 [`ValidationResult`],而结论由它的退出码算出。
    ///
    /// 采纳条件三条全部成立:绑定当前 revision、来源属于本会话(或双方都未
    /// 标注来源)、冻结的输入指纹没有被后续变更推翻。
    pub fn verification(&self) -> VerificationState {
        if self.revision_conflict {
            return VerificationState::Unverified {
                reason: StaleReason::RevisionConflict,
            };
        }
        let mut changed_paths: Vec<String> = Vec::new();
        let mut saw_matching_revision = false;
        let mut saw_foreign = false;
        let mut chosen: Option<&ValidationResult> = None;
        // 从新到旧找第一条可用的:最近一次对当前 revision 的验证说了算,
        // 不给"挑一条通过的"留口子。
        for result in self.validations.iter().rev() {
            if result.revision() != &self.revision {
                continue;
            }
            saw_matching_revision = true;
            if result.verdict().is_none() {
                continue;
            }
            if !self.accepts_origin(result.origin_session()) {
                saw_foreign = true;
                continue;
            }
            let changed = self.paths_changed_since(&result.fingerprints());
            if !changed.is_empty() {
                changed_paths.extend(changed);
                continue;
            }
            chosen = Some(result);
            break;
        }
        let Some(result) = chosen else {
            let reason = if !changed_paths.is_empty() {
                changed_paths.sort();
                changed_paths.dedup();
                StaleReason::InputsChanged {
                    paths: changed_paths,
                }
            } else if saw_foreign {
                StaleReason::ForeignOrigin
            } else if self
                .validations
                .iter()
                .any(|item| item.revision() != &self.revision)
            {
                StaleReason::RevisionChanged
            } else if saw_matching_revision {
                StaleReason::NoCheckRun
            } else {
                StaleReason::NeverRun
            };
            return VerificationState::Unverified { reason };
        };
        VerificationState::Verified {
            revision: self.revision.clone(),
            // 上面已确认 `verdict()` 有值。
            verdict: result.verdict().unwrap_or(ValidationVerdict::Failed),
            fingerprints: result.fingerprints(),
            covered: result.covered().to_vec(),
            at: result.at(),
        }
    }

    /// 任务结局;`None` = 仍在进行(尚未收口)。
    ///
    /// 收口时的裁决规则:
    /// - 当前 revision 上有有效的失败结论 → 验收失败;
    /// - 有有效通过结论**且**当前 revision 的验收项全部被它声明覆盖 → 验收通过;
    /// - 其余一律"未验证完成"(没跑过、只跑在旧 revision 上、跑完又改了文件、
    ///   结论来自别的会话、或者根本没有验收定义)。
    pub fn outcome(&self) -> Option<TaskOutcome> {
        match self.status {
            TaskStatus::Active => None,
            TaskStatus::Blocked => Some(TaskOutcome::Blocked),
            TaskStatus::Closed => match self.verification() {
                VerificationState::Unverified { .. } => Some(TaskOutcome::Unverified),
                VerificationState::Verified {
                    verdict: ValidationVerdict::Failed,
                    ..
                } => Some(TaskOutcome::Failed),
                VerificationState::Verified {
                    verdict: ValidationVerdict::Passed,
                    covered,
                    ..
                } => {
                    // 检查通过只代表它声明的范围:验收项必须全被覆盖。
                    let acceptance: Vec<&RequirementId> = self
                        .requirements
                        .iter()
                        .filter(|item| {
                            item.revision == self.revision
                                && item.kind == RequirementKind::Acceptance
                        })
                        .map(|item| &item.id)
                        .collect();
                    if !acceptance.is_empty() && acceptance.iter().all(|id| covered.contains(id)) {
                        Some(TaskOutcome::Passed)
                    } else {
                        Some(TaskOutcome::Unverified)
                    }
                }
            },
        }
    }

    /// 当前 revision 的要求 / 验收项。
    pub fn current_requirements(&self) -> impl Iterator<Item = &Requirement> {
        let revision = &self.revision;
        self.requirements
            .iter()
            .filter(move |item| &item.revision == revision)
    }

    /// 指纹基线里某个路径的当前摘要。
    pub fn fingerprint_of(&self, path: &str) -> Option<&str> {
        self.fingerprints
            .iter()
            .find(|item| item.path == path)
            .map(|item| item.digest.as_str())
    }

    /// 冻结指纹里哪些路径已经被后续变更改掉了(摘要不同即失效;基线里没有
    /// 该路径 = 没有记录显示它被改过,不算失效)。
    pub fn paths_changed_since(&self, frozen: &[FileFingerprint]) -> Vec<String> {
        frozen
            .iter()
            .filter(|item| match self.fingerprint_of(&item.path) {
                Some(current) => current != item.digest,
                None => false,
            })
            .map(|item| item.path.clone())
            .collect()
    }

    /// 记录的来源是否属于本账本:双方都未标注来源视为本地(旧日志与未标注的
    /// 宿主写入),任一侧有值时必须相等。保守:本会话已知而记录未标注来源时
    /// 不认(无法确认它属于本会话)。
    pub fn accepts_origin(&self, origin: Option<&str>) -> bool {
        match (self.session.as_deref(), origin) {
            (Some(local), Some(origin)) => local == origin,
            (None, None) => true,
            _ => false,
        }
    }

    /// 已经用过的全部 revision id(折叠端判"id 复用"用)。
    pub fn used_revisions(&self) -> impl Iterator<Item = &RevisionId> {
        self.revisions.iter().map(|item| &item.id)
    }
}

/// 按路径更新指纹(后写的覆盖先写的)。
pub fn upsert_fingerprint(fingerprints: &mut Vec<FileFingerprint>, fingerprint: &FileFingerprint) {
    if let Some(existing) = fingerprints
        .iter_mut()
        .find(|item| item.path == fingerprint.path)
    {
        existing.digest = fingerprint.digest.clone();
        return;
    }
    fingerprints.push(fingerprint.clone());
}

/// 任务账本的一次操作(事件负载)。
///
/// 与 [`crate::session::GoalOp`] 同一设计:操作即意图,状态由纯折叠得出。
/// `Open` / `Revise` / `Amend` / `ResolveQuestion` / `SetRemaining` / `Block` /
/// `Unblock` / `Close` 是模型可声明的意图(工具面还会再收窄);
/// `RecordEvidence` / `RecordValidation` 是宿主执行链的产物,模型无权产生。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TaskOp {
    /// 创建任务。`revision` 必须是新的 id;任务已存在且未收口时忽略(先收口或
    /// 换版,不能偷偷换身份),已收口时视为用户重新出发、替换旧任务。
    Open {
        task_id: TaskId,
        revision: RevisionId,
        goal: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        requirements: Vec<RequirementClaim>,
    },
    /// 换版:用户要求 / 验收项变了。新 id 必须是新的(见 [`RevisionId`]);
    /// 旧要求连同原始来源引用留在账上。
    Revise {
        revision: RevisionId,
        /// 为什么换版(用户加了约束 / 改了验收 / 上一版理解错了……)。
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "bindings", ts(optional))]
        goal: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        requirements: Vec<RequirementClaim>,
    },
    /// 追加断言:事实 / 假设 / 已失败的尝试 / 未决问题。
    Amend {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        notes: Vec<NoteClaim>,
    },
    /// 关闭一个未决问题(按稳定 id 定位)。
    ResolveQuestion { id: NoteId },
    /// 待完成工作整表替换(与 `todo-write` 同策:latest-wins,条目无需身份)。
    SetRemaining {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        items: Vec<String>,
    },
    /// 记录变更范围与改后的文件指纹(宿主写)。它同时更新指纹基线,因此会让
    /// 绑定旧指纹的验证结论失效。
    RecordChange {
        summary: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        fingerprints: Vec<FileFingerprint>,
    },
    /// 记录一条执行证据(宿主执行链写)。
    RecordEvidence { evidence: Evidence },
    /// 记录一次验证结论(宿主执行链写;模型不能伪造执行结果,也不能直接设置
    /// verified —— 只能跑检查,由宿主把运行事实写成 [`ValidationResult`])。
    RecordValidation { result: ValidationResult },
    /// 标记外部受阻(等用户 / 等外部系统 / 环境不可用)。
    Block {
        #[serde(default)]
        reason: String,
    },
    /// 解除受阻。
    Unblock,
    /// 收口。结局由折叠裁决,不由本操作指定 —— 写不出"验收通过"。
    Close,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(path: &str, digest: &str) -> FileFingerprint {
        FileFingerprint {
            path: path.into(),
            digest: digest.into(),
        }
    }

    fn check_run(exit_code: Option<i32>) -> CheckRun {
        CheckRun {
            command: "cargo test".into(),
            exit_code,
            expect_exit_code: 0,
            workdir: "/work".into(),
            fingerprints: vec![fingerprint("a.rs", "d1")],
        }
    }

    fn validation(
        revision: &str,
        exit_code: Option<i32>,
        covered: Vec<RequirementId>,
        origin: Option<&str>,
    ) -> ValidationResult {
        ValidationResult::from_check_runs(
            RevisionId::new(revision),
            vec![check_run(exit_code)],
            covered,
            origin.map(str::to_string),
            2_000,
        )
        .expect("有检查运行才构成结论")
    }

    /// 一个已收口的任务:一条验收项 + 一条基线指纹。
    fn closed_state(revision: &str) -> TaskState {
        TaskState {
            id: TaskId::new("task-1"),
            revision: RevisionId::new(revision),
            session: None,
            status: TaskStatus::Closed,
            goal: "修好解析器".into(),
            blocked_reason: None,
            revision_conflict: false,
            revision_rejected: 0,
            revisions: Vec::new(),
            requirements: vec![Requirement {
                id: RequirementId::new("req-1"),
                revision: RevisionId::new(revision),
                kind: RequirementKind::Acceptance,
                text: "cargo test 全绿".into(),
                source: SourceRef {
                    session: None,
                    seq: 1,
                    time_ms: 1_000,
                    quote: Some("跑通测试再说完事".into()),
                },
                declared_at: 1_000,
            }],
            facts: Vec::new(),
            assumptions: Vec::new(),
            failed_attempts: Vec::new(),
            open_questions: Vec::new(),
            changes: Vec::new(),
            remaining: Vec::new(),
            evidence: Vec::new(),
            validations: Vec::new(),
            fingerprints: vec![fingerprint("a.rs", "d1")],
            overflow: LedgerOverflow::default(),
            created_at: 1_000,
            updated_at: 2_000,
        }
    }

    #[test]
    fn verdict_follows_exit_code_and_empty_report_is_no_conclusion() {
        // 结论是算出来的,不是可以填的字段:退出码 0 → 通过,非零 / 没跑起来
        // (None)→ 失败。
        assert_eq!(
            validation("rev-1", Some(0), Vec::new(), None).verdict(),
            Some(ValidationVerdict::Passed)
        );
        assert_eq!(
            validation("rev-1", Some(1), Vec::new(), None).verdict(),
            Some(ValidationVerdict::Failed)
        );
        assert_eq!(
            validation("rev-1", None, Vec::new(), None).verdict(),
            Some(ValidationVerdict::Failed)
        );
        // 没有检查运行的报告不构成结论:构造入口直接回 None。
        assert_eq!(
            ValidationResult::from_check_runs(
                RevisionId::new("rev-1"),
                Vec::new(),
                Vec::new(),
                None,
                1
            ),
            None
        );
    }

    #[test]
    fn outcome_needs_covering_evidence_for_the_current_revision() {
        let mut state = closed_state("rev-1");
        // 没跑过任何检查:收口也只能是"未验证完成"。
        assert_eq!(state.outcome(), Some(TaskOutcome::Unverified));
        // 跑了检查但没有验收定义可对标(空覆盖):仍然不是"验收通过"。
        state.validations = vec![validation("rev-1", Some(0), Vec::new(), None)];
        assert_eq!(state.outcome(), Some(TaskOutcome::Unverified));
        // 验收项被声明覆盖 → 验收通过。
        state.validations = vec![validation(
            "rev-1",
            Some(0),
            vec![RequirementId::new("req-1")],
            None,
        )];
        assert_eq!(state.outcome(), Some(TaskOutcome::Passed));
        // 有失败结论 → 验收失败(即使上一条曾经通过)。
        state
            .validations
            .push(validation("rev-1", Some(1), Vec::new(), None));
        assert_eq!(state.outcome(), Some(TaskOutcome::Failed));
        // 只在旧 revision 上跑过:新版本不继承旧结论。
        let mut revised = closed_state("rev-2");
        revised.validations = vec![validation(
            "rev-1",
            Some(0),
            vec![RequirementId::new("req-1")],
            None,
        )];
        assert_eq!(revised.outcome(), Some(TaskOutcome::Unverified));
        assert_eq!(
            revised.verification(),
            VerificationState::Unverified {
                reason: StaleReason::RevisionChanged
            }
        );
        // 进行中的任务没有结局;外部受阻就是外部受阻。
        revised.status = TaskStatus::Active;
        assert_eq!(revised.outcome(), None);
        revised.status = TaskStatus::Blocked;
        assert_eq!(revised.outcome(), Some(TaskOutcome::Blocked));
    }

    #[test]
    fn change_after_validation_invalidates_it() {
        let mut state = closed_state("rev-1");
        state.validations = vec![validation(
            "rev-1",
            Some(0),
            vec![RequirementId::new("req-1")],
            None,
        )];
        assert_eq!(state.outcome(), Some(TaskOutcome::Passed));
        // 验证看过的是 d1;后来文件被改成 d2 —— 旧结论立刻失效。
        upsert_fingerprint(&mut state.fingerprints, &fingerprint("a.rs", "d2"));
        assert_eq!(
            state.verification(),
            VerificationState::Unverified {
                reason: StaleReason::InputsChanged {
                    paths: vec!["a.rs".into()]
                }
            }
        );
        assert_eq!(state.outcome(), Some(TaskOutcome::Unverified));
    }

    #[test]
    fn foreign_origin_and_revision_conflict_block_conclusions() {
        // 子代理不继承父的验收结论:来源与本会话不一致的结论不参与裁决。
        let mut child = closed_state("rev-1");
        child.session = Some("child".into());
        child.validations = vec![validation(
            "rev-1",
            Some(0),
            vec![RequirementId::new("req-1")],
            Some("parent"),
        )];
        assert_eq!(
            child.verification(),
            VerificationState::Unverified {
                reason: StaleReason::ForeignOrigin
            }
        );
        assert_eq!(child.outcome(), Some(TaskOutcome::Unverified));
        // 拒绝过复用旧 revision id 的换版:当前版本身份不干净,一律不可信。
        let mut conflicted = closed_state("rev-1");
        conflicted.validations = vec![validation(
            "rev-1",
            Some(0),
            vec![RequirementId::new("req-1")],
            None,
        )];
        conflicted.revision_conflict = true;
        assert_eq!(
            conflicted.verification(),
            VerificationState::Unverified {
                reason: StaleReason::RevisionConflict
            }
        );
    }

    #[test]
    fn truncate_claim_caps_chars_and_marks_the_cut() {
        let long = "汉".repeat(MAX_CLAIM_CHARS + 50);
        let cut = truncate_claim(&long);
        assert_eq!(cut.chars().count(), MAX_CLAIM_CHARS + 1);
        assert!(cut.ends_with('…'));
        let short = truncate_claim("  短句  ");
        assert_eq!(short, "短句", "两侧空白不落账");
    }
}
