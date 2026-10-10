//! 任务账本的工具面:`get_task` / `update_task` / `run_checks`。
//!
//! 三件事的分工写在类型与参数里,而不是靠纪律段口头约定:
//!
//! - `get_task` 只读:把折叠出的账本与结局读给模型(状态、结局、当前
//!   revision、验收项与覆盖情况、待完成工作、未决问题、验证结论与失效原因);
//! - `update_task` 是**模型声明意图**的唯一入口,只接受声明性操作
//!   (`open` / `revise` / `amend` / `resolve-question` / `set-remaining` /
//!   `block` / `unblock` / `close`)。它的参数里**没有**任何位置能写出
//!   `RecordEvidence` 或 `RecordValidation` —— 那两条是宿主执行链的产物,
//!   模型声称的"事实"只能走 `amend` 的 `fact` 通道,而引用核不到时由折叠层
//!   降级为假设。工具层因此不存在"直接写 verified"的入口;
//! - `run_checks` 由宿主真的跑命令:命令、期望退出码、工作目录、超时由模型
//!   给出,退出码与冻结的输入指纹由宿主填写,结论由退出码算出(见
//!   [`ValidationResult::from_check_runs`]),落成 `RecordValidation` 事件。
//!
//! ## 状态的真值只有一处
//!
//! 工具层不读会话日志、不缓存账本:它通过 [`TaskLedgerHost`] 向宿主问当前
//! 折叠快照与 id,自己只做"把模型的声明翻译成 [`TaskOp`]"。所以工具层既不
//! 依赖会话存储 crate,也不会造出第二份会漂移的状态。
//!
//! ## 检查的输入快照
//!
//! `run_checks` 冻结的输入快照**只覆盖工作目录与文件内容指纹**(复用
//! [`crate::read_state::FileStamp`] 的 `(mtime, size, digest)` 取样口径)。
//! 仓库里没有环境变量快照这个概念:依赖环境变量的检查不受指纹保护,改了
//! 环境而没改文件时旧结论不会被自动推翻 —— 这是已知边界,不假装有。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use denia_core::session::SessionEvent;
use denia_core::task::{
    CheckRun, FileFingerprint, MAX_CLAIM_CHARS, MAX_REMAINING, NoteClaim, NoteId, RequirementClaim,
    RequirementId, RequirementKind, RevisionId, SourceRef, StaleReason, TaskId, TaskOp, TaskState,
    TaskStatus, ValidationResult, ValidationVerdict, VerificationState,
};
use denia_core::tool::ToolSchema;
use serde::Deserialize;

use crate::read_state::FileStamp;
use crate::shell::{self, CommandSpec, DEFAULT_TIMEOUT_MS, MAX_TIMEOUT_MS, StreamSpec};
use crate::support::{parse_tool_args, tool_error};
use crate::{Tool, ToolContext, ToolOutput, end_reason};

/// 检查默认冻结的输入文件条数上限(省缺输入快照取本会话读过的文件)。
const MAX_INPUTS: usize = 32;
/// 结果里最多列出的输入路径条数(完整清单在结构化摘要里)。
const MAX_LISTED_INPUTS: usize = 8;

/// 宿主侧的账本接口:读当前折叠快照、分配不可复用的 id、核对用户原话。
///
/// 由宿主(会话驱动器)实现并挂到 [`ToolContext::task_ledger`]。工具层只经过
/// 它接触会话,因此"账本长什么样"与"id 从哪来"都留在宿主那一侧。
pub trait TaskLedgerHost: Send + Sync {
    /// 当前折叠出的账本;`None` = 日志里还没有任务事件。
    fn state(&self) -> Option<TaskState>;

    /// 分配任务 id(宿主分配,不随 rewind / 分支改变)。
    fn new_task_id(&self) -> TaskId;

    /// 分配 revision id。**必须来自不会复用的来源**:换版沿用旧 id 会让旧分支
    /// 的验证结论被当成当前版本的结论继承下来(见 [`RevisionId`] 的契约)。
    fn new_revision_id(&self) -> RevisionId;

    /// 分配断言 id(未决问题按它被关闭)。
    fn new_note_id(&self) -> NoteId;

    /// 分配要求 / 验收项 id(验证结论按它声明覆盖范围)。
    fn new_requirement_id(&self) -> RequirementId;

    /// 在会话日志里核对一段用户原话:找到包含它的**非注入**用户消息,返回
    /// 指向该事件的稳定引用;核不到返回 `None`。
    ///
    /// 引用的真实性只能由看到日志的一侧判定:模型给不出可核对的原话时,
    /// 要求会被拒绝、事实会降级为假设,而不是"复制一份文本就算出处"。
    fn user_reference(&self, quote: &str) -> Option<SourceRef>;
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 取账本宿主;没有会话归属的调用读不到账本,显式拒绝而不是静默作用于别处。
fn require_host(ctx: &ToolContext) -> Result<&Arc<dyn TaskLedgerHost>, ToolOutput> {
    ctx.task_ledger.as_ref().ok_or_else(|| {
        tool_error(
            "本次调用读不到会话任务账本(没有归属的代理会话)",
            "这三个工具只能在代理会话内使用",
        )
    })
}

/// 取会话事件通道;声明与结论都要落盘才算数,写不进去就拒绝。
fn require_sink(ctx: &ToolContext) -> Result<&crate::SessionEventSink, ToolOutput> {
    ctx.emit_event.as_ref().ok_or_else(|| {
        tool_error(
            "本次调用没有会话事件通道,账本操作无法落盘",
            "这三个工具只能在代理会话内使用",
        )
    })
}

/// 结论性短句:去空白、拒绝空文本与超长文本。
///
/// 超长**拒绝**而不是静默截断:账本只放结论性短句,完整内容留在回复与产物里;
/// 让折叠层替模型砍掉半句话,读的人分不清那是全文还是残句。
fn claim(raw: &str, field: &str) -> Result<String, String> {
    let text = raw.trim();
    if text.is_empty() {
        return Err(format!("{field}不能为空"));
    }
    if text.chars().count() > MAX_CLAIM_CHARS {
        return Err(format!(
            "{field}超过 {MAX_CLAIM_CHARS} 字符上限:账本只放结论性短句,完整内容写进回复或产物"
        ));
    }
    Ok(text.to_string())
}

// ---------------------------------------------------------------------------
// get_task
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GetTaskArgs {}

/// `get_task`:读取当前会话任务账本的状态与结局。
#[derive(Default)]
pub struct GetTaskTool;

impl GetTaskTool {
    fn build_schema() -> ToolSchema {
        ToolSchema {
            name: "get_task".to_string(),
            description: "读取当前会话任务账本:状态、结局、当前 revision、验收项及其覆盖情况、待完成工作、未决问题、验证结论(没结论时说清为什么)。会话还没有任务时返回明确说明。"
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }
}

#[async_trait]
impl Tool for GetTaskTool {
    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(Self::build_schema)
    }

    async fn execute(&self, arguments: &str, ctx: &ToolContext) -> ToolOutput {
        // 宽容解析:空参数/空对象都放行(本工具无参数)。
        if !arguments.trim().is_empty() {
            let _: GetTaskArgs = match parse_tool_args(arguments) {
                Ok(args) => args,
                Err(error) => {
                    return tool_error(format!("参数解析失败:{error}"), "get_task 不接受任何参数");
                }
            };
        }
        let host = match require_host(ctx) {
            Ok(host) => host,
            Err(output) => return output,
        };
        match host.state() {
            Some(state) => ToolOutput::text(render_task_state(&state)),
            None => ToolOutput::text(
                "当前会话还没有任务账本。用 update_task 的 open 声明目标与验收项后再继续。"
                    .to_string(),
            ),
        }
    }
}

/// 把账本渲染成模型可读的紧凑文本 + 一段结构化摘要。
///
/// 文本给人读(也顺带给模型读),末尾的 JSON 给机器读:两者同源,不会各说
/// 一套。id(验收项、未决问题、证据)一律带上,它们是模型后续操作要用的坐标。
fn render_task_state(state: &TaskState) -> String {
    let verification = state.verification();
    let covered: Vec<&RequirementId> = match &verification {
        VerificationState::Verified { covered, .. } => covered.iter().collect(),
        VerificationState::Unverified { .. } => Vec::new(),
    };
    let mut out = String::new();
    out.push_str(&format!("任务账本:{}\n", state.id.as_str()));
    out.push_str(&format!(
        "当前 revision:{}(第 {} 版)\n",
        state.revision.as_str(),
        state.revisions.len().max(1)
    ));
    match state.status {
        TaskStatus::Active => out.push_str("状态:进行中\n"),
        TaskStatus::Blocked => {
            let reason = state
                .blocked_reason
                .as_deref()
                .map(|reason| format!("(原因:{reason})"))
                .unwrap_or_default();
            out.push_str(&format!("状态:外部受阻{reason}\n"));
        }
        TaskStatus::Closed => out.push_str("状态:已收口\n"),
    }
    match state.outcome() {
        Some(outcome) => out.push_str(&format!("结局:{}\n", outcome.label())),
        None => out.push_str("结局:未定(任务仍在进行,尚未收口)\n"),
    }
    if state.revision_conflict {
        out.push_str(&format!(
            "换版冲突:{} 次换版被拒(复用了旧 revision id);当前版本身份不可信,结论一律作废\n",
            state.revision_rejected
        ));
    }
    out.push_str(&format!("目标:{}\n", state.goal));
    out.push_str(&format!("验证:{}\n", describe_verification(&verification)));

    let current: Vec<&denia_core::task::Requirement> = state.current_requirements().collect();
    let acceptance: Vec<&&denia_core::task::Requirement> = current
        .iter()
        .filter(|item| item.kind == RequirementKind::Acceptance)
        .collect();
    if acceptance.is_empty() {
        out.push_str("验收项:无(没有验收项的账本收口时只能判'未验证完成')\n");
    } else {
        out.push_str(&format!("验收项({}):\n", acceptance.len()));
        for item in &acceptance {
            let mark = if covered.contains(&&item.id) {
                "已覆盖"
            } else {
                "未覆盖"
            };
            out.push_str(&format!(
                "  - [{}] {} <{}>\n",
                item.id.as_str(),
                item.text,
                mark
            ));
        }
    }
    let others: Vec<&&denia_core::task::Requirement> = current
        .iter()
        .filter(|item| item.kind != RequirementKind::Acceptance)
        .collect();
    if !others.is_empty() {
        out.push_str(&format!("其他要求({}):\n", others.len()));
        for item in &others {
            out.push_str(&format!(
                "  - [{}] {}:{}\n",
                item.id.as_str(),
                requirement_kind_label(item.kind),
                item.text
            ));
        }
    }
    render_list(&mut out, "待完成工作", &state.remaining);
    if state.open_questions.is_empty() {
        out.push_str("未决问题:无\n");
    } else {
        out.push_str(&format!("未决问题({}):\n", state.open_questions.len()));
        for question in &state.open_questions {
            out.push_str(&format!(
                "  - [{}] {}\n",
                question.id.as_str(),
                question.text
            ));
        }
    }
    out.push_str(&format!(
        "记录:事实 {} / 假设 {} / 失败尝试 {} / 变更 {} / 证据 {}",
        state.facts.len(),
        state.assumptions.len(),
        state.failed_attempts.len(),
        state.changes.len(),
        state.evidence.len(),
    ));
    if !state.evidence.is_empty() {
        out.push_str(";证据 id:");
        for evidence in state.evidence.iter().take(MAX_LISTED_INPUTS) {
            out.push_str(&format!(" {}", evidence.id.as_str()));
        }
        if state.evidence.len() > MAX_LISTED_INPUTS {
            out.push_str(" …");
        }
    }
    out.push('\n');
    out.push_str(&format!(
        "指纹基线:{} 个路径(变更事件维护;验证后又被改过的路径会让结论失效)\n",
        state.fingerprints.len()
    ));
    let overflow = describe_overflow(&state.overflow);
    if !overflow.is_empty() {
        out.push_str(&format!("上限丢弃:{overflow}\n"));
    }
    out.push_str("```json\n");
    out.push_str(&task_summary(state, &covered).to_string());
    out.push_str("\n```");
    out
}

fn render_list(out: &mut String, title: &str, items: &[String]) {
    if items.is_empty() {
        out.push_str(&format!("{title}:无\n"));
        return;
    }
    out.push_str(&format!("{title}({}):\n", items.len()));
    for item in items {
        out.push_str(&format!("  - {item}\n"));
    }
}

fn requirement_kind_label(kind: RequirementKind) -> &'static str {
    match kind {
        RequirementKind::Goal => "目标",
        RequirementKind::TaskType => "任务类型",
        RequirementKind::Constraint => "约束",
        RequirementKind::Acceptance => "验收项",
    }
}

/// 验证状态的一句话说明:没有可用结论时**必须**说清是为什么,否则
/// "未验证完成"会被读成"通过了"。
fn describe_verification(state: &VerificationState) -> String {
    match state {
        VerificationState::Verified {
            revision,
            verdict,
            fingerprints,
            covered,
            at,
        } => {
            let label = match verdict {
                ValidationVerdict::Passed => "通过",
                ValidationVerdict::Failed => "失败",
            };
            format!(
                "{label},绑在 revision {} 上(覆盖 {} 个验收项,冻结 {} 个输入指纹,时间 {at})",
                revision.as_str(),
                covered.len(),
                fingerprints.len(),
            )
        }
        VerificationState::Unverified { reason } => {
            format!("无可用结论 —— {}", stale_reason_text(reason))
        }
    }
}

fn stale_reason_text(reason: &StaleReason) -> String {
    match reason {
        StaleReason::NeverRun => "当前 revision 上还没跑过检查".to_string(),
        StaleReason::RevisionChanged => "只在旧 revision 上跑过:换版后旧结论不继承".to_string(),
        StaleReason::InputsChanged { paths } => {
            format!("验证之后这些输入又被改过:{}", paths.join("、"))
        }
        StaleReason::NoCheckRun => "有验证记录,但里面没有任何检查运行(空报告)".to_string(),
        StaleReason::ForeignOrigin => {
            "结论来自别的会话(父会话 / 分支种子继承),不参与本会话裁决".to_string()
        }
        StaleReason::RevisionConflict => {
            "换版时复用了旧 revision id,当前版本身份不可信".to_string()
        }
    }
}

fn describe_overflow(overflow: &denia_core::task::LedgerOverflow) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (label, count) in [
        ("要求", overflow.requirements),
        ("事实", overflow.facts),
        ("假设", overflow.assumptions),
        ("失败尝试", overflow.failed_attempts),
        ("未决问题", overflow.open_questions),
        ("证据", overflow.evidence),
        ("验证", overflow.validations),
        ("变更", overflow.changes),
        ("待完成工作", overflow.remaining),
        ("revision", overflow.revisions),
    ] {
        if count > 0 {
            parts.push(format!("{label} {count}"));
        }
    }
    parts.join("、")
}

/// 结构化摘要(机器可读,与上面的文本同源)。
fn task_summary(state: &TaskState, covered: &[&RequirementId]) -> serde_json::Value {
    let acceptance: Vec<serde_json::Value> = state
        .current_requirements()
        .filter(|item| item.kind == RequirementKind::Acceptance)
        .map(|item| {
            serde_json::json!({
                "id": item.id.as_str(),
                "text": item.text,
                "covered": covered.iter().any(|id| **id == item.id),
            })
        })
        .collect();
    serde_json::json!({
        "taskId": state.id.as_str(),
        "revision": state.revision.as_str(),
        "status": state.status,
        "outcome": state.outcome(),
        "verification": state.verification(),
        "goal": state.goal,
        "acceptance": acceptance,
        "openQuestions": state
            .open_questions
            .iter()
            .map(|item| serde_json::json!({"id": item.id.as_str(), "text": item.text}))
            .collect::<Vec<_>>(),
        "remaining": state.remaining,
        "evidence": state
            .evidence
            .iter()
            .map(|item| serde_json::json!({"id": item.id.as_str(), "summary": item.summary}))
            .collect::<Vec<_>>(),
        "fingerprints": state.fingerprints.len(),
        "overflow": state.overflow,
        "updatedAt": state.updated_at,
    })
}

// ---------------------------------------------------------------------------
// update_task
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RequirementArg {
    /// `goal` / `task-type` / `constraint` / `acceptance`。
    kind: String,
    text: String,
    /// 用户原话摘录(可选):宿主在日志里核对,核不到即拒绝。
    #[serde(default)]
    quote: Option<String>,
}

impl RequirementArg {
    fn to_claim(
        &self,
        host: &dyn TaskLedgerHost,
        session: Option<&str>,
    ) -> Result<RequirementClaim, String> {
        let kind = match self.kind.trim() {
            "goal" => RequirementKind::Goal,
            "task-type" => RequirementKind::TaskType,
            "constraint" => RequirementKind::Constraint,
            "acceptance" => RequirementKind::Acceptance,
            other => {
                return Err(format!(
                    "未知的要求类型 {other:?}(可用:goal / task-type / constraint / acceptance)"
                ));
            }
        };
        let text = claim(&self.text, "要求文本")?;
        // 带 quote 就必须核得到:要求是用户约束的通道,伪造出处会直接改写
        // "用户到底要求了什么",这里宁可按失败处理,不接受只言片语冒充原话。
        let source = match self.quote.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
            Some(quote) => host.user_reference(quote).ok_or_else(|| {
                format!(
                    "引用的用户原话在日志里核对不到:{quote:?};改用真实摘录,或去掉 quote(只声明要求、不声称出处)"
                )
            })?,
            None => SourceRef {
                session: session.map(str::to_string),
                seq: 0,
                time_ms: 0,
                quote: None,
            },
        };
        Ok(RequirementClaim {
            id: host.new_requirement_id(),
            kind,
            text,
            source,
        })
    }
}

#[derive(Deserialize)]
struct NoteArg {
    /// `fact` / `assumption` / `failed-attempt` / `question`。
    kind: String,
    text: String,
    /// 用户原话摘录(`fact`):宿主核对后给出引用;核不到则按折叠层的规则降级为假设。
    #[serde(default)]
    quote: Option<String>,
    /// 账本上的证据 id(`fact` / `failed-attempt`)。
    #[serde(default)]
    evidence: Option<String>,
    /// 为什么这么猜(`assumption`)。
    #[serde(default)]
    rationale: Option<String>,
}

impl NoteArg {
    fn to_claim(&self, host: &dyn TaskLedgerHost) -> Result<NoteClaim, String> {
        let text = claim(&self.text, "断言文本")?;
        let id = host.new_note_id();
        let quote = self
            .quote
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty());
        let evidence = self
            .evidence
            .as_deref()
            .map(str::trim)
            .filter(|e| !e.is_empty());
        match self.kind.trim() {
            "fact" => {
                let citation = match (quote, evidence) {
                    (Some(_), Some(_)) => {
                        return Err(
                            "事实的出处只给一种:quote(用户原话)或 evidence(证据 id)".to_string()
                        );
                    }
                    // 核对得到就带上完整坐标(seq / time_ms);核不到只留摘录,
                    // 折叠层会把它降级为假设 —— "证不了就按假设记"是账本的规则,
                    // 不在工具层另立一套。
                    (Some(quote), None) => match host.user_reference(quote) {
                        Some(reference) => denia_core::task::FactCitation::User { reference },
                        None => denia_core::task::FactCitation::User {
                            reference: SourceRef {
                                session: None,
                                seq: 0,
                                time_ms: 0,
                                quote: Some(quote.to_string()),
                            },
                        },
                    },
                    (None, Some(evidence)) => denia_core::task::FactCitation::Evidence {
                        evidence: denia_core::task::EvidenceId::new(evidence),
                    },
                    (None, None) => {
                        return Err(
                            "事实必须给出处:quote(用户原话摘录)或 evidence(账本上的证据 id);拿不出处就按 assumption 记"
                                .to_string(),
                        );
                    }
                };
                Ok(NoteClaim::Fact { id, text, citation })
            }
            "assumption" => Ok(NoteClaim::Assumption {
                id,
                text,
                rationale: match self.rationale.as_deref().map(str::trim) {
                    Some(rationale) if !rationale.is_empty() => Some(claim(rationale, "假设理由")?),
                    _ => None,
                },
            }),
            "failed-attempt" => Ok(NoteClaim::FailedAttempt {
                id,
                text,
                evidence: evidence.map(denia_core::task::EvidenceId::new),
            }),
            "question" => Ok(NoteClaim::OpenQuestion { id, text }),
            other => Err(format!(
                "未知的断言类型 {other:?}(可用:fact / assumption / failed-attempt / question)"
            )),
        }
    }
}

#[derive(Deserialize)]
struct UpdateTaskArgs {
    action: String,
    #[serde(default)]
    goal: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    requirements: Vec<RequirementArg>,
    #[serde(default)]
    notes: Vec<NoteArg>,
    #[serde(default)]
    question_id: Option<String>,
    #[serde(default)]
    items: Vec<String>,
}

/// `update_task`:模型声明意图的入口(声明性操作,不含验证与证据)。
#[derive(Default)]
pub struct UpdateTaskTool;

impl UpdateTaskTool {
    fn build_schema() -> ToolSchema {
        ToolSchema {
            name: "update_task".to_string(),
            description: "声明当前任务账本的意图。action 取值:open(约定目标与验收项,需 goal 与至少一条 acceptance)、revise(用户要求或验收变了,换一版,需 reason)、amend(追加事实/假设/失败尝试/未决问题,需 notes)、resolve-question(关闭未决问题,需 question_id)、set-remaining(整表替换待完成工作,需 items)、block(标记外部受阻,需 reason)、unblock、close(收口;结局由证据裁决,不是由你声明)。\n不在这里的能力:验证结论与执行证据是宿主执行链的产物,模型无权写入 —— 要证明完成就跑 run_checks,由宿主按退出码写结论。事实必须能追溯:quote 要在日志里核对得到,核不到的会被降级为假设;拿不出处就用 assumption。"
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["open", "revise", "amend", "resolve-question", "set-remaining", "block", "unblock", "close"],
                        "description": "声明性动作;验证与证据不在其中(那两类只能由 run_checks 的执行链产生)。"
                    },
                    "goal": {
                        "type": "string",
                        "description": "open 必填:这次任务要达成什么(一句话)。revise 可选带上改写后的目标。"
                    },
                    "reason": {
                        "type": "string",
                        "description": "revise 必填:为什么换版(用户加了要求 / 改了验收 / 上一版理解错了)。block 也用它写受阻条件。"
                    },
                    "requirements": {
                        "type": "array",
                        "description": "open / revise 要声明的要求与验收项;完成裁决只看 acceptance。",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "kind": {
                                    "type": "string",
                                    "enum": ["goal", "task-type", "constraint", "acceptance"],
                                    "description": "要求类别;acceptance 是完成判定要逐项对上的那类。"
                                },
                                "text": {"type": "string", "description": "结论性短句(上限 400 字符)。"},
                                "quote": {"type": "string", "description": "用户原话摘录(可选):宿主在日志里核对,核不到会拒绝。"}
                            },
                            "required": ["kind", "text"]
                        }
                    },
                    "notes": {
                        "type": "array",
                        "description": "amend 要追加的断言。",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "kind": {
                                    "type": "string",
                                    "enum": ["fact", "assumption", "failed-attempt", "question"],
                                    "description": "fact 要给出处;拿不出处就用 assumption(读的人不必猜哪条是猜的)。"
                                },
                                "text": {"type": "string", "description": "结论性短句(上限 400 字符)。"},
                                "quote": {"type": "string", "description": "fact:用户原话摘录;核不到会降级为假设。"},
                                "evidence": {"type": "string", "description": "fact / failed-attempt:账本上的证据 id(见 get_task)。"},
                                "rationale": {"type": "string", "description": "assumption:为什么这么猜。"}
                            },
                            "required": ["kind", "text"]
                        }
                    },
                    "question_id": {
                        "type": "string",
                        "description": "resolve-question:要关闭的未决问题 id(`note-…`,见 get_task)。"
                    },
                    "items": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "set-remaining:整表替换的待完成工作条目(每条一句短句)。"
                    }
                },
                "required": ["action"]
            }),
        }
    }
}

#[async_trait]
impl Tool for UpdateTaskTool {
    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(Self::build_schema)
    }

    async fn execute(&self, arguments: &str, ctx: &ToolContext) -> ToolOutput {
        let args: UpdateTaskArgs = match parse_tool_args(arguments) {
            Ok(args) => args,
            Err(error) => {
                return tool_error(
                    format!("参数解析失败:{error}"),
                    "参数必须是 JSON 对象,必填 action;各 action 的必需字段见工具描述",
                );
            }
        };
        let host = match require_host(ctx) {
            Ok(host) => host,
            Err(output) => return output,
        };
        let sink = match require_sink(ctx) {
            Ok(sink) => sink,
            Err(output) => return output,
        };
        let session = ctx.session_id.as_deref();
        let state = host.state();
        let (op, message) = match args.action.trim() {
            "open" => match plan_open(&args, host.as_ref(), session, state.as_ref()) {
                Ok(planned) => planned,
                Err(message) => return tool_error(message, open_hint(&args)),
            },
            "revise" => match plan_revise(&args, host.as_ref(), session, state.as_ref()) {
                Ok(planned) => planned,
                Err(message) => return tool_error(message, revise_hint(&state)),
            },
            "amend" => match plan_amend(&args, host.as_ref(), state.as_ref()) {
                Ok(planned) => planned,
                Err(message) => return tool_error(message, "修正 notes 后重试"),
            },
            "resolve-question" => match plan_resolve(&args, state.as_ref()) {
                Ok(planned) => planned,
                Err(message) => return tool_error(message, "用 get_task 取未决问题 id"),
            },
            "set-remaining" => match plan_remaining(&args, state.as_ref()) {
                Ok(planned) => planned,
                Err(message) => return tool_error(message, "修正 items 后重试"),
            },
            "block" => match plan_block(&args, state.as_ref()) {
                Ok(planned) => planned,
                Err(message) => return tool_error(message, "补上受阻条件后重试"),
            },
            "unblock" => match plan_unblock(state.as_ref()) {
                Ok(planned) => planned,
                Err(message) => return tool_error(message, "用 get_task 确认当前状态"),
            },
            "close" => match plan_close(state.as_ref()) {
                Ok(planned) => planned,
                Err(message) => return tool_error(message, "收口前先确认账本状态"),
            },
            other => {
                return tool_error(
                    format!("未知 action:{other:?}"),
                    "可用:open / revise / amend / resolve-question / set-remaining / block / unblock / close。验证结论(record-validation)与执行证据(record-evidence)不在模型权限内:要证明完成就跑 run_checks,由宿主按退出码写结论。",
                );
            }
        };
        sink(SessionEvent::Task { op });
        ToolOutput::text(message)
    }
}

type Planned = (TaskOp, String);

fn build_requirements(
    args: &[RequirementArg],
    host: &dyn TaskLedgerHost,
    session: Option<&str>,
) -> Result<Vec<RequirementClaim>, String> {
    args.iter()
        .map(|item| item.to_claim(host, session))
        .collect()
}

fn plan_open(
    args: &UpdateTaskArgs,
    host: &dyn TaskLedgerHost,
    session: Option<&str>,
    state: Option<&TaskState>,
) -> Result<Planned, String> {
    let goal = match args.goal.as_deref() {
        Some(goal) => claim(goal, "goal")?,
        None => return Err("open 需要 goal:一句话说清这次任务要达成什么".to_string()),
    };
    if let Some(existing) = state
        && existing.status != TaskStatus::Closed
    {
        return Err(format!(
            "账本上已有进行中的任务({}):不能重复 open",
            existing.id.as_str()
        ));
    }
    let requirements = build_requirements(&args.requirements, host, session)?;
    let acceptance = requirements
        .iter()
        .filter(|item| item.kind == RequirementKind::Acceptance)
        .count();
    if acceptance == 0 {
        return Err(
            "open 至少要声明一条验收项(kind=\"acceptance\"):完成裁决只看验收项,没有验收项的账本收口时永远只能判'未验证完成'"
                .to_string(),
        );
    }
    let message = format!(
        "已开账:目标「{goal}」,{} 条要求(其中 {acceptance} 条验收项)。跑 run_checks 取证后再 close;现在可以用 revise 补要求。",
        requirements.len()
    );
    Ok((
        TaskOp::Open {
            task_id: host.new_task_id(),
            revision: host.new_revision_id(),
            goal,
            requirements,
        },
        message,
    ))
}

fn plan_revise(
    args: &UpdateTaskArgs,
    host: &dyn TaskLedgerHost,
    session: Option<&str>,
    state: Option<&TaskState>,
) -> Result<Planned, String> {
    let Some(existing) = state else {
        return Err("账本上还没有任务:先 open 声明目标与验收项,再谈换版".to_string());
    };
    let reason = match args.reason.as_deref() {
        Some(reason) => claim(reason, "reason")?,
        None => {
            return Err(
                "revise 需要 reason:说清为什么换版(用户加了要求 / 改了验收 / 上一版理解错了)"
                    .to_string(),
            );
        }
    };
    let goal = match args.goal.as_deref() {
        Some(goal) => Some(claim(goal, "goal")?),
        None => None,
    };
    let requirements = build_requirements(&args.requirements, host, session)?;
    let revision = host.new_revision_id();
    let message = format!(
        "已换版:新 revision {}(第 {} 版)。旧验证结论绑在旧 revision 上,不再适用于本版;取证请重跑 run_checks。",
        revision.as_str(),
        existing.revisions.len() + 1
    );
    Ok((
        TaskOp::Revise {
            revision,
            reason,
            goal,
            requirements,
        },
        message,
    ))
}

fn plan_amend(
    args: &UpdateTaskArgs,
    host: &dyn TaskLedgerHost,
    state: Option<&TaskState>,
) -> Result<Planned, String> {
    if state.is_none() {
        return Err("账本上还没有任务:先 open,再追加断言".to_string());
    }
    if args.notes.is_empty() {
        return Err(
            "amend 需要 notes:至少一条断言(fact / assumption / failed-attempt / question)"
                .to_string(),
        );
    }
    let notes: Vec<NoteClaim> = args
        .notes
        .iter()
        .map(|item| item.to_claim(host))
        .collect::<Result<_, _>>()?;
    let message = format!(
        "已追加 {} 条断言。引用核对不到的事实会按假设记录(见 get_task):事实的门槛就是出处能核对。",
        notes.len()
    );
    Ok((TaskOp::Amend { notes }, message))
}

fn plan_resolve(args: &UpdateTaskArgs, state: Option<&TaskState>) -> Result<Planned, String> {
    let Some(existing) = state else {
        return Err("账本上还没有任务".to_string());
    };
    let raw = args
        .question_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "resolve-question 需要 question_id".to_string())?;
    let id = NoteId::new(raw);
    if !existing.open_questions.iter().any(|item| item.id == id) {
        let open: Vec<&str> = existing
            .open_questions
            .iter()
            .map(|item| item.id.as_str())
            .collect();
        return Err(format!(
            "账本上没有这个未决问题:{raw}(当前未决问题:{})",
            if open.is_empty() {
                "无".to_string()
            } else {
                open.join("、")
            }
        ));
    }
    Ok((
        TaskOp::ResolveQuestion { id },
        "已关闭该未决问题。".to_string(),
    ))
}

fn plan_remaining(args: &UpdateTaskArgs, state: Option<&TaskState>) -> Result<Planned, String> {
    if state.is_none() {
        return Err("账本上还没有任务:先 open,再设置待完成工作".to_string());
    }
    if args.items.len() > MAX_REMAINING {
        return Err(format!(
            "待完成工作最多 {MAX_REMAINING} 条(当前 {} 条):只留真正待做的,做完的条目应当移除",
            args.items.len()
        ));
    }
    let items: Vec<String> = args
        .items
        .iter()
        .map(|item| claim(item, "待完成工作条目"))
        .collect::<Result<_, _>>()?;
    let message = if items.is_empty() {
        "已清空待完成工作。".to_string()
    } else {
        format!("已整表替换待完成工作({} 条)。", items.len())
    };
    Ok((TaskOp::SetRemaining { items }, message))
}

fn plan_block(args: &UpdateTaskArgs, state: Option<&TaskState>) -> Result<Planned, String> {
    let Some(existing) = state else {
        return Err("账本上还没有任务:先 open,再标记受阻".to_string());
    };
    if existing.status == TaskStatus::Closed {
        return Err("任务已收口:受阻状态无从谈起".to_string());
    }
    let reason = match args.reason.as_deref() {
        Some(reason) => claim(reason, "reason")?,
        None => return Err("block 需要 reason:说清被什么卡住、需要什么才能继续".to_string()),
    };
    Ok((TaskOp::Block { reason }, "已标记外部受阻。".to_string()))
}

fn plan_unblock(state: Option<&TaskState>) -> Result<Planned, String> {
    let Some(existing) = state else {
        return Err("账本上还没有任务".to_string());
    };
    if existing.status != TaskStatus::Blocked {
        return Err("当前不是受阻状态,unblock 无效".to_string());
    }
    Ok((TaskOp::Unblock, "已解除受阻,任务重新进行中。".to_string()))
}

fn plan_close(state: Option<&TaskState>) -> Result<Planned, String> {
    let Some(existing) = state else {
        return Err("账本上还没有任务:先 open".to_string());
    };
    match existing.status {
        TaskStatus::Active => Ok((
            TaskOp::Close,
            "已收口。结局由证据裁决,不由收口动作声明:用 get_task 看当前 revision 的验收项是否全被通过的检查覆盖(否则是'未验证完成')。"
                .to_string(),
        )),
        TaskStatus::Blocked => Err("任务处于受阻状态:先 unblock 或说明阻塞已解除,再收口".to_string()),
        TaskStatus::Closed => Err("任务已收口;要重新开始请等用户给出新要求后用 revise/open".to_string()),
    }
}

fn open_hint(args: &UpdateTaskArgs) -> &'static str {
    if args
        .goal
        .as_deref()
        .is_none_or(|goal| goal.trim().is_empty())
    {
        "补上 goal(一句话)与至少一条 acceptance 验收项后重试"
    } else {
        "补上至少一条 acceptance 验收项后重试"
    }
}

fn revise_hint(state: &Option<TaskState>) -> &'static str {
    match state {
        Some(_) => "补上 reason(为什么换版)后重试",
        None => "先用 action=open 开账",
    }
}

// ---------------------------------------------------------------------------
// run_checks
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CheckArg {
    command: String,
    /// 通过所需的退出码;缺省 0(非零即失败)。
    #[serde(default)]
    expect_exit_code: Option<i32>,
    /// 运行时的工作目录(相对会话工作区或绝对路径,必须落在工作区内)。
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
struct RunChecksArgs {
    checks: Vec<CheckArg>,
    /// 本次验证要冻结内容的输入文件;缺省时取本会话读过 / 写过的文件。
    #[serde(default)]
    inputs: Vec<String>,
    /// 本次验证声明覆盖的验收项 id。
    #[serde(default)]
    covered: Vec<String>,
}

/// `run_checks`:跑检查、产结构化结果、把验证结论落成账本事件。
#[derive(Default)]
pub struct RunChecksTool;

impl RunChecksTool {
    fn build_schema() -> ToolSchema {
        ToolSchema {
            name: "run_checks".to_string(),
            description: "按检查定义真的跑命令,并把结论记进任务账本(结论由退出码算出,不是可以填的字段)。每条检查:command 必填,expect_exit_code 默认 0(不符即该检查未通过),workdir 默认会话工作区,timeout_ms 默认 120000 / 上限 600000。inputs 声明本次验证要冻结内容的输入文件(相对工作区路径);省略时冻结本会话读过/写过的文件。covered 声明这次验证覆盖哪些验收项 id(见 get_task)——收口时当前 revision 的验收项要全被覆盖才算'验收通过'。\n输入快照只覆盖工作目录与文件内容指纹:仓库没有环境变量快照,依赖环境变量的检查不受指纹保护。命令没跑起来(启动失败/超时/被中断)都按**未通过**记,不会算通过。"
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "checks": {
                        "type": "array",
                        "minItems": 1,
                        "description": "要跑的检查,按顺序执行;每项是一次命令运行。",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "command": {"type": "string", "description": "检查命令(宿主 shell 方言),在 workdir 里执行。"},
                                "expect_exit_code": {"type": "integer", "description": "通过所需的退出码,默认 0;非零即失败。"},
                                "workdir": {"type": "string", "description": "运行时的工作目录(相对会话工作区或绝对路径,必须落在工作区内);默认会话工作区。"},
                                "timeout_ms": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_MS, "description": "单条检查的超时(毫秒),默认 120000。"}
                            },
                            "required": ["command"]
                        }
                    },
                    "inputs": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "本次验证冻结内容的输入文件(相对工作区路径):这些字节变了,结论就失效。省略时冻结本会话读过/写过的文件。"
                    },
                    "covered": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "本次验证声明覆盖的验收项 id(`req-…`,见 get_task)。检查通过只代表它声明的范围。"
                    }
                },
                "required": ["checks"]
            }),
        }
    }
}

#[async_trait]
impl Tool for RunChecksTool {
    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(Self::build_schema)
    }

    async fn execute(&self, arguments: &str, ctx: &ToolContext) -> ToolOutput {
        let args: RunChecksArgs = match parse_tool_args(arguments) {
            Ok(args) => args,
            Err(error) => {
                return tool_error(
                    format!("参数解析失败:{error}"),
                    "参数必须是 JSON 对象,必填 checks(数组,每项含 command)",
                );
            }
        };
        if args.checks.is_empty() {
            return tool_error("checks 不能为空", "至少给一条检查命令");
        }
        let host = match require_host(ctx) {
            Ok(host) => host,
            Err(output) => return output,
        };
        let sink = match require_sink(ctx) {
            Ok(sink) => sink,
            Err(output) => return output,
        };
        let Some(state) = host.state() else {
            return tool_error(
                "账本上还没有任务:验证结论要绑在当前 revision 上,没处可记",
                "先用 update_task 的 open 声明目标与验收项,再跑检查",
            );
        };
        // covered 只能声明当前 revision 上存在的验收项(其余一律不收:写不存在的
        // id 只会让覆盖率永远对不上,却看起来"声明过了")。
        let known: Vec<RequirementId> = state
            .current_requirements()
            .map(|item| item.id.clone())
            .collect();
        let mut covered: Vec<RequirementId> = Vec::new();
        for raw in &args.covered {
            let id = RequirementId::new(raw.trim());
            if !known.contains(&id) {
                return tool_error(
                    format!("covered 里的验收项 id 不在当前 revision 上:{raw}"),
                    "用 get_task 取当前 revision 的验收项 id",
                );
            }
            if !covered.contains(&id) {
                covered.push(id);
            }
        }
        let inputs = match resolve_inputs(&args.inputs, ctx) {
            Ok(inputs) => inputs,
            Err(message) => {
                return tool_error(message, "inputs 里的路径必须存在于会话工作区内");
            }
        };

        let mut runs: Vec<CheckRun> = Vec::new();
        let mut lines: Vec<String> = Vec::new();
        let mut failures: Vec<String> = Vec::new();
        let mut started = 0usize;
        for check in &args.checks {
            let command = match claim(&check.command, "检查命令") {
                Ok(command) => command,
                Err(message) => return tool_error(message, "补上命令后重试"),
            };
            let expect = check.expect_exit_code.unwrap_or(0);
            let workdir = match resolve_workdir(check.workdir.as_deref(), ctx) {
                Ok(workdir) => workdir,
                Err(message) => return tool_error(message, "workdir 必须是工作区内的路径"),
            };
            let timeout_ms = check
                .timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS);
            // 输入指纹在**每次运行开始时**冻结:检查自己改了文件时,后续检查
            // 冻的是改后的字节,结论分别绑在各自看到的那份内容上。
            let fingerprints = freeze_inputs(&inputs);
            let run = shell::run_command(
                CommandSpec {
                    command: &command,
                    cwd: &workdir,
                    timeout_ms,
                    output_store: ctx.output_store.as_ref(),
                    stream: Some(StreamSpec {
                        sink: ctx.emit_event.clone(),
                        call_id: ctx.call_id.clone(),
                    }),
                    preview_chars: (1_500, 3_000),
                    timeout_hint: "可拆小这条检查或调大它的 timeout_ms",
                },
                &ctx.cancel,
            )
            .await;
            if run.ended == end_reason::CANCELLED {
                // 被中断的运行不是验收结果:记一条失败会把"用户喊停"说成
                // "验收失败",所以整轮不落结论,如实告诉模型重跑。
                return tool_error(
                    format!("检查被中断,本轮没有记录验证结论(已跑 {} 条)", runs.len()),
                    "重跑 run_checks;中断那次的结果不作数",
                );
            }
            if run.not_started {
                let reason = run
                    .error
                    .clone()
                    .unwrap_or_else(|| "命令没跑起来".to_string());
                lines.push(format!("  - [未启动] {command} —— {reason}"));
                failures.push(format!("$ {command}(未启动:{reason})"));
            } else {
                started += 1;
                let passed = run.exit_code == Some(expect);
                let code = run
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "无退出码(超时或被中止)".to_string());
                lines.push(format!(
                    "  - [{}] {command}(退出码 {code},期望 {expect})",
                    if passed { "通过" } else { "失败" }
                ));
                if !passed {
                    let mut body = format!("$ {command}\n退出码:{code}\n{}", run.stdout);
                    if !run.stderr.is_empty() {
                        body.push_str(&format!("\n--- stderr ---\n{}", run.stderr));
                    }
                    failures.push(body);
                }
            }
            runs.push(CheckRun {
                command,
                exit_code: run.exit_code,
                expect_exit_code: expect,
                workdir: display_path(&ctx.cwd, &workdir),
                fingerprints,
            });
        }

        let passed = runs.iter().all(CheckRun::passed);
        let Some(result) = ValidationResult::from_check_runs(
            state.revision.clone(),
            runs,
            covered.clone(),
            ctx.session_id.clone(),
            now_ms(),
        ) else {
            return tool_error("没有可记录的检查运行", "至少给一条检查命令");
        };
        let accepted = uncovered_acceptance(&state, &covered);
        sink(SessionEvent::Task {
            op: TaskOp::RecordValidation { result },
        });

        let mut content = String::new();
        content.push_str(&format!(
            "检查结果:{}({} 条检查)\n",
            if passed {
                "验收通过"
            } else {
                "验收失败"
            },
            lines.len()
        ));
        for line in &lines {
            content.push_str(line);
            content.push('\n');
        }
        content.push_str(&format!(
            "已记入账本:revision {},覆盖 {} 个验收项,冻结 {} 个输入指纹。\n",
            state.revision.as_str(),
            covered.len(),
            inputs.len()
        ));
        if !inputs.is_empty() {
            content.push_str(&format!(
                "冻结的输入:{}{}\n",
                inputs
                    .iter()
                    .take(MAX_LISTED_INPUTS)
                    .map(|(_, path)| path.clone())
                    .collect::<Vec<String>>()
                    .join("、"),
                if inputs.len() > MAX_LISTED_INPUTS {
                    format!(" 等 {} 个", inputs.len())
                } else {
                    String::new()
                }
            ));
        }
        if passed && accepted > 0 {
            content.push_str(
                "覆盖范围只含本次声明的 covered:收口时当前 revision 的验收项必须全被覆盖,否则结局是'未验证完成'。\n",
            );
        }
        if !failures.is_empty() {
            content.push_str("\n未通过的检查输出:\n");
            for body in &failures {
                content.push_str(&body.replace('\n', "\n  "));
                content.push('\n');
            }
        }
        ToolOutput {
            content,
            // 全部命令都没能启动 = 这个环境根本跑不了检查(工具层面的问题);
            // 只要有一条真的跑过,失败就是检查结果,不是工具错误。
            is_error: started == 0,
            artifact: None,
            report: Some(crate::ExecutionReport {
                exit_code: None,
                end_reason: Some(if started == 0 {
                    end_reason::SPAWN_FAILED.to_string()
                } else {
                    end_reason::COMPLETED.to_string()
                }),
                files: Vec::new(),
                evidence_complete: None,
            }),
        }
    }
}

/// 当前 revision 还没被本次验证覆盖的验收项数(用于提示覆盖范围)。
fn uncovered_acceptance(state: &TaskState, covered: &[RequirementId]) -> usize {
    state
        .current_requirements()
        .filter(|item| item.kind == RequirementKind::Acceptance && !covered.contains(&item.id))
        .count()
}

/// 解析要冻结的输入:显式清单按工作区内的路径解析;缺省时取本会话读过 /
/// 写过的文件(那正是"这次验证依据的输入")。
fn resolve_inputs(raw: &[String], ctx: &ToolContext) -> Result<Vec<(PathBuf, String)>, String> {
    let mut out: Vec<(PathBuf, String)> = Vec::new();
    if raw.is_empty() {
        let Some(shared) = &ctx.read_state else {
            return Ok(out);
        };
        let Ok(state) = shared.lock() else {
            return Ok(out);
        };
        let mut paths: Vec<PathBuf> = state
            .recent_writable(MAX_INPUTS)
            .into_iter()
            .map(|(key, _)| key.path)
            .collect();
        paths.sort();
        paths.dedup();
        for path in paths {
            // 缺省输入只冻结仍然存在的文件:读过的文件后来被删掉不是错误。
            if digest_of(&path).is_some() {
                let display = display_path(&ctx.cwd, &path);
                out.push((path, display));
            }
        }
        out.sort_by(|a, b| a.1.cmp(&b.1));
        return Ok(out);
    }
    for item in raw {
        let path = crate::resolve_within(&ctx.cwd, item, ctx.confined)?;
        // 显式声明的输入必须读得到:声明了却不存在的输入应当当场失败,
        // 而不是冻结一个空指纹、让"这些字节变了"的判定到时候失效。
        if digest_of(&path).is_none() {
            return Err(format!("输入文件不存在或读不到:{item}"));
        }
        let display = display_path(&ctx.cwd, &path);
        if !out.iter().any(|(_, existing)| existing == &display) {
            out.push((path, display));
        }
    }
    Ok(out)
}

/// 冻结输入指纹(逐个读内容算 digest);读不到的文件跳过 —— 冻结不到就不假装
/// 冻结过,后续"输入变了"的判定自然也不会引用它。
fn freeze_inputs(inputs: &[(PathBuf, String)]) -> Vec<FileFingerprint> {
    inputs
        .iter()
        .filter_map(|(path, display)| {
            digest_of(path).map(|digest| FileFingerprint {
                path: display.clone(),
                digest,
            })
        })
        .collect()
}

/// 文件内容指纹:复用读状态表的取样口径([`FileStamp`]),digest 取十六进制。
fn digest_of(path: &Path) -> Option<String> {
    let stamp = FileStamp::of(path)?;
    stamp
        .fingerprint
        .map(|fingerprint| format!("{:016x}", fingerprint.as_u64()))
}

/// 路径口径:工作区内记相对路径(与 `ExecutionReport::files` 同口径,分隔符统一
/// 为 `/`),工作区外记绝对路径。
///
/// 写 `RecordChange` 的一侧必须用同一口径,否则折叠层的指纹比对
/// ([`TaskState::paths_changed_since`])对不上号,验证失效不会被发现。
fn display_path(cwd: &Path, path: &Path) -> String {
    match path.strip_prefix(cwd) {
        Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
        Err(_) => path.to_string_lossy().to_string(),
    }
}

/// 检查的工作目录:缺省会话工作区,显式给出时必须是工作区内的路径。
fn resolve_workdir(raw: Option<&str>, ctx: &ToolContext) -> Result<PathBuf, String> {
    match raw.map(str::trim).filter(|raw| !raw.is_empty()) {
        Some(raw) => crate::resolve_within(&ctx.cwd, raw, ctx.confined),
        None => Ok(ctx.cwd.clone()),
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------
//
// 这一层要证明的是**工具面本身的三条边界**,以及它写出的记录在裁决层怎么算:
//
// 1. 三个工具的 schema 是闭合的,`update_task` 的 action 枚举里没有"写结论"的位置;
// 2. 塞一个 `record-validation` / `record-evidence` 进去只会走参数错误分支,账本上一
//    条事件都不落;
// 3. `run_checks` 的结论由**真实命令的退出码**算出(不 mock 执行链),没跑起来的命
//    令按未通过记,并且写出的结论绑着当时的 revision 与输入指纹。
//
// 其中三条走**真实命令**(`exit 0` / `exit 3` / 不存在的可执行文件 / 不存在的工作目
// 录),不 mock 执行链 —— 与 `bash.rs` 的既有测试同一路数(一次性进程、stdin 为
// null,不是 `shell_session` 那类会被沙盒卡住的常驻管道)。它们依赖宿主有可用的
// shell;若某个环境没有,替代品是绕过工具、直接拿 `CheckRun` + `ValidationResult::
// from_check_runs` 断言裁决(见 `covered_declaration_decides_whether_acceptance_passed`),
// 但那样就测不到"工具真的跑了命令、按退出码算出结论"这层。
//
// 关于覆盖面的一句实话:验收裁决(`covered` 是否让结局变成"验收通过")住在
// `denia_core::task::TaskState::outcome` / `verification` 里,`crates/session` 的
// `project_task_outcome` 只是 `project_task(events).and_then(|state| state.outcome())`。
// tools crate 的依赖里**没有** denia-session(也没有 dev-dependency),本次改动被限
// 定在 `crates/tools/src/task.rs`,所以这里对着同一份核心裁决函数断言,而不是重新实
// 现一遍折叠 —— 重新实现只会测出另一份语义。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolContext;
    use denia_core::session::{PermissionMode, SessionEvent};
    use denia_core::task::{LedgerOverflow, Requirement, RevisionRecord, TaskOutcome};
    use std::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    /// 假宿主:只提供工具层真正问的三件事(折叠快照、id 分配、用户原话核对)。
    /// 它不折叠事件 —— 落不落盘看传给工具的事件 sink,那正是"模型有没有写出
    /// 验证结论"的判据。
    struct FakeHost {
        state: Option<TaskState>,
        allocated: Mutex<u32>,
    }

    impl FakeHost {
        fn new(state: Option<TaskState>) -> Arc<Self> {
            Arc::new(Self {
                state,
                allocated: Mutex::new(0),
            })
        }

        fn next_id(&self, prefix: &str) -> String {
            let mut count = self.allocated.lock().unwrap();
            *count += 1;
            format!("{prefix}-{count}")
        }
    }

    impl TaskLedgerHost for FakeHost {
        fn state(&self) -> Option<TaskState> {
            self.state.clone()
        }

        fn new_task_id(&self) -> TaskId {
            TaskId::new(self.next_id("task"))
        }

        fn new_revision_id(&self) -> RevisionId {
            RevisionId::new(self.next_id("rev"))
        }

        fn new_note_id(&self) -> NoteId {
            NoteId::new(self.next_id("note"))
        }

        fn new_requirement_id(&self) -> RequirementId {
            RequirementId::new(self.next_id("req"))
        }

        fn user_reference(&self, quote: &str) -> Option<SourceRef> {
            Some(SourceRef {
                session: None,
                seq: 1,
                time_ms: 1,
                quote: Some(quote.to_string()),
            })
        }
    }

    fn ctx_with(
        dir: &Path,
        ledger: Option<Arc<FakeHost>>,
        events: &Arc<Mutex<Vec<SessionEvent>>>,
    ) -> ToolContext {
        let sink: crate::SessionEventSink = {
            let events = events.clone();
            Arc::new(move |event| events.lock().unwrap().push(event))
        };
        ToolContext {
            output_store: None,
            session_id: None,
            selection: None,
            cwd: dir.to_path_buf(),
            cancel: CancellationToken::new(),
            confined: true,
            vision_supported: false,
            emit_event: Some(sink),
            file_history: None,
            permission_mode: PermissionMode::AutoEdit,
            ask: None,
            call_id: None,
            goal_reader: None,
            task_ledger: ledger.map(|host| host as Arc<dyn TaskLedgerHost>),
            read_state: None,
        }
    }

    /// 一个进行中的账本:目标 + 若干验收项,全部绑在给定 revision 上。
    fn ledger_state(revision: &str, acceptance: &[&str]) -> TaskState {
        let revision = RevisionId::new(revision);
        TaskState {
            id: TaskId::new("task-1"),
            revision: revision.clone(),
            session: None,
            status: TaskStatus::Active,
            goal: "给任务账本工具补回归".to_string(),
            blocked_reason: None,
            revision_conflict: false,
            revision_rejected: 0,
            revisions: vec![RevisionRecord {
                id: revision.clone(),
                ordinal: 1,
                reason: "初版".to_string(),
                at: 1,
            }],
            requirements: acceptance
                .iter()
                .enumerate()
                .map(|(index, text)| Requirement {
                    id: RequirementId::new(format!("req-{}", index + 1)),
                    revision: revision.clone(),
                    kind: RequirementKind::Acceptance,
                    text: (*text).to_string(),
                    source: SourceRef {
                        session: None,
                        seq: 0,
                        time_ms: 0,
                        quote: None,
                    },
                    declared_at: 1,
                })
                .collect(),
            facts: Vec::new(),
            assumptions: Vec::new(),
            failed_attempts: Vec::new(),
            open_questions: Vec::new(),
            changes: Vec::new(),
            remaining: Vec::new(),
            evidence: Vec::new(),
            validations: Vec::new(),
            fingerprints: Vec::new(),
            overflow: LedgerOverflow::default(),
            created_at: 1,
            updated_at: 1,
        }
    }

    fn task_ops(events: &Arc<Mutex<Vec<SessionEvent>>>) -> Vec<TaskOp> {
        events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                SessionEvent::Task { op } => Some(op.clone()),
                _ => None,
            })
            .collect()
    }

    fn validation_results(events: &Arc<Mutex<Vec<SessionEvent>>>) -> Vec<ValidationResult> {
        task_ops(events)
            .into_iter()
            .filter_map(|op| match op {
                TaskOp::RecordValidation { result } => Some(result),
                _ => None,
            })
            .collect()
    }

    /// 跑一次 `run_checks`,返回工具输出与它落进账本的全部验证结论。
    async fn run_checks_with(
        host: &Arc<FakeHost>,
        dir: &Path,
        arguments: serde_json::Value,
    ) -> (ToolOutput, Vec<ValidationResult>) {
        let events: Arc<Mutex<Vec<SessionEvent>>> = Arc::default();
        let ctx = ctx_with(dir, Some(host.clone()), &events);
        let out = RunChecksTool.execute(&arguments.to_string(), &ctx).await;
        (out, validation_results(&events))
    }

    /// 以指定退出码结束的短命令:Windows 走 `pwsh -Command`、Unix 走 `bash -c`,
    /// 两边 `exit N` 都是"以 N 退出"。
    ///
    /// 刻意不用 `cmd /C exit N`:实测经 pwsh 转发后拿到的是 1 而不是 N,拿它当
    /// 退出码来源会把"退出码决定结论"这条测试测成另一个东西。
    fn exit_with(code: i32) -> String {
        format!("exit {code}")
    }

    /// schema 是闭合的,且没有任何地方能填结论。
    #[test]
    fn schemas_are_closed_and_hold_no_verdict_field() {
        let get = GetTaskTool.schema();
        let update = UpdateTaskTool.schema();
        let checks = RunChecksTool.schema();

        for schema in [get, update, checks] {
            assert_eq!(
                schema.parameters["type"],
                serde_json::json!("object"),
                "{} 的参数根必须是对象",
                schema.name
            );
            assert_eq!(
                schema.parameters["additionalProperties"],
                serde_json::json!(false),
                "{} 必须 additionalProperties: false",
                schema.name
            );
        }

        let actions: Vec<&str> = update.parameters["properties"]["action"]["enum"]
            .as_array()
            .expect("update_task 的 action 必须是枚举")
            .iter()
            .map(|value| value.as_str().expect("枚举项是字符串"))
            .collect();
        assert_eq!(
            actions,
            vec![
                "open",
                "revise",
                "amend",
                "resolve-question",
                "set-remaining",
                "block",
                "unblock",
                "close"
            ],
            "action 枚举就是模型能声明的全部动作"
        );
        for forbidden in [
            "record-validation",
            "record-evidence",
            "record-change",
            "verify",
        ] {
            assert!(
                !actions.contains(&forbidden),
                "update_task 不该有 {forbidden} 动作"
            );
        }

        // 参数面逐字段点名:update_task 里没有 verdict/result/evidence 这类"写结论"的位置。
        let mut declared: Vec<&str> = update.parameters["properties"]
            .as_object()
            .expect("update_task 的 properties")
            .keys()
            .map(String::as_str)
            .collect();
        declared.sort();
        assert_eq!(
            declared,
            vec![
                "action",
                "goal",
                "items",
                "notes",
                "question_id",
                "reason",
                "requirements"
            ]
        );
        assert_eq!(
            update.parameters["properties"]["requirements"]["items"]["additionalProperties"],
            serde_json::json!(false)
        );
        assert_eq!(
            update.parameters["properties"]["notes"]["items"]["additionalProperties"],
            serde_json::json!(false)
        );

        // run_checks 只接受"跑什么、查什么、覆盖什么";每条检查的参数里没有
        // 可填的判定字段(结论由退出码算出)。
        let mut check_params: Vec<&str> = checks.parameters["properties"]
            .as_object()
            .expect("run_checks 的 properties")
            .keys()
            .map(String::as_str)
            .collect();
        check_params.sort();
        assert_eq!(check_params, vec!["checks", "covered", "inputs"]);
        let mut one_check: Vec<&str> =
            checks.parameters["properties"]["checks"]["items"]["properties"]
                .as_object()
                .expect("checks 项的 properties")
                .keys()
                .map(String::as_str)
                .collect();
        one_check.sort();
        assert_eq!(
            one_check,
            vec!["command", "expect_exit_code", "timeout_ms", "workdir"]
        );
        assert_eq!(
            checks.parameters["properties"]["checks"]["items"]["additionalProperties"],
            serde_json::json!(false)
        );
    }

    /// 模型塞不进验证结论:不在枚举里的 action 走参数错误分支,账本上一条事件都不落。
    #[tokio::test]
    async fn update_task_cannot_write_validation_or_evidence() {
        let events: Arc<Mutex<Vec<SessionEvent>>> = Arc::default();
        let host = FakeHost::new(Some(ledger_state("rev-1", &["检查全绿"])));
        let ctx = ctx_with(&std::env::temp_dir(), Some(host), &events);

        for action in [
            "record-validation",
            "record-evidence",
            "record-change",
            "set-verdict",
            "verify",
        ] {
            // 参数里连"结论长什么样"都塞满了,依然只是未知 action。
            let arguments = serde_json::json!({
                "action": action,
                "verdict": "passed",
                "result": {"verdict": "passed", "runs": [{"exitCode": 0}]},
            })
            .to_string();
            let out = UpdateTaskTool.execute(&arguments, &ctx).await;
            assert!(out.is_error, "{action} 必须被拒绝:{}", out.content);
            assert!(
                out.content.contains("未知 action"),
                "{action} 应走未知 action 分支:{}",
                out.content
            );
        }
        assert_eq!(
            events.lock().unwrap().len(),
            0,
            "被拒的 action 不得产生任何事件(尤其不是 RecordValidation / RecordEvidence)"
        );

        // 反证 sink 是通的:一个合法动作确实会落一条 Task 事件 ——
        // 否则上面的"事件数为 0"可能只是因为通道根本没接上。
        let ok = UpdateTaskTool.execute(r#"{"action":"close"}"#, &ctx).await;
        assert!(!ok.is_error, "{}", ok.content);
        let ops = task_ops(&events);
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], TaskOp::Close));
        assert!(
            !ops.iter().any(|op| matches!(
                op,
                TaskOp::RecordValidation { .. } | TaskOp::RecordEvidence { .. }
            )),
            "整个流程里不该出现宿主才写得出的事件"
        );
    }

    /// 结论由退出码算出:相等即通过,不等即失败(包括非零的期望码)。
    #[tokio::test]
    async fn run_checks_derives_the_verdict_from_the_exit_code() {
        let host = FakeHost::new(Some(ledger_state("rev-1", &["检查全绿"])));

        // 期望 3、实际 3 → 通过:通过与否只看"实际退出码 == 期望退出码"。
        let (out, results) = run_checks_with(
            &host,
            &std::env::temp_dir(),
            serde_json::json!({
                "checks": [{"command": exit_with(3), "expect_exit_code": 3}],
                "covered": ["req-1"],
            }),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("验收通过"), "{}", out.content);
        assert_eq!(results.len(), 1, "一次调用落一条验证结论");
        assert_eq!(results[0].runs().len(), 1);
        assert_eq!(results[0].runs()[0].exit_code, Some(3));
        assert_eq!(results[0].runs()[0].expect_exit_code, 3);
        assert!(results[0].runs()[0].passed());
        assert_eq!(results[0].verdict(), Some(ValidationVerdict::Passed));

        // 期望 0、实际 3 → 失败。工具本身不算出错(命令确实跑完了),失败的是检查。
        let (out, results) = run_checks_with(
            &host,
            &std::env::temp_dir(),
            serde_json::json!({
                "checks": [{"command": exit_with(3)}],
                "covered": ["req-1"],
            }),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("验收失败"), "{}", out.content);
        assert_eq!(results[0].runs()[0].exit_code, Some(3));
        assert_eq!(results[0].runs()[0].expect_exit_code, 0);
        assert!(!results[0].runs()[0].passed());
        assert_eq!(results[0].verdict(), Some(ValidationVerdict::Failed));
    }

    /// 命令没跑起来,绝不能算通过。
    ///
    /// 两条"没跑起来"的路:命令名不存在(shell 起来了、那条命令没有),以及工作目录
    /// 不存在(进程根本没启动,拿到的是没有退出码的 `not_started`)。两条都按未通过记。
    #[tokio::test]
    async fn run_checks_never_passes_a_command_that_did_not_run() {
        let host = FakeHost::new(Some(ledger_state("rev-1", &["检查全绿"])));

        let (out, results) = run_checks_with(
            &host,
            &std::env::temp_dir(),
            serde_json::json!({
                "checks": [{"command": "denia-no-such-binary-9f3a --version"}],
                "covered": ["req-1"],
            }),
        )
        .await;
        assert_eq!(results.len(), 1, "没跑起来也要落号:按未通过记");
        let run = &results[0].runs()[0];
        assert_ne!(run.exit_code, Some(0), "找不到的命令不可能拿到 0");
        assert!(!run.passed());
        assert_eq!(results[0].verdict(), Some(ValidationVerdict::Failed));
        assert!(out.content.contains("验收失败"), "{}", out.content);

        // 工作目录不存在 → 进程起不来:`not_started`,记录里没有退出码。
        let (out, results) = run_checks_with(
            &host,
            &std::env::temp_dir(),
            serde_json::json!({
                "checks": [{"command": exit_with(0), "workdir": "denia-missing-dir-9f3a"}],
                "covered": ["req-1"],
            }),
        )
        .await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].runs()[0].exit_code, None);
        assert!(!results[0].runs()[0].passed());
        assert_eq!(results[0].verdict(), Some(ValidationVerdict::Failed));
        assert!(
            out.is_error,
            "一条都没跑起来 = 这个环境跑不了检查:{}",
            out.content
        );
        assert!(out.content.contains("[未启动]"), "{}", out.content);
        assert_eq!(
            out.report
                .as_ref()
                .and_then(|report| report.end_reason.as_deref()),
            Some(end_reason::SPAWN_FAILED)
        );
    }

    /// 结论绑住当时的 revision 与输入字节:改了输入,新记录算出的指纹就该变;
    /// 折叠层据此把旧结论判成"输入已变",不再当可用结论。
    #[tokio::test]
    async fn run_checks_freezes_the_revision_and_the_input_bytes() {
        let dir = std::env::temp_dir().join(format!("denia-task-inputs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("input.txt");
        std::fs::write(&input, "v1").unwrap();

        let host = FakeHost::new(Some(ledger_state("rev-1", &["检查全绿"])));
        let arguments = serde_json::json!({
            "checks": [{"command": exit_with(0)}],
            "inputs": ["input.txt"],
            "covered": ["req-1"],
        });

        let (out, first) = run_checks_with(&host, &dir, arguments.clone()).await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].revision().as_str(), "rev-1", "结论绑当前 revision");
        let frozen = first[0].fingerprints();
        assert_eq!(frozen.len(), 1, "声明的输入要被冻进结论");
        assert_eq!(frozen[0].path, "input.txt", "路径按工作区内的相对口径记");
        let before = frozen[0].digest.clone();
        assert!(!before.is_empty());

        std::fs::write(&input, "v2").unwrap();
        let (out, second) = run_checks_with(&host, &dir, arguments).await;
        assert!(!out.is_error, "{}", out.content);
        let after = second[0].fingerprints()[0].digest.clone();
        assert_ne!(after, before, "改过的输入必须算出不同指纹");
        assert_eq!(second[0].fingerprints()[0].path, "input.txt");

        // 折叠侧:指纹基线被后续变更改到 v2 之后,绑 v1 的旧结论不再是可用结论。
        let mut state = ledger_state("rev-1", &["检查全绿"]);
        state.status = TaskStatus::Closed;
        state.validations = vec![first[0].clone()];
        state.fingerprints = vec![FileFingerprint {
            path: "input.txt".to_string(),
            digest: after.clone(),
        }];
        assert_eq!(
            state.verification(),
            VerificationState::Unverified {
                reason: StaleReason::InputsChanged {
                    paths: vec!["input.txt".to_string()],
                },
            }
        );
        assert_eq!(state.outcome(), Some(TaskOutcome::Unverified));

        // 基线仍是验证时那份字节时,同一条结论才是可用的通过结论。
        state.fingerprints = vec![FileFingerprint {
            path: "input.txt".to_string(),
            digest: before,
        }];
        assert_eq!(state.outcome(), Some(TaskOutcome::Passed));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 声明覆盖了验收项才算"验收通过";没声明覆盖只是"未验证完成"。
    #[test]
    fn covered_declaration_decides_whether_acceptance_passed() {
        let revision = RevisionId::new("rev-1");
        let conclusion = |covered: Vec<RequirementId>| {
            ValidationResult::from_check_runs(
                revision.clone(),
                vec![CheckRun {
                    command: "cargo test".to_string(),
                    exit_code: Some(0),
                    expect_exit_code: 0,
                    workdir: ".".to_string(),
                    fingerprints: Vec::new(),
                }],
                covered,
                None,
                7,
            )
            .expect("有一次真实运行就有结论")
        };
        let mut state = ledger_state("rev-1", &["检查全绿", "无回归"]);
        state.status = TaskStatus::Closed;

        state.validations = vec![conclusion(vec![
            RequirementId::new("req-1"),
            RequirementId::new("req-2"),
        ])];
        assert_eq!(state.outcome(), Some(TaskOutcome::Passed));

        // 检查通过只代表它声明的范围:少覆盖一条就只是"未验证完成"。
        state.validations = vec![conclusion(vec![RequirementId::new("req-1")])];
        assert_eq!(state.outcome(), Some(TaskOutcome::Unverified));

        state.validations = vec![conclusion(Vec::new())];
        assert_eq!(state.outcome(), Some(TaskOutcome::Unverified));
    }

    /// 同一件事从工具面走一遍:`covered` 只收当前 revision 上的验收项,
    /// 收到的那些进结论,结论进裁决。
    #[tokio::test]
    async fn run_checks_covered_flows_into_the_outcome() {
        let host = FakeHost::new(Some(ledger_state("rev-1", &["检查全绿", "无回归"])));

        let (out, rejected) = run_checks_with(
            &host,
            &std::env::temp_dir(),
            serde_json::json!({
                "checks": [{"command": exit_with(0)}],
                "covered": ["req-404"],
            }),
        )
        .await;
        assert!(out.is_error, "{}", out.content);
        assert!(
            out.content.contains("不在当前 revision 上"),
            "{}",
            out.content
        );
        assert!(rejected.is_empty(), "写错 id 不得落任何结论");

        let (out, results) = run_checks_with(
            &host,
            &std::env::temp_dir(),
            serde_json::json!({
                "checks": [{"command": exit_with(0)}],
                "covered": ["req-1", "req-2"],
            }),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0]
                .covered()
                .iter()
                .map(RequirementId::as_str)
                .collect::<Vec<_>>(),
            vec!["req-1", "req-2"]
        );

        let close = |validations: Vec<ValidationResult>| {
            let mut state = ledger_state("rev-1", &["检查全绿", "无回归"]);
            state.status = TaskStatus::Closed;
            state.validations = validations;
            state
        };
        assert_eq!(close(results.clone()).outcome(), Some(TaskOutcome::Passed));

        // 同一份通过记录,只声明覆盖一条 → 结局只是"未验证完成"。
        let (_, partial) = run_checks_with(
            &host,
            &std::env::temp_dir(),
            serde_json::json!({
                "checks": [{"command": exit_with(0)}],
                "covered": ["req-1"],
            }),
        )
        .await;
        assert_eq!(partial.len(), 1);
        assert_eq!(
            close(partial).outcome(),
            Some(TaskOutcome::Unverified),
            "检查通过只代表它声明的覆盖范围"
        );
    }
}
