//! The session driver: turn/step loop over the llm registry and tools.
//!
//! A step is one model request plus the tools it called; a turn keeps
//! stepping while the assistant message carries tool-call blocks and closes
//! on a tool-call-free message. Every event is appended to the session log
//! first and only then mirrored to `emit` — the log is the ordering
//! authority. Execution failures close any open step/turn when the log is
//! writable; session load repairs terminal events that could not be appended.
//!
//! 模块划分(按职责,`impl SessionDriver` 分散在各文件):
//! - [`turn`] — turn/step 主编排(终止判定、死循环检测接线);
//! - [`injections`] — 每 step 的背景注入管线(通道幂等);
//! - [`request`] — 请求构造 + attempt 重试环 + 失败分流;
//! - [`exec`] — 工具并行执行池 + 升权审批 + 结果兜底截断;
//! - [`errors`] — 失败分类处置与用户可读的中文报错摘要;
//! - [`loop_guard`] — 死循环检测(文本/工具两条指纹线);
//! - [`rescue`] — 文本伪工具调用救援;
//! - [`compact`] — LLM 总结压缩(压力驱动);
//! - [`microcompact`] — 工具结果微压缩(占位符替换,摘要前的廉价清理)。

mod compact;
mod compaction_state;
mod driver_settings;
mod errors;
mod exec;
mod injections;
mod loop_guard;
pub mod microcompact;
pub mod preset;
mod request;
mod rescue;
mod runtime_context;
mod turn;
mod workspace_instructions;

use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use denia_core::config::{LlmCallConfig, ModelSelection};
use denia_core::error::{LlmFailure, codes};
use denia_core::session::{RequestHeaderSnapshot, SessionEnvelope, SessionEvent, TurnEndReason};
use denia_llm::LlmRegistry;
use denia_session::Session;
use denia_system_prompt::SystemPrompt;
use denia_tools::{FileHistoryBackend, ToolRegistry};
pub use denia_token_meter::{
    BudgetRefusal, Clock, ExecutionBudget, ExecutionCounters, ExecutionLimits,
};
use denia_token_meter::{RequestCall as LedgerRequestCall, RequestSource};
use tokio_util::sync::CancellationToken;

pub use compact::{CompactOutcome, CompactionSettings};
pub use driver_settings::{DriverSettings, DriverSettingsSource};
pub use injections::GOAL_CHANNEL;
pub use loop_guard::LOOP_THRESHOLD;
pub use preset::AgentPresetSource;

/// 工具并行执行配置(学 codex `ToolCallRuntime` + dsh `maxParallelToolCalls`)。
///
/// 一次 step 内模型返回的多个工具调用并发执行,受滚动池上限约束;结果仍按
/// model-order 提交,保证每个 call 恰好一条 `ToolResult` 事件(事件源不变量)。
/// 需要审批的调用(升权)串行化——审批是交互式阻塞,并发弹窗会交错事件。
#[derive(Debug, Clone, PartialEq)]
pub struct ParallelSettings {
    /// 同时执行的工具调用上限(对齐 dsh `maxParallelToolCalls=10`)。
    pub max_parallel_tool_calls: usize,
}

impl Default for ParallelSettings {
    fn default() -> Self {
        Self {
            max_parallel_tool_calls: 10,
        }
    }
}

/// 文件历史提供者:server 侧实现,按会话提供备份句柄并在用户消息落库后
/// 固化快照。`None` 表示该部署不启用文件回退。
#[async_trait]
pub trait FileHistoryProvider: Send + Sync {
    /// 返回当前会话的工具备份句柄。
    async fn backend(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
    ) -> Option<Arc<dyn FileHistoryBackend>>;
    /// 用户消息落库后调用,固化该消息对应的文件快照。
    async fn snapshot(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        message_seq: u64,
    ) -> Result<(), String>;
}

/// 审批通道:driver 在遇到策略 Ask 决策(越界写文件、计划提交)时,向
/// 宿主请求一次用户决策。实现方负责把请求挂到 `LiveSession` 的 pending
/// 表并等待 REST 应答;取消 token 发生时实现方应返回 `Cancelled` 结局。
/// 计划批准的决策可携带执行档位/模型(由 driver 落会话事件与 TurnState)。
#[async_trait]
pub trait ApprovalBridge: Send + Sync {
    async fn request(
        &self,
        session_id: &str,
        request_id: &str,
        cancel: CancellationToken,
    ) -> denia_core::session::PlanReviewDecision;
}

/// 执行预算策略:上限来源(按 turn 读取一致快照)+ 账本时钟。
///
/// 上限来源由宿主装配(`server` 从运行时配置读),未装即用
/// [`ExecutionLimits::default`](128 步 / 256 次请求 / 60 分钟)。
#[derive(Default)]
struct BudgetPolicy {
    limits: Option<Arc<dyn Fn() -> ExecutionLimits + Send + Sync>>,
    clock: Option<Clock>,
}

impl BudgetPolicy {
    fn limits(&self) -> ExecutionLimits {
        match &self.limits {
            Some(source) => source(),
            None => ExecutionLimits::default(),
        }
    }
}

/// 按会话定位**当前 turn** 的账本登记表。
///
/// 账本按 turn 建、按 turn 撤;桥(审批/提问)只能拿到会话 id,所以用 id
/// 路由——它要的不是账本本身,而是“现在在等用户”这个事实。
type TurnBudgets = Arc<std::sync::Mutex<std::collections::HashMap<String, ExecutionBudget>>>;

/// 把执行预算账本接进 registry 的准入点(`denia_llm::RequestAdmission`)。
///
/// 两个枚举的翻译只做分类映射;**哪些来源吃用户预算**由账本判据
/// (`RequestKind::metered`)决定,这里不另写一份策略。
pub(crate) struct BudgetGate(pub(crate) ExecutionBudget);

impl denia_llm::RequestAdmission for BudgetGate {
    fn admit(&self, call: &denia_llm::RequestCall) -> Result<(), denia_llm::AdmissionRefusal> {
        use denia_llm::RequestKind as Llm;
        use denia_token_meter::RequestKind as Ledger;
        let kind = match call.kind {
            Llm::UserStep => Ledger::Step,
            Llm::Continuation => Ledger::Continuation,
            Llm::Compaction => Ledger::Compaction,
            Llm::Wake => Ledger::Wake,
            Llm::Subagent => Ledger::Subagent,
            Llm::SessionTitle => Ledger::SessionTitle,
            Llm::Connectivity => Ledger::Connectivity,
            Llm::Git => Ledger::Git,
            Llm::Other => Ledger::Other,
        };
        let source = if kind.metered() {
            RequestSource::Metered {
                session: call.session.clone(),
                turn: call.turn,
                step: call.step,
                kind,
            }
        } else {
            RequestSource::Bypass { kind }
        };
        self.0
            .admit_request(&LedgerRequestCall {
                source,
                sequence: call.sequence,
                attempt: call.attempt,
            })
            .map(|_| ())
            .map_err(|refusal| denia_llm::AdmissionRefusal {
                boundary: refusal.boundary.label().to_string(),
                limit: refusal.limit,
                used: refusal.used,
                message: refusal.message,
            })
    }
}

/// 审批等待区间的墙钟守卫:进桥前 `pause`,出桥后 `resume`。
///
/// 这是 turn 内**唯一**的“等用户”区间。子代理与后台任务的等待不在此列:
/// 子任务还在跑,父轮次并没有空闲(计划:仍运行的子任务不能伪装为空闲)。
struct PauseGuard(Option<ExecutionBudget>);

impl Drop for PauseGuard {
    fn drop(&mut self) {
        if let Some(budget) = &self.0 {
            budget.resume();
        }
    }
}

/// 审批桥包装:等待用户决策的墙钟不计入主动执行时间。
struct PausingApprovalBridge {
    inner: Arc<dyn ApprovalBridge>,
    budgets: TurnBudgets,
}

#[async_trait]
impl ApprovalBridge for PausingApprovalBridge {
    async fn request(
        &self,
        session_id: &str,
        request_id: &str,
        cancel: CancellationToken,
    ) -> denia_core::session::PlanReviewDecision {
        let _guard = pause_guard(&self.budgets, session_id);
        self.inner.request(session_id, request_id, cancel).await
    }
}

/// 提问桥包装:与审批同理。
struct PausingAskBridge {
    inner: Arc<dyn denia_tools::AskBridge>,
    budgets: TurnBudgets,
}

#[async_trait]
impl denia_tools::AskBridge for PausingAskBridge {
    async fn ask(
        &self,
        session_id: &str,
        request_id: &str,
        call_id: &str,
        questions: &[denia_core::session::AskQuestion],
        timeout_ms: u64,
        cancel: CancellationToken,
    ) -> denia_core::session::AskResolution {
        let _guard = pause_guard(&self.budgets, session_id);
        self.inner
            .ask(session_id, request_id, call_id, questions, timeout_ms, cancel)
            .await
    }
}

fn pause_guard(budgets: &TurnBudgets, session_id: &str) -> PauseGuard {
    let budget = budgets
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .get(session_id)
        .cloned();
    if let Some(budget) = &budget {
        budget.pause();
    }
    PauseGuard(budget)
}

/// Drives user turns on one session at a time.
pub struct SessionDriver {
    runtime: Option<Arc<dyn denia_tools::capabilities::AgentRuntime>>,
    registry: Arc<LlmRegistry>,
    /// 工具注册表。
    ///
    /// 用 `ArcSwap` 而不是裸 `Arc`:MCP 服务器配置变更会增删工具,整份
    /// 注册表整体替换而不是原地增删——读到的永远是一份自洽的工具面,
    /// 不会出现"提示词里有这个工具、注册表里却没了"的窗口期。
    tools: Arc<ArcSwap<ToolRegistry>>,
    /// 工具面变更时对外广播(供 server 侧刷新系统提示词的 MCP schema)。
    system_prompt: Arc<ArcSwap<SystemPrompt>>,
    file_history: Option<Arc<dyn FileHistoryProvider>>,
    approval: Option<Arc<dyn ApprovalBridge>>,
    /// 提问通道(`ask` 工具用);`None` 时该工具按 unavailable 结算。
    ask: Option<Arc<dyn denia_tools::AskBridge>>,
    /// agent preset 名册:按会话记录中的 preset 收窄工具面与 persona。
    /// `None` 时所有会话都运行出厂全量组装(无 preset 的部署)。
    presets: Option<Arc<dyn AgentPresetSource>>,
    /// 层叠上下文管理:LLM 总结压缩(学 dsh 压力驱动 + Claude Code compact)。
    settings: DriverSettings,
    settings_source: Option<Arc<dyn DriverSettingsSource>>,
    compaction_states:
        std::sync::Mutex<std::collections::HashMap<String, Arc<compaction_state::CompactionState>>>,
    /// 文件读取状态(按会话):重复读取去重 + 写前新鲜度校验。
    /// 同一会话的工具调用共享一张表。
    read_states: std::sync::Mutex<
        std::collections::HashMap<String, denia_tools::read_state::SharedReadState>,
    >,
    /// 会话级审批放行表(按会话 × 操作类别):自动编辑档下用户对某类
    /// 审批(bash 写/删除/区外写)选"本窗口放行"后,同类操作在本会话
    /// 内直接放行不再询问。运行时内存态,不落盘,进程重启后自然失效。
    ask_grants: std::sync::Mutex<
        std::collections::HashMap<
            String,
            std::collections::HashSet<denia_tools::permission::ActionClass>,
        >,
    >,
    /// 已装载完整参数定义的 MCP 工具(按会话):两段式工具面的第二段。
    /// 未装载的 `mcp__*` 工具在请求里只带名称与描述,首次调用拦截返回
    /// 参数定义并登记到此表,之后 schema 全量进请求、调用正常执行。
    /// 运行时内存态:进程重启或会话淘汰后回到轻量态,重新装载即可。
    mcp_loads:
        std::sync::Mutex<std::collections::HashMap<String, std::collections::HashSet<String>>>,
    /// 执行预算:上限来源与时钟(默认 128 步 / 256 次请求 / 60 分钟)。
    budget_policy: std::sync::RwLock<BudgetPolicy>,
    /// 按会话登记的当前 turn 账本(审批/提问等待期间暂停计时)。
    turn_budgets: TurnBudgets,
}

impl SessionDriver {
    pub fn new(
        registry: Arc<LlmRegistry>,
        tools: Arc<ToolRegistry>,
        system_prompt: Arc<ArcSwap<SystemPrompt>>,
    ) -> Self {
        Self {
            registry,
            tools: Arc::new(ArcSwap::from_pointee((*tools).clone())),
            system_prompt,
            runtime: None,
            file_history: None,
            approval: None,
            ask: None,
            presets: None,
            settings: DriverSettings::default(),
            settings_source: None,
            compaction_states: std::sync::Mutex::new(std::collections::HashMap::new()),
            read_states: std::sync::Mutex::new(std::collections::HashMap::new()),
            ask_grants: std::sync::Mutex::new(std::collections::HashMap::new()),
            mcp_loads: std::sync::Mutex::new(std::collections::HashMap::new()),
            budget_policy: std::sync::RwLock::new(BudgetPolicy::default()),
            turn_budgets: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    pub fn with_runtime(
        mut self,
        runtime: Arc<dyn denia_tools::capabilities::AgentRuntime>,
    ) -> Self {
        self.runtime = Some(runtime);
        self
    }

    /// 覆盖层叠上下文管理配置(LLM 压缩;默认值见
    /// [`CompactionSettings::default`])。
    pub fn with_compaction(mut self, settings: CompactionSettings) -> Self {
        self.settings.compaction = settings;
        self
    }

    /// 覆盖工具结果微压缩配置(默认值见
    /// [`MicrocompactSettings::default`])。
    pub fn with_microcompact(mut self, settings: microcompact::MicrocompactSettings) -> Self {
        self.settings.microcompact = settings;
        self
    }

    /// 当前微压缩配置(server 侧构造 driver 时同步设置)。
    pub fn microcompact_settings(&self) -> microcompact::MicrocompactSettings {
        self.driver_settings().microcompact.clone()
    }

    /// 摘要阈值 token 数(供微压缩取 90% 作为自己的触发线)。
    ///
    /// 与 [`compact::should_compact`] 同口径:有效窗口 = 窗口 - 输出预留,
    /// 阈值 = 有效窗口 - buffer。
    pub(crate) fn compaction_threshold_tokens(
        &self,
        pressure: &denia_token_meter::ContextPressure,
        settings: &CompactionSettings,
    ) -> Option<u64> {
        let window = pressure.context_window?;
        if window == 0 {
            return None;
        }
        let reserve = settings.summary_max_tokens.min(window);
        let effective = window.saturating_sub(reserve);
        Some(effective.saturating_sub(13_000))
    }

    /// 覆盖工具并行执行配置(默认值见 [`ParallelSettings::default`])。
    pub fn with_parallel(mut self, settings: ParallelSettings) -> Self {
        self.settings.parallel = settings;
        self
    }

    pub fn with_settings_source(mut self, source: Arc<dyn DriverSettingsSource>) -> Self {
        self.settings_source = Some(source);
        self
    }

    pub fn driver_settings(&self) -> Arc<DriverSettings> {
        Arc::new(
            self.settings_source
                .as_ref()
                .map_or_else(|| self.settings.clone(), |source| source.settings()),
        )
    }

    fn compaction_state_for(&self, id: &str) -> Arc<compaction_state::CompactionState> {
        self.compaction_states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(id.to_owned())
            .or_default()
            .clone()
    }

    pub fn clear_compaction_state(&self, id: &str) {
        self.compaction_states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }

    /// 启用文件历史:回退功能依赖此提供者。
    pub fn with_file_history(mut self, provider: Arc<dyn FileHistoryProvider>) -> Self {
        self.file_history = Some(provider);
        self
    }

    /// 启用审批通道(抄 dsh ctx.approval);无通道时升权请求 fail-closed。
    ///
    /// 桥外面包一层等待守卫:审批期间用户在思考,那段时间不算主动执行时间。
    pub fn with_approval(mut self, bridge: Arc<dyn ApprovalBridge>) -> Self {
        self.approval = Some(Arc::new(PausingApprovalBridge {
            inner: bridge,
            budgets: self.turn_budgets.clone(),
        }));
        self
    }

    /// 启用提问通道(`ask` 工具);无通道时该工具按 `unavailable` 结算,
    /// 模型仍能自行决策继续(不像 dsh 那样把整次调用变成硬错误)。
    pub fn with_ask(mut self, bridge: Arc<dyn denia_tools::AskBridge>) -> Self {
        self.ask = Some(Arc::new(PausingAskBridge {
            inner: bridge,
            budgets: self.turn_budgets.clone(),
        }));
        self
    }

    /// 安装执行预算上限来源:宿主按 turn 读取配置快照(在途轮次不受
    /// 中途改配置影响)。未装即用默认边界。
    pub fn set_execution_limits_source(
        &self,
        source: Arc<dyn Fn() -> ExecutionLimits + Send + Sync>,
    ) {
        self.budget_policy
            .write()
            .unwrap_or_else(|poison| poison.into_inner())
            .limits = Some(source);
    }

    /// 注入账本时钟(测试用假时钟推进主动执行时间)。
    pub fn with_budget_clock(mut self, clock: Clock) -> Self {
        self.budget_policy
            .get_mut()
            .unwrap_or_else(|poison| poison.into_inner())
            .clock = Some(clock);
        self
    }

    /// 当前生效的执行预算上限快照。
    pub fn execution_limits(&self) -> ExecutionLimits {
        self.budget_policy
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .limits()
    }

    /// 开始一轮:按当前策略建账本快照并登记到会话名下。
    pub(crate) fn begin_turn_budget(&self, session_id: &str) -> ExecutionBudget {
        let policy = self
            .budget_policy
            .read()
            .unwrap_or_else(|poison| poison.into_inner());
        let limits = policy.limits();
        let budget = match &policy.clock {
            Some(clock) => ExecutionBudget::with_clock(limits, clock.clone()),
            None => ExecutionBudget::new(limits),
        };
        drop(policy);
        self.turn_budgets
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(session_id.to_string(), budget.clone());
        budget
    }

    /// 该会话当前在途轮次的账本(`None` = 没有正在跑的轮次)。
    ///
    /// 手动压缩这类轮次外的请求拿不到账本:它们不由轮次预算管(一次用户
    /// 主动发起的压缩不该撞上“本轮请求已用完”),但它们的用量照样入账。
    pub fn active_budget(&self, session_id: &str) -> Option<ExecutionBudget> {
        self.turn_budgets
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(session_id)
            .cloned()
    }

    /// 一轮结束:撤下登记。下一轮重新建账本(预算按轮重新获得)。
    pub(crate) fn end_turn_budget(&self, session_id: &str) {
        self.turn_budgets
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(session_id);
    }

    /// 启用 agent preset 名册:会话按自己日志里的 preset 组装工具面与 persona。
    pub fn with_presets(mut self, presets: Arc<dyn AgentPresetSource>) -> Self {
        self.presets = Some(presets);
        self
    }

    /// 解析某会话应当运行的组装。
    ///
    /// 会话未指定 preset 时用部署默认;日志里的 id 已从名册消失(用户删掉
    /// 它、或文件损坏)时同样回退到默认组装——运行中的会话不该因为一个坏
    /// 文件就连请求都发不出去,名册会把这个事实以 broken 行呈现给用户。
    pub fn preset_for(
        &self,
        session_preset: Option<&str>,
    ) -> Option<denia_core::preset::AgentPreset> {
        let source = self.presets.as_ref()?;
        let requested = session_preset
            .map(str::to_string)
            .unwrap_or_else(|| source.default_id());
        source
            .resolve(&requested)
            .or_else(|| source.resolve(&source.default_id()))
    }

    /// 会话生效 preset 的功能开关(无名册部署 = 全开)。注入通道、压缩闸门
    /// 与 goal 续跑等"装配之外"的联动统一从这里取开关,保证与工具面收窄
    /// 读到的是同一份声明。
    pub fn preset_features(
        &self,
        session_preset: Option<&str>,
    ) -> denia_core::preset::PresetFeatures {
        self.preset_for(session_preset)
            .map(|preset| preset.features)
            .unwrap_or_default()
    }

    /// 某会话实际生效的功能开关。
    ///
    /// 子代理优先用**派遣时冻结**的父 features 快照：父 preset 文件后来被删或
    /// 损坏时，child 不会回退到部署默认（全开）而悄悄扩大能力（计划 H07）。
    /// 快照缺失（旧会话）时退回按会话记录的 preset 解析。
    pub fn features_for(&self, session: &Session) -> denia_core::preset::PresetFeatures {
        if let Some(child) = session.header().subagent.as_ref()
            && let Some(features) = child.parent_features
        {
            return features;
        }
        self.preset_features(session.agent_preset().as_deref())
    }

    /// 当前工具注册表(读路径无锁,拿到的是一份自洽快照)。
    pub fn tools(&self) -> Arc<ToolRegistry> {
        Arc::clone(&self.tools.load())
    }

    /// 宿主能力接口（会话授权查询等）；无 runtime 的部署返回 None。
    pub fn agent_runtime(&self) -> Option<Arc<dyn denia_tools::capabilities::AgentRuntime>> {
        self.runtime.clone()
    }

    /// 会话当前**有效**的可执行工具名；`None` = 没有任何收窄（按注册表全集）。
    ///
    /// 子代理用派遣时冻结的授权（旧描述符按历史只读上限保守构造）；其余会话
    /// 用 preset 的 features 与 tools 白名单收窄。装配（schema）、目录投影与
    /// 执行派发共用这一个判定，避免"模型看得见却执行不了"或反之。
    pub fn session_tool_face(&self, session: &Session) -> Option<Vec<String>> {
        if let Some(child) = session.header().subagent.as_ref() {
            return Some(child.effective_tools.clone().unwrap_or_else(|| {
                denia_core::subagent::legacy_child_tools(child.allowed_tools.as_deref())
            }));
        }
        let preset = self.preset_for(session.agent_preset().as_deref())?;
        if preset.tools.is_none() && preset.features.is_default() {
            return None;
        }
        let excluded = preset.features.excluded_tools();
        let mut face: Vec<String> = self
            .tools()
            .names()
            .into_iter()
            .filter(|name| !excluded.contains(&name.as_str()))
            .filter(|name| {
                preset
                    .tools
                    .as_ref()
                    .is_none_or(|list| list.iter().any(|item| item == name))
            })
            .collect();
        face.sort();
        face.dedup();
        Some(face)
    }

    /// 整体替换工具注册表(MCP 服务器配置变更后由 server 调用)。
    ///
    /// 已在飞的工具调用持有旧注册表的 `Arc`,不受影响;新 step 的装配与
    /// 派发都用新表,模型可见与可执行在同一次替换里对齐。
    pub fn replace_tools(&self, registry: ToolRegistry) {
        self.tools.store(Arc::new(registry));
    }

    /// 当前层叠上下文管理配置(server 热更新用)。
    pub fn compaction_settings(&self) -> CompactionSettings {
        self.driver_settings().compaction.clone()
    }

    /// 供 server 端热加载写入同一 `ArcSwap`。
    pub fn system_prompt_handle(&self) -> Arc<ArcSwap<SystemPrompt>> {
        self.system_prompt.clone()
    }

    /// 估算当前提示词的组成部分大小(字节):系统提示词(含模型框架)与
    /// 工具声明。供前端"上下文窗口占用"面板展示占比。
    pub fn prompt_parts(
        &self,
        cwd: &str,
        selection: &ModelSelection,
    ) -> Result<(usize, usize), String> {
        use denia_system_prompt::AssembleContext;
        let assembly = self
            .system_prompt
            .load()
            .assemble(&AssembleContext {
                cwd: Some(cwd.to_string()),
                model: Some(selection.model.clone()),
                provider: Some(selection.provider.clone()),
                ..Default::default()
            })
            .map_err(|error| error.to_string())?;
        let system = denia_system_prompt::render_prompt(&assembly);
        let framed = denia_system_prompt::frame_system_prompt_for_model(&system);
        let tools = serde_json::to_string(&assembly.tools).map_err(|error| error.to_string())?;
        Ok((system.len() + framed.len(), tools.len()))
    }

    /// 手动压缩(上下文面板按钮):从日志最近一次请求头部恢复请求形态
    /// (模型 / 系统提示 / 工具集),执行与自动路径完全相同的总结压缩。
    /// 绕过压力闸门 —— 用户主动触发,不要求压力 ≥ `compact_ratio`。
    /// 会话组装关闭了压缩功能时拒绝(与自动闸门同一开关)。
    ///
    /// 调用方负责:运行位占用与 cancel 生命周期、提前拒绝无请求历史的
    /// 会话、以及 `CompactionSummary` 事件落盘与广播。
    pub async fn compact_manually(
        &self,
        session: &Arc<Session>,
        cancel: CancellationToken,
    ) -> Result<Option<CompactOutcome>, LlmFailure> {
        if !self.features_for(session).compaction {
            return Err(LlmFailure::new(
                codes::UNKNOWN,
                "本会话的 agent 组装未启用上下文压缩".to_string(),
            ));
        }
        let (header, turn) = session.with_events(|events| {
            let header = events.iter().rev().find_map(|item| match &item.event {
                SessionEvent::RequestHeader { header, .. } => Some(header.clone()),
                _ => None,
            });
            let turn = events
                .iter()
                .rev()
                .find_map(|item| match item.event {
                    SessionEvent::TurnStart { turn } => Some(turn),
                    _ => None,
                })
                .unwrap_or(0);
            (header, turn)
        });
        let header = header.ok_or_else(|| {
            LlmFailure::new(
                codes::UNKNOWN,
                "session has no request header to restore".to_string(),
            )
        })?;
        let selection = ModelSelection {
            provider: header.config.provider.clone(),
            model: header.config.model.clone(),
            reasoning_effort: header.config.reasoning_effort.clone(),
        };
        let framed_system = header.system.clone().unwrap_or_default();
        // 压缩的是已发生的历史:事件归属于日志中最后一次出现的轮次,step 0
        // 表示轮次外的合成步骤(与自动压缩一样仅作元数据)。
        let settings = self.driver_settings();
        if !settings.compaction.compact_enabled {
            return Err(LlmFailure::new(codes::UNKNOWN, "上下文压缩已在设置中关闭"));
        }
        self.compact_context(
            session,
            &selection,
            &framed_system,
            &header.tools,
            turn,
            0,
            &cancel,
            None,
            &settings.compaction,
            // 手动压缩是用户主动发起的一次请求,不属于任何在途轮次:没有账本
            // 就不过准入(也不该撞上"本轮请求已用完"),但用量照样入账。
            None,
        )
        .await
    }

    /// Runs one user turn to completion and returns the reason it ended.
    ///
    /// `session` is shared (`Arc`) because tools like `todo_write` append
    /// log-only events through a `'static` sink wired back to the same log.
    /// `images` are inline pasted images (vision models); `files` are paths of
    /// uploaded files; `quoted` are trajectory references (title, text),
    /// injected as a context message before the prompt — same channel as the
    /// file notice.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_turn(
        &self,
        session: &Arc<Session>,
        selection: &ModelSelection,
        prompt: &str,
        images: Vec<denia_core::message::ImageData>,
        files: Vec<String>,
        quoted: Vec<(String, String)>,
        // 当前模型是否标记为可识图(read_file 图片注入与图片发送的依据)。
        vision_supported: bool,
        cancel: CancellationToken,
        emit: Arc<dyn Fn(&SessionEnvelope) + Send + Sync>,
    ) -> TurnEndReason {
        // 每轮独立检测重复输出，用户的新指令不继承上一轮的指纹。
        let loop_guard = loop_guard::LoopGuard::default();
        // 文件读取状态同样按会话持有:跨 turn 保留(同一会话的下一轮里,
        // "上一轮读过的文件"依然算读过,不该要求重读)。
        let read_state = self.read_state_for(session.id());
        let mut state = TurnState::new(
            self,
            session.clone(),
            emit,
            selection.clone(),
            vision_supported,
            cancel,
            loop_guard,
            read_state,
        );
        state.output_store =
            denia_tools::output::OutputStore::open(session.directory().join("tool-output"))
                .await
                .ok();
        let started_after = session.with_events(injections::last_seq);
        let result = turn::run_turn_inner(self, &mut state, prompt, images, files, quoted).await;
        // 轮的账本随轮撤下:下一轮重新建(预算按轮重新获得;轮与轮之间的
        // 等待——goal paused、等用户下一条消息——根本不在任何账本区间内)。
        self.end_turn_budget(session.id());
        match result {
            Ok(reason) => reason,
            Err(failure) => close_failed_turn(&state, started_after, failure).unwrap_or_else(
                |failure| TurnEndReason::Error { failure },
            ),
        }
    }

    /// 取(或首次创建)某会话的文件读取状态表。
    ///
    /// 按会话持有:同一会话的所有工具调用共享一张表,不同会话互不干扰。
    /// 表本身是 `Arc<Mutex<..>>`,借出的是克隆的句柄。
    pub fn read_state_for(&self, session_id: &str) -> denia_tools::read_state::SharedReadState {
        let mut map = self
            .read_states
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        map.entry(session_id.to_string())
            .or_insert_with(denia_tools::read_state::shared)
            .clone()
    }

    /// 清空某会话的文件读取状态(压缩后调用)。
    ///
    /// 压缩摘要已接管旧内容的记忆职责;清空后模型若需要文件内容会重新读取,
    /// 而不是依赖"读过"的标记去写一个自己已经看不到内容的文件。
    ///
    /// 整条移除而不是留空表:表按会话 id 累积,长期运行下"开过的每个会话"
    /// 都会留下一个条目;清空时顺手删掉,让不再用的表能被回收。
    pub fn clear_read_state(&self, session_id: &str) {
        let mut map = self
            .read_states
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(state) = map.remove(session_id)
            && let Ok(mut state) = state.lock()
        {
            state.clear();
        }
    }

    /// 会话是否已放行某类审批操作(自动编辑档"本窗口放行"的查询口)。
    pub fn ask_granted(
        &self,
        session_id: &str,
        class: denia_tools::permission::ActionClass,
    ) -> bool {
        self.ask_grants
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(session_id)
            .is_some_and(|set| set.contains(&class))
    }

    /// 记录一次"本窗口放行":该会话内同类审批操作不再询问。
    pub fn grant_ask_class(&self, session_id: &str, class: denia_tools::permission::ActionClass) {
        self.ask_grants
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .entry(session_id.to_string())
            .or_default()
            .insert(class);
    }

    /// 清空某会话的审批放行表(会话删除时调用,防止句柄泄漏式累积)。
    pub fn clear_ask_grants(&self, session_id: &str) {
        self.ask_grants
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(session_id);
    }

    /// 某会话是否已装载某 MCP 工具的完整参数定义。
    pub fn mcp_loaded(&self, session_id: &str, qualified: &str) -> bool {
        self.mcp_loads
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(session_id)
            .is_some_and(|set| set.contains(qualified))
    }

    /// 登记一次 MCP 工具装载:之后该工具的完整 schema 随请求下发,
    /// 调用不再被拦截。重复登记无害(集合语义)。
    pub fn load_mcp_tool(&self, session_id: &str, qualified: &str) {
        self.mcp_loads
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .entry(session_id.to_string())
            .or_default()
            .insert(qualified.to_string());
    }

    /// 清空某会话的 MCP 装载表(会话淘汰时调用,防句柄累积)。
    /// 清空后回到轻量态:工具只剩名称与描述,再次调用会重新装载。
    pub fn clear_mcp_loads(&self, session_id: &str) {
        self.mcp_loads
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(session_id);
    }
}

fn close_failed_turn(
    state: &TurnState,
    started_after: u64,
    failure: LlmFailure,
) -> Result<TurnEndReason, LlmFailure> {
    let reason = TurnEndReason::Error { failure };
    let open_steps = state.session.with_events(|events| {
        // 输入或 turn-start 写入失败时，不能替本轮之前的裸开日志补终态。
        let current = &events[events.partition_point(|event| event.seq <= started_after)..];
        let start = current.iter().position(|event| {
            matches!(event.event, SessionEvent::TurnStart { turn } if turn == state.turn)
        })?;
        let mut open_steps = std::collections::BTreeSet::new();
        for event in &current[start + 1..] {
            match event.event {
                SessionEvent::StepStart { turn, step } if turn == state.turn => {
                    open_steps.insert(step);
                }
                SessionEvent::StepEnd { turn, step } if turn == state.turn => {
                    open_steps.remove(&step);
                }
                SessionEvent::TurnEnd { turn, .. } if turn == state.turn => return None,
                _ => {}
            }
        }
        Some(open_steps)
    });
    if let Some(open_steps) = open_steps {
        for step in open_steps {
            append(
                &state.session,
                &state.emit,
                SessionEvent::StepEnd {
                    turn: state.turn,
                    step,
                },
            )?;
        }
        append(
            &state.session,
            &state.emit,
            SessionEvent::TurnEnd {
                turn: state.turn,
                reason: reason.clone(),
            },
        )?;
    }
    Ok(reason)
}

/// Append one event to the log, then mirror it to the live subscribers.
/// The log is the ordering authority; emit failures are ignored (subscribers
/// re-sync from the log snapshot on reconnect).
pub(crate) fn append(
    session: &Session,
    emit: &Arc<dyn Fn(&SessionEnvelope) + Send + Sync>,
    event: SessionEvent,
) -> Result<SessionEnvelope, LlmFailure> {
    let envelope = session.append(event).map_err(|error| {
        LlmFailure::new(
            codes::STORAGE,
            format!("会话日志写入失败:{error}。磁盘可能已满或目录不可写。"),
        )
    })?;
    emit(&envelope);
    Ok(envelope)
}

/// Should the user-audience system-prompt copy be logged for this step?
/// O(1): the session maintains the last logged copy at append time.
fn should_log_system_prompt(session: &Session, step: u32, text: &str) -> bool {
    if step == 1 {
        return true;
    }
    session.last_system_prompt().as_deref() != Some(text)
}

/// 请求前的日志刷盘检查点(对齐 dsh checkpoint-policy):请求前缀落盘
/// 成功才派发;失败 fail-closed(不发出请求)。
pub(crate) fn flush_before_dispatch(session: &Session) -> Result<(), LlmFailure> {
    session.flush().map_err(|error| {
        LlmFailure::new(
            codes::STORAGE,
            format!("会话日志刷盘失败:{error}。请求未派发,避免崩溃后请求上下文不完整。"),
        )
    })
}

/// Build a [`RequestHeaderSnapshot`] for the upcoming request.
pub(crate) fn build_header_snapshot(
    selection: &ModelSelection,
    framed_system: &str,
    tools: &[denia_core::tool::ToolSchema],
) -> RequestHeaderSnapshot {
    RequestHeaderSnapshot {
        config: LlmCallConfig {
            provider: selection.provider.clone(),
            model: selection.model.clone(),
            reasoning_effort: selection.reasoning_effort.clone(),
            temperature: None,
            max_tokens: None,
            stop: Vec::new(),
        },
        system: Some(framed_system.to_string()),
        tools: tools.to_vec(),
    }
}

/// One user turn's shared, mutable state (event wiring + loop counters).
pub(crate) struct TurnState {
    pub session: Arc<Session>,
    pub emit: Arc<dyn Fn(&SessionEnvelope) + Send + Sync>,
    pub selection: ModelSelection,
    pub vision_supported: bool,
    pub cancel: CancellationToken,
    pub file_history: Option<Arc<dyn FileHistoryBackend>>,
    /// 本轮 turn 号(从 1 开始)。
    pub turn: u32,
    /// 自纠反馈注入计数(每轮上限 [`errors::MAX_FEEDBACK`])。
    pub feedback: u32,
    /// 当前轮次的死循环检测器。
    pub loop_guard: loop_guard::LoopGuard,
    /// 文件读取状态表(按会话共享):重复读取去重 + 写前新鲜度校验。
    pub read_state: denia_tools::read_state::SharedReadState,
    pub output_store: Option<Arc<denia_tools::output::OutputStore>>,
    pub replay: denia_llm::ReplayPolicy,
    pub output_budget: u64,
    pub continuations: u32,
    pub last_real_input: u64,
    /// dsh 对齐:请求头按需落盘的基准(最近快照即重建)。
    pub last_header: Option<RequestHeaderSnapshot>,
    /// 路由元数据(仅在变化时写 request-context)。
    pub last_context: Option<(String, String, Option<u64>)>,
    /// 日志里是否已有任何 request-header(resume/initial 判定)。
    pub has_request_header: bool,
    /// 路由通告的上下文窗口(解析失败为 None,不影响主流程)。
    pub context_window: Option<u64>,
    /// 每步已自动重试的次数(报错摘要用;attempt 环内更新)。
    pub step_retries: u32,
    pub settings: Arc<DriverSettings>,
    pub compaction_state: Arc<compaction_state::CompactionState>,
    /// 本轮的执行预算账本(步数 / 请求次数 / 主动执行时间;上限取本轮
    /// 开始时的快照)。turn 内所有请求路径共享同一份句柄。
    pub budget: ExecutionBudget,
}

impl TurnState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        driver: &SessionDriver,
        session: Arc<Session>,
        emit: Arc<dyn Fn(&SessionEnvelope) + Send + Sync>,
        selection: ModelSelection,
        vision_supported: bool,
        cancel: CancellationToken,
        loop_guard: loop_guard::LoopGuard,
        read_state: denia_tools::read_state::SharedReadState,
    ) -> Self {
        let settings = driver.driver_settings();
        let compaction_state = driver.compaction_state_for(session.id());
        let budget = driver.begin_turn_budget(session.id());
        Self {
            settings,
            compaction_state,
            budget,
            session,
            emit,
            selection,
            vision_supported,
            cancel,
            file_history: None,
            turn: 0,
            feedback: 0,
            loop_guard,
            read_state,
            output_store: None,
            replay: denia_llm::ReplayPolicy::default(),
            output_budget: 32_768,
            continuations: 0,
            last_real_input: 0,
            last_header: None,
            last_context: None,
            has_request_header: false,
            context_window: None,
            step_retries: 0,
        }
    }

    pub(crate) fn cwd(&self) -> PathBuf {
        PathBuf::from(self.session.header().cwd.clone())
    }

    /// 当前会话权限模式(O(1) 投影)。
    pub(crate) fn permission_mode(&self) -> denia_core::session::PermissionMode {
        self.session.permission_mode()
    }
}

#[cfg(test)]
mod tests;
