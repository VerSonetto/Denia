//! 验证账本的领域词汇:revision 身份、检查运行、验证结论与结局判定。
//!
//! ## 这个模块现在管什么
//!
//! 只留"有证据的完成判定"那条硬链路:宿主真的跑检查命令,把**命令 + 退出码 +
//! 冻结的输入指纹**记成一次验证结论;结局由折叠层按结论算出来,不由谁声明。
//!
//! ## 为什么是这些形状
//!
//! - **状态只能折出来**。本模块只定义事件负载([`TaskOp`])与折叠后的快照
//!   ([`TaskState`]);当前状态与结局由 `denia-session::task_projection` 从
//!   耐久事件折叠(与 [`crate::session::GoalOp`] / `crate::session::apply_goal_op`]
//!   同一套路)。没有"直接改状态"的入口,也就没有第二份会漂移的真值。
//! - **模型侧没有写入口**。[`TaskOp`] 只剩宿主执行链写的 [`TaskOp::RecordValidation`];
//!   [`ValidationResult`] 字段私有、只能由 [`ValidationResult::from_check_runs`]
//!   构造,结论由检查命令的退出码算出 —— 类型层不存在"直接写 verified"的位置。
//! - **revision 是身份,不是可以重数的序号**。[`RevisionId`] 由宿主从不会复用
//!   的来源分配(如 uuid)。rewind 截断日志后旧分支的结论不能再被继承:宿主把
//!   被截掉的身份声明进报废集合,折叠层看到当前 revision 落在报废集合里就把
//!   结论一律作废(见 [`TaskState::revision_retired`])。
//! - **结论绑字节**。每条结论绑在"哪个 revision + 哪些输入文件的哪份内容"上;
//!   相关输入被改过之后旧结论不再算数(见 [`TaskState::paths_changed_since`])。
//! - **有界**。验证结论有上限([`MAX_VALIDATIONS`]):超限保留最新的,丢弃条数
//!   记进 [`LedgerOverflow`],不静默蒸发。
//! - **不持久化模型内部思维链**。本模块没有任何字段承载推理全文。
//!
//! ## 旧日志
//!
//! 已经落盘的日志里有被裁掉的记账事件(`open` / `amend` / `close` …)。它们由
//! [`TaskOp::Legacy`] 兜底反序列化,折叠时忽略 —— 读得进来,但不影响裁决。

use serde::{Deserialize, Serialize};

/// 一条检查命令文本的字符上限。账本只放结论性短句(命令 + 退出码),完整命令
/// 输出留在会话日志与产物文件里。
pub const MAX_CHECK_COMMAND_CHARS: usize = 400;
/// 验证结论保留条数上限(保留最新的;更老的只作审计)。
pub const MAX_VALIDATIONS: usize = 16;

/// 任务 revision 的身份。**宿主分配,不可复用**:rewind 之后重新推进必须给
/// 新的 id,否则旧分支的验证结论会被当成新版本的结论继承下来。
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

/// 一个文件的内容指纹。`digest` 由宿主计算(核心不引入哈希依赖):它是"验证
/// 时看到的那份字节"的凭据。要求覆盖相关脏文件、未跟踪输入或显式输入清单,
/// **不能**只绑 Git HEAD —— HEAD 看不见的改动同样必须让旧验证失效。
///
/// 折叠层也会往指纹基线里放条目:对"经工具层写过的路径"只能留一个"已与冻结时
/// 不同"的记号,拿不到哈希(折叠不读磁盘)。见 [`TaskState::fingerprints`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct FileFingerprint {
    pub path: String,
    pub digest: String,
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
        origin_session: Option<String>,
        at: u64,
    ) -> Option<Self> {
        if runs.is_empty() {
            return None;
        }
        Some(Self {
            revision,
            runs,
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

/// 有界保存的丢弃计数:每个集合超上限时保留最新的,丢弃条数记在这里。
/// 上限造成的损失必须可见 —— "结论被悄悄丢掉"正是要防的事。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct LedgerOverflow {
    #[serde(default)]
    pub validations: u32,
}

/// 任务结局(与 `TurnEndReason::Completed` 的"本轮正常结束"是两件事)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "kebab-case")]
pub enum TaskOutcome {
    /// 未验证完成:没有能支撑当前 revision 的有效验收证据。
    Unverified,
    /// 验收通过:当前 revision 上最新一条有效结论是通过。
    Passed,
    /// 验收失败:当前 revision 上最新一条有效结论是失败。
    Failed,
}

impl TaskOutcome {
    /// 中文标签(模型侧注入与 UI 的统一口径)。
    pub fn label(self) -> &'static str {
        match self {
            Self::Unverified => "未验证完成",
            Self::Passed => "验收通过",
            Self::Failed => "验收失败",
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
    /// 结论绑的 revision 已经不是当前版本(换过版,或旧身份被 rewind 报废):
    /// 旧结论不自动继承。
    RevisionChanged,
    /// 相关文件在验证之后又被改过(指纹不再匹配)。
    InputsChanged { paths: Vec<String> },
    /// 有记录但没有可用的检查运行(空报告)。
    NoCheckRun,
    /// 结论来自别的会话(父 / 分支 seed 继承),不参与当前裁决。
    ForeignOrigin,
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
        at: u64,
    },
}

/// 折叠后的验证账本快照。
///
/// 所有字段都是**折叠的产物**;结论类信息不在字段里(见 [`Self::verification`] /
/// [`Self::outcome`]),因此重复序列化不会让结论与证据脱钩。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct TaskState {
    /// 当前 revision = 宿主最近一次取证绑定的身份。
    pub revision: RevisionId,
    /// 当前 revision 已被 rewind 报废(旧分支的身份):绑在它上面的结论一律
    /// 不继承。rewind 是物理截断,被截掉的 id 在日志里查不到,只有宿主交出的
    /// 报废集合还记得(见 `TaskFoldScope::retired_revisions`)。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub revision_retired: bool,
    /// 本账本所属会话(折叠时由作用域带入):把跨会话 seed 继承来的验证结论挡
    /// 在裁决之外 —— 子代理不继承父的验收结论。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    pub session: Option<String>,
    #[serde(default)]
    pub validations: Vec<ValidationResult>,
    /// 当前生效的文件内容指纹基线(path → digest,后写的覆盖先写的)。验证结论
    /// 据此判断是否已失效:基线里没有该路径 = 没有记录显示它被改过,不算失效。
    ///
    /// 生产者在折叠期(见 `denia-session::task_projection`),两处都不读磁盘:
    /// - 记下一条验证结论时,把它冻结的输入重新锚到基线上 —— 那些摘要就是运行
    ///   开始那一刻磁盘上的字节;
    /// - 会话日志里经**工具层**写文件的调用(`edit` / `write_file`)按目标路径
    ///   记一个"已与冻结时不同"的记号(`digest` 这时可能不是哈希,而是这个记号)。
    ///
    /// 边界:只有经工具层的写入会在这里留痕。`bash` 改文件、外部编辑器改动不在
    /// 覆盖范围 —— 那是工具侧"写前内容指纹"那条防线,基线认不出来,也不假装认得。
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
    /// 标注来源)、冻结的输入指纹没有被后续变更推翻。当前 revision 若已被
    /// rewind 报废,则一条都不采纳。
    pub fn verification(&self) -> VerificationState {
        if self.revision_retired {
            return VerificationState::Unverified {
                reason: StaleReason::RevisionChanged,
            };
        }
        let mut changed_paths: Vec<String> = Vec::new();
        let mut saw_current_revision = false;
        let mut saw_other_revision = false;
        let mut saw_foreign = false;
        // 从新到旧找第一条可用的:最近一次对当前 revision 的验证说了算,
        // 不给"挑一条通过的"留口子。
        for result in self.validations.iter().rev() {
            if result.revision() != &self.revision {
                saw_other_revision = true;
                continue;
            }
            saw_current_revision = true;
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
            return VerificationState::Verified {
                revision: self.revision.clone(),
                // 上面已确认 `verdict()` 有值。
                verdict: result.verdict().unwrap_or(ValidationVerdict::Failed),
                fingerprints: result.fingerprints(),
                at: result.at(),
            };
        }
        let reason = if !changed_paths.is_empty() {
            changed_paths.sort();
            changed_paths.dedup();
            StaleReason::InputsChanged {
                paths: changed_paths,
            }
        } else if saw_foreign {
            StaleReason::ForeignOrigin
        } else if saw_other_revision {
            StaleReason::RevisionChanged
        } else if saw_current_revision {
            StaleReason::NoCheckRun
        } else {
            StaleReason::NeverRun
        };
        VerificationState::Unverified { reason }
    }

    /// 任务结局。三选一,没有第四种:
    /// - 有绑当前 revision、来源属于本会话、指纹仍成立的有效失败结论 → 验收失败;
    /// - 同类有效通过结论 → 验收通过;
    /// - 其余(没跑过 / 只在旧 revision 跑过 / 指纹已变 / 外来来源)→ 未验证完成。
    pub fn outcome(&self) -> TaskOutcome {
        match self.verification() {
            VerificationState::Unverified { .. } => TaskOutcome::Unverified,
            VerificationState::Verified {
                verdict: ValidationVerdict::Passed,
                ..
            } => TaskOutcome::Passed,
            VerificationState::Verified {
                verdict: ValidationVerdict::Failed,
                ..
            } => TaskOutcome::Failed,
        }
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

/// 验证账本的一次操作(事件负载)。
///
/// 只剩宿主执行链写的那一条:模型既不能声明意图,也写不出结论。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TaskOp {
    /// 记录一次验证结论(宿主执行链写;模型不能伪造执行结果,也不能直接设置
    /// verified —— 只能跑检查,由宿主把运行事实写成 [`ValidationResult`])。
    RecordValidation { result: ValidationResult },
    /// 旧日志里被裁掉的记账操作(`open` / `revise` / `amend` / `close` …)。
    ///
    /// 反序列化兜底:读旧日志不能因为一条当年的记账事件就整份打开失败。折叠层
    /// 忽略它 —— 旧账本里的目标、验收项、事实、待办都不再进裁决,但同一条日志
    /// 里宿主写过的验证结论照常折出来。
    #[serde(other)]
    #[cfg_attr(feature = "bindings", ts(skip))]
    Legacy,
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
        origin: Option<&str>,
    ) -> ValidationResult {
        ValidationResult::from_check_runs(
            RevisionId::new(revision),
            vec![check_run(exit_code)],
            origin.map(str::to_string),
            2_000,
        )
        .expect("有检查运行才构成结论")
    }

    /// 一个折出来的账本:当前 revision + 一条基线指纹。
    fn state(revision: &str) -> TaskState {
        TaskState {
            revision: RevisionId::new(revision),
            revision_retired: false,
            session: None,
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
            validation("rev-1", Some(0), None).verdict(),
            Some(ValidationVerdict::Passed)
        );
        assert_eq!(
            validation("rev-1", Some(1), None).verdict(),
            Some(ValidationVerdict::Failed)
        );
        assert_eq!(
            validation("rev-1", None, None).verdict(),
            Some(ValidationVerdict::Failed)
        );
        // 没有检查运行的报告不构成结论:构造入口直接回 None。
        assert_eq!(
            ValidationResult::from_check_runs(RevisionId::new("rev-1"), Vec::new(), None, 1),
            None
        );
    }

    #[test]
    fn the_newest_valid_conclusion_decides_and_old_revisions_do_not_count() {
        let mut state = state("rev-1");
        // 没跑过任何检查:只能是"未验证完成"。
        assert_eq!(state.outcome(), TaskOutcome::Unverified);
        assert_eq!(
            state.verification(),
            VerificationState::Unverified {
                reason: StaleReason::NeverRun
            }
        );
        // 有通过结论 → 验收通过。
        state.validations = vec![validation("rev-1", Some(0), None)];
        assert_eq!(state.outcome(), TaskOutcome::Passed);
        // 最新一条是失败 → 验收失败,即使更早有一条通过(不给"挑一条通过的"留口子)。
        state.validations.push(validation("rev-1", Some(1), None));
        assert_eq!(state.outcome(), TaskOutcome::Failed);
        // 只在别的 revision 上跑过:当前版本不继承旧结论。
        state.validations = vec![validation("rev-2", Some(0), None)];
        assert_eq!(
            state.verification(),
            VerificationState::Unverified {
                reason: StaleReason::RevisionChanged
            }
        );
        assert_eq!(state.outcome(), TaskOutcome::Unverified);
    }

    #[test]
    fn a_retired_revision_is_never_inherited() {
        // rewind 把当前 revision 的身份报废:日志里还留着的那条结论不算数
        // (旧分支的"通过"不能借尸还魂),要重新取证。
        let mut state = state("rev-1");
        state.validations = vec![validation("rev-1", Some(0), None)];
        assert_eq!(state.outcome(), TaskOutcome::Passed);
        state.revision_retired = true;
        assert_eq!(
            state.verification(),
            VerificationState::Unverified {
                reason: StaleReason::RevisionChanged
            }
        );
        assert_eq!(state.outcome(), TaskOutcome::Unverified);
    }

    #[test]
    fn change_after_validation_invalidates_it() {
        let mut state = state("rev-1");
        state.validations = vec![validation("rev-1", Some(0), None)];
        assert_eq!(state.outcome(), TaskOutcome::Passed);
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
        assert_eq!(state.outcome(), TaskOutcome::Unverified);
    }

    #[test]
    fn foreign_origin_and_empty_reports_block_conclusions() {
        // 子代理不继承父的验收结论:来源与本会话不一致的结论不参与裁决。
        let mut child = state("rev-1");
        child.session = Some("child".into());
        child.validations = vec![validation("rev-1", Some(0), Some("parent"))];
        assert_eq!(
            child.verification(),
            VerificationState::Unverified {
                reason: StaleReason::ForeignOrigin
            }
        );
        assert_eq!(child.outcome(), TaskOutcome::Unverified);
        // 没有检查运行的空报告(回放出来的畸形记录)不构成结论。
        let mut empty = state("rev-1");
        empty.validations = vec![ValidationResult {
            revision: RevisionId::new("rev-1"),
            runs: Vec::new(),
            origin_session: None,
            at: 1,
        }];
        assert_eq!(
            empty.verification(),
            VerificationState::Unverified {
                reason: StaleReason::NoCheckRun
            }
        );
        assert_eq!(empty.outcome(), TaskOutcome::Unverified);
    }

    #[test]
    fn legacy_task_ops_deserialize_and_are_not_written_back() {
        // 旧日志里的记账事件必须还能读进来(整个会话不能因为一条旧事件打不开)。
        for legacy in [
            r#"{"kind":"open","task_id":"task-1","revision":"rev-a","goal":"修好解析器","requirements":[]}"#,
            r#"{"kind":"amend","notes":[{"kind":"question","id":"note-1","text":"要不要兼容旧格式?"}]}"#,
            r#"{"kind":"record-change","summary":"改了 src/lib.rs","fingerprints":[]}"#,
            r#"{"kind":"close"}"#,
        ] {
            let op: TaskOp = serde_json::from_str(legacy).expect("旧 op 必须能反序列化");
            assert_eq!(op, TaskOp::Legacy, "{legacy}");
        }
        // 新写入的一侧只有宿主的那一条,不存在把 Legacy 再写回去的路径。
        let written = serde_json::to_string(&TaskOp::Legacy).unwrap();
        assert_eq!(written, r#"{"kind":"legacy"}"#);
    }
}
