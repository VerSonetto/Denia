//! 执行预算账本(阶段五):`steps / requests / active_ms` 三计数与上限。
//!
//! 与 [`crate::ContextMeter`] 同一套做法:纯折叠 + 共享句柄。一份账本按
//! **一个 turn** 建立(`SessionDriver::begin_turn_budget`),turn 内所有请求
//! 路径(普通 step、续写、压缩、唤醒、子代理)共享同一个 `Arc<Mutex<..>>`
//! 计数;上限按 turn 开始时读取的**一致快照**生效(中途改配置不影响在途
//! 轮次)。
//!
//! 与 token 账本的分工:token 账本回答"花了多少"(provider 上报的精确
//! usage),执行预算回答"还能不能再来一次"——它必须在**请求发出之前**给出
//! 答案,而 usage 只有请求回来才知道。
//!
//! 三道边界:
//! - `steps`:一个 turn 里开始几个模型步骤(模型面的一次往返);
//! - `requests`:物理请求尝试次数,含注册表内部重试、step 内流重试、续写与
//!   压缩请求——按 provider 计费口径,每一次尝试都是一次真实开销;
//! - `active_ms`:主动执行时间(见 [`ExecutionBudget::active_ms`] 的区间界定)。
//!
//! 来源区分:旁路调用(会话标题、`/api/llm` 连通测试、git 摘要)只累计
//! `bypass_requests` 供观测,不吃用户预算——它们不是用户任务的执行成本。

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// 账本计量版本。
///
/// 口径变化(计数含义、区间界定、去重键)必须升版本:新旧总量不可直接相减,
/// 面板与评测报告按版本对账(计划"新账本有计量版本,不用新总量减旧口径预算基数")。
pub const EXECUTION_METERING_VERSION: u32 = 1;

/// 单调毫秒时钟(相对任意起点,只用于做差)。
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

fn real_clock() -> Clock {
    let base = Instant::now();
    Arc::new(move || base.elapsed().as_millis() as u64)
}

/// 一个 turn 的执行预算上限(按 turn 读取一致快照)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionLimits {
    /// 模型步骤上限(默认 128)。
    pub max_steps: u64,
    /// 物理请求尝试上限(默认 256;含注册表重试、流重试、续写、压缩)。
    pub max_requests: u64,
    /// 主动执行时间上限(默认 3600000ms = 60 分钟)。
    pub max_active_ms: u64,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            max_steps: 128,
            max_requests: 256,
            max_active_ms: 3_600_000,
        }
    }
}

impl ExecutionLimits {
    /// 全部边界关闭(测试注入与"显式不限"的部署)。
    pub const fn unbounded() -> Self {
        Self {
            max_steps: u64::MAX,
            max_requests: u64::MAX,
            max_active_ms: u64::MAX,
        }
    }
}

/// 账本计数快照。`metered_requests` 是吃用户预算的那部分;旁路单独计数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionCounters {
    pub steps: u64,
    pub metered_requests: u64,
    pub bypass_requests: u64,
    pub active_ms: u64,
}

impl ExecutionCounters {
    /// 全部物理请求尝试(用户预算请求 + 旁路请求)。
    pub fn requests(&self) -> u64 {
        self.metered_requests.saturating_add(self.bypass_requests)
    }
}

/// 拒绝来自哪一道边界。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetBoundary {
    Steps,
    Requests,
    ActiveTime,
}

impl BudgetBoundary {
    /// 边界的中文人称(进错误消息,用户看得到)。
    pub fn label(self) -> &'static str {
        match self {
            Self::Steps => "模型步骤",
            Self::Requests => "模型请求尝试",
            Self::ActiveTime => "主动执行时间",
        }
    }
}

/// 一次准入拒绝:边界 + 上限 + 已用量 + 可直接展示的消息。
///
/// 消息是给用户看的结论,不夹带实现细节:模型请求侧的拒绝会经
/// `llm` 层转成 `BUDGET_EXHAUSTED` 失败,最终落成 turn-end 摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetRefusal {
    pub boundary: BudgetBoundary,
    pub limit: u64,
    pub used: u64,
    pub message: String,
}

impl BudgetRefusal {
    fn new(boundary: BudgetBoundary, limit: u64, used: u64) -> Self {
        let message = match boundary {
            BudgetBoundary::Steps => format!(
                "执行预算已用尽:本轮{}({used} 步)已达上限 {limit} 步,不再开始新的模型步骤,本轮收尾。",
                boundary.label(),
            ),
            BudgetBoundary::Requests => format!(
                "执行预算已用尽:本轮{}({used} 次)已达上限 {limit} 次——含内部重试、续写与压缩请求;不再发出新的模型请求,本轮收尾。",
                boundary.label(),
            ),
            BudgetBoundary::ActiveTime => format!(
                "执行预算已用尽:本轮{}已达上限 {limit} 毫秒(≈{} 分钟),等待用户审批/提问的区间已排除;不再发出新的模型请求,本轮收尾。",
                boundary.label(),
                limit / 60_000,
            ),
        };
        Self {
            boundary,
            limit,
            used,
            message,
        }
    }
}

/// 请求来源分类:账本据此判定是否吃用户预算。
///
/// 旁路(标题/连通测试/git)不是用户任务的执行成本,不该被用户预算拦住,
/// 也不该挤占用户预算的名额。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RequestKind {
    /// 普通 step 的主请求。
    Step,
    /// 输出截断后的续写请求(`MAX_TOKENS_CONTINUATION`)。
    Continuation,
    /// 压缩摘要请求(自动压缩、上下文超限后的强压、手动压缩)。
    Compaction,
    /// 后台任务完成 / 子代理消息触发的唤醒轮请求。
    Wake,
    /// 子代理自身的请求(每个子代理各自一轮,独立账本)。
    Subagent,
    /// 会话标题生成。
    SessionTitle,
    /// `/api/llm` 连通性测试。
    Connectivity,
    /// git 摘要等宿主辅助调用。
    Git,
    /// 未分类的宿主调用(计入预算:未知来源按用户任务成本兜底)。
    Other,
}

impl RequestKind {
    /// 是否计入用户预算。未识别来源按"计入"兜底(宁可多算,不漏算)。
    pub fn metered(self) -> bool {
        !matches!(self, Self::SessionTitle | Self::Connectivity | Self::Git)
    }
}

/// 一次请求的来源与身份。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RequestSource {
    /// 计入用户预算:带会话/turn/step 身份,便于对账与去重。
    Metered {
        session: String,
        turn: u32,
        step: u32,
        kind: RequestKind,
    },
    /// 旁路:只计数,不吃用户预算(无会话身份)。
    Bypass { kind: RequestKind },
}

impl RequestSource {
    pub fn kind(&self) -> RequestKind {
        match self {
            Self::Metered { kind, .. } | Self::Bypass { kind } => *kind,
        }
    }
}

/// 调用身份:来源 + 逻辑调用序号 + 物理尝试序号(1 起)。
///
/// 去重只按**完全相同的身份**:
/// - 同一次逻辑调用内部的显式重试 → `attempt` 递增,是新身份(它真的又发了
///   一次请求、又付了一次钱),被容纳而不是被判成重复;
/// - 同一个 step 里重建一次请求(建立失败 / 上下文超限强压 / replay 降级
///   之后重发)→ `sequence` 递增,同样是新身份;
/// - 同一个物理尝试被两层同时准入 → 身份完全相同,第二次幂等。
///
/// 只按 `attempt` 去重会漏算:两个不同的逻辑调用各自从 1 开始计物理尝试,
/// 第二个会被错认为“重复”。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RequestCall {
    pub source: RequestSource,
    /// 调用方给出的逻辑调用序号(同一 step 里第几次建流;旁路调用给 0)。
    pub sequence: u32,
    /// 物理尝试序号(注册表填写:同一次逻辑调用里第几次真的发出请求)。
    pub attempt: u32,
}

struct BudgetState {
    limits: ExecutionLimits,
    steps: u64,
    metered_requests: u64,
    bypass_requests: u64,
    started_at: u64,
    paused_ms: u64,
    paused_since: Option<u64>,
    /// 暂停嵌套深度(审批/提问桥各自成对 pause/resume)。
    pause_depth: u32,
    seen: HashSet<RequestCall>,
}

impl BudgetState {
    fn active_ms(&self, now: u64) -> u64 {
        let paused = self.paused_ms.saturating_add(
            self.paused_since
                .map(|since| now.saturating_sub(since))
                .unwrap_or(0),
        );
        now.saturating_sub(self.started_at).saturating_sub(paused)
    }
}

/// 共享执行预算账本(按 turn 一份;`clone` 共享同一份计数)。
#[derive(Clone)]
pub struct ExecutionBudget {
    inner: Arc<Mutex<BudgetState>>,
    clock: Clock,
}

impl ExecutionBudget {
    pub fn new(limits: ExecutionLimits) -> Self {
        Self::with_clock(limits, real_clock())
    }

    /// 注入时钟(测试用假时钟推进主动执行时间)。
    pub fn with_clock(limits: ExecutionLimits, clock: Clock) -> Self {
        let started_at = clock();
        Self {
            inner: Arc::new(Mutex::new(BudgetState {
                limits,
                steps: 0,
                metered_requests: 0,
                bypass_requests: 0,
                started_at,
                paused_ms: 0,
                paused_since: None,
                pause_depth: 0,
                seen: HashSet::new(),
            })),
            clock,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BudgetState> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    pub fn limits(&self) -> ExecutionLimits {
        self.lock().limits
    }

    pub fn metering_version(&self) -> u32 {
        EXECUTION_METERING_VERSION
    }

    /// 计数快照(主动时间取当前时刻,不落盘)。
    pub fn counters(&self) -> ExecutionCounters {
        let now = (self.clock)();
        let state = self.lock();
        ExecutionCounters {
            steps: state.steps,
            metered_requests: state.metered_requests,
            bypass_requests: state.bypass_requests,
            active_ms: state.active_ms(now),
        }
    }

    /// 当前主动执行时间(毫秒):本 turn 起点到现在,减去**显式暂停**区间。
    ///
    /// 区间界定(与计划"等待用户的暂停区间排除,但仍运行的子任务不能伪装为
    /// 空闲"对齐):
    /// - 起点 = 账本建立时刻(即 turn 开始);
    /// - 暂停区间只由 [`Self::pause`]/[`Self::resume`] 标出,目前唯一的
    ///   来源是 driver 包在审批桥 / 提问桥外面的守卫——turn 内唯一的
    ///   "等用户"就是这两处;
    /// - 等子代理(`wait_agent`)、等后台任务(`job_*`)**不暂停**:子任务还在
    ///   跑,父轮次并没有空闲;
    /// - turn 与 turn 之间的等待(goal paused、等用户下一条消息、唤醒 nudge
    ///   间隔)根本不进任何账本——每轮各自新建账本,天然不计。
    pub fn active_ms(&self) -> u64 {
        let now = (self.clock)();
        self.lock().active_ms(now)
    }

    /// 进入"等用户"区间:墙钟继续走,但不算主动执行时间。
    pub fn pause(&self) {
        let now = (self.clock)();
        let mut state = self.lock();
        state.pause_depth = state.pause_depth.saturating_add(1);
        if state.paused_since.is_none() {
            state.paused_since = Some(now);
        }
    }

    /// 离开"等用户"区间:把这一段并进已暂停累计。
    pub fn resume(&self) {
        let now = (self.clock)();
        let mut state = self.lock();
        state.pause_depth = state.pause_depth.saturating_sub(1);
        if state.pause_depth > 0 {
            return;
        }
        if let Some(since) = state.paused_since.take() {
            state.paused_ms = state.paused_ms.saturating_add(now.saturating_sub(since));
        }
    }

    pub fn is_paused(&self) -> bool {
        self.lock().paused_since.is_some()
    }

    /// 步骤准入:返回本 turn 已开始的步骤数。超限返回拒绝(不计数)。
    pub fn admit_step(&self) -> Result<u64, BudgetRefusal> {
        let now = (self.clock)();
        let mut state = self.lock();
        let active = state.active_ms(now);
        if active >= state.limits.max_active_ms {
            let limit = state.limits.max_active_ms;
            return Err(BudgetRefusal::new(
                BudgetBoundary::ActiveTime,
                limit,
                active,
            ));
        }
        if state.steps >= state.limits.max_steps {
            let limit = state.limits.max_steps;
            return Err(BudgetRefusal::new(
                BudgetBoundary::Steps,
                limit,
                state.steps,
            ));
        }
        state.steps = state.steps.saturating_add(1);
        Ok(state.steps)
    }

    /// 请求准入:返回本 turn 已准入的**计入预算**的请求尝试次数。
    ///
    /// 旁路请求只累计观测计数,不受上限约束也不会被拒绝。
    pub fn admit_request(&self, call: &RequestCall) -> Result<u64, BudgetRefusal> {
        let now = (self.clock)();
        let mut state = self.lock();
        let active = state.active_ms(now);
        if active >= state.limits.max_active_ms {
            let limit = state.limits.max_active_ms;
            return Err(BudgetRefusal::new(
                BudgetBoundary::ActiveTime,
                limit,
                active,
            ));
        }
        if !call.source.kind().metered() {
            state.bypass_requests = state.bypass_requests.saturating_add(1);
            return Ok(state.metered_requests);
        }
        // 完全相同的调用身份:同一物理尝试被重复准入是幂等的,不重复计数,
        // 也不因为"预算刚好吃满"把一个已经在途的尝试判成新的超支。
        if state.seen.contains(call) {
            return Ok(state.metered_requests);
        }
        if state.metered_requests >= state.limits.max_requests {
            let limit = state.limits.max_requests;
            return Err(BudgetRefusal::new(
                BudgetBoundary::Requests,
                limit,
                state.metered_requests,
            ));
        }
        state.metered_requests = state.metered_requests.saturating_add(1);
        state.seen.insert(call.clone());
        Ok(state.metered_requests)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn fake_clock(start: u64) -> (Clock, Arc<AtomicU64>) {
        let now = Arc::new(AtomicU64::new(start));
        let handle = now.clone();
        (Arc::new(move || handle.load(Ordering::SeqCst)), now)
    }

    fn metered(kind: RequestKind, attempt: u32) -> RequestCall {
        RequestCall {
            source: RequestSource::Metered {
                session: "s1".into(),
                turn: 1,
                step: 1,
                kind,
            },
            sequence: 0,
            attempt,
        }
    }

    #[test]
    fn defaults_match_the_protection_baseline() {
        let limits = ExecutionLimits::default();
        assert_eq!(limits.max_steps, 128);
        assert_eq!(limits.max_requests, 256);
        assert_eq!(limits.max_active_ms, 60 * 60 * 1_000);
        let budget = ExecutionBudget::new(limits);
        assert_eq!(budget.metering_version(), EXECUTION_METERING_VERSION);
        assert_eq!(budget.limits(), limits);
    }

    #[test]
    fn steps_boundary_refuses_after_the_limit() {
        let budget = ExecutionBudget::new(ExecutionLimits {
            max_steps: 2,
            ..ExecutionLimits::unbounded()
        });
        assert_eq!(budget.admit_step().unwrap(), 1);
        assert_eq!(budget.admit_step().unwrap(), 2);
        let refusal = budget.admit_step().expect_err("third step must refuse");
        assert_eq!(refusal.boundary, BudgetBoundary::Steps);
        assert_eq!(refusal.used, 2);
        // 拒绝不计数:账本停在 2 步。
        assert_eq!(budget.counters().steps, 2);
        assert!(refusal.message.contains("模型步骤"));
    }

    #[test]
    fn requests_boundary_counts_retries_and_refuses_at_the_limit() {
        let budget = ExecutionBudget::new(ExecutionLimits {
            max_requests: 2,
            ..ExecutionLimits::unbounded()
        });
        assert_eq!(
            budget
                .admit_request(&metered(RequestKind::Step, 1))
                .unwrap(),
            1
        );
        // 显式重试是新的尝试序号:容纳,并且真的计数。
        assert_eq!(
            budget
                .admit_request(&metered(RequestKind::Step, 2))
                .unwrap(),
            2
        );
        let refusal = budget
            .admit_request(&metered(RequestKind::Step, 3))
            .expect_err("third attempt must refuse");
        assert_eq!(refusal.boundary, BudgetBoundary::Requests);
        assert_eq!(budget.counters().metered_requests, 2);
    }

    #[test]
    fn identical_call_identity_is_idempotent() {
        let budget = ExecutionBudget::new(ExecutionLimits {
            max_requests: 1,
            ..ExecutionLimits::unbounded()
        });
        let call = metered(RequestKind::Compaction, 1);
        assert_eq!(budget.admit_request(&call).unwrap(), 1);
        // 同一物理尝试被再次准入(两层同时拦):不重复计数、不判超支。
        assert_eq!(budget.admit_request(&call).unwrap(), 1);
        assert_eq!(budget.counters().metered_requests, 1);
    }

    #[test]
    fn a_rebuilt_call_in_the_same_step_is_a_new_identity() {
        // 同一 step 里第二次建流(建立失败重发/replay 降级):sequence 变了,
        // 物理尝试又从 1 开始。它是一次真实的新请求,不能被误判成“重复”。
        let budget = ExecutionBudget::new(ExecutionLimits {
            max_requests: 2,
            ..ExecutionLimits::unbounded()
        });
        let first = metered(RequestKind::Step, 1);
        let rebuilt = RequestCall {
            sequence: 1,
            ..first.clone()
        };
        assert_eq!(budget.admit_request(&first).unwrap(), 1);
        assert_eq!(
            budget.admit_request(&rebuilt).unwrap(),
            2,
            "新身份要真的计数"
        );
        assert_eq!(budget.counters().metered_requests, 2);
        // 又重建一次(sequence 2):新身份,撞上限被拒。
        let rebuilt_again = RequestCall {
            sequence: 2,
            ..first.clone()
        };
        let refusal = budget
            .admit_request(&rebuilt_again)
            .expect_err("第三次请求超限");
        assert_eq!(refusal.boundary, BudgetBoundary::Requests);
        // 而已经完全相同的身份重复准入仍是幂等,不重复计数。
        assert_eq!(budget.admit_request(&first).unwrap(), 2);
        assert_eq!(budget.counters().metered_requests, 2);
    }

    #[test]
    fn bypass_calls_never_consume_the_user_budget() {
        let budget = ExecutionBudget::new(ExecutionLimits {
            max_requests: 0,
            ..ExecutionLimits::unbounded()
        });
        for kind in [
            RequestKind::SessionTitle,
            RequestKind::Connectivity,
            RequestKind::Git,
        ] {
            let call = RequestCall {
                source: RequestSource::Bypass { kind },
                sequence: 0,
                attempt: 1,
            };
            assert!(
                budget.admit_request(&call).is_ok(),
                "{kind:?} 是旁路,不该被用户预算拦住"
            );
        }
        let counters = budget.counters();
        assert_eq!(counters.metered_requests, 0, "旁路不计入用户预算");
        assert_eq!(counters.bypass_requests, 3, "旁路只累计观测计数");
        // 同样的额度下,用户请求仍然被拦住。
        assert!(
            budget
                .admit_request(&metered(RequestKind::Step, 1))
                .is_err()
        );
    }

    #[test]
    fn paused_intervals_leave_the_active_clock() {
        let (clock, now) = fake_clock(1_000);
        let budget = ExecutionBudget::with_clock(
            ExecutionLimits {
                max_active_ms: 10_000,
                ..ExecutionLimits::unbounded()
            },
            clock,
        );
        now.store(4_000, Ordering::SeqCst); // 走了 3s 主动时间
        assert_eq!(budget.active_ms(), 3_000);
        budget.pause();
        now.store(60_000, Ordering::SeqCst); // 等用户 56s:不计
        assert_eq!(budget.active_ms(), 3_000);
        budget.resume();
        now.store(61_000, Ordering::SeqCst);
        assert_eq!(budget.active_ms(), 4_000);
        assert!(budget.admit_step().is_ok());
    }

    #[test]
    fn nested_pauses_only_resume_at_the_outermost_exit() {
        let (clock, now) = fake_clock(0);
        let budget = ExecutionBudget::with_clock(ExecutionLimits::unbounded(), clock);
        budget.pause();
        budget.pause();
        now.store(30_000, Ordering::SeqCst);
        budget.resume(); // 内层退出:还在等用户
        assert!(budget.is_paused());
        assert_eq!(budget.active_ms(), 0);
        now.store(40_000, Ordering::SeqCst);
        budget.resume();
        assert!(!budget.is_paused());
        // 整个等待区间都被排除:恢复瞬间的主动时间仍是 0。
        assert_eq!(budget.active_ms(), 0);
        now.store(45_000, Ordering::SeqCst);
        assert_eq!(budget.active_ms(), 5_000, "恢复后重新累计");
    }

    #[test]
    fn active_time_boundary_refuses_before_the_next_admission() {
        let (clock, now) = fake_clock(0);
        let budget = ExecutionBudget::with_clock(
            ExecutionLimits {
                max_active_ms: 1_000,
                ..ExecutionLimits::unbounded()
            },
            clock,
        );
        assert!(budget.admit_step().is_ok());
        assert!(budget.admit_request(&metered(RequestKind::Step, 1)).is_ok());
        now.store(1_500, Ordering::SeqCst);
        let refusal = budget
            .admit_request(&metered(RequestKind::Step, 2))
            .expect_err("active time is over the limit");
        assert_eq!(refusal.boundary, BudgetBoundary::ActiveTime);
        assert!(refusal.message.contains("主动执行时间"));
        let step_refusal = budget.admit_step().expect_err("no new step either");
        assert_eq!(step_refusal.boundary, BudgetBoundary::ActiveTime);
    }

    #[test]
    fn unknown_sources_are_metered_by_default() {
        assert!(RequestKind::Other.metered());
        assert!(RequestKind::Step.metered());
        assert!(RequestKind::Wake.metered());
        assert!(RequestKind::Subagent.metered());
        assert!(RequestKind::Continuation.metered());
        assert!(RequestKind::Compaction.metered());
        assert!(!RequestKind::SessionTitle.metered());
        assert!(!RequestKind::Connectivity.metered());
        assert!(!RequestKind::Git.metered());
    }
}
