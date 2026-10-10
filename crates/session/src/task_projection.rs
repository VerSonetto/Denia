//! 任务账本的纯折叠与完成裁决(共享给 server / 工具面 / 前端快照)。
//!
//! 与 `apply_goal_op`(goal 状态机)同一套路,只是账本字段更多、还多一层
//! "证据能不能支撑结论"的裁决。三条不变量:
//!
//! 1. **纯函数、确定性、可重放**:只读 `&[SessionEnvelope]`,不读时钟、不取
//!    随机数、不看磁盘;同一份日志折叠两次逐字段相等。时间字段一律取事件时间。
//! 2. **revision 不复用**:换版事件里的 id 必须是新的(既不在本任务用过的
//!    revision 历史里,也不在宿主声明的 [`TaskFoldScope::retired_revisions`]
//!    里)。复用即拒绝换版,并把当前版本标成 [`TaskState::revision_conflict`] ——
//!    当前结论一律作废,宁可判"未验证"。
//! 3. **缺证据不得通过**:结局由 [`TaskState::outcome`] 算;没有绑在当前
//!    revision、来源属于本会话、且冻结指纹仍然成立的验证结论,就只能是
//!    "未验证完成"。折叠层没有任何入口能写"通过"。
//!
//! 本模块还提供宿主侧的 id 分配助手([`fresh_task_id`] 等)。它们**不属于**
//! 折叠:折叠只读事件里的 id,从不自己造一个 —— 这样重放才确定。

use std::collections::HashSet;

use denia_core::session::{SessionEnvelope, SessionEvent};
use denia_core::task::{
    Assumption, Evidence, EvidenceId, Fact, FactCitation, FailedAttempt, FileFingerprint,
    LedgerOverflow, MAX_ASSUMPTIONS, MAX_CHANGES, MAX_EVIDENCE, MAX_FACTS, MAX_FAILED_ATTEMPTS,
    MAX_OPEN_QUESTIONS, MAX_REMAINING, MAX_REQUIREMENTS, MAX_REVISIONS, MAX_VALIDATIONS, NoteClaim,
    NoteId, OPEN_REVISION_REASON, OpenQuestion, Requirement, RequirementClaim, RequirementId,
    RevisionId, RevisionRecord, SourceRef, TaskId, TaskOp, TaskOutcome, TaskState, TaskStatus,
    ValidationResult, truncate_claim, upsert_fingerprint,
};

/// 折叠作用域:折叠方是谁,以及 rewind 之后哪些 revision 已经报废。
#[derive(Debug, Clone, Default)]
pub struct TaskFoldScope {
    /// 本会话 id。写进折出的 [`TaskState::session`],并决定哪些验证结论算数:
    /// 来源与它不一致的记录(父会话/其它分支 seed 继承来的)只作审计,不进
    /// 当前裁决 —— 子代理不继承父的验收结论。
    pub session: Option<String>,
    /// 已报废的 revision id。rewind 物理截断日志后,这些 id 在日志里已经不存在,
    /// 只有宿主还记得(回退审计/内存);折叠再看到它们就拒绝换版,防止旧分支的
    /// 验证结论被"复用同一个 id"带回新分支。
    pub retired_revisions: Vec<RevisionId>,
}

/// 折叠整份日志得到当前任务账本。`None` = 日志里没有任务事件(旧会话、
/// 普通问答会话)—— 旧日志不阻塞,也不被推导出 verified。
pub fn project_task(events: &[SessionEnvelope]) -> Option<TaskState> {
    project_task_scoped(events, &TaskFoldScope::default())
}

/// [`project_task`] 的带作用域版本(会话身份 + 已报废 revision)。
pub fn project_task_scoped(events: &[SessionEnvelope], scope: &TaskFoldScope) -> Option<TaskState> {
    let mut folder = Folder::new(events, scope);
    for envelope in events {
        folder.apply(envelope);
    }
    folder.state
}

/// 折叠出任务结局;`None` = 没有任务,或任务仍在进行(尚未收口)。
pub fn project_task_outcome(events: &[SessionEnvelope]) -> Option<TaskOutcome> {
    project_task(events).and_then(|state| state.outcome())
}

/// [`project_task_outcome`] 的带作用域版本。
pub fn project_task_outcome_scoped(
    events: &[SessionEnvelope],
    scope: &TaskFoldScope,
) -> Option<TaskOutcome> {
    project_task_scoped(events, scope).and_then(|state| state.outcome())
}

/// 宿主侧 id 分配:任务 id。
///
/// 稳定、不随 rewind / 分支改变;折叠与证据引用只认它。uuid v4 不依赖日志
/// 状态,所以 rewind 之后不会与截断掉的旧身份撞号。
pub fn fresh_task_id() -> TaskId {
    TaskId::new(format!("task-{}", uuid::Uuid::new_v4()))
}

/// 宿主侧 id 分配:revision id。
///
/// **必须**从不会复用的来源取。任务要求/验收项一变就换一版,而 rewind 会
/// 把旧分支的事件整段截掉:如果新版本沿用旧 id,旧分支的验证结论就会"对上号"
/// 被当成当前结论。折叠端另有一道拒绝复用旧 id 的防线,但那是兜底,不是许可。
pub fn fresh_revision_id() -> RevisionId {
    RevisionId::new(format!("rev-{}", uuid::Uuid::new_v4()))
}

/// 宿主侧 id 分配:证据 id(证据关联用它,不用会被 fork 重编号的 seq)。
pub fn fresh_evidence_id() -> EvidenceId {
    EvidenceId::new(format!("ev-{}", uuid::Uuid::new_v4()))
}

/// 宿主侧 id 分配:要求/验收项 id(验证结论按它声明覆盖范围)。
pub fn fresh_requirement_id() -> RequirementId {
    RequirementId::new(format!("req-{}", uuid::Uuid::new_v4()))
}

/// 宿主侧 id 分配:断言 id(未决问题按它被关闭)。
pub fn fresh_note_id() -> NoteId {
    NoteId::new(format!("note-{}", uuid::Uuid::new_v4()))
}

/// 折叠器:一缕事件一条状态。字段都是折叠期的簿记,不落盘。
struct Folder<'a> {
    events: &'a [SessionEnvelope],
    scope: &'a TaskFoldScope,
    /// 已经用过的 revision id(含宿主声明报废的):换版必须换新 id。
    retired: HashSet<RevisionId>,
    state: Option<TaskState>,
}

impl<'a> Folder<'a> {
    fn new(events: &'a [SessionEnvelope], scope: &'a TaskFoldScope) -> Self {
        Self {
            events,
            scope,
            retired: scope.retired_revisions.iter().cloned().collect(),
            state: None,
        }
    }

    fn apply(&mut self, envelope: &SessionEnvelope) {
        if let SessionEvent::Task { op } = &envelope.event {
            self.apply_op(op, envelope.time);
        }
    }

    fn apply_op(&mut self, op: &TaskOp, now: u64) {
        match op {
            TaskOp::Open {
                task_id,
                revision,
                goal,
                requirements,
            } => self.open(task_id, revision, goal, requirements, now),
            TaskOp::Revise {
                revision,
                reason,
                goal,
                requirements,
            } => self.revise(revision, reason, goal, requirements, now),
            TaskOp::Amend { notes } => self.amend(notes, now),
            TaskOp::ResolveQuestion { id } => self.resolve_question(id, now),
            TaskOp::SetRemaining { items } => self.set_remaining(items, now),
            TaskOp::RecordChange {
                summary,
                fingerprints,
            } => self.record_change(summary, fingerprints, now),
            TaskOp::RecordEvidence { evidence } => self.record_evidence(evidence, now),
            TaskOp::RecordValidation { result } => self.record_validation(result, now),
            TaskOp::Block { reason } => self.block(reason, now),
            TaskOp::Unblock => self.unblock(now),
            TaskOp::Close => self.close(now),
        }
    }

    /// 换版是否被接受:新 id 必须是全新的。
    fn accepts_revision(&self, id: &RevisionId) -> bool {
        if self.retired.contains(id) {
            return false;
        }
        match &self.state {
            Some(state) => !state.used_revisions().any(|used| used == id),
            None => true,
        }
    }

    /// 拒绝换版:当前版本的要求集已经变了却没有合法的新身份,所以当前结论
    /// 一律不可信(见 [`TaskState::revision_conflict`])。拒绝本身也要可见。
    fn reject_revision(&mut self, now: u64) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        state.revision_conflict = true;
        state.revision_rejected = state.revision_rejected.saturating_add(1);
        state.updated_at = now;
    }

    fn open(
        &mut self,
        task_id: &TaskId,
        revision: &RevisionId,
        goal: &str,
        requirements: &[RequirementClaim],
        now: u64,
    ) {
        // 已有任务:未收口时忽略(不能偷偷换身份);已收口时视为用户重新出发,
        // 替换旧任务(与 `GoalOp::Set` 对待 complete 目标同策)。
        if let Some(existing) = &self.state {
            if existing.status != TaskStatus::Closed {
                return;
            }
            // 旧任务的 revision 一并进报废集:新任务也不能复用旧身份。
            for record in &existing.revisions {
                self.retired.insert(record.id.clone());
            }
        }
        if !self.accepts_revision(revision) {
            self.reject_revision(now);
            return;
        }
        let mut state = TaskState {
            id: task_id.clone(),
            revision: revision.clone(),
            // 折叠方身份写进账本:结论裁决据此识别外来记录。
            session: self.scope.session.clone(),
            status: TaskStatus::Active,
            goal: truncate_claim(goal),
            blocked_reason: None,
            revision_conflict: false,
            revision_rejected: 0,
            revisions: Vec::new(),
            requirements: Vec::new(),
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
            created_at: now,
            updated_at: now,
        };
        // 初版 revision 没有"上一版"可解释,理由用固定值保持记录形状统一。
        state.revisions.push(RevisionRecord {
            id: revision.clone(),
            ordinal: 1,
            reason: OPEN_REVISION_REASON.to_string(),
            at: now,
        });
        for claim in requirements {
            push_requirement(&mut state, claim, revision, now);
        }
        self.state = Some(state);
    }

    fn revise(
        &mut self,
        revision: &RevisionId,
        reason: &str,
        goal: &Option<String>,
        requirements: &[RequirementClaim],
        now: u64,
    ) {
        if self.state.is_none() {
            return;
        }
        if !self.accepts_revision(revision) {
            self.reject_revision(now);
            return;
        }
        // 上一版进报废集:即便之后有人把旧 id 再写回来,也不会被当成本版。
        if let Some(state) = self.state.as_ref() {
            self.retired.insert(state.revision.clone());
        }
        let state = self.state.as_mut().expect("checked above");
        state.revision = revision.clone();
        // 新身份是干净的:上一次的冲突到此为止(旧结论本来也绑在旧 id 上)。
        state.revision_conflict = false;
        // 受阻是外部条件,换版不解除它;其余情形视为任务重新活跃(已收口的
        // 任务被换版 = 用户加了新要求,重新算在办)。
        if state.status != TaskStatus::Blocked {
            state.status = TaskStatus::Active;
        }
        let ordinal = state.revisions.len() as u32 + 1;
        state.overflow.revisions += push_bounded(
            &mut state.revisions,
            RevisionRecord {
                id: revision.clone(),
                ordinal,
                reason: truncate_claim(reason),
                at: now,
            },
            MAX_REVISIONS,
        );
        if let Some(goal) = goal
            && !goal.trim().is_empty()
        {
            state.goal = truncate_claim(goal);
        }
        for claim in requirements {
            push_requirement(state, claim, revision, now);
        }
        state.updated_at = now;
    }

    fn amend(&mut self, notes: &[NoteClaim], now: u64) {
        let events = self.events;
        let Some(state) = self.state.as_mut() else {
            return;
        };
        let revision = state.revision.clone();
        for claim in notes {
            match claim {
                NoteClaim::Fact { id, text, citation } => {
                    // 引用要能核对:用户原话必须在日志里,证据必须在账本上。
                    let resolved = match citation {
                        FactCitation::User { reference }
                            if reference_resolves(events, reference) =>
                        {
                            Some(citation.clone())
                        }
                        FactCitation::Evidence { evidence }
                            if state.evidence.iter().any(|item| &item.id == evidence) =>
                        {
                            Some(citation.clone())
                        }
                        _ => None,
                    };
                    match resolved {
                        Some(citation) => {
                            state.overflow.facts += push_bounded(
                                &mut state.facts,
                                Fact {
                                    id: id.clone(),
                                    revision: revision.clone(),
                                    text: truncate_claim(text),
                                    citation,
                                    at: now,
                                },
                                MAX_FACTS,
                            );
                        }
                        None => {
                            // 核不到引用的事实主张只能算假设,并把原因写明 ——
                            // 这正是"复制一份文本就算出处"的堵口。
                            let rationale = match citation {
                                FactCitation::User { .. } => {
                                    "引用的用户消息在日志里核对不到,按假设处理"
                                }
                                FactCitation::Evidence { .. } => "引用的证据不在账本上,按假设处理",
                            };
                            state.overflow.assumptions += push_bounded(
                                &mut state.assumptions,
                                Assumption {
                                    id: id.clone(),
                                    revision: revision.clone(),
                                    text: truncate_claim(text),
                                    rationale: Some(rationale.to_string()),
                                    at: now,
                                },
                                MAX_ASSUMPTIONS,
                            );
                        }
                    }
                }
                NoteClaim::Assumption {
                    id,
                    text,
                    rationale,
                } => {
                    state.overflow.assumptions += push_bounded(
                        &mut state.assumptions,
                        Assumption {
                            id: id.clone(),
                            revision: revision.clone(),
                            text: truncate_claim(text),
                            rationale: rationale.as_deref().map(truncate_claim),
                            at: now,
                        },
                        MAX_ASSUMPTIONS,
                    );
                }
                NoteClaim::FailedAttempt { id, text, evidence } => {
                    // 失败尝试不是事实主张:证据引用核不到就退回"无证据",
                    // 不去编造。
                    let evidence = evidence
                        .clone()
                        .filter(|id| state.evidence.iter().any(|item| &item.id == id));
                    state.overflow.failed_attempts += push_bounded(
                        &mut state.failed_attempts,
                        FailedAttempt {
                            id: id.clone(),
                            revision: revision.clone(),
                            text: truncate_claim(text),
                            evidence,
                            at: now,
                        },
                        MAX_FAILED_ATTEMPTS,
                    );
                }
                NoteClaim::OpenQuestion { id, text } => {
                    state.overflow.open_questions += push_bounded(
                        &mut state.open_questions,
                        OpenQuestion {
                            id: id.clone(),
                            revision: revision.clone(),
                            text: truncate_claim(text),
                            at: now,
                        },
                        MAX_OPEN_QUESTIONS,
                    );
                }
            }
        }
        state.updated_at = now;
    }

    fn resolve_question(&mut self, id: &NoteId, now: u64) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        let before = state.open_questions.len();
        state.open_questions.retain(|item| &item.id != id);
        if state.open_questions.len() != before {
            state.updated_at = now;
        }
    }

    fn set_remaining(&mut self, items: &[String], now: u64) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        state.overflow.remaining += items.len().saturating_sub(MAX_REMAINING) as u32;
        state.remaining = items
            .iter()
            .take(MAX_REMAINING)
            .map(|item| truncate_claim(item))
            .collect();
        state.updated_at = now;
    }

    fn record_change(&mut self, summary: &str, fingerprints: &[FileFingerprint], now: u64) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        // 指纹基线更新 = 旧验证的失效判据;所以这一步必须发生在收口之后也照算
        // ("验证后又被改过"不能靠时间顺序赖掉)。
        for fingerprint in fingerprints {
            upsert_fingerprint(&mut state.fingerprints, fingerprint);
        }
        state.overflow.changes += push_bounded(
            &mut state.changes,
            denia_core::task::TaskChange {
                revision: state.revision.clone(),
                summary: truncate_claim(summary),
                fingerprints: fingerprints.to_vec(),
                at: now,
            },
            MAX_CHANGES,
        );
        state.updated_at = now;
    }

    fn record_evidence(&mut self, evidence: &Evidence, now: u64) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        // 外来来源(父/分支 seed)照样留档供审计;能"继承"的只有验证结论,
        // 而结论由 `TaskState::verification` 按来源挡掉。
        let mut evidence = evidence.clone();
        evidence.summary = truncate_claim(&evidence.summary);
        state.overflow.evidence += push_bounded(&mut state.evidence, evidence, MAX_EVIDENCE);
        state.updated_at = now;
    }

    fn record_validation(&mut self, result: &ValidationResult, now: u64) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        // 绑在别的 revision、来源不属于本会话、指纹已被推翻的记录都留档,
        // 是否算数由 `TaskState::verification` 裁决。
        state.overflow.validations +=
            push_bounded(&mut state.validations, result.clone(), MAX_VALIDATIONS);
        state.updated_at = now;
    }

    fn block(&mut self, reason: &str, now: u64) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if state.status == TaskStatus::Closed {
            return;
        }
        state.status = TaskStatus::Blocked;
        state.blocked_reason = (!reason.trim().is_empty()).then(|| truncate_claim(reason));
        state.updated_at = now;
    }

    fn unblock(&mut self, now: u64) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if state.status == TaskStatus::Blocked {
            state.status = TaskStatus::Active;
            state.blocked_reason = None;
            state.updated_at = now;
        }
    }

    fn close(&mut self, now: u64) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        // 受阻时谈"收口"没有意义(结局已经是外部受阻),先 `Unblock`;已收口
        // 再收口是无操作。
        if state.status == TaskStatus::Active {
            state.status = TaskStatus::Closed;
            state.updated_at = now;
        }
    }
}

/// 追加一条要求,超出上限时保留最新的并把丢弃条数记账。
fn push_requirement(
    state: &mut TaskState,
    claim: &RequirementClaim,
    revision: &RevisionId,
    now: u64,
) {
    state.overflow.requirements += push_bounded(
        &mut state.requirements,
        Requirement {
            id: claim.id.clone(),
            revision: revision.clone(),
            kind: claim.kind,
            text: truncate_claim(&claim.text),
            source: claim.source.clone(),
            declared_at: now,
        },
        MAX_REQUIREMENTS,
    );
}

/// 追加并把集合压到上限:保留最新的,返回丢弃条数。
///
/// 上限造成的损失必须可见 —— 调用方把它累加进 [`LedgerOverflow`],"用户约束
/// 被悄悄删掉"正是要防的事。
fn push_bounded<T>(items: &mut Vec<T>, item: T, cap: usize) -> u32 {
    items.push(item);
    let dropped = items.len().saturating_sub(cap);
    if dropped > 0 {
        items.drain(0..dropped);
    }
    dropped as u32
}

/// 来源引用能不能在日志里核对到"用户原话"那一条事件。
///
/// 先按 `time_ms`(分支种子回放保留源事件时间戳,重编号也稳定),再退化到
/// `seq`(同一份日志内的坐标)。两边都对不上就是核不到 —— 引用不算数,主张
/// 降级为假设。
fn reference_resolves(events: &[SessionEnvelope], reference: &SourceRef) -> bool {
    let is_user_message = |envelope: &SessionEnvelope| {
        matches!(
            &envelope.event,
            SessionEvent::UserMessage {
                injected: false,
                ..
            }
        )
    };
    if reference.time_ms != 0
        && events
            .iter()
            .any(|envelope| envelope.time == reference.time_ms && is_user_message(envelope))
    {
        return true;
    }
    reference.seq != 0
        && events
            .iter()
            .any(|envelope| envelope.seq == reference.seq && is_user_message(envelope))
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::task::{CheckRun, EvidenceSource, RequirementKind, StaleReason};

    fn envelope(seq: u64, event: SessionEvent) -> SessionEnvelope {
        SessionEnvelope {
            seq,
            time: 1_000 + seq,
            event,
        }
    }

    fn user(seq: u64) -> SessionEnvelope {
        envelope(
            seq,
            SessionEvent::UserMessage {
                text: "修好解析器,跑通测试再说完事".into(),
                injected: false,
                channel: None,
                images: Vec::new(),
            },
        )
    }

    fn task(seq: u64, op: TaskOp) -> SessionEnvelope {
        envelope(seq, SessionEvent::Task { op })
    }

    fn rev(id: &str) -> RevisionId {
        RevisionId::new(id)
    }

    /// 指向 seq 这条用户消息的来源引用(时间戳与日志里那条一致)。
    fn cite(seq: u64) -> SourceRef {
        SourceRef {
            session: None,
            seq,
            time_ms: 1_000 + seq,
            quote: Some("跑通测试".into()),
        }
    }

    fn acceptance(id: &str, seq: u64) -> RequirementClaim {
        RequirementClaim {
            id: RequirementId::new(id),
            kind: RequirementKind::Acceptance,
            text: "cargo test 全绿".into(),
            source: cite(seq),
        }
    }

    fn open(revision: &str, requirements: Vec<RequirementClaim>) -> TaskOp {
        TaskOp::Open {
            task_id: TaskId::new("task-1"),
            revision: rev(revision),
            goal: "修好解析器".into(),
            requirements,
        }
    }

    /// 一次通过的检查运行:冻结 `src/lib.rs` 的摘要 `digest`。
    fn passing(
        revision: &str,
        covered: &[&str],
        digest: &str,
        origin: Option<&str>,
        at: u64,
    ) -> ValidationResult {
        ValidationResult::from_check_runs(
            rev(revision),
            vec![CheckRun {
                command: "cargo test".into(),
                exit_code: Some(0),
                expect_exit_code: 0,
                workdir: ".".into(),
                fingerprints: vec![FileFingerprint {
                    path: "src/lib.rs".into(),
                    digest: digest.into(),
                }],
            }],
            covered.iter().map(|id| RequirementId::new(*id)).collect(),
            origin.map(str::to_string),
            at,
        )
        .expect("有检查运行才构成结论")
    }

    fn evidence(id: &str) -> EvidenceSource {
        EvidenceSource::ToolCall {
            call_id: format!("call-{id}"),
            tool: "read_file".into(),
        }
    }

    #[test]
    fn legacy_log_without_task_events_folds_to_empty_state() {
        // 旧日志(与普通问答会话):没有任务事件 → 折叠出"没有任务",
        // 既不阻塞也不被推导出 verified。
        let events = vec![user(1), user(2), user(3)];
        assert_eq!(project_task(&events), None);
        assert_eq!(project_task_outcome(&events), None);
    }

    #[test]
    fn fold_is_deterministic_and_replayable() {
        let events = vec![
            user(1),
            task(2, open("rev-a", vec![acceptance("req-1", 1)])),
            task(
                3,
                TaskOp::RecordChange {
                    summary: "改了 src/lib.rs 的解析分支".into(),
                    fingerprints: vec![FileFingerprint {
                        path: "src/lib.rs".into(),
                        digest: "d1".into(),
                    }],
                },
            ),
            task(
                4,
                TaskOp::RecordEvidence {
                    evidence: Evidence {
                        id: EvidenceId::new("ev-1"),
                        revision: rev("rev-a"),
                        source: evidence("ev-1"),
                        fingerprints: vec![FileFingerprint {
                            path: "src/lib.rs".into(),
                            digest: "d1".into(),
                        }],
                        summary: "读到解析分支".into(),
                        origin_session: None,
                        at: 1_004,
                    },
                },
            ),
            task(
                5,
                TaskOp::Amend {
                    notes: vec![NoteClaim::Fact {
                        id: NoteId::new("n1"),
                        text: "解析分支在 src/lib.rs".into(),
                        citation: FactCitation::Evidence {
                            evidence: EvidenceId::new("ev-1"),
                        },
                    }],
                },
            ),
            task(
                6,
                TaskOp::SetRemaining {
                    items: vec!["补一条回归测试".into()],
                },
            ),
            task(
                7,
                TaskOp::RecordValidation {
                    result: passing("rev-a", &["req-1"], "d1", None, 1_007),
                },
            ),
            task(8, TaskOp::Close),
        ];
        let first = project_task(&events).expect("有任务事件就有账本");
        let second = project_task(&events).expect("重放必须得到同一份账本");
        assert_eq!(first, second, "折叠两次必须逐字段相等");
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap(),
            "序列化形状也必须一致(快照/绑定同源)"
        );
        // 账本内容抽查:来源引用、指纹基线、结局。
        assert_eq!(first.requirements.len(), 1);
        assert_eq!(first.requirements[0].source.seq, 1);
        assert_eq!(first.fingerprint_of("src/lib.rs"), Some("d1"));
        assert_eq!(first.facts.len(), 1);
        assert_eq!(first.remaining, vec!["补一条回归测试".to_string()]);
        assert_eq!(first.outcome(), Some(TaskOutcome::Passed));
        assert_eq!(first.overflow, LedgerOverflow::default());
    }

    #[test]
    fn revision_reuse_after_rewind_is_rejected_and_invalidates_conclusions() {
        // 完整分支:rev-a 上跑过检查、收口 → 验收通过。
        let full = vec![
            user(1),
            task(2, open("rev-a", vec![acceptance("req-1", 1)])),
            task(
                3,
                TaskOp::RecordValidation {
                    result: passing("rev-a", &["req-1"], "d1", None, 1_003),
                },
            ),
            task(4, TaskOp::Close),
        ];
        assert_eq!(project_task_outcome(&full), Some(TaskOutcome::Passed));

        // rewind 到任务创建之后(把验证与收口截掉),宿主在旧状态上继续推进却
        // 复用了 rev-a:换版必须被拒,而且当前结论作废 —— 旧分支的"通过"不能
        // 靠复用同一个 id 借尸还魂。
        let after_rewind = vec![
            user(1),
            task(2, open("rev-a", vec![acceptance("req-1", 1)])),
            task(
                3,
                TaskOp::Revise {
                    revision: rev("rev-a"),
                    reason: "用户追加了验收项".into(),
                    goal: None,
                    requirements: vec![acceptance("req-2", 1)],
                },
            ),
            task(
                4,
                TaskOp::RecordValidation {
                    result: passing("rev-a", &["req-1"], "d1", None, 1_004),
                },
            ),
            task(5, TaskOp::Close),
        ];
        let state = project_task(&after_rewind).expect("账本还在");
        assert_eq!(state.revision_rejected, 1, "复用旧 id 的换版被拒");
        assert!(state.revision_conflict, "当前版本身份不干净必须可见");
        assert_eq!(state.revisions.len(), 1, "被拒的换版不落新 revision");
        assert_eq!(state.requirements.len(), 1, "被拒的换版不落新要求");
        assert_eq!(
            state.verification(),
            denia_core::task::VerificationState::Unverified {
                reason: StaleReason::RevisionConflict
            }
        );
        assert_eq!(
            state.outcome(),
            Some(TaskOutcome::Unverified),
            "复用旧 revision id 之后,旧的通过结论不得被继承"
        );
    }

    #[test]
    fn retired_revision_from_before_the_rewind_is_rejected() {
        // rewind 到任务创建之前:日志里连 rev-a 都没有,只有宿主记得它报废过。
        // 不加报废集时同一份日志会被判通过 —— 说明这道防线确实在起作用。
        let replayed = vec![
            user(1),
            task(2, open("rev-a", vec![acceptance("req-1", 1)])),
            task(
                3,
                TaskOp::RecordValidation {
                    result: passing("rev-a", &["req-1"], "d1", None, 1_003),
                },
            ),
            task(4, TaskOp::Close),
        ];
        assert_eq!(
            project_task_outcome(&replayed),
            Some(TaskOutcome::Passed),
            "没有报废集时,复用 rev-a 的日志确实能判通过"
        );
        let scope = TaskFoldScope {
            session: None,
            retired_revisions: vec![rev("rev-a")],
        };
        let state = project_task_scoped(&replayed, &scope);
        assert!(state.is_none(), "宿主声明 rev-a 已报废后,复用被拒");
    }

    #[test]
    fn fresh_revision_after_rewind_does_not_inherit_the_old_conclusion() {
        // 正确做法:rewind 之后换新 id。旧结论绑在旧 id 上,新版本从未验证开始。
        let events = vec![
            user(1),
            task(2, open("rev-a", vec![acceptance("req-1", 1)])),
            task(
                3,
                TaskOp::RecordValidation {
                    result: passing("rev-a", &["req-1"], "d1", None, 1_003),
                },
            ),
            task(
                4,
                TaskOp::Revise {
                    // 同一条要求,但换了一版身份(宿主重新分配 id)。
                    revision: rev("rev-b"),
                    reason: "rewind 后重新推进".into(),
                    goal: None,
                    requirements: vec![acceptance("req-1", 1)],
                },
            ),
            task(5, TaskOp::Close),
        ];
        let state = project_task(&events).expect("账本在");
        assert_eq!(state.revision, rev("rev-b"));
        assert!(!state.revision_conflict);
        assert_eq!(
            state.verification(),
            denia_core::task::VerificationState::Unverified {
                reason: StaleReason::RevisionChanged
            }
        );
        assert_eq!(
            state.outcome(),
            Some(TaskOutcome::Unverified),
            "新 revision 不继承旧 revision 的验证结论"
        );
    }

    #[test]
    fn missing_or_uncovered_evidence_never_passes() {
        // 没有验收定义时收口:可以结束,但不能冒充 verified。
        let no_acceptance = vec![
            user(1),
            task(2, open("rev-a", Vec::new())),
            task(3, TaskOp::Close),
        ];
        assert_eq!(
            project_task_outcome(&no_acceptance),
            Some(TaskOutcome::Unverified)
        );

        // 有验收定义但没跑过检查:同样只是"未验证完成"。
        let unchecked = vec![
            user(1),
            task(2, open("rev-a", vec![acceptance("req-1", 1)])),
            task(3, TaskOp::Close),
        ];
        assert_eq!(
            project_task_outcome(&unchecked),
            Some(TaskOutcome::Unverified)
        );

        // 跑了检查但没声明覆盖任何验收项:检查通过只代表它声明的范围。
        let uncovered = vec![
            user(1),
            task(2, open("rev-a", vec![acceptance("req-1", 1)])),
            task(
                3,
                TaskOp::RecordValidation {
                    result: passing("rev-a", &[], "d1", None, 1_003),
                },
            ),
            task(4, TaskOp::Close),
        ];
        assert_eq!(
            project_task_outcome(&uncovered),
            Some(TaskOutcome::Unverified)
        );
    }

    #[test]
    fn change_after_validation_invalidates_the_conclusion() {
        // 收口之后文件又被改过(指纹变了):旧结论作废,结局退回未验证。
        let events = vec![
            user(1),
            task(2, open("rev-a", vec![acceptance("req-1", 1)])),
            task(
                3,
                TaskOp::RecordValidation {
                    result: passing("rev-a", &["req-1"], "d1", None, 1_003),
                },
            ),
            task(4, TaskOp::Close),
            task(
                5,
                TaskOp::RecordChange {
                    summary: "收口后又改了一行".into(),
                    fingerprints: vec![FileFingerprint {
                        path: "src/lib.rs".into(),
                        digest: "d2".into(),
                    }],
                },
            ),
        ];
        let state = project_task(&events).expect("账本在");
        assert_eq!(
            state.verification(),
            denia_core::task::VerificationState::Unverified {
                reason: StaleReason::InputsChanged {
                    paths: vec!["src/lib.rs".into()]
                }
            }
        );
        assert_eq!(state.outcome(), Some(TaskOutcome::Unverified));
    }

    #[test]
    fn subagent_does_not_inherit_the_parent_verification() {
        // 父会话写的结论被 seed 进子会话(或折叠方是另一个会话):只作审计。
        let events = vec![
            user(1),
            task(2, open("rev-a", vec![acceptance("req-1", 1)])),
            task(
                3,
                TaskOp::RecordValidation {
                    result: passing("rev-a", &["req-1"], "d1", Some("parent"), 1_003),
                },
            ),
            task(4, TaskOp::Close),
        ];
        let child_scope = TaskFoldScope {
            session: Some("child".into()),
            retired_revisions: Vec::new(),
        };
        let child = project_task_scoped(&events, &child_scope).expect("账本在");
        assert_eq!(child.session.as_deref(), Some("child"));
        assert_eq!(
            child.verification(),
            denia_core::task::VerificationState::Unverified {
                reason: StaleReason::ForeignOrigin
            }
        );
        assert_eq!(child.outcome(), Some(TaskOutcome::Unverified));
        // 不声明会话身份时也一样保守:标注了来源、但与本折叠方不一致的记录不算数。
        assert_eq!(project_task_outcome(&events), Some(TaskOutcome::Unverified));
    }

    #[test]
    fn unsourced_fact_claims_are_demoted_to_assumptions() {
        let events = vec![
            user(1),
            task(2, open("rev-a", vec![acceptance("req-1", 1)])),
            task(
                3,
                TaskOp::RecordEvidence {
                    evidence: Evidence {
                        id: EvidenceId::new("ev-1"),
                        revision: rev("rev-a"),
                        source: evidence("ev-1"),
                        fingerprints: Vec::new(),
                        summary: "读到解析分支".into(),
                        origin_session: None,
                        at: 1_003,
                    },
                },
            ),
            task(
                4,
                TaskOp::Amend {
                    notes: vec![
                        // 引用得到用户原话:是事实。
                        NoteClaim::Fact {
                            id: NoteId::new("n1"),
                            text: "用户要求跑通测试".into(),
                            citation: FactCitation::User { reference: cite(1) },
                        },
                        // 引用的用户消息在日志里核不到:降级为假设。
                        NoteClaim::Fact {
                            id: NoteId::new("n2"),
                            text: "用户要求兼容 Windows".into(),
                            citation: FactCitation::User {
                                reference: SourceRef {
                                    session: None,
                                    seq: 99,
                                    time_ms: 9_999,
                                    quote: Some("要兼容 Windows".into()),
                                },
                            },
                        },
                        // 引用的证据不在账本上:同样只能算假设。
                        NoteClaim::Fact {
                            id: NoteId::new("n3"),
                            text: "测试已经全绿了".into(),
                            citation: FactCitation::Evidence {
                                evidence: EvidenceId::new("ev-404"),
                            },
                        },
                        // 引用得到账本上的证据:是事实。
                        NoteClaim::Fact {
                            id: NoteId::new("n4"),
                            text: "解析分支在 src/lib.rs".into(),
                            citation: FactCitation::Evidence {
                                evidence: EvidenceId::new("ev-1"),
                            },
                        },
                    ],
                },
            ),
        ];
        let state = project_task(&events).expect("账本在");
        let facts: Vec<&str> = state.facts.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(facts, vec!["n1", "n4"], "只有核得到的引用才配叫事实");
        let demoted: Vec<&str> = state
            .assumptions
            .iter()
            .map(|item| item.id.as_str())
            .collect();
        assert_eq!(demoted, vec!["n2", "n3"], "核不到引用的一律降级为假设");
        for item in &state.assumptions {
            assert!(
                item.rationale
                    .as_deref()
                    .is_some_and(|text| text.contains("按假设处理")),
                "降级原因必须写明: {item:?}"
            );
        }
    }

    #[test]
    fn ledger_bounds_hold_and_dropped_items_are_counted() {
        let mut events = vec![
            user(1),
            task(2, open("rev-a", vec![acceptance("req-1", 1)])),
        ];
        let mut seq = 3;
        let assumptions = MAX_ASSUMPTIONS + 5;
        for index in 0..assumptions {
            events.push(task(
                seq,
                TaskOp::Amend {
                    notes: vec![NoteClaim::Assumption {
                        id: NoteId::new(format!("note-{index}")),
                        text: format!("猜测 {index}"),
                        rationale: None,
                    }],
                },
            ));
            seq += 1;
        }
        let evidences = MAX_EVIDENCE + 3;
        for index in 0..evidences {
            events.push(task(
                seq,
                TaskOp::RecordEvidence {
                    evidence: Evidence {
                        id: EvidenceId::new(format!("ev-{index}")),
                        revision: rev("rev-a"),
                        source: evidence(&format!("ev-{index}")),
                        fingerprints: Vec::new(),
                        summary: "读到一段".into(),
                        origin_session: None,
                        at: 1_000 + seq,
                    },
                },
            ));
            seq += 1;
        }
        let state = project_task(&events).expect("账本在");
        assert_eq!(
            state.assumptions.len(),
            MAX_ASSUMPTIONS,
            "假设条目按上限截住"
        );
        assert_eq!(
            state.overflow.assumptions as usize,
            assumptions - MAX_ASSUMPTIONS,
            "被丢弃的假设条数必须记账"
        );
        assert_eq!(state.evidence.len(), MAX_EVIDENCE, "证据条目按上限截住");
        assert_eq!(
            state.overflow.evidence as usize,
            evidences - MAX_EVIDENCE,
            "被丢弃的证据条数必须记账"
        );
        // 保留的是最新的:最早的 note-0 已被挤掉,最新的还在。
        assert_eq!(
            state.assumptions.first().map(|item| item.id.as_str()),
            Some("note-5")
        );
    }

    #[test]
    fn blocked_and_unblocked_are_visible_in_the_outcome() {
        let blocked = vec![
            user(1),
            task(2, open("rev-a", vec![acceptance("req-1", 1)])),
            task(
                3,
                TaskOp::Block {
                    reason: "等用户提供凭据".into(),
                },
            ),
        ];
        let state = project_task(&blocked).expect("账本在");
        assert_eq!(state.status, TaskStatus::Blocked);
        assert_eq!(state.blocked_reason.as_deref(), Some("等用户提供凭据"));
        assert_eq!(state.outcome(), Some(TaskOutcome::Blocked));

        let mut resumed = blocked.clone();
        resumed.push(task(4, TaskOp::Unblock));
        resumed.push(task(5, TaskOp::Close));
        let state = project_task(&resumed).expect("账本在");
        assert_eq!(state.status, TaskStatus::Closed);
        assert_eq!(state.blocked_reason, None);
        assert_eq!(
            state.outcome(),
            Some(TaskOutcome::Unverified),
            "解除受阻并不会把任务变成验收通过,只是回到可验证状态"
        );
    }

    #[test]
    fn revoked_question_disappears_from_the_ledger() {
        let events = vec![
            user(1),
            task(2, open("rev-a", Vec::new())),
            task(
                3,
                TaskOp::Amend {
                    notes: vec![
                        NoteClaim::OpenQuestion {
                            id: NoteId::new("q1"),
                            text: "要不要兼容旧格式?".into(),
                        },
                        NoteClaim::OpenQuestion {
                            id: NoteId::new("q2"),
                            text: "日志要不要轮转?".into(),
                        },
                    ],
                },
            ),
            task(
                4,
                TaskOp::ResolveQuestion {
                    id: NoteId::new("q1"),
                },
            ),
        ];
        let state = project_task(&events).expect("账本在");
        let open: Vec<&str> = state
            .open_questions
            .iter()
            .map(|item| item.id.as_str())
            .collect();
        assert_eq!(open, vec!["q2"]);
    }

    #[test]
    fn host_id_allocators_never_reuse() {
        assert!(fresh_task_id().as_str().starts_with("task-"));
        assert!(fresh_revision_id().as_str().starts_with("rev-"));
        assert!(fresh_evidence_id().as_str().starts_with("ev-"));
        assert!(fresh_requirement_id().as_str().starts_with("req-"));
        assert!(fresh_note_id().as_str().starts_with("note-"));
        // rewind 之后的重新分配不依赖日志状态,所以不可能与旧身份撞号。
        assert_ne!(fresh_revision_id(), fresh_revision_id());
        assert_ne!(fresh_task_id(), fresh_task_id());
    }
}
