//! 任务验证账本的纯折叠与完成裁决(共享给宿主、工具面与前端快照)。
//!
//! 账本只剩下"有证据的完成判定"这一条链路:宿主跑检查、写下结论,折叠层按
//! 结论算结局。四条不变量:
//!
//! 1. **纯函数、确定性、可重放**:只读 `&[SessionEnvelope]` 与折叠作用域,
//!    不读时钟、不取随机数、不看磁盘;同一份日志折叠两次逐字段相等。时间字段
//!    一律取事件时间。
//! 2. **revision 不复用**:当前 revision 取最新一条验证结论绑定的身份;宿主
//!    换版本(包括 rewind 之后重新推进)必须分配新的 id。被 rewind 物理截断
//!    掉的旧身份由宿主交出的 [`TaskFoldScope::retired_revisions`] 挡掉 ——
//!    日志里查不到它们,只有报废集合还记得,落在里面的当前版本结论一律作废。
//! 3. **缺证据不得通过**:结局由 [`TaskState::outcome`] 算;没有绑在当前
//!    revision、来源属于本会话、且冻结指纹仍然成立的验证结论,就只能是
//!    "未验证完成"。折叠层没有任何入口能写"通过"。
//! 4. **失效也折自日志,不来自磁盘**:结论绑的是那次运行看到的那批字节的摘要,
//!    "之后又有人写过它"同样从日志里读 —— 折叠看工具调用
//!    ([`SessionEvent::ToolCall`])里 `edit` / `write_file` 的目标路径,与结论
//!    冻结的输入路径比对,命中就让那条结论失效(失效表示复用
//!    [`StaleReason::InputsChanged`],没有第二套判定)。
//!
//!    **覆盖范围就是这么窄,这是刻意的**:折叠是纯函数,读不到文件系统,也不
//!    知道工作目录,只有参数里带得出目标路径的工具能在这里留痕。`bash` 里改
//!    文件、外部编辑器、构建脚本产物**不在覆盖范围** —— 它们改没改、改了哪个
//!    文件,日志里无从判断。别把这里当成"文件变了就一定认得出来":经工具层
//!    写入之外的那条防线是工具侧"写前内容指纹"(改写前校验磁盘上的字节与上次
//!    读到的概览一致),与折叠层这条各管一段,不互相冒充。
//!
//! 记账层(`open` / `amend` / `close` 那批模型声明意图的操作)已经裁掉:它们
//! 在旧日志里由 [`denia_core::task::TaskOp::Legacy`] 兜底反序列化,折叠时被
//! 忽略 —— 读得进来,但不进裁决。当年由 `RecordChange` 事件写入的指纹基线也
//! 随之消失,现在改由折叠从工具调用推导(见上面的第 4 条)。
//!
//! 本模块还提供宿主侧的 id 分配助手([`fresh_revision_id`])。它**不属于**
//! 折叠:折叠只读事件里的 id,从不自己造一个 —— 这样重放才确定。

use std::collections::HashSet;

use denia_core::session::{SessionEnvelope, SessionEvent};
use denia_core::task::{
    FileFingerprint, LedgerOverflow, MAX_VALIDATIONS, RevisionId, TaskOp, TaskOutcome, TaskState,
    ValidationResult, upsert_fingerprint,
};

/// 折叠作用域:折叠方是谁,以及 rewind 之后哪些 revision 已经报废。
#[derive(Debug, Clone, Default)]
pub struct TaskFoldScope {
    /// 本会话 id。写进折出的 [`TaskState::session`],并决定哪些验证结论算数:
    /// 来源与它不一致的记录(父会话/其它分支 seed 继承来的)只作审计,不进
    /// 当前裁决 —— 子代理不继承父的验收结论。
    pub session: Option<String>,
    /// 已报废的 revision id。rewind 物理截断日志后,这些 id 在日志里已经不存在,
    /// 只有宿主还记得(回退审计/内存);落在这里的身份上的结论一律不继承。
    pub retired_revisions: Vec<RevisionId>,
}

/// 折叠整份日志得到当前验证账本。`None` = 日志里没有任务事件(旧会话、
/// 普通问答会话)—— 旧日志不阻塞,也不被推导出 verified。
pub fn project_task(events: &[SessionEnvelope]) -> Option<TaskState> {
    project_task_scoped(events, &TaskFoldScope::default())
}

/// [`project_task`] 的带作用域版本(会话身份 + 已报废 revision)。
pub fn project_task_scoped(events: &[SessionEnvelope], scope: &TaskFoldScope) -> Option<TaskState> {
    let mut folder = Folder::new(scope);
    for envelope in events {
        folder.apply(envelope);
    }
    folder.state
}

/// 折叠出任务结局;`None` = 日志里没有任务事件(没有可裁决的账本)。
pub fn project_task_outcome(events: &[SessionEnvelope]) -> Option<TaskOutcome> {
    project_task(events).map(|state| state.outcome())
}

/// [`project_task_outcome`] 的带作用域版本。
pub fn project_task_outcome_scoped(
    events: &[SessionEnvelope],
    scope: &TaskFoldScope,
) -> Option<TaskOutcome> {
    project_task_scoped(events, scope).map(|state| state.outcome())
}

/// 折叠输入:折叠器**只读**这些事件,宿主(冷载切片、整份重放)必须用同一口径
/// 构建输入 —— 口径只此一处,冷/热两条路径不会各自漂移。
///
/// 返回的是**归约后**的事件:写入类工具调用只保留目标路径,参数正文(`write_file`
/// 的整份内容可能有几十 KB)对折叠没用,而切片是全会话驻留的。
///
/// 为什么写入类工具调用必须进折叠输入:验证结论的失效判定要看“结论之后有谁
/// 写过它冻结的输入”(见 `Folder::note_tool_write`)。切片里没有这类事件,冷载
/// 就会折出和热态相反的结局。
#[must_use]
pub fn task_fold_input(envelope: &SessionEnvelope) -> Option<SessionEnvelope> {
    match &envelope.event {
        SessionEvent::Task { .. } => Some(envelope.clone()),
        SessionEvent::ToolCall {
            turn,
            step,
            call_id,
            name,
            arguments,
        } => {
            let path = write_target(name, arguments)?;
            Some(SessionEnvelope {
                seq: envelope.seq,
                time: envelope.time,
                event: SessionEvent::ToolCall {
                    turn: *turn,
                    step: *step,
                    call_id: call_id.clone(),
                    name: name.clone(),
                    arguments: serde_json::json!({ "path": path }).to_string(),
                },
            })
        }
        _ => None,
    }
}

/// 宿主侧 id 分配:revision id。
///
/// **必须**从不会复用的来源取。换版本(用户加了要求 / rewind 之后重新推进)
/// 就要换一版,而 rewind 会把旧分支的事件整段截掉:如果新版本沿用旧 id,旧分支
/// 的验证结论就会"对上号"被当成当前结论。报废集合是兜底防线,不是复用许可。
pub fn fresh_revision_id() -> RevisionId {
    RevisionId::new(format!("rev-{}", uuid::Uuid::new_v4()))
}

/// 会改写工作区文件、且参数里带得出目标路径的工具。只有这两个:其余工具要么
/// 不改文件,要么改文件也说不清改了哪个(`bash`)。
const WRITE_TOOLS: &[&str] = &["edit", "write_file"];

/// 写入类工具参数里的路径键:`path` 优先,其后是工具侧的宽容别名口径
/// (模型偶尔把路径放在 `file` / `file_path` 里,见 `denia-tools` 的参数解析)。
const PATH_KEYS: &[&str] = &["path", "file", "filepath", "filename", "file_path"];

/// 工具层写入留在指纹基线里的记号。
///
/// 折叠不读文件系统,算不出写后内容的真实摘要;能确定的只有"这个路径现在的内容
/// 与冻结时不同"(严格说未必 —— 把同样的字节再写一遍也会走到这里,那时会保守地
/// 误判失效,方向是安全的:宁可要求重新取证,也不放过一次真改动)。宿主侧的真实
/// 摘要是 16 位十六进制,这个记号不可能与之相撞,于是
/// [`TaskState::paths_changed_since`] 一律把它判成"已改"。
const TOOL_WRITE_DIGEST: &str = "written-by-tool-layer";

/// 从工具调用参数里取写入的目标路径。
///
/// 参数是模型给的原文(未必严格符合 schema),所以按工具侧的宽容口径来:取第一个
/// JSON 值,`path` 优先,其次常见别名。取不到就返回 `None` —— 认不出的调用宁可
/// 不记账,也不猜一个路径出来。
///
/// 口径对齐只做纯字符串能做的两件事:抹平反斜杠与 `./` 前缀(冻结侧记的是工作区
/// 内的相对路径、分隔符统一为 `/`)。真正的路径解析(绝对路径 ↔ 相对、大小写、
/// `..`、软链接)需要 cwd 与文件系统,这里不做 —— 认不出就是这条防线的已知边界。
/// 写入类工具调用的目标路径;不是写入类工具就返回 `None`。折叠用它的两处
/// ([`task_fold_input`] 与 `Folder::note_tool_write`)共享同一个“算不算写入”的口径。
fn write_target(name: &str, arguments: &str) -> Option<String> {
    if !WRITE_TOOLS.contains(&name) {
        return None;
    }
    written_path(arguments)
}

fn written_path(arguments: &str) -> Option<String> {
    let value = serde_json::Deserializer::from_str(arguments.trim())
        .into_iter::<serde_json::Value>()
        .next()?
        .ok()?;
    let raw = PATH_KEYS
        .iter()
        .find_map(|key| value.get(*key).and_then(serde_json::Value::as_str))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = trimmed.replace('\\', "/");
    Some(match normalized.strip_prefix("./") {
        Some(rest) => rest.to_string(),
        None => normalized,
    })
}

/// 折叠器:一缕事件一条状态。字段都是折叠期的簿记,不落盘。
struct Folder<'a> {
    scope: &'a TaskFoldScope,
    /// 已报废的 revision id:落在里面的身份上的结论不继承。
    retired: HashSet<RevisionId>,
    state: Option<TaskState>,
}

impl<'a> Folder<'a> {
    fn new(scope: &'a TaskFoldScope) -> Self {
        Self {
            scope,
            retired: scope.retired_revisions.iter().cloned().collect(),
            state: None,
        }
    }

    fn apply(&mut self, envelope: &SessionEnvelope) {
        match &envelope.event {
            SessionEvent::Task { op } => self.apply_op(op, envelope.time),
            SessionEvent::ToolCall {
                name, arguments, ..
            } => self.note_tool_write(name, arguments),
            _ => {}
        }
    }

    /// 记下一次**经工具层**的文件写入(`edit` / `write_file`)。
    ///
    /// 这是"结论绑字节"那条保证在生产侧唯一的写入点:`RecordChange` 事件(当年的
    /// 指纹基线生产者)已经裁掉,改成从会话日志里已有的工具调用推导。判定本身不在
    /// 这里做 —— 只把"这个路径被写过"写进基线,失效与否仍由
    /// [`TaskState::paths_changed_since`] 拿结论冻结的输入路径去比。
    ///
    /// 两个刻意的取舍:
    /// - **只看路径,不看内容**。折叠读不到写后的字节(纯函数、不读盘),记的是
    ///   [`TOOL_WRITE_DIGEST`] 这个"已与冻结时不同"的记号。
    /// - **也不看这次写入成没成**。判定绑在 `ToolCall` 上,失败的 `edit`(旧串没
    ///   匹配上、路径不存在)同样留痕。往保守方向偏:多判一次失效只是让宿主重新
    ///   取证;漏判才是那条"改完文件照旧算通过"的老毛病。
    fn note_tool_write(&mut self, name: &str, arguments: &str) {
        // 账本还不存在(日志里还没有任何验证结论)= 没有可失效的结论,不必记账。
        let Some(state) = self.state.as_mut() else {
            return;
        };
        let Some(path) = write_target(name, arguments) else {
            return;
        };
        upsert_fingerprint(
            &mut state.fingerprints,
            &FileFingerprint {
                path,
                digest: TOOL_WRITE_DIGEST.to_string(),
            },
        );
    }

    fn apply_op(&mut self, op: &TaskOp, now: u64) {
        match op {
            TaskOp::RecordValidation { result } => self.record_validation(result, now),
            // 旧日志里被裁掉的记账操作:忽略。整份会话照常打开,裁决只看
            // 宿主写下的验证结论。
            TaskOp::Legacy => {}
        }
    }

    fn record_validation(&mut self, result: &ValidationResult, now: u64) {
        let revision = result.revision().clone();
        let session = self.scope.session.clone();
        let state = self.state.get_or_insert_with(|| TaskState {
            revision: revision.clone(),
            revision_retired: false,
            session,
            validations: Vec::new(),
            fingerprints: Vec::new(),
            overflow: LedgerOverflow::default(),
            created_at: now,
            updated_at: now,
        });
        // 绑到新 revision 的结论 = 宿主换了一版(rewind 之后重新推进同理)。
        // 当前 revision 随最新一条结论走,旧 id 上的结论不再是当前版本的结论。
        if state.revision != revision {
            state.revision = revision;
        }
        // 当前身份若已被 rewind 报废,结论一律不继承(见 TaskState::revision_retired)。
        state.revision_retired = self.retired.contains(&state.revision);
        // 这条结论冻结的输入 = 这次运行开始那一刻磁盘上的字节:把基线重新锚到这些
        // 摘要上。顺序敏感就落在这里 —— 只有**结论之后**发生的写入才让结论失效;
        // 结论之前那些写入的字节已经被这次运行看过,不倒扣(见 note_tool_write)。
        for fingerprint in result.fingerprints() {
            upsert_fingerprint(&mut state.fingerprints, &fingerprint);
        }
        state.overflow.validations +=
            push_bounded(&mut state.validations, result.clone(), MAX_VALIDATIONS);
        state.updated_at = now;
    }
}

/// 追加并把集合压到上限:保留最新的,返回丢弃条数。
///
/// 上限造成的损失必须可见 —— 调用方把它累加进 [`LedgerOverflow`]。
fn push_bounded<T>(items: &mut Vec<T>, item: T, cap: usize) -> u32 {
    items.push(item);
    let dropped = items.len().saturating_sub(cap);
    if dropped > 0 {
        items.drain(0..dropped);
    }
    dropped as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::task::{
        CheckRun, FileFingerprint, StaleReason, ValidationVerdict, VerificationState,
    };

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

    /// 一条工具调用事件(写入类工具的路径就在参数里)。
    fn tool_call(seq: u64, name: &str, arguments: &str) -> SessionEnvelope {
        envelope(
            seq,
            SessionEvent::ToolCall {
                turn: 1,
                step: 1,
                call_id: format!("call-{seq}"),
                name: name.into(),
                arguments: arguments.into(),
            },
        )
    }

    /// 一次 `edit`:改的是 `path`。
    fn edit(seq: u64, path: &str) -> SessionEnvelope {
        tool_call(
            seq,
            "edit",
            &format!(r#"{{"path":"{path}","old_string":"a","new_string":"b"}}"#),
        )
    }

    fn rev(id: &str) -> RevisionId {
        RevisionId::new(id)
    }

    /// 一次检查运行:冻结 `src/lib.rs` 的摘要 `digest`,退出码决定结论。
    fn check(
        revision: &str,
        exit_code: i32,
        digest: &str,
        origin: Option<&str>,
        at: u64,
    ) -> TaskOp {
        let result = ValidationResult::from_check_runs(
            rev(revision),
            vec![CheckRun {
                command: "cargo test".into(),
                exit_code: Some(exit_code),
                expect_exit_code: 0,
                workdir: ".".into(),
                fingerprints: vec![FileFingerprint {
                    path: "src/lib.rs".into(),
                    digest: digest.into(),
                }],
            }],
            origin.map(str::to_string),
            at,
        )
        .expect("有检查运行才构成结论");
        TaskOp::RecordValidation { result }
    }

    #[test]
    fn legacy_log_without_task_events_folds_to_empty_state() {
        // 旧日志(与普通问答会话):没有任务事件 → 折叠出"没有账本",
        // 既不阻塞也不被推导出 verified。
        let events = vec![user(1), user(2), user(3)];
        assert_eq!(project_task(&events), None);
        assert_eq!(project_task_outcome(&events), None);
    }

    #[test]
    fn legacy_ledger_ops_are_ignored_and_do_not_block_the_fold() {
        // 旧日志里那些被裁掉的记账事件照样读进来(由 TaskOp::Legacy 兜底),
        // 但不再贡献任何状态;同一条日志里宿主写下的验证结论照常生效。
        let events = vec![
            user(1),
            task(2, TaskOp::Legacy),
            task(3, check("rev-a", 0, "d1", None, 1_003)),
            task(4, TaskOp::Legacy),
        ];
        let state = project_task(&events).expect("有验证结论就有账本");
        assert_eq!(state.revision, rev("rev-a"));
        assert_eq!(state.validations.len(), 1);
        assert!(!state.revision_retired);
        assert_eq!(state.outcome(), TaskOutcome::Passed);

        // 整份日志只有被裁掉的操作:折不出账本,但也没有把谁判成通过。
        let legacy_only = vec![user(1), task(2, TaskOp::Legacy), task(3, TaskOp::Legacy)];
        assert_eq!(project_task(&legacy_only), None);
        assert_eq!(project_task_outcome(&legacy_only), None);
    }

    #[test]
    fn fold_is_deterministic_and_replayable() {
        let events = vec![
            user(1),
            task(2, check("rev-a", 0, "d1", None, 1_002)),
            task(3, check("rev-a", 1, "d2", None, 1_003)),
        ];
        let first = project_task(&events).expect("有任务事件就有账本");
        let second = project_task(&events).expect("重放必须得到同一份账本");
        assert_eq!(first, second, "折叠两次必须逐字段相等");
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap(),
            "序列化形状也必须一致(快照/绑定同源)"
        );
        // 结论是算出来的:最新一条(退出码 1)说了算,不挑早先那条通过的。
        assert_eq!(first.outcome(), TaskOutcome::Failed);
        assert_eq!(first.overflow, LedgerOverflow::default());
    }

    /// 一条没有任何检查运行的空报告(回放出来的彤形记录)。
    fn empty_report(revision: &str) -> TaskOp {
        TaskOp::RecordValidation {
            result: serde_json::from_value(serde_json::json!({
                "revision": revision,
                "runs": [],
                "at": 1_004,
            }))
            .expect("空报告是合法的事件负载"),
        }
    }

    #[test]
    fn the_newest_conclusion_on_the_current_revision_decides() {
        // 宿主写下通过结论:验收通过。
        let passed = vec![user(1), task(2, check("rev-a", 0, "d1", None, 1_002))];
        assert_eq!(
            project_task_outcome(&passed),
            Some(TaskOutcome::Passed),
            "宿主写下通过结论就是验收通过"
        );
        // 最新一条失败 → 验收失败(即使更早有一条通过)。
        let failed = vec![
            user(1),
            task(2, check("rev-a", 0, "d1", None, 1_002)),
            task(3, check("rev-a", 1, "d1", None, 1_003)),
        ];
        let state = project_task(&failed).expect("账本在");
        assert!(matches!(
            state.verification(),
            VerificationState::Verified {
                verdict: ValidationVerdict::Failed,
                ..
            }
        ));
        assert_eq!(state.outcome(), TaskOutcome::Failed);
        // 当前 revision 上没有可用结论、但更早的结论绑在别的 revision 上:
        // 旧的不能拿来充当当前版本的结论。
        let mixed = vec![
            user(1),
            task(2, check("rev-a", 0, "d1", None, 1_002)),
            task(3, empty_report("rev-b")),
        ];
        let state = project_task(&mixed).expect("账本在");
        assert_eq!(state.revision, rev("rev-b"), "当前 revision 随最新结论走");
        assert_eq!(
            state.verification(),
            VerificationState::Unverified {
                reason: StaleReason::RevisionChanged
            }
        );
        assert_eq!(state.outcome(), TaskOutcome::Unverified);
    }

    #[test]
    fn no_task_events_means_no_outcome_to_judge() {
        let unchecked = vec![user(1)];
        assert_eq!(project_task(&unchecked), None);
        assert_eq!(project_task_outcome(&unchecked), None);
    }

    #[test]
    fn a_retired_revision_is_never_inherited() {
        // rewind 把 rev-a 的身份报废(它在被截断的区间里出现过):日志里还留着
        // 的那条通过结论不算数 —— 旧分支的"通过"不能借尸还魂。
        let events = vec![user(1), task(2, check("rev-a", 0, "d1", None, 1_002))];
        assert_eq!(project_task_outcome(&events), Some(TaskOutcome::Passed));
        let scope = TaskFoldScope {
            session: None,
            retired_revisions: vec![rev("rev-a")],
        };
        let state = project_task_scoped(&events, &scope).expect("账本还在");
        assert!(state.revision_retired, "报废身份必须可见");
        assert_eq!(
            state.verification(),
            VerificationState::Unverified {
                reason: StaleReason::RevisionChanged
            }
        );
        assert_eq!(state.outcome(), TaskOutcome::Unverified);

        // 宿主换新 id 重新取证:新版本从零开始,旧的报废身份不再挡路。
        let mut events = events;
        events.push(task(3, check("rev-b", 0, "d1", None, 1_003)));
        let state = project_task_scoped(&events, &scope).expect("账本在");
        assert_eq!(state.revision, rev("rev-b"));
        assert!(!state.revision_retired);
        assert_eq!(state.outcome(), TaskOutcome::Passed);
    }

    #[test]
    fn subagent_does_not_inherit_the_parent_verification() {
        // 父会话写的结论被 seed 进子会话(或折叠方是另一个会话):只作审计。
        let events = vec![
            user(1),
            task(2, check("rev-a", 0, "d1", Some("parent"), 1_002)),
        ];
        let child_scope = TaskFoldScope {
            session: Some("child".into()),
            retired_revisions: Vec::new(),
        };
        let child = project_task_scoped(&events, &child_scope).expect("账本在");
        assert_eq!(child.session.as_deref(), Some("child"));
        assert_eq!(
            child.verification(),
            VerificationState::Unverified {
                reason: StaleReason::ForeignOrigin
            }
        );
        assert_eq!(child.outcome(), TaskOutcome::Unverified);
        // 不声明会话身份时也一样保守:标注了来源、但与本折叠方不一致的记录不算数。
        assert_eq!(project_task_outcome(&events), Some(TaskOutcome::Unverified));
    }

    #[test]
    fn ledger_bounds_hold_and_dropped_conclusions_are_counted() {
        let mut events = vec![user(1)];
        let total = MAX_VALIDATIONS + 5;
        for index in 0..total {
            events.push(task(
                2 + index as u64,
                check("rev-a", 0, "d1", None, 1_002 + index as u64),
            ));
        }
        let state = project_task(&events).expect("账本在");
        assert_eq!(state.validations.len(), MAX_VALIDATIONS, "结论按上限截住");
        assert_eq!(
            state.overflow.validations as usize,
            total - MAX_VALIDATIONS,
            "被丢弃的结论条数必须记账"
        );
    }

    #[test]
    fn host_revision_allocator_never_reuses() {
        assert!(fresh_revision_id().as_str().starts_with("rev-"));
        // rewind 之后的重新分配不依赖日志状态,所以不可能与旧身份撞号。
        assert_ne!(fresh_revision_id(), fresh_revision_id());
    }

    /// 验证通过 → 工具层改了被冻结的输入(`edit`)→ 这条结论立刻失效,结局回到
    /// 未验证完成。这就是那条"跑完检查、补一个 edit、结论照旧算通过"的收紧。
    #[test]
    fn edit_after_the_conclusion_invalidates_it() {
        let events = vec![
            user(1),
            task(2, check("rev-a", 0, "d1", None, 1_002)),
            edit(3, "src/lib.rs"),
        ];
        let state = project_task(&events).expect("账本在");
        assert_eq!(
            state.verification(),
            VerificationState::Unverified {
                reason: StaleReason::InputsChanged {
                    paths: vec!["src/lib.rs".into()]
                }
            },
            "结论冻结过的输入被工具层改过:那条结论不再是可用结论"
        );
        assert_eq!(
            state.outcome(),
            TaskOutcome::Unverified,
            "结局回到未验证完成"
        );
        // 结论本身还在账上(审计用):失效说的是"不算数",不是"被抹掉"。
        assert_eq!(state.validations.len(), 1);
        // 折叠仍然纯函数:同一份日志折两次逐字段相等。
        assert_eq!(project_task(&events), Some(state));
    }

    /// 改的是无关文件:写入确实被记下,但它不在结论冻结的输入集合里 —— 结论照
    /// 旧有效(否则"只要动过手就失效"就没法干活了)。
    #[test]
    fn a_write_to_an_unrelated_file_leaves_the_conclusion_intact() {
        let events = vec![
            user(1),
            task(2, check("rev-a", 0, "d1", None, 1_002)),
            tool_call(3, "write_file", r#"{"path":"docs/notes.md","content":"x"}"#),
        ];
        let state = project_task(&events).expect("账本在");
        // 写入在基线上留了痕(只是与这条结论无关):关掉新判定时首先断在这里。
        assert_eq!(
            state.fingerprint_of("docs/notes.md"),
            Some(TOOL_WRITE_DIGEST),
            "工具层写入要在基线上留痕"
        );
        assert_eq!(
            state.verification(),
            VerificationState::Verified {
                revision: rev("rev-a"),
                verdict: ValidationVerdict::Passed,
                fingerprints: vec![FileFingerprint {
                    path: "src/lib.rs".into(),
                    digest: "d1".into(),
                }],
                at: 1_002,
            }
        );
        assert_eq!(state.outcome(), TaskOutcome::Passed);
    }

    /// 结论之后没有任何写入:当然照旧有效 —— 读文件、跑命令这类调用不改字节,
    /// 一根指纹也不该进基线。
    #[test]
    fn a_conclusion_without_any_later_write_stays_valid() {
        let events = vec![
            user(1),
            task(2, check("rev-a", 0, "d1", None, 1_002)),
            tool_call(3, "read_file", r#"{"path":"src/lib.rs"}"#),
            tool_call(4, "bash", r#"{"command":"cargo test --locked"}"#),
        ];
        let state = project_task(&events).expect("账本在");
        assert_eq!(
            state.fingerprints.len(),
            1,
            "只有结论冻结的那一条输入在基线上:{:?}",
            state.fingerprints
        );
        assert_eq!(state.outcome(), TaskOutcome::Passed);
    }

    /// 顺序敏感:写入发生在结论**之前** —— 那些字节已经被这次运行看过,不倒扣。
    /// (这就是"只要日志里有写入就失效"那种写法会踩的坑;顺序敏感靠的是记结论时
    /// 把基线重新锚回它冻结的摘要。)
    #[test]
    fn a_write_before_the_conclusion_does_not_invalidate_it() {
        let events = vec![
            user(1),
            edit(2, "src/lib.rs"),
            task(3, check("rev-a", 0, "d1", None, 1_003)),
        ];
        assert_eq!(
            project_task_outcome(&events),
            Some(TaskOutcome::Passed),
            "结论里冻结的就是写入之后的字节"
        );
    }

    /// 参数口径:路径放在别名键里、带反斜杠或 `./` 前缀时要认得出;取不到路径的
    /// 调用不记账 —— 宁可不记,也不猜。
    #[test]
    fn written_paths_are_read_leniently_from_the_tool_arguments() {
        for (name, arguments) in [
            (
                "edit",
                r#"{"file_path":"src/lib.rs","old_string":"a","new_string":"b"}"#,
            ),
            ("write_file", r#"{"path":".\\src\\lib.rs","content":"x"}"#),
        ] {
            let events = vec![
                user(1),
                task(2, check("rev-a", 0, "d1", None, 1_002)),
                tool_call(3, name, arguments),
            ];
            let state = project_task(&events).expect("账本在");
            assert_eq!(
                state.outcome(),
                TaskOutcome::Unverified,
                "{name} {arguments} 的目标路径必须与冻结路径对上"
            );
        }
        // 参数里给不出路径(残缺调用 / 不是 JSON):不记账,结论不受影响。
        let events = vec![
            user(1),
            task(2, check("rev-a", 0, "d1", None, 1_002)),
            tool_call(3, "write_file", r#"{"content":"x"}"#),
            tool_call(4, "edit", "不是 JSON"),
        ];
        let state = project_task(&events).expect("账本在");
        assert_eq!(state.fingerprints.len(), 1, "认不出路径的调用不该污染基线");
        assert_eq!(state.outcome(), TaskOutcome::Passed);
    }
}
