use super::*;

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeConfig {
    pub max_jobs: usize,
    pub retained_jobs: usize,
    pub output_bytes: usize,
    pub max_wait_ms: u64,
    pub job_timeout_ms: u64,
    pub max_pending_messages: usize,
    pub max_consecutive_wakes: usize,
    /// 工作区指令渲染总预算（字节）；0 = 禁用 AGENTS.md 注入。
    pub workspace_instructions_max_bytes: u64,
    /// 单个 AGENTS.md 的读取上限（字节）；超限整份跳过。
    pub workspace_instructions_max_source_bytes: u64,
    /// 技能目录里单条描述的最大字符数。
    pub skill_catalog_description_max_chars: usize,
    /// 子代理派遣目录的渲染预算（字节）：超限按 UTF-8 边界截断并明示。
    /// 与工作区指令预算分开：目录是每步注入的固定义务，不能挤占指令预算。
    pub subagent_catalog_max_bytes: u64,
    /// 项目记忆总闸:关闭后注入与写入同时停(读写两端同步)。
    pub memory_enabled: bool,
    /// MEMORY.md 索引注入的字节预算(超限 UTF-8 边界截断并附告警)。
    pub project_memory_max_bytes: u64,
}
impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_jobs: 8,
            retained_jobs: 64,
            output_bytes: 64 * 1024,
            max_wait_ms: 600_000,
            job_timeout_ms: 600_000,
            max_pending_messages: 64,
            max_consecutive_wakes: 3,
            workspace_instructions_max_bytes: 65_536,
            workspace_instructions_max_source_bytes: 1_048_576,
            skill_catalog_description_max_chars: 500,
            subagent_catalog_max_bytes: 8_192,
            memory_enabled: true,
            project_memory_max_bytes: 25_600,
        }
    }
}
pub fn validate_config(value: Value) -> Result<Value, String> {
    let c: RuntimeConfig =
        serde_json::from_value(value).map_err(|e| format!("运行时配置无效：{e}"))?;
    if !(1..=64).contains(&c.max_jobs)
        || c.retained_jobs < c.max_jobs
        || c.retained_jobs > 1024
        || !(1024..=1048576).contains(&c.output_bytes)
        || !(1..=600_000).contains(&c.max_wait_ms)
        || !(1..=86_400_000).contains(&c.job_timeout_ms)
        || !(1..=1024).contains(&c.max_pending_messages)
        || !(1..=16).contains(&c.max_consecutive_wakes)
        || c.workspace_instructions_max_bytes > 1_048_576
        || !(1..=16_777_216).contains(&c.workspace_instructions_max_source_bytes)
        || !(1..=65_536).contains(&c.skill_catalog_description_max_chars)
        || !(1_024..=1_048_576).contains(&c.subagent_catalog_max_bytes)
        || !(1..=1_048_576).contains(&c.project_memory_max_bytes)
    {
        return Err("运行时配置超出允许范围".into());
    }
    serde_json::to_value(c).map_err(|e| e.to_string())
}

/// goals 模式配置。
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct GoalsConfig {
    /// 全局 token 预算上限:设置目标预算时超过此值被拒绝。
    pub max_goal_token_budget: u64,
    /// 每个目标的续跑轮数上限;耗尽后停止自动续跑(状态保持 active,
    /// resume 被拒)。
    pub max_rounds: u32,
    /// 连续执行失败(Error/LoopDetected)达到该次数即标记 blocked
    pub max_consecutive_failures: u32,
    /// 执行预算:一个 turn 内允许的模型步骤上限(0 = 不限,默认不限)。
    ///
    /// 归在 goals 命名空间下而不是新建一个:命名空间注册在 `state.rs`。
    pub max_execution_steps: u64,
    /// 执行预算:一个 turn 内的物理请求尝试上限(0 = 不限;含注册表内部
    /// 重试、流重试、续写与压缩请求)。
    pub max_execution_requests: u64,
    /// 执行预算:一个 turn 的主动执行时间上限,毫秒(0 = 不限;
    /// 等待用户审批/提问的区间已排除)。
    pub max_execution_active_ms: u64,
}
impl Default for GoalsConfig {
    fn default() -> Self {
        Self {
            max_goal_token_budget: 10_000_000,
            max_rounds: 256,
            max_consecutive_failures: 3,
            max_execution_steps: 0,
            max_execution_requests: 0,
            max_execution_active_ms: 0,
        }
    }
}
pub fn validate_goals_config(value: Value) -> Result<Value, String> {
    let c: GoalsConfig =
        serde_json::from_value(value).map_err(|e| format!("goals 配置无效：{e}"))?;
    if !(1..=100_000_000).contains(&c.max_goal_token_budget)
        || !(1..=10_000).contains(&c.max_rounds)
        || !(1..=16).contains(&c.max_consecutive_failures)
        // 执行预算允许 0:显式关掉某一道边界(其余两道仍生效)。
        || !(0..=100_000).contains(&c.max_execution_steps)
        || !(0..=1_000_000).contains(&c.max_execution_requests)
        || !(0..=86_400_000).contains(&c.max_execution_active_ms)
    {
        return Err("goals 配置超出允许范围".into());
    }
    serde_json::to_value(c).map_err(|e| e.to_string())
}

/// 从设置存储读执行预算上限。
///
/// 驱动器每轮取一次快照(调用者把它包成一个限源):读失败或字段缺失都回退
/// [`GoalsConfig::default`](默认不限)。配置里 `0` = 关闭该道边界,映射成
/// `u64::MAX`(与账本的 `0 = 不限` 等价,避免两层对“0”理解不一致)。
pub(super) fn execution_limits_from(
    settings: &denia_settings::SettingsStore,
) -> denia_agent_loop::ExecutionLimits {
    let config: GoalsConfig = settings
        .resolved("goals")
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    let map = |v: u64| if v == 0 { u64::MAX } else { v };
    denia_agent_loop::ExecutionLimits {
        max_steps: map(config.max_execution_steps),
        max_requests: map(config.max_execution_requests),
        max_active_ms: map(config.max_execution_active_ms),
    }
}
