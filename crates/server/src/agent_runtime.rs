//! Rust 原生子代理编排；复用 SessionDriver，只有一份会话日志与运行状态。
use crate::{
    jobs::Jobs,
    state::{LiveSessions, RunningGuard, ServerEvent},
    subagents::delivery::{Delivery, Notice},
};
use async_trait::async_trait;
use denia_core::{
    config::ModelSelection,
    session::{GoalOp, SessionEnvelope, SessionEvent, SubagentDescriptor, TurnEndReason},
    task::{StaleReason, TaskOutcome, TaskState, TaskStatus, ValidationVerdict, VerificationState},
};
use denia_tools::{ToolContext, capabilities::AgentRuntime};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak, atomic::Ordering},
};
use tokio_util::sync::CancellationToken;

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
    /// 技能目录里单条描述的最大字符数（对齐 dsh catalogDescriptionMaxLength）。
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

/// goals 模式配置(工作逻辑对照 codex `[goals]`;轮数上限对齐 dsh
/// `maxGoalRounds`)。
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct GoalsConfig {
    /// 全局 token 预算上限:设置目标预算时超过此值被拒绝(codex
    /// `max_goal_token_budget`)。
    pub max_goal_token_budget: u64,
    /// 每个目标的续跑轮数上限;耗尽后停止自动续跑(状态保持 active,
    /// resume 被拒),对齐 dsh 256 轮语义。
    pub max_rounds: u32,
    /// 连续执行失败(Error/LoopDetected)达到该次数即标记 blocked
    /// (codex `stop_active_goal_after_repeated_execution_failures`)。
    pub max_consecutive_failures: u32,
    /// 执行预算:一个 turn 内允许的模型步骤上限(默认 128)。
    ///
    /// 归在 goals 命名空间下而不是新建一个:命名空间注册在 `state.rs`。
    /// 执行预算本就是目标执行策略的一部分——它护栏的正是“目标续跑把一整轮
    /// 跑飞”这个失效模式。
    pub max_execution_steps: u64,
    /// 执行预算:一个 turn 内的物理请求尝试上限(默认 256;含注册表内部
    /// 重试、流重试、续写与压缩请求)。
    pub max_execution_requests: u64,
    /// 执行预算:一个 turn 的主动执行时间上限,毫秒(默认 60 分钟;
    /// 等待用户审批/提问的区间已排除)。
    pub max_execution_active_ms: u64,
}
impl Default for GoalsConfig {
    fn default() -> Self {
        Self {
            max_goal_token_budget: 10_000_000,
            max_rounds: 256,
            max_consecutive_failures: 3,
            max_execution_steps: 128,
            max_execution_requests: 256,
            max_execution_active_ms: 3_600_000,
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
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Child {
    id: String,
    parent_id: String,
    descriptor: SubagentDescriptor,
}

/// 从设置存储读执行预算上限。
///
/// 驱动器每轮取一次快照(调用者把它包成一个限源):读失败或字段缺失都回退
/// [`GoalsConfig::default`],即防护基线——预算闸门不会因为读不到配置而消失。
fn execution_limits_from(
    settings: &denia_settings::SettingsStore,
) -> denia_agent_loop::ExecutionLimits {
    let config: GoalsConfig = settings
        .resolved("goals")
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    denia_agent_loop::ExecutionLimits {
        max_steps: config.max_execution_steps,
        max_requests: config.max_execution_requests,
        max_active_ms: config.max_execution_active_ms,
    }
}
#[derive(Default)]
struct Inbox {
    seq: u64,
    pending: BTreeMap<u64, (String, String, String)>,
    seen: HashSet<String>,
}
struct Inner {
    home: PathBuf,
    sessions: Arc<denia_session::SessionStore>,
    live: Arc<LiveSessions>,
    settings: Arc<denia_settings::SettingsStore>,
    events: tokio::sync::broadcast::Sender<ServerEvent>,
    driver: OnceLock<Weak<denia_agent_loop::SessionDriver>>,
    children: Mutex<BTreeMap<String, Child>>,
    inboxes: Mutex<HashMap<String, Inbox>>,
    selections: Mutex<HashMap<String, ModelSelection>>,
    wakes: Mutex<HashMap<String, usize>>,
    /// 被占用而挂起的唤醒:目标会话当时正在跑,唤醒请求记在这里,由它的
    /// [`RunningGuard`]/`on_idle` 消费。
    ///
    /// 没有这张表时,"主代理正卡在 wait_agent 等子代理、子代理刚好结束"这种
    /// 常规交错会**永久丢消息**:子代理侧 enqueue 触发 wake,resume_pending
    /// 的 running CAS 失败后直接返回;而主代理那次 wait_agent 轮次结束时
    /// on_idle 只扫当时存在的 inbox 条目,投递与那一次扫描之间没有任何
    /// 再唤醒边沿。表现就是子代理已完成、主代理停在"等结果",必须用户再
    /// 发一条消息才收到。挂起表把"想唤醒"这个意图与"能不能现在唤醒"解耦。
    deferred_wakes: Mutex<HashSet<String>>,
    /// 完成通知的“已处理水位”：目标会话最后一次**真正开跑**时收件箱的
    /// 最大待收 seq。同一批待收消息反复唤醒失败（如磁盘不可写导致
    /// drain 失败）时，用它回到普通唤醒上限，避免“完成通知不受
    /// 上限约束”退化成无界自旋。
    completion_marks: Mutex<HashMap<String, u64>>,
    delivery: Arc<Delivery>,
    paused: Mutex<HashSet<String>>,
    /// 统一准入:创建、终态 child 恢复、消息唤醒共用同一份槽位账。
    admission: crate::subagents::policy::Admission,
    /// goal 轮连续失败计数(会话级内存护栏;成功轮清零,blocked 后移除)。
    goal_failures: Mutex<HashMap<String, u32>>,
    registry: Arc<denia_llm::LlmRegistry>,
    workspaces: Arc<crate::workspace::WorkspaceRegistry>,
    /// 子代理定义仓库（派遣解析的唯一来源）。
    profiles: Arc<crate::subagents::ProfileStore>,
    /// 浏览器中枢：child 停止/结束时只关闭它自己的 tab（计划 6.4）。
    /// 用 `RwLock<Option<..>>` 而不是 OnceLock：装配顺序上浏览器中枢晚于
    /// Runtime 建立，测试也需要替换成记录型中枢。
    browser: std::sync::RwLock<Option<denia_tools::BrowserHub>>,
    /// 测试专用故障注入点（生产构建里不存在这个字段）。
    #[cfg(test)]
    faults: Mutex<HashSet<String>>,
    pub jobs: Arc<Jobs>,
}
#[derive(Clone)]
pub struct Runtime {
    inner: Arc<Inner>,
}
impl Runtime {
    pub fn new(
        home: &Path,
        sessions: Arc<denia_session::SessionStore>,
        live: Arc<LiveSessions>,
        settings: Arc<denia_settings::SettingsStore>,
        events: tokio::sync::broadcast::Sender<ServerEvent>,
        registry: Arc<denia_llm::LlmRegistry>,
        workspaces: Arc<crate::workspace::WorkspaceRegistry>,
        profiles: Arc<crate::subagents::ProfileStore>,
    ) -> Result<Arc<Self>, String> {
        let mut children = BTreeMap::new();
        // 只读会话头，不加载/修复另一个实例正在写入的日志。
        use std::io::BufRead;
        for entry in sessions.list().map_err(|e| e.to_string())? {
            if entry.parent_session.is_none() {
                continue;
            }
            let file = std::fs::File::open(sessions.root().join(&entry.id).join("session.jsonl"))
                .map_err(|e| e.to_string())?;
            let line = std::io::BufReader::new(file)
                .lines()
                .next()
                .transpose()
                .map_err(|e| e.to_string())?
                .unwrap_or_default();
            let header: denia_core::session::SessionHeader =
                serde_json::from_str(&line).map_err(|e| e.to_string())?;
            if let (Some(descriptor), Some(parent_id)) = (header.subagent, header.parent_session) {
                children.insert(
                    entry.id.clone(),
                    Child {
                        id: entry.id,
                        parent_id,
                        descriptor,
                    },
                );
            }
        }
        Ok(Arc::new(Self {
            inner: Arc::new(Inner {
                home: home.into(),
                sessions,
                live,
                settings,
                events,
                driver: OnceLock::new(),
                children: Mutex::new(children),
                inboxes: Mutex::new(HashMap::new()),
                selections: Mutex::new(HashMap::new()),
                wakes: Mutex::new(HashMap::new()),
                deferred_wakes: Mutex::new(HashSet::new()),
                completion_marks: Mutex::new(HashMap::new()),
                delivery: Arc::new(Delivery::default()),
                paused: Mutex::new(HashSet::new()),
                admission: crate::subagents::policy::Admission::new(),
                goal_failures: Mutex::new(HashMap::new()),
                registry,
                workspaces,
                profiles,
                browser: std::sync::RwLock::new(None),
                #[cfg(test)]
                faults: Mutex::new(HashSet::new()),
                jobs: Jobs::new(),
            }),
        }))
    }
    pub fn attach(&self, driver: &Arc<denia_agent_loop::SessionDriver>) {
        if self.inner.driver.set(Arc::downgrade(driver)).is_err() {
            return;
        }
        self.start_delivery_worker();
        // 执行预算的上限来源:每轮读一次运行时配置(改配置下一轮生效;
        // 在途轮次保持自己的快照)。只捕设置存储,不捕整个 Runtime——
        // 后者会让驱动器把它拉活到进程末尾。
        let settings = self.inner.settings.clone();
        driver.set_execution_limits_source(Arc::new(move || execution_limits_from(&settings)));
        let runtime = self.clone();
        let mut done = self.inner.jobs.done.subscribe();
        tokio::spawn(async move {
            loop {
                match done.recv().await {
                    Ok(job) => {
                        if let Some(result) = runtime.inner.jobs.claim_notice(&job.id, &job.owner)
                            && let Err(e) = runtime
                                .enqueue(
                                    &job.owner,
                                    format!("job:{}", job.id),
                                    format!("[后台任务完成]\n{result}"),
                                    "job-completed".into(),
                                )
                                .await
                        {
                            runtime.inner.jobs.release_notice(&job.id, &job.owner);
                            tracing::error!(error=%e,"任务通知投递失败");
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        tracing::error!("后台任务通知队列溢出，请用 job_list 领取结果");
                    }
                    Err(_) => break,
                }
            }
        });
    }
    pub fn jobs(&self) -> Arc<Jobs> {
        self.inner.jobs.clone()
    }

    /// 挂载浏览器中枢：child 停止/结束时按会话归属关闭它自己的 tab。
    pub fn attach_browser(&self, hub: denia_tools::BrowserHub) {
        *self
            .inner
            .browser
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(hub);
    }

    /// 当前挂载的浏览器中枢（没有则不返回）。
    fn browser_hub(&self) -> Option<denia_tools::BrowserHub> {
        self.inner
            .browser
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    /// 关闭某会话名下的浏览器 tab（父会话与不支持归属的中枢都是 no-op）。
    async fn close_owned_tabs(&self, id: &str) {
        let Some(hub) = self.browser_hub() else {
            return;
        };
        let closed = hub.close_owned(id).await;
        if closed > 0 {
            tracing::info!(session = id, closed, "已关闭会话名下遗留的浏览器 tab");
        }
    }

    /// 测试专用故障注入：在生产构建里恒为 `false`。
    ///
    /// 用于覆盖派遣的崩溃窗口——快照落盘后、入队前后、返回 pending 前后，
    /// 断言"任一步失败都回滚本次新增的会话与槽位，且不丢已持久化的结果"。
    fn test_fault(&self, point: &str) -> bool {
        #[cfg(test)]
        {
            return self
                .inner
                .faults
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains(point);
        }
        #[cfg(not(test))]
        {
            let _ = point;
            false
        }
    }

    /// 设置一个测试故障点（仅测试构建可用）。
    #[cfg(test)]
    fn set_test_fault(&self, point: &str) {
        self.inner
            .faults
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(point.to_string());
    }
    pub fn config(&self) -> RuntimeConfig {
        self.inner
            .settings
            .resolved("runtime")
            .ok()
            .and_then(|s| serde_json::from_value(s).ok())
            .unwrap_or_default()
    }
    /// 子代理调度策略：并发上限的唯一来源（`runtime.maxAgents` 已迁移并删除）。
    pub fn policy(&self) -> crate::subagents::SubagentPolicyConfig {
        self.inner
            .settings
            .resolved(crate::subagents::SETTINGS_NS)
            .ok()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default()
    }
    pub fn goals_config(&self) -> GoalsConfig {
        self.inner
            .settings
            .resolved("goals")
            .ok()
            .and_then(|s| serde_json::from_value(s).ok())
            .unwrap_or_default()
    }

    /// 执行预算上限(步数 / 请求尝试 / 主动执行时间)。
    ///
    /// 在 **turn 开始时**读取一次:驱动器把它当本轮的一致快照,中途改配置不会
    /// 改变在途轮次的限额(否则一次设置改动能把已经用掉的额度变小)。
    pub fn execution_limits(&self) -> denia_agent_loop::ExecutionLimits {
        execution_limits_from(&self.inner.settings)
    }
    pub fn human_turn(&self, id: &str, selection: &ModelSelection) {
        self.inner.paused.lock().unwrap().remove(id);
        self.inner
            .selections
            .lock()
            .unwrap()
            .insert(id.into(), selection.clone());
        self.inner.wakes.lock().unwrap().insert(id.into(), 0);
        // 用户的真实轮次 = 模型重新获得成功执行:goal 失败计数清零。
        self.inner.goal_failures.lock().unwrap().remove(id);
    }
    fn is_active(&self, id: &str) -> bool {
        self.inner
            .live
            .get(id)
            .is_some_and(|l| l.running.load(Ordering::SeqCst))
            || (!self.inner.paused.lock().unwrap().contains(id)
                && self
                    .inner
                    .inboxes
                    .lock()
                    .unwrap()
                    .get(id)
                    .is_some_and(|i| !i.pending.is_empty()))
    }
    fn descendants(&self, root: &str) -> Vec<Child> {
        let children = self.inner.children.lock().unwrap();
        let mut ids = HashSet::from([root.to_string()]);
        let mut result = Vec::new();
        loop {
            let next: Vec<_> = children
                .values()
                .filter(|c| ids.contains(&c.parent_id) && !ids.contains(&c.id))
                .cloned()
                .collect();
            if next.is_empty() {
                break;
            }
            for child in next {
                ids.insert(child.id.clone());
                result.push(child);
            }
        }
        result
    }
    pub fn list(&self, owner: &str) -> Value {
        json!(self.descendants(owner).into_iter().filter(|child| child.descriptor.mode != "memory").map(|child|{
        let live=self.inner.live.get(&child.id);
        let running=live.as_ref().is_some_and(|l|l.running.load(Ordering::SeqCst));
        let waiting=self.descendants(&child.id).iter().any(|c|self.inner.live.get(&c.id).is_some_and(|l|l.running.load(Ordering::SeqCst)));
        let descriptor = &child.descriptor;
        let legacy = descriptor.snapshot_version < denia_core::subagent::SUBAGENT_SNAPSHOT_VERSION;
        // 等待交互：未结算的审批/提问请求数（等待状态必须能被父代理看见，
        // 否则卡片只存在于子会话页面里）。同一遍扫描顺带给出最后终态摘要。
        let (pending_approvals, pending_asks, result) = live
            .as_ref()
            .map(|live| pending_interactions(&live.session))
            .unwrap_or((0, 0, None));
        let projection_applied = live
            .as_ref()
            .is_some_and(|live| live.session.history_projection().is_some());
        let waiting_interaction = pending_approvals > 0 || pending_asks > 0;
        json!({"id":child.id,"parentId":child.parent_id,"label":descriptor.label,"depth":descriptor.depth,"mode":descriptor.mode,"selection":descriptor.selection,
            "status":if running{"running"}else if waiting{"waiting"}else{"settled"},
            "profile":descriptor.profile,
            "name":descriptor.name,
            "description":descriptor.description,
            "toolCount":descriptor.effective_tools.as_ref().map(Vec::len).unwrap_or_else(|| denia_core::subagent::legacy_child_tools(descriptor.allowed_tools.as_deref()).len()),
            "tools":descriptor.effective_tools.clone().unwrap_or_else(|| denia_core::subagent::legacy_child_tools(descriptor.allowed_tools.as_deref())),
            "permissionCeiling":descriptor.permission_ceiling.as_str(),
            "createdPermissionMode":descriptor.created_permission_mode,
            "instructionScope":descriptor.instruction_scope,
            "delegationAllowed":false,
            "pendingApprovals":pending_approvals,
            "pendingAsks":pending_asks,
            "waitingInteraction":waiting_interaction,
            "result":result,
            "migration": if legacy { json!({
                // 旧 child 不再需要重新派遣：首次继续时自动建立模型历史投影
                // （见 HISTORY_PROJECTION_FILE）。这里只报告投影是否已建立。
                "needsRedispatch": false,
                "version": descriptor.snapshot_version,
                "projection": if projection_applied { "applied" } else { "pending" },
                "diagnostic": if projection_applied {
                    "旧子代理已建立模型历史投影：旧父运行态与全部自动注入通道不再进模型历史，审计日志保持不变。"
                } else {
                    "旧子代理尚未建立模型历史投影；首次继续（发消息/唤醒）时自动建立，之后在线与冷恢复一致。"
                }}) } else { Value::Null }})
    }).collect::<Vec<_>>())
    }
    fn authorize(&self, owner: &str, target: &str, ancestor: bool) -> Result<(), String> {
        if owner == target {
            return Err("不能向自身执行代理控制操作".into());
        }
        let children = self.inner.children.lock().unwrap();
        if children.get(target).is_some_and(|c| c.parent_id == owner)
            || (!ancestor && children.get(owner).is_some_and(|c| c.parent_id == target))
        {
            return Ok(());
        }
        drop(children);
        if ancestor && self.descendants(owner).iter().any(|c| c.id == target) {
            return Ok(());
        }
        Err("目标不在当前代理允许控制的谱系范围内".into())
    }
    async fn live(&self, id: &str) -> Result<Arc<crate::state::LiveSession>, String> {
        let inner = self.inner.clone();
        let id = id.to_string();
        let live = tokio::task::spawn_blocking({
            let inner = inner.clone();
            move || {
                inner
                    .live
                    .get_or_load(&inner.sessions, &id)
                    .map_err(|e| e.to_string())
            }
        })
        .await
        .map_err(|e| e.to_string())??;
        // 运行时参与路径(派生子代理/投递指令/收件箱同步)需要事件驻留。
        // 走 LiveSessions 的入口:热升级后立刻复核驻留预算。
        inner.live.ensure_hot(&live).map_err(|e| e.to_string())?;
        Ok(live)
    }
    fn sync_inbox(inbox: &mut Inbox, session: &denia_session::Session) {
        for event in session.events_after(inbox.seq) {
            inbox.seq = event.seq;
            match event.event {
                SessionEvent::AgentInbox { id, text, source } => {
                    inbox.seen.insert(id.clone());
                    inbox.pending.insert(event.seq, (id, text, source));
                }
                SessionEvent::AgentDelivery { id, .. } => {
                    inbox.seen.insert(id.clone());
                    inbox.pending.retain(|_, (key, _, _)| *key != id);
                }
                _ => {}
            }
        }
    }
    pub async fn enqueue(
        &self,
        target: &str,
        id: String,
        text: String,
        source: String,
    ) -> Result<String, String> {
        if text.len() > self.config().output_bytes * 2 {
            return Err("代理消息超过配置的体积上限".into());
        }
        if self.test_fault("completion-enqueue") && source == "subagent-settled" {
            return Err("注入故障：完成通知入队失败".into());
        }
        if source.starts_with("agent:") {
            self.inner.paused.lock().unwrap().remove(target);
            self.inner.wakes.lock().unwrap().insert(target.into(), 0);
        }
        let live = self.live(target).await?;
        let inner = self.inner.clone();
        let target = target.to_string();
        let result_id = id.clone();
        let wake_target = target.clone();
        let limit = self.config().max_pending_messages;
        tokio::task::spawn_blocking(move || {
            let mut inboxes = inner.inboxes.lock().unwrap();
            let inbox = inboxes.entry(target).or_default();
            Self::sync_inbox(inbox, &live.session);
            if inbox.seen.contains(&id) {
                return Ok(());
            }
            if inbox.pending.len() >= limit {
                return Err("目标代理待收消息已达上限".to_string());
            }
            let event = live
                .session
                .append(SessionEvent::AgentInbox { id, text, source })
                .map_err(|e| e.to_string())?;
            live.session.flush().map_err(|e| e.to_string())?;
            let _ = live.followers.send(event);
            Self::sync_inbox(inbox, &live.session);
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())??;
        self.wake(&wake_target);
        Ok(result_id)
    }
    fn wake(&self, id: &str) {
        self.defer_wake(id);
        if !self.inner.delivery.start_wake(id) {
            return;
        }
        let runtime = self.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            use futures::FutureExt;
            let before = runtime.inner.live.get(&id).map(|live| live.session.next_turn_number());
            let result = std::panic::AssertUnwindSafe(runtime.resume_pending(&id))
                .catch_unwind().await;
            match result {
                Ok(Err(error)) => tracing::error!(session=%id,%error,"恢复代理消息失败"),
                Err(_) => tracing::error!(session=%id,"代理运行任务异常退出"),
                _ => {}
            }
            runtime.inner.delivery.finish_wake(&id);
            let after = runtime.inner.live.get(&id).map(|live| live.session.next_turn_number());
            if after != before {
                runtime.inner.delivery.changed.notify_one();
            }
        });
    }

    /// 登记一次"现在跑不了、但必须跑"的唤醒。
    ///
    /// 消费点有两处,都不依赖消息到达那一刻的时序:
    /// ① [`Runtime::on_idle`] —— 任何会话（含子代理自己）每轮结束时扫全量
    ///    inbox 并逐个唤醒,这正是"主代理 wait_agent 轮次结束"的那个边沿;
    /// ② 准入槽位释放 —— 见 `Admission` 释放路径的连带扫描。
    fn defer_wake(&self, id: &str) {
        self.inner.deferred_wakes.lock().unwrap().insert(id.into());
        tracing::debug!(session = id, "唤醒已挂起,等待目标空闲后重试");
    }

    async fn resume_pending(&self, id: &str) -> Result<(), String> {
        if self.inner.paused.lock().unwrap().contains(id) {
            return Ok(());
        }
        let live = self.live(id).await?;
        // 旧 child（本计划之前的描述符，快照版本 0）没有可复现的指令投影：
        // 它的历史里是否已混入全局 AGENTS.md 的自动注入无法按正文可靠区分
        // （计划 9.4 禁止按正文猜）。首次继续前建立**模型历史投影**并落盘：
        // 明确按事件类别排除旧父运行态与全部自动注入通道，审计 UI 仍看完整
        // 日志；投影文件使在线加载与冷恢复得到同一结果。建立失败才退回
        // "拒绝续跑 + 重新派遣"。
        if let Some(child) = live.session.header().subagent.as_ref()
            && child.snapshot_version < denia_core::subagent::SUBAGENT_SNAPSHOT_VERSION
        {
            if live.session.history_projection().is_none() {
                match live.session.apply_history_projection() {
                    Ok(true) => {
                        tracing::info!(session = id, "已为旧子代理建立模型历史投影");
                    }
                    Ok(false) => {}
                    Err(error) => {
                        tracing::error!(%error, session = id, "旧子代理历史投影建立失败");
                        self.inner.paused.lock().unwrap().insert(id.into());
                        // 单独取父 id：锁守卫不能跨 await（if-let 临时值活到块尾）。
                        let parent = self
                            .inner
                            .children
                            .lock()
                            .unwrap()
                            .get(id)
                            .map(|child| child.parent_id.clone());
                        if let Some(parent) = parent {
                            let text = format!(
                                "[子代理 {id} 无法直接继续]\n为旧子代理建立模型历史投影失败：{error}。\
                                 为不把可能含有全局规则注入的旧上下文继续送模型，已拒绝续跑。\n\
                                 处理方式：查看它的日志确认结论，然后用 spawn_agent 以自包含 prompt 重新派遣。（{}）",
                                denia_core::subagent::codes::LEGACY_RESUME_UNSUPPORTED
                            );
                            if let Err(error) = self
                                .enqueue(
                                    &parent,
                                    format!("legacy-resume:{id}"),
                                    text,
                                    "subagent-legacy".into(),
                                )
                                .await
                            {
                                tracing::error!(%error, "旧子代理迁移诊断投递失败");
                            }
                        }
                        return Ok(());
                    }
                }
            }
        }
        let (pending, completion, last_pending_seq) = {
            let mut inboxes = self.inner.inboxes.lock().unwrap();
            let inbox = inboxes.entry(id.into()).or_default();
            Self::sync_inbox(inbox, &live.session);
            (
                !inbox.pending.is_empty(),
                inbox.pending.values().any(|(_, _, source)| source == "subagent-settled"),
                inbox.pending.keys().next_back().copied(),
            )
        };
        if !pending {
            self.inner.deferred_wakes.lock().unwrap().remove(id);
            self.inner.completion_marks.lock().unwrap().remove(id);
            return Ok(());
        }
        if self.inner.delivery.is_cleaning(id) {
            return Ok(());
        }
        let selection = self
            .inner
            .selections
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .or_else(|| {
                live.session
                    .header()
                    .subagent
                    .as_ref()
                    .map(|s| s.selection.clone())
            })
            .or_else(|| {
                live.session.events().iter().rev().find_map(|e| {
                    if let SessionEvent::RequestHeader {
                        header: snapshot, ..
                    } = &e.event
                    {
                        Some(ModelSelection {
                            provider: snapshot.config.provider.clone(),
                            model: snapshot.config.model.clone(),
                            reasoning_effort: snapshot.config.reasoning_effort.clone(),
                        })
                    } else {
                        None
                    }
                })
            });
        let admission = self.inner.admission.enter().await;
        if self.inner.paused.lock().unwrap().contains(id) {
            return Ok(());
        }
        let Some(selection) = selection else {
            return Ok(());
        };
        // 这一批待收消息是否还没被“真正开跑”处理过：是则完成通知可以越过
        // 普通唤醒上限；否则按普通上限计，避免失败重试无界自旋。
        let completion_fresh = completion && {
            let marks = self.inner.completion_marks.lock().unwrap();
            match last_pending_seq {
                Some(seq) => marks.get(id).copied() != Some(seq),
                None => false,
            }
        };
        {
            let mut wakes = self.inner.wakes.lock().unwrap();
            let n = wakes.entry(id.into()).or_default();
            if !completion_fresh && *n >= self.config().max_consecutive_wakes {
                return Ok(());
            }
            if live
                .running
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                // 目标正在跑（典型：主代理卡在 wait_agent 等子代理结果）。这次
                // 唤醒没有别的边沿会再来,必须登记挂起,由它自己那轮结束时消费,
                // 否则消息永久躺在 inbox 里直到用户手动发消息。
                drop(wakes);
                self.defer_wake(id);
                return Ok(());
            }
            if live.session.header().subagent.is_some() {
                // 终态 child 的消息唤醒同样走统一准入（与创建共用一份槽位账）。
                let limit = self.policy().max_concurrent_runs;
                if !self.inner.admission.reserve(id, limit) {
                    live.running.store(false, Ordering::SeqCst);
                    // 槽位已满同样是"现在跑不了"而非"不需要跑"：别的子代理结束
                    // 时会走 on_idle 的全量扫描重新捞起这里的挂起项,漏登记就是
                    // 子代理补一条指令后永远收不到回复。
                    drop(wakes);
                    self.defer_wake(id);
                    return Ok(());
                }
            }
            if !completion_fresh {
                *n += 1;
            } else if let Some(seq) = last_pending_seq {
                self.inner.completion_marks.lock().unwrap().insert(id.into(), seq);
            }
        }
        self.inner.deferred_wakes.lock().unwrap().remove(id);
        let runtime = self.clone();
        let guard = RunningGuard::with_on_idle(live.clone(), self.inner.events.clone(), Arc::new(move |id| runtime.on_idle(id)));
        let token = CancellationToken::new();
        *live.cancel.lock().unwrap() = Some(token.clone());
        drop(admission);
        let _ = self.inner.events.send(ServerEvent::RunningChanged {
            id: id.into(),
            running: true,
        });
        let driver = self
            .inner
            .driver
            .get()
            .and_then(Weak::upgrade)
            .ok_or("会话驱动器尚未就绪")?;
        let followers = live.followers.clone();
        let runtime = self.clone();
        let session_id = id.to_string();
        let emit: Arc<dyn Fn(&SessionEnvelope) + Send + Sync> = Arc::new(move |e| {
            if matches!(e.event, SessionEvent::TurnEnd { .. }) {
                runtime.capture_completion(&session_id);
            }
            let _ = followers.send(e.clone());
        });
        driver
            .run_turn(
                &live.session,
                &selection,
                "",
                Vec::new(),
                Vec::new(),
                Vec::new(),
                self.inner
                    .registry
                    .resolve_call(
                        &selection.provider,
                        &selection.model,
                        selection.reasoning_effort.as_deref(),
                    )
                    .await
                    .map(|r| r.info.input_modalities.iter().any(|m| m == "image"))
                    .unwrap_or(false),
                token,
                emit,
            )
            .await;
        drop(guard);
        Ok(())
    }
    /// 写一个 goal 操作事件(阻塞 fs → spawn_blocking)并推给 follow 订阅者。
    async fn publish_goal(
        live: &Arc<crate::state::LiveSession>,
        op: GoalOp,
    ) -> Result<SessionEnvelope, String> {
        let owned = live.clone();
        let envelope = tokio::task::spawn_blocking(move || -> Result<SessionEnvelope, String> {
            let envelope = owned.session.apply_goal(op).map_err(|e| e.to_string())?;
            owned.session.flush().map_err(|e| e.to_string())?;
            Ok(envelope)
        })
        .await
        .map_err(|e| e.to_string())??;
        let _ = live.followers.send(envelope.clone());
        Ok(envelope)
    }

    fn turn_failure_summary(reason: &denia_core::session::TurnEndReason) -> String {
        match reason {
            denia_core::session::TurnEndReason::Error { failure } => {
                format!("{}({})", failure.message, failure.code)
            }
            denia_core::session::TurnEndReason::LoopDetected { repeats } => {
                format!("死循环保护触发(连续重复 {repeats} 次)")
            }
            _ => "执行失败".into(),
        }
    }

    /// 最近一次 goal 状态变更之后落下的最后一条 turn 结局。
    ///
    /// 水位 = 日志里最后一条 `SessionEvent::Goal` 的 `seq`(每条 goal 操作
    /// 都落事件:set/edit/pause/resume/round…)。只有水位之后的 `TurnEnd`
    /// 才算"上一轮的结果":水位之前的结局已经被该操作消化掉了。
    ///
    /// 没有水位会读错结局——resume 之后还没有新 `TurnEnd` 时,日志里最后一条
    /// `TurnEnd` 仍是暂停前那条 `Aborted`,于是刚恢复就又落一次 Pause,恢复
    /// 永远无效(判据不变,必然复现)。冷日志(无 goal 事件)水位取 0,与
    /// 旧行为一致。
    fn outcome_since_goal_change(events: &[SessionEnvelope]) -> Option<TurnEndReason> {
        let watermark = events
            .iter()
            .rev()
            .find_map(|e| matches!(e.event, SessionEvent::Goal { .. }).then_some(e.seq))
            .unwrap_or(0);
        events
            .iter()
            .rev()
            .take_while(|e| e.seq > watermark)
            .find_map(|e| match &e.event {
                SessionEvent::TurnEnd { reason, .. } => Some(reason.clone()),
                _ => None,
            })
    }

    /// goal 续跑入口:turn 结束(idle)或状态恢复后调用。目标 active 且
    /// 预算/轮次/失败护栏都放行时,自动发起新的 goal 轮(对照 codex
    /// `continue_active_goal_for_idle_thread`);与 inbox 唤醒通过 running
    /// CAS 自然互斥,后到者放弃,等下一轮 on_idle 收敛。
    pub fn continue_goal(&self, id: &str) {
        let runtime = self.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            if let Err(error) = runtime.continue_goal_inner(&id).await {
                tracing::warn!(session = %id, %error, "goal 续跑检查失败");
            }
        });
    }

    async fn continue_goal_inner(&self, id: &str) -> Result<(), String> {
        let live = self.live(id).await?;
        if live.session.header().subagent.is_some() {
            return Ok(());
        }
        // 组装关闭 goal 功能时续跑一并关闭:装配层已摘 goal 工具与纪律段,
        // 注入管线已关状态通道,这里关掉续跑状态机,三个层面读同一份声明。
        if let Some(driver) = self.inner.driver.get().and_then(|weak| weak.upgrade())
            && !driver.features_for(&live.session).goal
        {
            return Ok(());
        }
        let Some(goal) = live.session.goal() else {
            return Ok(());
        };
        if goal.status != denia_core::session::GoalStatus::Active {
            return Ok(());
        }
        let config = self.goals_config();

        // 上一轮结局分类(最近一次 goal 状态变更之后没有 turn 视为正常,
        // 直接开跑——resume 后即属此列)。
        let last_reason = live.session.with_events(Self::outcome_since_goal_change);
        match &last_reason {
            // 用户手动停止:目标自动暂停,等用户恢复——避免"停不下来"。
            Some(denia_core::session::TurnEndReason::Aborted { .. }) => {
                Self::publish_goal(&live, GoalOp::Pause).await?;
                return Ok(());
            }
            // 执行预算耗尽不是“执行失败”:它是护栏生效。用 goal 预算语义
            // 停下自动续跑(不累计连续失败、不标 blocked)——继续跑只会把
            // 同一个失效模式再烧一轮。
            Some(denia_core::session::TurnEndReason::Error { failure })
                if failure.code == denia_core::error::codes::BUDGET_EXHAUSTED =>
            {
                Self::publish_goal(&live, GoalOp::BudgetLimit).await?;
                return Ok(());
            }
            // 执行失败:计数 +1;达到上限标记 blocked,未达限等用户处理
            // (自动重试大概率再失败,烧预算)。
            Some(
                reason @ (denia_core::session::TurnEndReason::Error { .. }
                | denia_core::session::TurnEndReason::LoopDetected { .. }),
            ) => {
                let failures = {
                    let mut map = self.inner.goal_failures.lock().unwrap();
                    let count = map.entry(id.to_string()).or_default();
                    *count += 1;
                    *count
                };
                if failures >= config.max_consecutive_failures {
                    self.inner.goal_failures.lock().unwrap().remove(id);
                    let summary = Self::turn_failure_summary(reason);
                    Self::publish_goal(
                        &live,
                        GoalOp::Block {
                            reason: format!("连续 {failures} 轮执行失败,最近一次:{summary}"),
                        },
                    )
                    .await?;
                }
                return Ok(());
            }
            Some(_) => {
                self.inner.goal_failures.lock().unwrap().remove(id);
            }
            None => {}
        }

        // 预算检查:耗尽 → budget_limited + 最后一轮收尾(收尾轮结束后
        // 状态非 active,自然停止;用户提高预算会回到 active)。
        let used = live.session.goal_tokens_used().unwrap_or(0);
        let wrapup = goal.token_budget.is_some_and(|budget| used >= budget);
        if wrapup {
            Self::publish_goal(&live, GoalOp::BudgetLimit).await?;
        } else if goal.rounds_started >= config.max_rounds {
            // 轮次耗尽:状态保持 active,不再续跑(dsh 语义:resume 被拒)。
            return Ok(());
        }

        // selection 回推(仿 resume_pending:内存优先,日志 RequestHeader 兜底)。
        let selection = self
            .inner
            .selections
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .or_else(|| {
                live.session.events().iter().rev().find_map(|e| {
                    if let SessionEvent::RequestHeader {
                        header: snapshot, ..
                    } = &e.event
                    {
                        Some(ModelSelection {
                            provider: snapshot.config.provider.clone(),
                            model: snapshot.config.model.clone(),
                            reasoning_effort: snapshot.config.reasoning_effort.clone(),
                        })
                    } else {
                        None
                    }
                })
            });
        let Some(selection) = selection else {
            return Ok(());
        };
        if live
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Ok(());
        }
        let runtime = self.clone();
        let guard = RunningGuard::with_on_idle(live.clone(), self.inner.events.clone(), Arc::new(move |id| runtime.on_idle(id)));
        let token = CancellationToken::new();
        *live.cancel.lock().unwrap() = Some(token.clone());
        let _ = self.inner.events.send(ServerEvent::RunningChanged {
            id: id.into(),
            running: true,
        });

        // Round 事件:轮次推进落盘。续跑提示不再单独落 goal-round 消息——
        // 目标状态块(channel "goal")携带轮次与用量,每轮用量变化使注入
        // 内容自动更新一次,避免同一轮出现两条重复的"[denia 目标]"消息。
        let round = goal.rounds_started + 1;
        let _round_envelope = Self::publish_goal(&live, GoalOp::Round).await?;
        tracing::debug!(session = id, round = round, "goal 轮次推进");

        let vision_supported = self
            .inner
            .registry
            .resolve_call(
                &selection.provider,
                &selection.model,
                selection.reasoning_effort.as_deref(),
            )
            .await
            .map(|r| r.info.input_modalities.iter().any(|m| m == "image"))
            .unwrap_or(false);
        let driver = self
            .inner
            .driver
            .get()
            .and_then(Weak::upgrade)
            .ok_or("会话驱动器尚未就绪")?;
        let followers = live.followers.clone();
        let runtime = self.clone();
        let session_id = id.to_string();
        let emit: Arc<dyn Fn(&SessionEnvelope) + Send + Sync> = Arc::new(move |e| {
            if matches!(e.event, SessionEvent::TurnEnd { .. }) {
                runtime.capture_completion(&session_id);
            }
            let _ = followers.send(e.clone());
        });
        driver
            .run_turn(
                &live.session,
                &selection,
                "",
                Vec::new(),
                Vec::new(),
                Vec::new(),
                vision_supported,
                token,
                emit,
            )
            .await;
        drop(guard);
        Ok(())
    }

    pub fn on_idle(&self, id: &str) {
        self.capture_completion(id);
        self.inner.admission.release(id);
        let aborted = self.inner.live.get(id).is_some_and(|l| {
            l.session
                .with_events(Self::outcome_since_goal_change)
                .is_some_and(|reason| matches!(reason, TurnEndReason::Aborted { .. }))
        });
        if aborted {
            self.inner.paused.lock().unwrap().insert(id.into());
        } else {
            // 这个会话刚走完一轮:自己的挂起唤醒先撤下再重推,语义是"我空闲了,
            // 现在能不能跑由 resume_pending 判定";判定不过(忙/暂停/额度)会再登记回去。
            self.inner.deferred_wakes.lock().unwrap().remove(id);
            self.wake(id);
        }
        self.inner.delivery.expedite(id);
        let queued: Vec<_> = self.inner.inboxes.lock().unwrap().iter()
            .filter(|(_, inbox)| !inbox.pending.is_empty())
            .map(|(id, _)| id.clone()).collect();
        for queued_id in queued {
            if queued_id != id {
                self.wake(&queued_id);
            }
        }
        self.continue_goal(id);
    }
    pub async fn interrupt(&self, owner: &str, target: &str) -> Result<(), String> {
        let _admission = self.inner.admission.enter().await;
        self.authorize(owner, target, true)?;
        self.inner.paused.lock().unwrap().insert(target.into());
        if self
            .inner
            .live
            .get(target)
            .is_none_or(|l| !l.running.load(Ordering::SeqCst))
        {
            self.inner.admission.release(target);
        }
        if let Some(live) = self.inner.live.get(target)
            && let Some(token) = live.cancel.lock().unwrap().as_ref()
        {
            token.cancel();
        }
        // 停止 child 时一并终止它自己的后台进程树：只释放槽位而让进程继续写
        // 文件是最坏的结果（计划 10.4）。取消是幂等的，正常结束前重复调用无害。
        self.inner.jobs.cancel_owner(target).await;
        // 浏览器资源按 child 归属清理：只关它自己的 tab，不碰父代理与其他
        // 子代理的（计划 6.4 / L04）。
        self.close_owned_tabs(target).await;
        Ok(())
    }
    pub fn ensure_removable(&self, owner: &str) -> Result<(), String> {
        if self
            .descendants(owner)
            .iter()
            .any(|c| self.is_active(&c.id))
        {
            return Err("仍有运行中或待处理的子代理，请先停止后再删除会话".into());
        }
        Ok(())
    }
    pub async fn remove_owner(&self, owner: &str) -> Result<(), String> {
        self.ensure_removable(owner)?;
        let children = self.descendants(owner);
        if children.iter().any(|c| {
            self.inner
                .live
                .get(&c.id)
                .is_some_and(|l| l.running.load(Ordering::SeqCst))
        }) {
            return Err("仍有运行中的子代理，请先停止后再删除会话".into());
        }
        for child in children.iter().rev() {
            self.inner.jobs.cancel_owner(&child.id).await;
            self.close_owned_tabs(&child.id).await;
            let sessions = self.inner.sessions.clone();
            let id = child.id.clone();
            tokio::task::spawn_blocking(move || sessions.delete(&id))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
            self.inner.live.remove(&child.id);
            self.inner.workspaces.detach_session(&child.id);
            self.inner.children.lock().unwrap().remove(&child.id);
        }
        self.inner.jobs.cancel_owner(owner).await;
        self.inner.children.lock().unwrap().remove(owner);
        // 会话没了,它名下的挂起唤醒再被消费也只是对着空会话空转。
        self.inner.deferred_wakes.lock().unwrap().remove(owner);
        Ok(())
    }
    async fn rollback_child(&self, id: &str) {
        self.inner.admission.release(id);
        self.inner.children.lock().unwrap().remove(id);
        self.inner.selections.lock().unwrap().remove(id);
        self.inner.inboxes.lock().unwrap().remove(id);
        self.inner.delivery.forget(id);
        self.inner.completion_marks.lock().unwrap().remove(id);
        self.inner.live.remove(id);
        self.inner.workspaces.detach_session(id);
        let sessions = self.inner.sessions.clone();
        let id = id.to_string();
        if let Err(e) = tokio::task::spawn_blocking(move || sessions.delete(&id)).await {
            tracing::error!(error=%e,"回滚子代理失败");
        }
    }
    /// 派遣：解析定义/临时规格 → 校验身份与授权 → 冻结快照 → 持久入队。
    ///
    /// 硬规则（第 6.2 节）在这里做**第一层**服务端判定：任何宿主派遣入口都必须
    /// 在任何分配副作用之前确认调用者不是子代理。判定依据是真实会话头里的
    /// `subagent` 身份，不看 label、depth、调用参数，也不依赖内存 children 表。
    async fn delegate(
        &self,
        fork: bool,
        args: denia_tools::runtime_command::DelegateArgs,
        ctx: &ToolContext,
    ) -> Result<Value, String> {
        use denia_core::session::SubagentProfileRef;
        use denia_core::subagent::codes as subagent_codes;

        let owner = ctx.session_id.as_deref().ok_or("缺少会话身份")?;
        let config = self.config();
        let parent = self.live(owner).await?;

        // ① 子代理禁止派遣子代理：硬拒，且不产生任何副作用。
        if parent.session.header().subagent.is_some() {
            return Err(format!(
                "子代理不能派遣子代理（{}）：请把可并行的子任务交回父代理，或在本次任务内自己完成。",
                subagent_codes::DELEGATION_FORBIDDEN
            ));
        }
        // ② 参数契约（max_depth、profile/inline 互斥、旧字段歧义、模型成套）。
        crate::subagents::resolver::validate_delegate_args(&args)
            .map_err(|error| error.render())?;

        // ③ 父会话授权上下文：注册表 + preset/features/权限档 → parentGrant。
        let driver = self
            .inner
            .driver
            .get()
            .and_then(Weak::upgrade)
            .ok_or("会话驱动器尚未就绪，暂时无法派遣子代理")?;
        let registered = driver.tools().names();
        let parent_preset_id = parent.session.agent_preset();
        let preset = driver.preset_for(parent_preset_id.as_deref());
        let features = driver.preset_features(parent_preset_id.as_deref());
        let preset_tools = preset.as_ref().and_then(|preset| preset.tools.as_deref());
        let parent_mode = parent.session.permission_mode();
        let grant = crate::subagents::resolver::parent_grant(
            &registered,
            &features,
            preset_tools,
            parent_mode,
        );

        // ④ 解析规格：profile_id / inline / 默认 develop / 旧参数兼容路径。
        let cwd = ctx.cwd.clone();
        let project_root = cwd
            .is_dir()
            .then(|| crate::subagents::project_root_for(&cwd));
        let parent_selection = ctx.selection.clone().ok_or("子代理无法继承模型选择")?;
        let legacy_path = args.profile_id.is_none()
            && args.inline.is_none()
            && (args.persona.is_some() || args.allowed_tools.is_some());
        let dispatch = if legacy_path {
            crate::subagents::resolver::legacy_dispatch(&args, fork, &parent_selection, &registered)
                .map_err(|error| error.render())?
        } else {
            let resolved = match (&args.profile_id, &args.inline) {
                (Some(raw), _) => Some(
                    self.inner
                        .profiles
                        .resolve(project_root.as_deref(), raw)
                        .map_err(|error| error.render())?,
                ),
                (None, Some(_)) => None,
                (None, None) => Some(
                    self.inner
                        .profiles
                        .resolve_default(project_root.as_deref())
                        .map_err(|error| error.render())?,
                ),
            };
            crate::subagents::resolver::resolve_dispatch(
                &args,
                fork,
                &parent_selection,
                &grant,
                &registered,
                resolved.as_ref(),
                args.profile_id.is_none() && args.inline.is_none(),
            )
            .map_err(|error| error.render())?
        };
        self.inner
            .registry
            .resolve_call(
                &dispatch.selection.provider,
                &dispatch.selection.model,
                dispatch.selection.reasoning_effort.as_deref(),
            )
            .await
            .map_err(|e| e.to_string())?;
        let prompt = args.prompt.clone();
        if prompt.len() > config.output_bytes {
            return Err("子代理提示词过大".into());
        }
        if dispatch.instructions.len() > denia_core::subagent::INSTRUCTIONS_MAX_BYTES {
            return Err("子代理角色提示词过大".into());
        }
        let mut deprecations: Vec<String> = Vec::new();
        if args.run_in_background.is_some() {
            deprecations
                .push("run_in_background 已废弃：派遣始终后台执行，该参数被忽略".to_string());
        }
        if legacy_path {
            deprecations.push(
                "persona/allowed_tools 是旧参数（已按保守历史上限解释）；请改用 profile_id 或 inline"
                    .to_string(),
            );
        }

        let label = args
            .description
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or(&dispatch.name)
            .chars()
            .take(200)
            .collect::<String>();
        let descriptor = SubagentDescriptor {
            label,
            // 新 child 恒为 1；谱系展示与旧数据兼容用，不作为派遣准入依据。
            depth: 1,
            mode: if fork { "fork" } else { "spawn" }.into(),
            selection: dispatch.selection.clone(),
            // 旧字段不再承载新 child 的授权语义。
            persona: None,
            allowed_tools: None,
            snapshot_version: denia_core::subagent::SUBAGENT_SNAPSHOT_VERSION,
            name: Some(dispatch.name.clone()),
            description: Some(dispatch.description.clone()),
            profile: Some(SubagentProfileRef {
                qualified_id: dispatch.qualified_id.clone(),
                source: dispatch.source,
                revision: dispatch.revision,
                inline: dispatch.inline,
            }),
            effective_tools: Some(dispatch.tools.clone()),
            permission_ceiling: dispatch.permission_ceiling,
            created_permission_mode: Some(parent_mode.as_str().to_string()),
            // 审计字段：恒 false。真正的硬拒在服务端身份判定与 exec 层。
            delegation_allowed: false,
            instruction_scope: Some("project-only".into()),
            parent_preset: parent_preset_id.clone(),
            parent_preset_persona: preset
                .as_ref()
                .and_then(|preset| preset.persona.clone())
                .filter(|text| !text.trim().is_empty()),
            parent_features: Some(features),
            instructions_ref: Some(SUBAGENT_SNAPSHOT_FILE.to_string()),
            instructions_hash: Some(instructions_hash(&dispatch.instructions)),
            fork: None,
            legacy: None,
        };
        let descriptor = SubagentDescriptor {
            fork: fork.then(|| denia_core::session::SubagentForkProjection {
                source_session: owner.to_string(),
                cut_seq: 0,
                projection_version: denia_session::SUBAGENT_SEED_VERSION,
                dropped: Vec::new(),
            }),
            ..descriptor
        };

        // ⑤ 准入：与终态 child 恢复、消息唤醒共用同一份槽位账。
        let admission = self.inner.admission.enter().await;
        let limit = self.policy().max_concurrent_runs;
        if self.inner.admission.reserved_count() >= limit {
            return Err(format!(
                "子代理并发已达上限（{limit}）；请等已有子代理结束，或在设置里提高 subagent-policy.maxConcurrentRuns。"
            ));
        }
        if ctx.cancel.is_cancelled() {
            return Err("子代理启动已取消".into());
        }

        // ⑥ fork 投影：只取已闭合轮次，丢弃父注入/摘要/收件箱/运行态。
        let (source, fork_projection) = if fork {
            let seed = denia_session::build_subagent_seed(parent.session.events().as_slice());
            (
                seed.events,
                Some(denia_core::session::SubagentForkProjection {
                    source_session: owner.to_string(),
                    cut_seq: seed.cut_seq,
                    projection_version: denia_session::SUBAGENT_SEED_VERSION,
                    dropped: seed.dropped,
                }),
            )
        } else {
            (Vec::new(), None)
        };
        let descriptor = SubagentDescriptor {
            fork: fork_projection,
            ..descriptor
        };

        let sessions = self.inner.sessions.clone();
        let child_cwd = cwd.clone();
        let sandbox = parent.session.header().sandbox;
        let parent_id = owner.to_string();
        let desc = descriptor.clone();
        let instructions = dispatch.instructions.clone();
        let child_permission = crate::subagents::resolver::effective_permission_mode(
            dispatch.permission_ceiling,
            parent_mode,
        );
        let child_preset = parent_preset_id.clone();
        // —— 崩溃窗口（测试注入）：快照落盘后 / 入队前后 / 返回 pending 前后 ——
        // 任何一个窗口失败都必须回滚本次新增的会话、槽位与目录条目。
        if self.test_fault("create") {
            return Err("注入故障：创建子会话前失败".to_string());
        }
        let id = tokio::task::spawn_blocking(move || {
            let child = sessions
                .create_subagent(&child_cwd, sandbox, &parent_id, desc)
                .map_err(|e| e.to_string())?;
            let result: Result<String, String> = (|| {
                child.seed_from(&source).map_err(|e| e.to_string())?;
                write_subagent_snapshot(&sessions, child.id(), &instructions)?;
                child
                    .set_permission_mode(child_permission)
                    .map_err(|e| e.to_string())?;
                if let Some(preset) = &child_preset {
                    child.set_agent_preset(preset).map_err(|e| e.to_string())?;
                }
                child.flush().map_err(|e| e.to_string())?;
                Ok(child.id().to_string())
            })();
            if result.is_err() {
                let _ = sessions.delete(child.id());
            }
            result
        })
        .await
        .map_err(|e| e.to_string())??;
        if self.test_fault("snapshot") {
            // 会话与快照都已落盘，随后这一步失败：走真实回滚路径。
            self.rollback_child(&id).await;
            return Err("注入故障：快照落盘后失败".to_string());
        }
        if !self.inner.admission.reserve(&id, limit) {
            self.rollback_child(&id).await;
            return Err(format!("子代理并发已达上限（{limit}）"));
        }
        for workspace in self.inner.workspaces.list() {
            if workspace.session_ids.iter().any(|s| s == owner)
                && !self.inner.workspaces.attach(&workspace.id, &id)
            {
                self.rollback_child(&id).await;
                return Err("父工作区已删除，子代理创建已回滚".into());
            }
        }
        self.inner.children.lock().unwrap().insert(
            id.clone(),
            Child {
                id: id.clone(),
                parent_id: owner.into(),
                descriptor: descriptor.clone(),
            },
        );
        self.inner
            .selections
            .lock()
            .unwrap()
            .insert(id.clone(), dispatch.selection.clone());
        if ctx.cancel.is_cancelled() {
            self.rollback_child(&id).await;
            return Err("子代理启动已取消".into());
        }
        if self.test_fault("pre-enqueue") {
            self.rollback_child(&id).await;
            return Err("注入故障：入队前失败".to_string());
        }
        let enqueued = if self.test_fault("enqueue") {
            Err("注入故障：入队失败".to_string())
        } else {
            self.enqueue(
                &id,
                // 通知身份按 childId + generation 生成，续跑不复用上一次结果。
                uuid::Uuid::new_v4().to_string(),
                format!("[父代理 {owner} 委派任务]\n{prompt}"),
                format!("agent:{owner}"),
            )
            .await
        };
        let message_id = match enqueued {
            Ok(id) => id,
            Err(error) => {
                self.rollback_child(&id).await;
                return Err(error);
            }
        };
        if self.test_fault("post-enqueue") {
            // 派发已持久化：这里失败**不**回滚——child 的会话、首条 inbox 与
            // 完成通知都是持久事实，工具结果丢了也不能删掉正在运行的 child。
            return Err("注入故障：返回 pending 前失败".to_string());
        }
        let _ = self.inner.events.send(ServerEvent::SessionsUpdated);
        drop(admission);
        let mut result = background_result(json!({"childId":id,"messageId":message_id}), true);
        // 只回摘要，不回整份提示快照（计划 8.3）。
        result["profile"] = json!({
            "qualifiedId": dispatch.qualified_id,
            "name": dispatch.name,
            "inline": dispatch.inline,
            "tools": dispatch.tools,
            "toolCount": dispatch.tools.len(),
            "model": dispatch.selection,
            "permissionCeiling": dispatch.permission_ceiling.as_str(),
            "fingerprint": dispatch.fingerprint(),
        });
        if !deprecations.is_empty() {
            result["deprecations"] = json!(deprecations);
        }
        Ok(result)
    }
    pub async fn skills(&self, cwd: PathBuf) -> Result<Vec<crate::skills::Skill>, String> {
        let home = self.inner.home.clone();
        tokio::task::spawn_blocking(move || crate::skills::discover(&home, &cwd))
            .await
            .map_err(|e| e.to_string())?
    }
    pub async fn load_skill(
        &self,
        cwd: PathBuf,
        name: String,
        user: bool,
    ) -> Result<Value, String> {
        let skills = self.skills(cwd).await?;
        tokio::task::spawn_blocking(move || crate::skills::load(&skills, &name, user))
            .await
            .map_err(|e| e.to_string())?
    }
}

fn string(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("缺少非空参数：{key}"))
}
#[cfg(test)]
fn validate_wait_timeout(args: &Value) -> Result<(), String> {
    let mut args = args.clone();
    args["id"] = json!("test");
    denia_tools::runtime_command::RuntimeCommand::parse("job_output", args).map(|_| ())
}

fn background_result(mut result: Value, pending: bool) -> Value {
    result["status"] = json!(if pending { "pending" } else { "ready" });
    result["pending"] = json!(pending);
    if pending {
        result["notification"] = json!("automatic");
        result["next_action"] = json!(
            "后台仍在执行，完成结果会自动送达本会话。不要轮询或重复等待；可以继续独立工作，或先向用户回复当前进度并结束本轮。pending 不代表执行成功。"
        );
    }
    result
}

/// 子代理终态的三分类(宿主读子会话终态事件 + 任务账本算出)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalOutcome {
    /// 本轮正常结束。**注意**:它不等于验收通过 —— 宿主证据是否支持见
    /// [`TerminalState::host_verified`]。
    Completed,
    /// 失败:本轮以错误 / 死循环 / 输出预算耗尽结束,或账本上的结论是验收失败。
    Failed,
    /// 没有正常终态(被取消 / 中断 / 无终态事件)或账本标记外部受阻:需要父代理处置。
    NeedsDecision,
}

impl TerminalOutcome {
    /// 同一套字面量:JSON 枚举名与通知文本共用一个来源,不各写一份。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::NeedsDecision => "needs_decision",
        }
    }
}

/// 终态的依据:把"子代理自己说完成"与"宿主证据说了算"分开。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalBasis {
    /// 宿主证据支持:账本上绑当前 revision 的验收结论是通过。
    HostVerified,
    /// 只有子代理自己的轮次终态(它最后那句话);宿主拿不到可核验的结论。
    SelfReported,
    /// 宿主证据:账本标记外部受阻。
    HostBlocked,
    /// 本轮被取消 / 中断(含崩溃孤儿轮次的合成闭合)。
    Interrupted,
    /// 子会话里没有终态事件。
    NoTerminalEvent,
}

impl TerminalBasis {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HostVerified => "host_verified",
            Self::SelfReported => "self_reported",
            Self::HostBlocked => "host_blocked",
            Self::Interrupted => "interrupted",
            Self::NoTerminalEvent => "no_terminal_event",
        }
    }
}

/// 子代理的终态:枚举 + 依据 + 原因 + 一句人读交代。
///
/// `outcome == completed` 与 `host_verified == true` 是两件事:前者可能是子代理
/// 自称的("这轮没发工具调用"),后者要求宿主账本上绑当前 revision 的验收结论
/// 是通过。两者的差别必须能一眼看出来,否则"自称完成"就会被当成验收通过。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalState {
    pub outcome: TerminalOutcome,
    pub basis: TerminalBasis,
    /// 子会话轮次终态的中文标签(如"已完成");`None` = 没有终态事件。
    pub reason: Option<String>,
    /// 宿主证据是否支持"完成"。只有账本上"结论通过"且"任务已收口且验收项被
    /// 全部覆盖"同时成立才为 `true`。
    pub host_verified: bool,
    /// `false` = 子代理仍在运行,以下终态取自当前日志快照,不是最终结果。
    pub finalized: bool,
    /// 一句人读交代。
    pub detail: String,
}

/// 结构化的子代理用量(provider 精确 usage 累计),不是拼进文本的字符串。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffUsage {
    pub uncached_input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    /// 计费口径的全量(五分量之和)。
    pub total_tokens: u64,
}

/// 一条产物引用(`ToolResult.meta.outputArtifact`)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactRef {
    /// 产物 id(`outputArtifact.output_id`);`read_tool_output` 按它读取。
    pub output_id: String,
    /// 产生它的工具调用 id 与工具名(`-` = 日志里找不到对应的调用)。
    pub call_id: String,
    pub tool: String,
    /// 输出是否完整保留;`None` = 工具没报这个事实。
    pub complete: Option<bool>,
}

/// 子代理交接信息。
///
/// **单一计算来源**:完成通知(注入父会话的那条 `AgentDelivery` 文本)与
/// `wait_agent` 的返回对象都从这一份结构渲染(见 [`subagent_handoff`])。两处各
/// 算一份必然会漂移 —— 父代理在通知里读到的字段与查询到的字段必须逐项相等。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentHandoff {
    /// 子会话 id。
    pub id: String,
    /// 展示名(定义名优先,回退标签)。
    pub name: String,
    /// profile qualifiedId;`-` = 旧描述符没有 profile。
    pub profile: String,
    pub terminal: TerminalState,
    pub usage: HandoffUsage,
    /// 子代理改动过的文件;口径见 [`SubagentHandoff::changed_paths_note`]。
    pub changed_paths: Vec<String>,
    /// `changedPaths` 的口径,以及"为什么可能是空的"。
    pub changed_paths_note: String,
    /// 子会话产生的产物引用。
    pub artifacts: Vec<ArtifactRef>,
    /// 任务账本的验证状态;`None` = 拿不到可用结论(没有账本,或账本上没跑过
    /// / 跑过但结论已失效)。**复用** `core::task::VerificationState`,不另造一套。
    pub verification: Option<VerificationState>,
    /// 验证面的人读交代(拿不到结论时说清原因)。
    pub verification_note: String,
    /// 父代理必须看到的提醒(未验证 / 验收失败);为空 = 确有可核验的通过结论。
    pub advisories: Vec<String>,
}

impl SubagentHandoff {
    /// 交接字段表(对象):`wait_agent` 的返回**直接用这一份**,不手抄一份键表
    /// —— 手抄的键表会与通知文本漂移,而这里就是同源的接缝。
    pub fn fields(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({}))
    }

    /// 完成通知正文(父会话里那条 `AgentDelivery` 的文本)。
    ///
    /// 所有字段都从本结构取:通知里报的用量、终态、变更文件必须与 `wait_agent`
    /// 返回的 JSON 逐项一致。`body` / `truncated` / `cancelled_note` 由调用方
    /// 提供(摘要正文、截断说明、被取消的后台工作说明)。
    pub fn notice_text(&self, body: &str, truncated: &str, cancelled_note: &str) -> String {
        let changed = if self.changed_paths.is_empty() {
            "（无）".to_string()
        } else {
            self.changed_paths.join("、")
        };
        let artifacts = if self.artifacts.is_empty() {
            "（无）".to_string()
        } else {
            self.artifacts
                .iter()
                .map(|item| format!("{}（{}）", item.output_id, item.tool))
                .collect::<Vec<_>>()
                .join("、")
        };
        let reason = self
            .terminal
            .reason
            .clone()
            .unwrap_or_else(|| "未知（没有终态事件）".to_string());
        let mut text = format!(
            "[子代理执行结束] {name}\n\
             子代理：{id}（{profile}）\n\
             结果：{reason}\n\
             终态：{outcome}（依据 {basis}，宿主验证 {verified}{provisional}）—— {detail}\n\
             本次子代理用量：{total} tokens（输入 {input} / 输出 {output} / 缓存读 {cache_read} / 推理 {reasoning}）\n\
             变更文件：{changed}\n\
             产物：{artifacts}\n\
             验证：{verification}\n\
             结果引用：会话 {id} 的日志与产物（read_tool_output 只能读本会话，父代理需要全文时用 wait_agent 查看该子代理）",
            name = self.name,
            id = self.id,
            profile = self.profile,
            outcome = self.terminal.outcome.as_str(),
            basis = self.terminal.basis.as_str(),
            verified = if self.terminal.host_verified {
                "有"
            } else {
                "无"
            },
            provisional = if self.terminal.finalized {
                ""
            } else {
                "，非最终"
            },
            detail = self.terminal.detail,
            total = self.usage.total_tokens,
            input = self.usage.uncached_input_tokens,
            output = self.usage.output_tokens,
            cache_read = self.usage.cache_read_tokens,
            reasoning = self.usage.reasoning_tokens,
            verification = self.verification_note,
        );
        for advisory in &self.advisories {
            text.push('\n');
            text.push_str(advisory);
        }
        // 口径说明永远要写:空数组也必须能区分"没有改动"与"读不到改动"。
        text.push_str("\n变更文件口径：");
        text.push_str(&self.changed_paths_note);
        text.push('\n');
        text.push_str(body);
        text.push_str(truncated);
        text.push_str(cancelled_note);
        text
    }
}

/// 从子会话算出一份交接信息(**唯一入口**)。
///
/// `settle` 的完成通知与 `wait_agent` 的返回都调它;`running` 为 `true` 时终态
/// 只是当前快照(`finalized = false`),不能当成最终结果。
fn subagent_handoff(
    child: Option<&Child>,
    session: &denia_session::Session,
    running: bool,
) -> SubagentHandoff {
    subagent_handoff_at(child, session, running, &subagent_events(session))
}

fn subagent_handoff_at(
    child: Option<&Child>,
    session: &denia_session::Session,
    running: bool,
    events: &[SessionEnvelope],
) -> SubagentHandoff {
    let id = session.id().to_string();
    let (name, profile) = match child {
        Some(child) => (
            child
                .descriptor
                .name
                .clone()
                .unwrap_or_else(|| child.descriptor.label.clone()),
            child
                .descriptor
                .profile
                .as_ref()
                .map(|profile| profile.qualified_id.clone())
                .unwrap_or_else(|| "-".to_string()),
        ),
        None => (id.clone(), "-".to_string()),
    };
    // 冻结授权里有没有账本工具:决定"天然不会产生验证结论"是解释还是异常。
    let ledger_tools = child.is_some_and(|child| {
        child
            .descriptor
            .effective_tools
            .as_ref()
            .is_some_and(|tools| {
                tools
                    .iter()
                    .any(|name| name == "run_checks" || name == "get_task")
            })
    });
    let task = denia_session::task_projection::project_task_scoped(events, &session.task_fold_scope());
    let verification = task.as_ref().and_then(|state| match state.verification() {
        verified @ VerificationState::Verified { .. } => Some(verified),
        // 拿不到可用结论一律给 `None`:父代理要判断的是"有没有可核验的结论",
        // 失效原因放在 `verification_note` 里,不混进这个字段的语义。
        VerificationState::Unverified { .. } => None,
    });
    let verification_note = verification_note(task.as_ref(), verification.as_ref(), ledger_tools);
    // 日志取一次、三处共用(终态 / 变更文件 / 产物),不重复扫描。
    let reason = last_turn_end_reason(events);
    let terminal = terminal_state(
        reason.as_ref(),
        task.as_ref(),
        verification.as_ref(),
        running,
    );
    // `denia-server` 不直接依赖 `denia-token-meter`:这里不写 `TurnTokenUsage`
    // 这个类型名,只读它暴露的五个分量与总数(`total()` 是计费口径的唯一来源,
    // 不在这里重算一份会漂移的总量)。
    let raw_usage = denia_session::Session::project_turn_token_usage(events);
    let usage = HandoffUsage {
        uncached_input_tokens: raw_usage.uncached_input_tokens,
        output_tokens: raw_usage.output_tokens,
        cache_read_tokens: raw_usage.cache_read_tokens,
        cache_write_tokens: raw_usage.cache_write_tokens,
        reasoning_tokens: raw_usage.reasoning_tokens,
        total_tokens: raw_usage.total(),
    };
    let (changed_paths, changed_paths_note) = changed_paths(events, session, task.as_ref());
    let artifacts = artifacts(events);
    let advisories = advisories(&terminal, verification.as_ref(), &verification_note);
    SubagentHandoff {
        id,
        name,
        profile,
        terminal,
        usage,
        changed_paths,
        changed_paths_note,
        artifacts,
        verification,
        verification_note,
        advisories,
    }
}

/// 取子会话日志(热/冷两条路唯一的入口)。
///
/// 热会话从内存取;冷会话的事件表**不驻留**(见 `state::LiveSessions::get_or_load`
/// 的冷加载),`events_after` 会走磁盘回放。用 `events()` 会把"冷会话读不到事件"
/// 静默说成"没有改动 / 没有终态" —— 那是假话,交接信息宁可说清楚读不到。
/// 代价是一次全量克隆(冷会话再加一次日志扫描),与本路径上已有的
/// `derive_messages` 相比不是瓶颈。
fn subagent_events(session: &denia_session::Session) -> Vec<SessionEnvelope> {
    session.events_after(0)
}

/// 子会话最近一次轮次终态(`None` = 日志里没有终态事件)。
fn last_turn_end_reason(events: &[SessionEnvelope]) -> Option<TurnEndReason> {
    events
        .iter()
        .rev()
        .find_map(|envelope| match &envelope.event {
            SessionEvent::TurnEnd { reason, .. } => Some(reason.clone()),
            _ => None,
        })
}

/// 终态判定:账本证据优先于子代理自己的轮次终态。
fn terminal_state(
    reason: Option<&TurnEndReason>,
    task: Option<&TaskState>,
    verification: Option<&VerificationState>,
    running: bool,
) -> TerminalState {
    let verdict = match verification {
        Some(VerificationState::Verified { verdict, .. }) => Some(*verdict),
        _ => None,
    };
    let outcome_at_ledger = task.and_then(TaskState::outcome);
    let (outcome, basis, host_verified, detail) = if matches!(
        outcome_at_ledger,
        Some(TaskOutcome::Blocked)
    ) || matches!(
        task.map(|state| state.status),
        Some(TaskStatus::Blocked)
    ) {
        (
            TerminalOutcome::NeedsDecision,
            TerminalBasis::HostBlocked,
            false,
            format!(
                "子代理在自己的任务账本上标记为外部受阻（{}），需要父代理处置。",
                task.and_then(|state| state.blocked_reason.clone())
                    .unwrap_or_else(|| "未给原因".to_string())
            ),
        )
    } else {
        // 失败型轮次终态:账本上的失败结论比子代理自己的话更硬。
        let failure_basis = if verdict == Some(ValidationVerdict::Failed) {
            TerminalBasis::HostVerified
        } else {
            TerminalBasis::SelfReported
        };
        match reason {
            Some(TurnEndReason::Completed) => match verdict {
                Some(ValidationVerdict::Passed)
                    if matches!(outcome_at_ledger, Some(TaskOutcome::Passed)) =>
                {
                    (
                        TerminalOutcome::Completed,
                        TerminalBasis::HostVerified,
                        true,
                        "子代理正常结束，且任务账本上绑当前 revision 的验收结论是通过（验收通过）。"
                            .to_string(),
                    )
                }
                Some(ValidationVerdict::Passed) => (
                    TerminalOutcome::Completed,
                    TerminalBasis::SelfReported,
                    false,
                    "子代理正常结束，账本上有通过的检查结论，但任务未收口或验收项未被全部覆盖，不构成验收通过。"
                        .to_string(),
                ),
                Some(ValidationVerdict::Failed) => (
                    TerminalOutcome::Failed,
                    TerminalBasis::HostVerified,
                    false,
                    "子代理正常结束，但任务账本上绑当前 revision 的结论是验收失败。".to_string(),
                ),
                None => (
                    TerminalOutcome::Completed,
                    TerminalBasis::SelfReported,
                    false,
                    "子代理自称完成（本轮正常结束），宿主没有可核验的验证结论——这个\"完成\"只来自它自己的最后发言。"
                        .to_string(),
                ),
            },
            Some(TurnEndReason::MaxTokens) => (
                TerminalOutcome::Failed,
                failure_basis,
                false,
                "子代理输出预算耗尽，本轮没有正常结束。".to_string(),
            ),
            Some(TurnEndReason::LoopDetected { repeats }) => (
                TerminalOutcome::Failed,
                failure_basis,
                false,
                format!("宿主检测到死循环（连续 {repeats} 次重复）后强制中断本轮。"),
            ),
            Some(TurnEndReason::Error { failure }) => (
                TerminalOutcome::Failed,
                failure_basis,
                false,
                format!("子代理本轮失败：{}（{}）。", failure.message, failure.code),
            ),
            Some(TurnEndReason::Aborted { .. }) | Some(TurnEndReason::Interrupted) => (
                TerminalOutcome::NeedsDecision,
                TerminalBasis::Interrupted,
                false,
                "子代理本轮被取消 / 中断，没有正常终态，需要父代理决定是否继续。".to_string(),
            ),
            None => (
                TerminalOutcome::NeedsDecision,
                TerminalBasis::NoTerminalEvent,
                false,
                "子会话里没有终态事件，无法判定子代理是否真的结束，需要父代理确认。".to_string(),
            ),
        }
    };
    TerminalState {
        outcome,
        basis,
        reason: reason.map(turn_end_label),
        host_verified,
        finalized: !running,
        detail: if running {
            format!("[进行中，非最终] {detail}")
        } else {
            detail
        },
    }
}

/// 验证面的人读交代。
fn verification_note(
    task: Option<&TaskState>,
    verification: Option<&VerificationState>,
    ledger_tools: bool,
) -> String {
    match (task, verification) {
        (_, Some(VerificationState::Verified {
            revision,
            verdict,
            covered,
            at,
            ..
        })) => format!(
            "{}（revision {}，覆盖 {} 个验收项，时间戳 {}）",
            match verdict {
                ValidationVerdict::Passed => "验收通过",
                ValidationVerdict::Failed => "验收失败",
            },
            revision.as_str(),
            covered.len(),
            at
        ),
        (Some(state), _) => match state.verification() {
            VerificationState::Unverified { reason } => format!(
                "无：账本在，但当前 revision 上拿不到可用结论（{}）。",
                stale_reason_label(&reason)
            ),
            VerificationState::Verified { .. } => "无：账本在，但当前 revision 上拿不到可用结论。".to_string(),
        },
        (None, _) if ledger_tools => {
            "无：该子代理的授权里含 get_task / run_checks，但账本上没有任何记录，没有可核验的验证结论。"
                .to_string()
        }
        (None, _) => "无：该子代理没有任务账本（内置的 develop / explore / verify 授权里都不含 get_task / run_checks），天然不会产生验证结论。"
            .to_string(),
    }
}

/// [`StaleReason`] 的中文口径(tools crate 里的那份是私有的,这里只给交接文案用)。
fn stale_reason_label(reason: &StaleReason) -> String {
    match reason {
        StaleReason::NeverRun => "当前 revision 上还没跑过任何验证".to_string(),
        StaleReason::RevisionChanged => "只在别的 revision 上跑过验证".to_string(),
        StaleReason::InputsChanged { paths } => {
            format!("验证之后这些文件又被改过：{}", paths.join("、"))
        }
        StaleReason::NoCheckRun => "有记录但没有可用的检查运行（空报告）".to_string(),
        StaleReason::ForeignOrigin => "结论来自别的会话，不参与本会话的裁决".to_string(),
        StaleReason::RevisionConflict => "换版复用了旧 revision id，当前版本身份不干净".to_string(),
    }
}

/// 父代理侧的提醒:没有可核验结论或结论是失败时必须显式说出来。
///
/// 这条提醒会随完成通知落进父会话的上下文(`AgentDelivery` 的文本) —— 注入通道
/// 不变,标注加在文本里。它存在的理由很具体:子代理的"完成"只由它自己的轮次
/// 终态决定([`TurnEndReason::Completed`] 的含义是"这轮没发工具调用"),宿主侧没有
/// 验收,父代理不能据此判定验收通过。
fn advisories(
    terminal: &TerminalState,
    verification: Option<&VerificationState>,
    verification_note: &str,
) -> Vec<String> {
    if !terminal.finalized {
        return vec![
            "[进行中] 子代理仍未结束，以下交接信息取自当前日志快照，不是最终结果。".to_string(),
        ];
    }
    match verification {
        None => vec![format!(
            "[未验证] 该子代理未产生可核验的验证结论（{verification_note}）；它的\"完成\"不等于验收通过，父代理不得据此判定完成，请自行核验（读回改动 / 跑测试 / 看子会话日志）。"
        )],
        Some(VerificationState::Verified {
            verdict: ValidationVerdict::Failed,
            ..
        }) => vec![
            "[验收失败] 宿主账本上的验证结论是验收失败，不能按已完成的子任务集成。".to_string(),
        ],
        Some(_) => Vec::new(),
    }
}

/// 归集"该子会话写过的文件"与它的口径说明。
///
/// 为什么是这个来源:宿主的变更账本(`RecordChange`)目前没有生产写入点,文件
/// 历史后端只按会话持有 `tracked` 且不对外暴露 —— 所以从**子会话日志**归集:
/// `write_file` / `edit` 调用里带 `path` 且**执行成功**(对应 `ToolResult` 不是
/// 错误)的那些。被拒的调用不算改动(否则只读子代理的幻觉写也会被报成更改),
/// `bash` 的重定向 / 删除 / 脚本写入推断不出来(命令是任意程序),一律不计入
/// —— 宁可少报也不猜。账本里已有的变更范围一并并入。
fn changed_paths(
    events: &[SessionEnvelope],
    session: &denia_session::Session,
    task: Option<&TaskState>,
) -> (Vec<String>, String) {
    let cwd = PathBuf::from(session.header().cwd.clone());
    let mut paths: Vec<String> = Vec::new();
    let mut saw_log_write = false;
    let mut from_ledger = 0usize;
    {
        let mut succeeded: HashSet<&str> = HashSet::new();
        for envelope in events {
            if let SessionEvent::ToolResult {
                call_id, is_error, ..
            } = &envelope.event
                && !*is_error
            {
                succeeded.insert(call_id.as_str());
            }
        }
        for envelope in events {
            let SessionEvent::ToolCall {
                call_id,
                name,
                arguments,
                ..
            } = &envelope.event
            else {
                continue;
            };
            if !matches!(name.as_str(), "write_file" | "edit")
                || !succeeded.contains(call_id.as_str())
            {
                continue;
            }
            let Some(path) = tool_path_argument(arguments) else {
                continue;
            };
            saw_log_write = true;
            push_unique(&mut paths, display_path(&cwd, &path));
        }
    }
    if let Some(state) = task {
        for change in &state.changes {
            for fingerprint in &change.fingerprints {
                if push_unique(&mut paths, fingerprint.path.clone()) {
                    from_ledger += 1;
                }
            }
        }
    }
    let mut note = if events.is_empty() {
        "口径：会话日志为空（子会话没有任何事件）—— 无法判断落盘改动。".to_string()
    } else if saw_log_write {
        "口径：子会话日志里执行成功的 write_file / edit 调用的 path（去重）。".to_string()
    } else {
        "口径：子会话日志里没有执行成功的 write_file / edit 调用。".to_string()
    };
    note.push_str("bash 的重定向 / 删除 / 脚本写入推断不出来，不计入。");
    if from_ledger > 0 {
        note.push_str(&format!("另含任务账本上记录的 {from_ledger} 个变更文件。"));
    }
    if paths.is_empty() {
        note.push_str("本字段为空 = 没有可识别的落盘改动，不是读取失败。");
    }
    (paths, note)
}

/// 一条工具调用的 `path` 参数(相对或绝对都可以)。
fn tool_path_argument(arguments: &str) -> Option<PathBuf> {
    let value: serde_json::Value = serde_json::from_str(arguments.trim()).ok()?;
    let raw = value.get("path")?.as_str()?.trim();
    (!raw.is_empty()).then(|| PathBuf::from(raw))
}

/// 展示口径:工作区内记相对路径(分隔符统一为 `/`),工作区外记绝对路径 ——
/// 与 `run_checks` 冻结输入指纹时用的是同一套口径。
fn display_path(cwd: &Path, path: &Path) -> String {
    match path.strip_prefix(cwd) {
        Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
        Err(_) => path.to_string_lossy().replace('\\', "/"),
    }
}

/// 去重插入;返回值表示是否真的插入了(口径统计用)。
fn push_unique(items: &mut Vec<String>, item: String) -> bool {
    if items.iter().any(|existing| existing == &item) {
        return false;
    }
    items.push(item);
    true
}

/// 归集子会话产生的产物引用(`ToolResult.meta.outputArtifact`)。
fn artifacts(events: &[SessionEnvelope]) -> Vec<ArtifactRef> {
    let mut names: HashMap<&str, &str> = HashMap::new();
    for envelope in events {
        if let SessionEvent::ToolCall { call_id, name, .. } = &envelope.event {
            names.insert(call_id.as_str(), name.as_str());
        }
    }
    let mut out: Vec<ArtifactRef> = Vec::new();
    for envelope in events {
        let SessionEvent::ToolResult {
            call_id,
            meta: Some(meta),
            ..
        } = &envelope.event
        else {
            continue;
        };
        let Some(artifact) = meta.get("outputArtifact") else {
            continue;
        };
        let Some(output_id) = artifact.get("output_id").and_then(Value::as_str) else {
            continue;
        };
        if out.iter().any(|item| item.output_id == output_id) {
            continue;
        }
        out.push(ArtifactRef {
            output_id: output_id.to_string(),
            call_id: call_id.clone(),
            tool: names
                .get(call_id.as_str())
                .copied()
                .unwrap_or("-")
                .to_string(),
            complete: artifact.get("complete").and_then(Value::as_bool),
        });
    }
    out
}

/// 子代理运行快照文件：放在会话目录内的不可变大文本。
///
/// header 只保存引用与 hash（否则会话列表每行都会携带最多 64 KiB 的角色全文）。
/// 引用不存在或 hash 不匹配一律拒绝启动，不回退默认。
const SUBAGENT_SNAPSHOT_FILE: &str = "subagent.json";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubagentSnapshotFile {
    snapshot_version: u32,
    instructions: String,
}

/// 稳定内容 hash（FNV-1a）：校验快照文件与 header 记录是否一致。
pub(crate) fn instructions_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn write_subagent_snapshot(
    sessions: &denia_session::SessionStore,
    id: &str,
    instructions: &str,
) -> Result<(), String> {
    let dir = sessions.root().join(id);
    std::fs::create_dir_all(&dir).map_err(|error| format!("创建会话目录失败：{error}"))?;
    let payload = serde_json::to_string(&SubagentSnapshotFile {
        snapshot_version: denia_core::subagent::SUBAGENT_SNAPSHOT_VERSION,
        instructions: instructions.to_string(),
    })
    .map_err(|error| format!("序列化子代理快照失败：{error}"))?;
    let tmp = dir.join("subagent.json.tmp");
    std::fs::write(&tmp, payload.as_bytes()).map_err(|error| {
        format!(
            "写入子代理快照失败（{}）：{error}（snapshot/io）",
            dir.display()
        )
    })?;
    std::fs::rename(&tmp, dir.join(SUBAGENT_SNAPSHOT_FILE))
        .map_err(|error| format!("替换子代理快照失败：{error}（snapshot/io）"))?;
    Ok(())
}

/// 读取并校验一份子代理快照。
///
/// - 新快照（`snapshotVersion >= 1`）：文件必须存在，且角色正文 hash 与 header
///   一致；不一致说明会话目录被外部改动，拒绝启动而不是回退到空角色。
/// - 旧快照（版本 0）：没有引用文件，角色正文取旧 `persona` 字段（解释为
///   角色补充），授权按 [`crate::subagents::resolver::legacy_grant`] 保守构造。
pub(crate) fn read_subagent_snapshot(
    sessions: &denia_session::SessionStore,
    id: &str,
    descriptor: &SubagentDescriptor,
) -> Result<String, String> {
    use denia_core::subagent::codes as subagent_codes;
    if descriptor.snapshot_version < denia_core::subagent::SUBAGENT_SNAPSHOT_VERSION {
        return Ok(descriptor.persona.clone().unwrap_or_default());
    }
    let Some(reference) = descriptor.instructions_ref.as_deref() else {
        return Err(format!(
            "子代理快照缺少引用（{}）：拒绝启动，避免以空角色运行",
            subagent_codes::SNAPSHOT_INVALID
        ));
    };
    if reference != SUBAGENT_SNAPSHOT_FILE {
        return Err(format!(
            "子代理快照引用了未知文件 `{reference}`（{}）",
            subagent_codes::SNAPSHOT_INVALID
        ));
    }
    let path = sessions.root().join(id).join(reference);
    let raw = std::fs::read_to_string(&path).map_err(|error| {
        format!(
            "子代理快照文件不可读（{}）：{}（{error}）",
            path.display(),
            subagent_codes::SNAPSHOT_INVALID
        )
    })?;
    let file: SubagentSnapshotFile = serde_json::from_str(&raw).map_err(|error| {
        format!(
            "子代理快照文件格式无效（{}）：{error}",
            subagent_codes::SNAPSHOT_INVALID
        )
    })?;
    if let Some(expected) = descriptor.instructions_hash.as_deref()
        && expected != instructions_hash(&file.instructions)
    {
        return Err(format!(
            "子代理快照与 header 记录的 hash 不一致（{}）：拒绝启动，不回退默认角色",
            subagent_codes::SNAPSHOT_INVALID
        ));
    }
    Ok(file.instructions)
}

#[async_trait]
impl AgentRuntime for Runtime {
    async fn execute_command(
        &self,
        command: denia_tools::runtime_command::RuntimeCommand,
        ctx: &ToolContext,
    ) -> Result<Value, String> {
        let owner = ctx.session_id.as_deref().ok_or("该能力需要会话身份")?;
        let config = self.config();
        use denia_tools::runtime_command::{RuntimeCommand, SkillCommand};
        match command {
            RuntimeCommand::Skill(action) => match action {
                SkillCommand::List => Ok(json!(
                    self.skills(ctx.cwd.clone())
                        .await?
                        .into_iter()
                        .filter(|s| s.model_invocable)
                        .collect::<Vec<_>>()
                )),
                SkillCommand::Resource { name, path } => {
                    let skills = self.skills(ctx.cwd.clone()).await?;
                    tokio::task::spawn_blocking(move || {
                        crate::skills::resource(&skills, &name, &path)
                    })
                    .await
                    .map_err(|e| e.to_string())?
                }
                SkillCommand::Load { name } => {
                    // dsh 对齐:SKILL.md 全文直接随工具结果返回,一次性进历史;
                    // load 前经 skills::load 校验 model_invocable(user=false),
                    // disable-model-invocation 技能只能走用户 /name 手势。
                    self.load_skill(ctx.cwd.clone(), name, false).await
                }
            },
            RuntimeCommand::JobStart(args) => {
                let command = args.command;
                let timeout = args
                    .timeout_ms
                    .unwrap_or(config.job_timeout_ms)
                    .min(config.job_timeout_ms);
                Ok(json!(self.inner.jobs.start(
                    ctx,
                    &command,
                    args.label.as_deref().unwrap_or(&command),
                    timeout,
                    config.max_jobs,
                    config.retained_jobs,
                    config.output_bytes
                )?))
            }
            RuntimeCommand::JobList(_) => Ok(json!(self.inner.jobs.list(owner))),
            RuntimeCommand::JobOutput(args) => {
                let id = args.id;
                let _lease = self.inner.jobs.wait_lease(&id, owner)?;
                let result = self.inner.jobs.read(&id, owner)?;
                let pending = result["job"]["finishedAt"].is_null();
                Ok(background_result(result, pending))
            }
            RuntimeCommand::JobKill(args) => {
                self.inner.jobs.kill(&args.id, owner)?;
                Ok(json!({"ok":true}))
            }
            RuntimeCommand::ListAgents(_) => Ok(self.list(owner)),
            RuntimeCommand::InterruptAgent(args) => {
                self.interrupt(owner, &args.target).await?;
                Ok(json!({"ok":true}))
            }
            RuntimeCommand::SendMessage(args) => {
                let target = args.target;
                self.authorize(owner, &target, false)?;
                let id = self
                    .enqueue(
                        &target,
                        uuid::Uuid::new_v4().to_string(),
                        format!("[代理 {owner} 发来消息]\n{}", args.message),
                        format!("agent:{owner}"),
                    )
                    .await?;
                Ok(json!({"messageId":id,"target":target}))
            }
            RuntimeCommand::WaitAgent(args) => {
                let target = args.target;
                self.authorize(owner, &target, true)?;
                let live = self.live(&target).await?;
                let pending = self.is_active(&target);
                let messages = if pending {
                    Vec::new()
                } else {
                    live.session
                        .derive_messages()
                        .into_iter()
                        .rev()
                        .take(1)
                        .collect::<Vec<_>>()
                };
                // 交接信息与完成通知**同一份计算**(`subagent_handoff`):返回对象
                // 直接铺开这份结构的字段(不是手抄一份键表),父代理在通知里读到
                // 的字段与这里查到的因此不可能漂移。
                let child = self.inner.children.lock().unwrap().get(&target).cloned();
                let handoff = subagent_handoff(child.as_ref(), &live.session, pending);
                let mut result = handoff.fields();
                // 既有字段原位保留:调用方与前端读的就是这三个(`id` 与交接里的
                // 子会话 id 同值,这里以命令参数为准重写一次)。
                result["id"] = json!(target);
                result["running"] = json!(pending);
                result["messages"] = json!(messages);
                Ok(background_result(result, pending))
            }
            RuntimeCommand::Delegate { fork, args } => {
                let runtime = self.clone();
                let ctx = ctx.clone();
                tokio::spawn(async move { runtime.delegate(fork, args, &ctx).await })
                    .await
                    .map_err(|e| format!("子代理启动任务失败：{e}"))?
            }
        }
    }
    async fn context(&self, session: &str, cwd: &Path) -> Result<Vec<String>, String> {
        let _ = cwd;
        let parent = self
            .inner
            .children
            .lock()
            .unwrap()
            .get(session)
            .map(|c| c.parent_id.clone());
        Ok(vec![format!(
            "[denia 能力上下文]\n当前代理：{session}；父代理：{}。",
            parent.as_deref().unwrap_or("无")
        )])
    }
    async fn skill_catalog(
        &self,
        session: &str,
        cwd: &Path,
    ) -> Result<Vec<(String, String)>, String> {
        let _ = session;
        let max = self.config().skill_catalog_description_max_chars;
        let skills = self.skills(cwd.to_path_buf()).await?;
        Ok(skills
            .iter()
            .filter(|s| s.model_invocable)
            .map(|s| {
                (
                    s.name.clone(),
                    crate::skills::truncate_chars(&s.description, max),
                )
            })
            .collect())
    }
    async fn user_skill(
        &self,
        session: &str,
        name: &str,
        cwd: &Path,
    ) -> Result<Option<(String, String)>, String> {
        let _ = session;
        // load(user=true) 校验 user_invocable;找不到/不允许/解析失败一律 None,
        // 手势降级为普通文本(dsh:未知名字不是这条边界认识的声明)。
        match self
            .load_skill(cwd.to_path_buf(), name.to_string(), true)
            .await
        {
            Ok(value) => Ok(Some((
                value["skill"]["source"].as_str().unwrap_or_default().into(),
                value["body"].as_str().unwrap_or_default().into(),
            ))),
            Err(_) => Ok(None),
        }
    }
    async fn workspace_instructions(
        &self,
        session: &str,
        cwd: &Path,
        touched: &[PathBuf],
        previous: Option<&str>,
    ) -> Result<Option<String>, String> {
        let config = self.config();
        if config.workspace_instructions_max_bytes == 0 {
            return Ok(None);
        }
        // 作用域由宿主按**真实会话身份**决定，模型与 profile 都无法指定。
        // 父 preset 关闭 agentsMd 时整条通道由调用方跳过（全局排除不等于重新
        // 打开父已禁用的注入能力）。
        let sessions = self.inner.sessions.clone();
        let session = session.to_string();
        let is_child = tokio::task::spawn_blocking(move || {
            sessions
                .read_log_header(&session)
                .map(|header| header.subagent.is_some())
        })
        .await
        .map_err(|error| error.to_string())?
        .unwrap_or(false);
        let scope = if is_child {
            crate::workspace_instructions::InstructionScope::ProjectOnly
        } else {
            crate::workspace_instructions::InstructionScope::GlobalAndProject
        };
        let home = self.inner.home.clone();
        let cwd = cwd.to_path_buf();
        let touched = touched.to_vec();
        // 每次重新发现并渲染，没有跨会话/跨作用域缓存：root 的项目指令缓存
        // 不可能被 child 复用（计划 9.2 的缓存键要求在此实现下天然成立）。
        let files = tokio::task::spawn_blocking(move || {
            crate::workspace_instructions::discover(
                &home,
                &cwd,
                &touched,
                config.workspace_instructions_max_source_bytes,
                scope,
            )
        })
        .await
        .map_err(|e| e.to_string())?;
        Ok(crate::workspace_instructions::render(
            &files,
            config.workspace_instructions_max_bytes as usize,
            previous,
        ))
    }
    fn memory_root_for(&self, cwd: &Path) -> Option<PathBuf> {
        if !self.config().memory_enabled {
            return None;
        }
        Some(crate::project_memory::memory_root(&self.inner.home, cwd))
    }
    /// 项目记忆索引(MEMORY.md)注入文本;None = 未启用。选择层按触碰路径
    /// 与最近用户消息挑相关条目,被省略的条目在渲染层留全文指针。
    async fn project_memory_index(
        &self,
        cwd: &Path,
        touched: &[PathBuf],
        recent_user: Option<&str>,
    ) -> Result<Option<String>, String> {
        let config = self.config();
        if !config.memory_enabled {
            return Ok(None);
        }
        let home = self.inner.home.clone();
        let cwd = cwd.to_path_buf();
        let touched = touched.to_vec();
        let recent_user = recent_user.map(str::to_string);
        let max_bytes = config.project_memory_max_bytes as usize;
        // 索引非空才注入;空桶不注入——记忆目录路径已由 assemble 阶段
        // 内联进「# 项目记忆」段,模型任何 step 都可见。

        tokio::task::spawn_blocking(move || {
            let root = crate::project_memory::memory_root(&home, &cwd);
            match crate::project_memory::read_index_for_injection(
                &root,
                &cwd,
                &touched,
                recent_user.as_deref(),
                max_bytes,
            ) {
                Some(selection) if !selection.body.trim().is_empty() => {
                    Ok(Some(crate::project_memory::render_index_block(
                        &root,
                        &selection.body,
                        selection.omitted,
                    )))
                }
                _ => Ok(None),
            }
        })
        .await
        .map_err(|e| e.to_string())?
    }
    /// 子代理运行快照：角色正文从会话目录内的文件读取并校验，授权取派遣时
    /// 冻结的显式列表。旧描述符（版本 0）没有文件与显式列表，按历史只读上限
    /// 保守构造，绝不等于 inherit。
    async fn subagent_prompt(
        &self,
        session: &str,
    ) -> Result<Option<denia_tools::capabilities::SubagentPrompt>, String> {
        let live = self.live(session).await?;
        let Some(descriptor) = live.session.header().subagent.clone() else {
            return Ok(None);
        };
        let sessions = self.inner.sessions.clone();
        let id = session.to_string();
        let desc = descriptor.clone();
        let instructions =
            tokio::task::spawn_blocking(move || read_subagent_snapshot(&sessions, &id, &desc))
                .await
                .map_err(|error| error.to_string())??;
        let legacy = descriptor.snapshot_version < denia_core::subagent::SUBAGENT_SNAPSHOT_VERSION;
        let effective_tools = descriptor.effective_tools.clone().unwrap_or_else(|| {
            crate::subagents::resolver::legacy_grant(descriptor.allowed_tools.as_deref())
        });
        Ok(Some(denia_tools::capabilities::SubagentPrompt {
            name: descriptor
                .name
                .clone()
                .or_else(|| Some(descriptor.label.clone())),
            instructions,
            effective_tools,
            permission_ceiling: Some(descriptor.permission_ceiling.as_str().to_string()),
            parent_preset_persona: descriptor.parent_preset_persona.clone(),
            legacy,
        }))
    }

    /// 父代理可见的派遣目录 + 派遣纪律。
    ///
    /// 只公布限定 id、名称、描述与摘要（不含 instructions 全文）；子代理永不
    /// 注入；父 preset 关闭 subagents 时不出现（工具与纪律段同进退）。
    async fn subagent_catalog(&self, session: &str) -> Result<Option<String>, String> {
        let live = self.live(session).await?;
        if live.session.header().subagent.is_some() {
            return Ok(None);
        }
        let Some(driver) = self.inner.driver.get().and_then(Weak::upgrade) else {
            return Ok(None);
        };
        if !driver.features_for(&live.session).subagents {
            return Ok(None);
        }
        let cwd = PathBuf::from(live.session.header().cwd.clone());
        let root = cwd
            .is_dir()
            .then(|| crate::subagents::project_root_for(&cwd));
        let entries = self.inner.profiles.catalog_entries(root.as_deref());
        if entries.is_empty() {
            return Ok(None);
        }
        let mut lines = vec![
            "[denia 子代理目录]".to_string(),
            "可用子代理类型（qualifiedId｜名称｜用途｜工具｜模型）：".to_string(),
        ];
        for entry in &entries {
            lines.push(format!(
                "- {}｜{}｜{}｜{}｜{}",
                entry.qualified_id, entry.name, entry.description, entry.tools, entry.model
            ));
        }
        lines.push(String::new());
        lines.push(
            "派遣方式：profile_id 用上面的 qualifiedId；inline 在调用时给出临时定义（不落盘）；两者都省略时使用默认的 develop 定义。profile_id 与 inline 互斥。"
                .to_string(),
        );
        lines.push(
            "分工与等待：按不重叠的文件或模块分工，最终由你统一集成。派遣后立即返回 pending，不要轮询——继续独立工作或先向用户回复进度并结束本轮；完成结果会自动送达。执行完成前不要宣称工作已完成。"
                .to_string(),
        );
        lines.push(
            "结果汇报区分三种情形：成功结果、任务失败（附失败原因）、等待你决策。子代理不能再派遣子代理；子代理不会自动继承全局 AGENTS.md，只发现项目级规则。"
                .to_string(),
        );
        let text = lines.join("\n");
        // 独立字节预算：目录每步注入，必须自带上限与诊断，不能挤占指令预算，
        // 也不能因为超限就静默截断——截断必须明示。
        let budget = self.config().subagent_catalog_max_bytes as usize;
        if text.len() > budget {
            tracing::warn!(
                session = session,
                bytes = text.len(),
                budget,
                "子代理目录超出预算，已按 UTF-8 边界截断"
            );
            return Ok(Some(format!(
                "{}\n（目录已按 {budget} 字节预算截断，部分定义未列出；未列出的定义仍可直接用 profile_id 指定。）",
                truncate_utf8(&text, budget)
            )));
        }
        Ok(Some(text))
    }

    async fn drain(&self, session: &str) -> Result<Vec<String>, String> {
        let live = self.live(session).await?;
        let inner = self.inner.clone();
        let id = session.to_string();
        tokio::task::spawn_blocking(move || {
            let mut inboxes = inner.inboxes.lock().unwrap();
            let inbox = inboxes.entry(id).or_default();
            Self::sync_inbox(inbox, &live.session);
            let pending: Vec<_> = inbox.pending.values().cloned().collect();
            for (id, text, source) in pending {
                let event = live
                    .session
                    .append(SessionEvent::AgentDelivery { id, text, source })
                    .map_err(|e| e.to_string())?;
                let _ = live.followers.send(event);
            }
            live.session.flush().map_err(|e| e.to_string())?;
            Self::sync_inbox(inbox, &live.session);
            inner.delivery.expedite(live.session.id());
            Ok(Vec::new())
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// 会话的冻结工具授权：MCP 目录等按会话投影的目录必须用它过滤。
    fn granted_tools(&self, session: &str) -> Option<Vec<String>> {
        if let Some(live) = self.inner.live.get(session) {
            return grant_of(live.session.header());
        }
        self.inner
            .sessions
            .read_log_header(session)
            .ok()
            .and_then(|header| grant_of(&header))
    }
}

/// 会话头里的冻结授权；非子代理返回 None（= 没有被定义收窄）。
fn grant_of(header: &denia_core::session::SessionHeader) -> Option<Vec<String>> {
    let child = header.subagent.as_ref()?;
    Some(child.effective_tools.clone().unwrap_or_else(|| {
        denia_core::subagent::legacy_child_tools(child.allowed_tools.as_deref())
    }))
}

/// UTF-8 边界截断：预算切在多字节序列中间时回退到前一个字符边界。
fn truncate_utf8(text: &str, budget: usize) -> &str {
    if text.len() <= budget {
        return text;
    }
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// 未结算的审批/提问数量与最后一次终态摘要。///
/// 扫描窗口限定在最近 [`INTERACTION_SCAN_TAIL`] 条事件：等待中的交互必然是
/// 最近发生的（进程正阻塞在它上面），而每次列表刷新都全量扫长会话代价太大。
/// 比窗口更早的未结算请求已由加载期收敛（见 `session::recovery`）补上终态。
const INTERACTION_SCAN_TAIL: usize = 2000;

fn pending_interactions(session: &denia_session::Session) -> (usize, usize, Option<String>) {
    let events = session.events();
    let start = events.len().saturating_sub(INTERACTION_SCAN_TAIL);
    let mut approvals: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut asks: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut result: Option<String> = None;
    for envelope in &events[start..] {
        match &envelope.event {
            SessionEvent::ApprovalAsked { request_id, .. } => {
                approvals.insert(request_id.as_str());
            }
            SessionEvent::ApprovalDecided { request_id, .. } => {
                approvals.remove(request_id.as_str());
            }
            SessionEvent::AskRequested { request_id, .. } => {
                asks.insert(request_id.as_str());
            }
            SessionEvent::AskResolved { request_id, .. } => {
                asks.remove(request_id.as_str());
            }
            SessionEvent::TurnEnd { reason, .. } => result = Some(turn_end_label(reason)),
            _ => {}
        }
    }
    (approvals.len(), asks.len(), result)
}

fn turn_end_label(reason: &denia_core::session::TurnEndReason) -> String {
    use denia_core::session::TurnEndReason;
    match reason {
        TurnEndReason::Completed => "已完成".to_string(),
        TurnEndReason::Aborted { .. } => "被取消".to_string(),
        TurnEndReason::MaxTokens => "输出预算耗尽".to_string(),
        TurnEndReason::LoopDetected { repeats } => format!("检测到死循环（连续 {repeats} 次）"),
        TurnEndReason::Interrupted => "中断".to_string(),
        TurnEndReason::Error { failure } => {
            format!("失败：{}（{}）", failure.message, failure.code)
        }
    }
}

include!("agent_runtime_delivery.rs");

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::{
        error::LlmError,
        stream::{ContentBlock, FinishReason, StreamChunk},
    };
    use denia_llm::{
        ChunkStream, GenerateRequest, LlmAdapter, LlmModelInfo, LlmResolvedModelInfo, ProviderInfo,
    };
    struct Mock;
    #[async_trait]
    impl LlmAdapter for Mock {
        fn provider_info(&self, p: &str) -> ProviderInfo {
            ProviderInfo {
                id: p.into(),
                name: "测试".into(),
            }
        }
        async fn list_models(&self, _: &str) -> Result<Vec<LlmModelInfo>, LlmError> {
            Ok(Vec::new())
        }
        async fn resolve_model(&self, p: &str, m: &str) -> Result<LlmResolvedModelInfo, LlmError> {
            Ok(LlmResolvedModelInfo {
                info: LlmModelInfo {
                    provider: p.into(),
                    id: m.into(),
                    name: m.into(),
                    description: None,
                    input_modalities: vec!["text".into()],
                },
                context_window: Some(100_000),
                default_max_tokens: Some(4000),
                reasoning: None,
            })
        }
        async fn stream(
            &self,
            _: &str,
            request: &GenerateRequest,
        ) -> Result<ChunkStream, LlmError> {
            // 不再全局断言"必有 skill"：受限子代理（如只授予 bash/jobs 的
            // 定义）本来就不该看到 skill。需要它的分支各自断言。
            if request.model == "hold" {
                return Ok(Box::pin(futures::stream::pending()));
            }
            if request.model == "background-agent-parent" {
                let results: Vec<_> = request
                    .messages
                    .iter()
                    .filter(|message| message.role == denia_core::message::ChatRole::Tool)
                    .collect();
                let call = match results.len() {
                    0 => Some((
                        "spawn_agent",
                        json!({"prompt":"继续后台调查","model":"hold","run_in_background":false}),
                    )),
                    1 => Some((
                        "wait_agent",
                        json!({"target":serde_json::from_str::<Value>(&results[0].content).unwrap()["childId"],"timeout_ms":600_000}),
                    )),
                    _ => None,
                };
                if let Some((name, args)) = call {
                    return Ok(Box::pin(futures::stream::iter(vec![
                        Ok(StreamChunk::BlockEnd {
                            index: 0,
                            block: ContentBlock::ToolCall {
                                id: format!("background-call-{}", results.len()),
                                name: name.into(),
                                arguments: args.to_string(),
                                incomplete: false,
                            },
                        }),
                        Ok(StreamChunk::Finish {
                            reason: FinishReason::ToolCalls,
                        }),
                    ])));
                }
                let completed = request
                    .messages
                    .iter()
                    .any(|message| message.content.contains("[子代理执行结束]"));
                return Ok(Box::pin(futures::stream::iter(vec![
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: ContentBlock::Text {
                            text: if completed {
                                "后台结果已自动返回"
                            } else {
                                "任务仍在后台运行，我先回复当前进度"
                            }
                            .into(),
                        },
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::Stop,
                    }),
                ])));
            }
            // —— 执行型子代理的真实副作用链路（无需真实模型）——
            // exec-parent 派一个 develop 子代理；子代理读→写→跑命令，然后收尾。
            if request.model == "exec-parent" {
                let results = request
                    .messages
                    .iter()
                    .filter(|m| m.role == denia_core::message::ChatRole::Tool)
                    .count();
                if results == 0 {
                    return Ok(tool_call_stream(
                        "exec-spawn",
                        "spawn_agent",
                        json!({"prompt":"SUBTASK-EXEC","description":"开发子代理","model":"exec-child"}),
                    ));
                }
                return Ok(text_stream("子代理已完成任务"));
            }
            if request.model == "exec-child" {
                // 子代理的有效工具来自**冻结授权**：develop 含写与命令。
                assert!(
                    request.tools.iter().any(|s| s.name == "write_file"),
                    "develop 子代理必须有 write_file"
                );
                assert!(
                    request.tools.iter().any(|s| s.name == "bash"),
                    "develop 子代理必须有 bash"
                );
                for denied in ["spawn_agent", "fork_agent", "exit_plan", "get_goal"] {
                    assert!(
                        !request.tools.iter().any(|s| s.name == denied),
                        "{denied} 不得出现在子代理工具面"
                    );
                }
                let results = request
                    .messages
                    .iter()
                    .filter(|m| m.role == denia_core::message::ChatRole::Tool)
                    .count();
                let call = match results {
                    0 => Some(("read_file", json!({"path": "existing.txt"}))),
                    1 => Some((
                        "write_file",
                        json!({"path": "child-wrote.txt","content":"子代理写入\n"}),
                    )),
                    2 => Some(("bash", json!({"command":"echo child-ran > child-ran.txt"}))),
                    // 没被授予 job_start 的子代理请求后台执行：必须被拒，且不能
                    // 真的起一个后台任务（只禁 job_start 不是收口）。
                    3 => Some((
                        "bash",
                        json!({"command":"echo should-not-run","run_in_background":true}),
                    )),
                    _ => None,
                };
                if let Some((name, args)) = call {
                    return Ok(tool_call_stream(&format!("exec-{results}"), name, args));
                }
                return Ok(text_stream("开发子代理完成"));
            }
            // explore 子代理：模型幻觉调用写工具与命令，必须被拒且文件无变化。
            if request.model == "explore-parent" {
                let results = request
                    .messages
                    .iter()
                    .filter(|m| m.role == denia_core::message::ChatRole::Tool)
                    .count();
                if results == 0 {
                    return Ok(tool_call_stream(
                        "explore-spawn",
                        "spawn_agent",
                        json!({"prompt":"SUBTASK-EXPLORE","profile_id":"builtin:explore","model":"explore-child"}),
                    ));
                }
                return Ok(text_stream("探索子代理已回结论"));
            }
            if request.model == "explore-child" {
                // explore 的冻结授权只读：写与命令既不在 schema 里，也不可执行。
                for denied in ["write_file", "edit", "bash", "job_start"] {
                    assert!(
                        !request.tools.iter().any(|s| s.name == denied),
                        "explore 子代理的工具面不得含 {denied}"
                    );
                }
                assert!(
                    request.tools.iter().any(|s| s.name == "read_file"),
                    "explore 子代理必须有 read_file"
                );
                let results = request
                    .messages
                    .iter()
                    .filter(|m| m.role == denia_core::message::ChatRole::Tool)
                    .count();
                let call = match results {
                    0 => Some((
                        "write_file",
                        json!({"path": "explore-wrote.txt","content":"不应写入\n"}),
                    )),
                    1 => Some(("bash", json!({"command":"echo nope > explore-wrote.txt"}))),
                    _ => None,
                };
                if let Some((name, args)) = call {
                    return Ok(tool_call_stream(&format!("explore-{results}"), name, args));
                }
                return Ok(text_stream("只读探索完成"));
            }
            // 子代理遗留后台任务：正常结束时必须被取消并写进结果通知。
            if request.model == "jobs-parent" {
                let results = request
                    .messages
                    .iter()
                    .filter(|m| m.role == denia_core::message::ChatRole::Tool)
                    .count();
                if results == 0 {
                    return Ok(tool_call_stream(
                        "jobs-spawn",
                        "spawn_agent",
                        json!({
                            "prompt": "SUBTASK-JOBS",
                            "description": "带后台任务的子代理",
                            "model": "jobs-child",
                            "inline": {
                                "name": "后台任务员",
                                "description": "启动一个长任务后收尾",
                                "tools": {"mode": "allowlist", "names": ["bash", "job_start", "job_list"]},
                                "model": {"mode": "inherit"},
                                "permissionCeiling": "inherit"
                            }
                        }),
                    ));
                }
                return Ok(text_stream("子代理已回结果"));
            }
            if request.model == "jobs-child" {
                // 冻结授权里有 job_start，因此 run_in_background 参数也必须在。
                assert!(
                    request.tools.iter().any(|s| s.name == "job_start"),
                    "被授予的 job_start 必须可见"
                );
                assert!(
                    request.tools.iter().any(|s| s.name == "bash"),
                    "被授予的 bash 必须可见"
                );
                assert!(
                    !request.tools.iter().any(|s| s.name == "job_kill"),
                    "未授予的 job_kill 不得出现"
                );
                let results = request
                    .messages
                    .iter()
                    .filter(|m| m.role == denia_core::message::ChatRole::Tool)
                    .count();
                if results == 0 {
                    return Ok(tool_call_stream(
                        "jobs-0",
                        "job_start",
                        json!({"command": LONG_RUNNING_PROBE, "label": "leftover-probe"}),
                    ));
                }
                return Ok(text_stream("子代理启动后台任务后收尾"));
            }
            if request.model == "legacy-child" {
                // 旧子代理建立投影后：旧自动注入不得再进模型请求，任务保留。
                assert!(
                    !request
                        .messages
                        .iter()
                        .any(|message| message.content.contains("GLOBAL-SENTINEL")),
                    "旧自动注入不得进入模型请求：{:?}",
                    request
                        .messages
                        .iter()
                        .map(|message| message.content.clone())
                        .collect::<Vec<_>>()
                );
                assert!(
                    request
                        .messages
                        .iter()
                        .any(|message| message.content.contains("调查登录流程")),
                    "child 自己的任务投递必须保留"
                );
                // H06：续跑不得扩大快照授权（旧描述符 → 保守只读集合）。
                for forbidden in ["bash", "write_file", "edit", "job_start", "spawn_agent"] {
                    assert!(
                        !request.tools.iter().any(|schema| schema.name == forbidden),
                        "续跑旧子代理不得扩权到 {forbidden}：{:?}",
                        request
                            .tools
                            .iter()
                            .map(|schema| schema.name.clone())
                            .collect::<Vec<_>>()
                    );
                }
                assert!(request.tools.iter().all(|schema| {
                    denia_core::subagent::LEGACY_READ_ONLY_TOOLS.contains(&schema.name.as_str())
                }));
                return Ok(text_stream("旧子代理已按投影继续"));
            }
            if request.model == "orchestrator" {
                let results: Vec<_> = request
                    .messages
                    .iter()
                    .filter(|m| m.role == denia_core::message::ChatRole::Tool)
                    .collect();
                let call = match results.len() {
                    0 => Some((
                        "spawn_agent",
                        json!({"prompt":"执行子任务","model":"done","description":"接口验证子代理"}),
                    )),
                    1 => Some((
                        "bash",
                        json!({"command":"echo runtime-e2e","run_in_background":true}),
                    )),
                    2 => Some((
                        "job_output",
                        json!({"id":serde_json::from_str::<Value>(&results[1].content).unwrap()["id"],"wait":true}),
                    )),
                    3 => Some(("skill", json!({"action":"load","name":"runtime-test"}))),
                    _ => None,
                };
                if let Some((name, args)) = call {
                    return Ok(Box::pin(futures::stream::iter(vec![
                        Ok(StreamChunk::BlockEnd {
                            index: 0,
                            block: ContentBlock::ToolCall {
                                id: format!("call-{}", results.len()),
                                name: name.into(),
                                arguments: args.to_string(),
                                incomplete: false,
                            },
                        }),
                        Ok(StreamChunk::Finish {
                            reason: FinishReason::ToolCalls,
                        }),
                    ])));
                }
            }
            Ok(Box::pin(futures::stream::iter(vec![
                Ok(StreamChunk::BlockEnd {
                    index: 0,
                    block: ContentBlock::Text {
                        text: "子代理测试完成".into(),
                    },
                }),
                Ok(StreamChunk::Finish {
                    reason: FinishReason::Stop,
                }),
            ])))
        }
    }
    /// 长跑探针：足够久，保证子代理结束的那一刻它仍在运行（用于验证
    /// "子代理不得遗留后台任务"）。
    const LONG_RUNNING_PROBE: &str = if cfg!(windows) {
        "ping -n 30 127.0.0.1 > NUL"
    } else {
        "sleep 30"
    };

    /// 单步工具调用流：调用一次工具并结束本轮 step。
    fn tool_call_stream(id: &str, name: &str, args: Value) -> ChunkStream {
        Box::pin(futures::stream::iter(vec![
            Ok(StreamChunk::BlockEnd {
                index: 0,
                block: ContentBlock::ToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    arguments: args.to_string(),
                    incomplete: false,
                },
            }),
            Ok(StreamChunk::Finish {
                reason: FinishReason::ToolCalls,
            }),
        ]))
    }

    /// 纯文本回复流（轮次闭合）。
    fn text_stream(text: &str) -> ChunkStream {
        Box::pin(futures::stream::iter(vec![
            Ok(StreamChunk::BlockEnd {
                index: 0,
                block: ContentBlock::Text {
                    text: text.to_string(),
                },
            }),
            Ok(StreamChunk::Finish {
                reason: FinishReason::Stop,
            }),
        ]))
    }

    async fn setup() -> (crate::state::AppState, ToolContext) {
        let home =
            std::env::temp_dir().join(format!("denia-runtime-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let state = crate::state::build_state(&home, false, 3600).await.unwrap();
        state
            .registry
            .register(
                &["runtime-test".into()],
                Arc::new(Mock),
                denia_llm::RetryPolicy::default(),
            )
            .unwrap();
        let session = state.sessions.create(&home, true).unwrap();
        let id = session.id().to_string();
        drop(session);
        let live = state.live.get_or_load(&state.sessions, &id).unwrap();
        live.running.store(true, Ordering::SeqCst); // 父轮次由测试控制，通知先排队。
        let ctx = ToolContext {
            output_store: None,
            session_id: Some(id),
            selection: Some(ModelSelection {
                provider: "runtime-test".into(),
                model: "done".into(),
                reasoning_effort: None,
            }),
            cwd: home,
            cancel: CancellationToken::new(),
            confined: true,
            vision_supported: false,
            emit_event: None,
            file_history: None,
            permission_mode: denia_core::session::PermissionMode::AutoEdit,
            ask: None,
            call_id: None,
            goal_reader: None,
            task_ledger: None,
            read_state: None,
        };
        (state, ctx)
    }
    async fn settle(runtime: &Runtime, id: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while runtime.is_active(id) {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("子代理应在测试时限内结束");
    }

    #[test]
    fn background_waits_keep_legacy_timeout_validation() {
        assert!(validate_wait_timeout(&json!({})).is_ok());
        assert!(validate_wait_timeout(&json!({"timeout_ms":600_000})).is_ok());
        for timeout in [json!(0), json!(-1), json!("30000")] {
            assert!(validate_wait_timeout(&json!({"timeout_ms":timeout})).is_err());
        }
        let pending = background_result(json!({"output":"partial"}), true);
        assert_eq!(pending["status"], "pending");
        assert_eq!(pending["notification"], "automatic");
        assert_eq!(pending["output"], "partial");
        let ready = background_result(json!({"job":{"status":"failed"}}), false);
        assert_eq!(ready["status"], "ready");
        assert_eq!(ready["job"]["status"], "failed");
        assert!(ready.get("next_action").is_none());
    }

    #[tokio::test]
    async fn background_agent_wait_allows_parent_reply_and_automatic_followup() {
        use tower::ServiceExt;

        let (state, ctx) = setup().await;
        let owner = ctx.session_id.as_deref().unwrap().to_string();
        let live = state.live.get(&owner).unwrap();
        live.running.store(false, Ordering::SeqCst);
        let state = Arc::new(state);
        let response = crate::api::router()
            .with_state(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/api/sessions/{owner}/prompt"))
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        json!({"prompt":"后台调查并先回复进度","provider":"runtime-test","model":"background-agent-parent"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let replied = live.session.events().iter().any(|event| {
                    matches!(&event.event, SessionEvent::AssistantMessage { blocks, .. }
                        if blocks.iter().any(|block| matches!(block, ContentBlock::Text { text } if text.contains("我先回复当前进度"))))
                });
                if replied && !live.running.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("等待子代理不得阻止父会话先回复");
        let children = state.runtime.list(&owner);
        let child = children[0]["id"].as_str().unwrap();
        assert!(state.runtime.is_active(child));
        let results: Vec<_> = live
            .session
            .events()
            .into_iter()
            .filter_map(|event| match event.event {
                SessionEvent::ToolResult {
                    content,
                    is_error: false,
                    ..
                } => Some(serde_json::from_str::<Value>(&content).unwrap()),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|result| result["status"] == "pending"));
        assert_eq!(results[1]["messages"], json!([]));
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while state
                .live
                .get(child)
                .unwrap()
                .cancel
                .lock()
                .unwrap()
                .is_none()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        state.runtime.interrupt(&owner, child).await.unwrap();
        settle(&state.runtime, child).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let replied = live.session.events().iter().any(|event| {
                    matches!(&event.event, SessionEvent::AssistantMessage { blocks, .. }
                        if blocks.iter().any(|block| matches!(block, ContentBlock::Text { text } if text.contains("后台结果已自动返回"))))
                });
                if replied && !live.running.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("子代理结束后应自动唤醒父会话回复");
        let delivered = live
            .session
            .events()
            .iter()
            .filter(|event| matches!(&event.event, SessionEvent::AgentDelivery { source, .. } if source == "subagent-settled"))
            .count();
        assert_eq!(delivered, 1);
        let ready = state
            .runtime
            .execute("wait_agent", json!({"target":child}), &ctx)
            .await
            .unwrap();
        assert_eq!(ready["status"], "ready");
        assert_eq!(ready["running"], false);
    }

    #[tokio::test]
    async fn background_job_output_returns_pending_and_resumes_idle_parent() {
        let (state, ctx) = setup().await;
        let owner = ctx.session_id.as_deref().unwrap();
        state
            .runtime
            .human_turn(owner, ctx.selection.as_ref().unwrap());
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 2; Write-Output background-job-result"
        } else {
            "sleep 2; printf background-job-result"
        };
        let started = state
            .runtime
            .execute("job_start", json!({"command":command}), &ctx)
            .await
            .unwrap();
        let job = started["id"].as_str().unwrap();
        let pending = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            state.runtime.execute(
                "job_output",
                json!({"id":job,"wait":true,"timeout_ms":600_000}),
                &ctx,
            ),
        )
        .await
        .expect("读取后台输出不得等待任务结束")
        .unwrap();
        assert_eq!(pending["status"], "pending");
        assert!(pending["job"]["finishedAt"].is_null());
        let live = state.live.get(owner).unwrap();
        live.running.store(false, Ordering::SeqCst);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let events = live.session.events();
                let delivered = events.iter().any(|event| {
                    matches!(&event.event, SessionEvent::AgentDelivery { source, text, .. }
                        if source == "job-completed" && text.contains("background-job-result"))
                });
                let replied = events
                    .iter()
                    .any(|event| matches!(event.event, SessionEvent::AssistantMessage { .. }));
                if delivered && replied && !live.running.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("后台完成结果应自动唤醒父会话");
        let ready = state
            .runtime
            .execute("job_output", json!({"id":job,"wait":true}), &ctx)
            .await
            .unwrap();
        assert_eq!(ready["status"], "ready");
        assert_eq!(ready["job"]["exitCode"], 0);
        assert!(
            ready["output"]
                .as_str()
                .unwrap()
                .contains("background-job-result")
        );
        assert_eq!(
            ready,
            state
                .runtime
                .execute("job_output", json!({"id":job}), &ctx)
                .await
                .unwrap()
        );
        assert_eq!(live.session.events().iter().filter(|event| {
            matches!(&event.event, SessionEvent::AgentDelivery { source, .. } if source == "job-completed")
        }).count(), 1);
        state
            .runtime
            .inner
            .paused
            .lock()
            .unwrap()
            .insert(owner.into());
        let turns = live.session.next_turn_number();
        state
            .runtime
            .enqueue(
                owner,
                "paused-completion".into(),
                "后台结果已保存".into(),
                "job-completed".into(),
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(live.session.next_turn_number(), turns);
        assert!(!live.session.events().iter().any(|event| {
            matches!(&event.event, SessionEvent::AgentDelivery { id, .. } if id == "paused-completion")
        }));
    }

    #[tokio::test]
    async fn spawn_resume_lineage_and_exactly_once_delivery() {
        let (state, ctx) = setup().await;
        let owner = ctx.session_id.as_ref().unwrap();
        let result = state
            .runtime
            .execute(
                "spawn_agent",
                json!({"prompt":"检查实现","description":"测试子代理"}),
                &ctx,
            )
            .await
            .unwrap();
        let id = result["childId"].as_str().unwrap();
        settle(&state.runtime, id).await;
        let live = state.live.get(id).unwrap();
        assert_eq!(
            live.session.header().parent_session.as_deref(),
            Some(owner.as_str())
        );
        assert!(live.session.header().subagent.is_some());
        assert!(
            state
                .sessions
                .list()
                .unwrap()
                .iter()
                .find(|s| s.id == id)
                .unwrap()
                .excerpt
                .is_some()
        );
        assert!(
            state
                .runtime
                .execute(
                    "send_message",
                    json!({"target":"foreign","message":"不允许"}),
                    &ctx
                )
                .await
                .is_err()
        );
        let first = live.session.next_turn_number();
        state
            .runtime
            .execute(
                "send_message",
                json!({"target":id,"message":"继续检查"}),
                &ctx,
            )
            .await
            .unwrap();
        settle(&state.runtime, id).await;
        assert!(live.session.next_turn_number() > first);
        let events = live.session.events();
        let delivered = events
            .iter()
            .filter(|e| matches!(e.event, SessionEvent::AgentDelivery { .. }))
            .count();
        assert_eq!(delivered, 2);
        state.runtime.drain(id).await.unwrap();
        assert_eq!(
            live.session
                .events()
                .iter()
                .filter(|e| matches!(e.event, SessionEvent::AgentDelivery { .. }))
                .count(),
            2
        );
        // 冷恢复重建目录和收件箱，从持久化领取事件排除已投递项。
        let restored = Runtime::new(
            &state.home,
            state.sessions.clone(),
            state.live.clone(),
            state.settings.clone(),
            state.events.clone(),
            state.registry.clone(),
            state.workspaces.clone(),
            state.subagent_profiles.clone(),
        )
        .unwrap();
        assert_eq!(restored.list(owner).as_array().unwrap().len(), 1);
        restored.drain(id).await.unwrap();
        assert_eq!(
            live.session
                .events()
                .iter()
                .filter(|e| matches!(e.event, SessionEvent::AgentDelivery { .. }))
                .count(),
            2
        );
    }
    /// 子代理的能力来自**派遣时解析的定义**：默认走 develop（有写能力），
    /// explore 只读；硬禁项（派遣/会话主控/宿主配置）任何定义都拿不到；
    /// 旧参数路径按历史只读上限保守解释，绝不自动变全工具。
    #[tokio::test]
    async fn subagents_follow_profile_capability_and_hard_denied_rules() {
        let (state, ctx) = setup().await;
        // ① 省略 profile_id/inline = 默认 develop 定义。
        let result = state
            .runtime
            .execute(
                "spawn_agent",
                json!({"prompt":"实现功能","description":"开发子代理"}),
                &ctx,
            )
            .await
            .unwrap();
        let id = result["childId"].as_str().unwrap();
        assert_eq!(result["profile"]["qualifiedId"], "builtin:develop");
        let live = state.live.get(id).unwrap();
        let descriptor = live.session.header().subagent.clone().unwrap();
        assert_eq!(
            descriptor.snapshot_version,
            denia_core::subagent::SUBAGENT_SNAPSHOT_VERSION
        );
        assert!(
            !descriptor.delegation_allowed,
            "delegationAllowed 必须恒为 false（审计字段）"
        );
        assert_eq!(
            descriptor.instruction_scope.as_deref(),
            Some("project-only")
        );
        assert_eq!(
            descriptor.created_permission_mode.as_deref(),
            Some("auto-edit")
        );
        let tools = descriptor.effective_tools.clone().unwrap();
        for expected in ["write_file", "edit", "bash", "todo_write", "read_file"] {
            assert!(
                tools.contains(&expected.to_string()),
                "develop 应含 {expected}"
            );
        }
        for forbidden in [
            "spawn_agent",
            "fork_agent",
            "list_agents",
            "wait_agent",
            "interrupt_agent",
            "exit_plan",
            "get_goal",
            "update_goal",
            "create_preset",
        ] {
            assert!(
                !tools.contains(&forbidden.to_string()),
                "{forbidden} 是硬禁项，任何定义都不得授予"
            );
        }
        // 角色正文不放 header，只存引用与 hash。
        assert_eq!(
            descriptor.instructions_ref.as_deref(),
            Some("subagent.json")
        );
        assert!(descriptor.instructions_hash.is_some());
        assert!(descriptor.persona.is_none() && descriptor.allowed_tools.is_none());

        // ② explore 定义：只读上限，且不授予 browser/bash/写工具。
        let explore = state
            .runtime
            .execute(
                "spawn_agent",
                json!({"prompt":"调查","profile_id":"builtin:explore"}),
                &ctx,
            )
            .await
            .unwrap();
        let explore_live = state
            .live
            .get(explore["childId"].as_str().unwrap())
            .unwrap();
        let explore_desc = explore_live.session.header().subagent.clone().unwrap();
        assert_eq!(
            explore_desc.permission_ceiling,
            denia_core::subagent::PermissionCeiling::ReadOnly
        );
        let explore_tools = explore_desc.effective_tools.clone().unwrap();
        for forbidden in ["bash", "write_file", "edit", "browser", "ask", "job_start"] {
            assert!(
                !explore_tools.contains(&forbidden.to_string()),
                "explore 不得含 {forbidden}"
            );
        }
        assert!(explore_tools.contains(&"read_file".to_string()));
        assert!(explore_tools.contains(&"send_message".to_string()));

        // ③ 显式请求父未授予/硬禁的工具 → 逐项报错，不静默少给。
        let denied = state
            .runtime
            .execute(
                "spawn_agent",
                json!({"prompt":"越权","inline":{
                    "name":"越权","description":"尝试拿硬禁工具",
                    "tools":{"mode":"allowlist","names":["read_file","spawn_agent"]}
                }}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(denied.contains("subagent/tool-hard-denied"), "{denied}");

        // ④ inline 临时定义：不落盘、工具严格匹配。
        let inline = state
            .runtime
            .execute(
                "spawn_agent",
                json!({"prompt":"查缓存","inline":{
                    "name":"缓存调查员",
                    "description":"专注缓存失效链路",
                    "instructions":"不修改文件",
                    "tools":{"mode":"allowlist","names":["read_file","grep"]},
                    "permissionCeiling":"read-only"
                }}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(
            inline["profile"]["qualifiedId"].as_str().unwrap(),
            "inline:缓存调查员"
        );
        assert_eq!(inline["profile"]["toolCount"], 2);
        let inline_live = state.live.get(inline["childId"].as_str().unwrap()).unwrap();
        let inline_desc = inline_live.session.header().subagent.clone().unwrap();
        assert_eq!(
            inline_desc.effective_tools.clone().unwrap(),
            vec!["grep".to_string(), "read_file".to_string()]
        );

        // ⑤ 旧参数路径：显式列表最多与历史只读上限相交，绝不自动变全工具。
        let legacy = state
            .runtime
            .execute(
                "spawn_agent",
                json!({"prompt":"旧参数","allowed_tools":["read_file","write_file","bash"]}),
                &ctx,
            )
            .await
            .unwrap();
        let legacy_live = state.live.get(legacy["childId"].as_str().unwrap()).unwrap();
        let legacy_desc = legacy_live.session.header().subagent.clone().unwrap();
        let legacy_tools = legacy_desc.effective_tools.clone().unwrap();
        assert!(legacy_tools.contains(&"read_file".to_string()));
        assert!(
            !legacy_tools.contains(&"write_file".to_string())
                && !legacy_tools.contains(&"bash".to_string()),
            "旧 allowed_tools 不得放开写与命令:{legacy_tools:?}"
        );
        assert_eq!(legacy_desc.snapshot_version, 0 + 1);
        // 旧 persona 映射为角色补充（走快照文件）。
        assert!(legacy_desc.instructions_ref.is_some());

        // ⑥ 未知工具名明确失败（不静默忽略）。
        assert!(
            state
                .runtime
                .execute(
                    "spawn_agent",
                    json!({"prompt":"未知","allowed_tools":["nope"]}),
                    &ctx
                )
                .await
                .is_err()
        );
    }

    /// 子代理禁止派遣子代理：模型入口、直接 Runtime 入口、恢复路径一律拒绝，
    /// 且不产生新会话、不占槽、不发模型请求。
    #[tokio::test]
    async fn subagents_cannot_delegate_from_any_entry() {
        let (state, ctx) = setup().await;
        let child = state
            .runtime
            .execute(
                "spawn_agent",
                json!({"prompt":"调查","profile_id":"builtin:explore"}),
                &ctx,
            )
            .await
            .unwrap();
        let child_id = child["childId"].as_str().unwrap().to_string();
        settle(&state.runtime, &child_id).await;
        let before = state.sessions.list().unwrap().len();
        let mut child_ctx = ctx.clone();
        child_ctx.session_id = Some(child_id.clone());
        let error = state
            .runtime
            .execute("spawn_agent", json!({"prompt":"再派一个"}), &child_ctx)
            .await
            .unwrap_err();
        assert!(
            error.contains("subagent/delegation-forbidden"),
            "子代理派遣必须返回稳定 code:{error}"
        );
        // fork 同样拒绝。
        assert!(
            state
                .runtime
                .execute("fork_agent", json!({"prompt":"fork"}), &child_ctx)
                .await
                .is_err()
        );
        // 旧参数里伪造 max_depth 也不放宽：直接报已移除。
        let depth = state
            .runtime
            .execute("spawn_agent", json!({"prompt":"x","max_depth":3}), &ctx)
            .await
            .unwrap_err();
        assert!(depth.contains("max_depth"), "{depth}");
        assert_eq!(
            state.sessions.list().unwrap().len(),
            before,
            "拒绝路径不得创建会话"
        );
        assert_eq!(state.runtime.inner.admission.reserved_count(), 0);
    }

    /// 普通用户分支有 parent_session 但没有 subagent 描述符：仍可正常派遣，
    /// 禁止规则不能以"有父会话"代替"是子代理"。
    #[tokio::test]
    async fn user_fork_with_parent_session_can_still_delegate() {
        let (state, ctx) = setup().await;
        let parent_id = ctx.session_id.clone().unwrap();
        // 普通用户分支：create_forked 写 parent_session，不写 subagent。
        let fork = state
            .sessions
            .create_forked(&[], 0, ctx.cwd.as_path(), true, &parent_id)
            .unwrap();
        assert!(fork.header().subagent.is_none());
        let mut fork_ctx = ctx.clone();
        fork_ctx.session_id = Some(fork.id().to_string());
        let result = state
            .runtime
            .execute("spawn_agent", json!({"prompt":"正常派遣"}), &fork_ctx)
            .await;
        assert!(result.is_ok(), "普通分支必须仍可派遣:{result:?}");
    }

    /// 并发上限走新的 subagent-policy 命名空间；创建与唤醒共用同一份槽位账。
    /// 子代理未在期限内完成 `want` 个工具结果就失败（防挂死）。
    async fn wait_for_tool_results(state: &crate::state::AppState, child: &str, want: usize) {
        for _ in 0..1000 {
            let done = state
                .live
                .get(child)
                .map(|live| {
                    let count = live
                        .session
                        .events()
                        .iter()
                        .filter(|event| matches!(event.event, SessionEvent::ToolResult { .. }))
                        .count();
                    count >= want && !live.running.load(Ordering::SeqCst)
                })
                .unwrap_or(false);
            if done {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("子代理未在期限内产出 {want} 个工具结果");
    }

    /// 等某个 child 真正开始运行（取消令牌已挂上）。
    async fn wait_for_running(state: &crate::state::AppState, id: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while state
                .live
                .get(id)
                .is_none_or(|live| live.cancel.lock().unwrap().is_none())
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    /// 等父会话的第一个子代理出现并返回其 id。
    async fn wait_for_child(state: &crate::state::AppState, parent: &str) -> String {
        for _ in 0..500 {
            if let Some(entry) = state
                .runtime
                .list(parent)
                .as_array()
                .and_then(|rows| rows.first())
                .cloned()
            {
                return entry["id"].as_str().unwrap().to_string();
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("子代理未创建");
    }

    /// T02 / T01：子代理的执行能力严格来自**派遣时冻结的授权**。
    ///
    /// - develop 子代理真实落盘并真的跑了命令（断言文件与命令副作用，不是
    ///   只断言 schema 字符串）；
    /// - explore 子代理（只读上限）即使模型幻觉调用 write_file/bash，schema 里
    ///   没有、执行被拒、文件无任何变化。
    #[tokio::test]
    async fn child_execution_side_effects_follow_the_frozen_grant() {
        let (state, _) = setup().await;
        let state = Arc::new(state);
        let workspace = state.home.join("devfixture");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("existing.txt"), "旧内容\n").unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = crate::api::router().with_state(state.clone());
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let client = reqwest::Client::new();
        let new_session = |cwd: std::path::PathBuf| {
            let client = client.clone();
            let base = base.clone();
            async move {
                let created: Value = client
                    .post(format!("{base}/api/sessions"))
                    .json(&json!({"cwd": cwd}))
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                created["session"]["id"].as_str().unwrap().to_string()
            }
        };

        // —— develop：真写、真跑 ——
        let dev_parent = new_session(workspace.clone()).await;
        // 完全访问档：执行型子代理仍走正常权限引擎，这里避免测试卡在审批上。
        assert!(
            client
                .put(format!("{base}/api/sessions/{dev_parent}/permission"))
                .json(&json!({"mode": "full"}))
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
        assert!(
            client
                .post(format!("{base}/api/sessions/{dev_parent}/prompt"))
                .json(&json!({"prompt":"委派开发任务","provider":"runtime-test","model":"exec-parent"}))
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
        let dev_child = wait_for_child(&state, &dev_parent).await;
        wait_for_tool_results(&state, &dev_child, 4).await;
        assert_eq!(
            std::fs::read_to_string(workspace.join("child-wrote.txt")).unwrap(),
            "子代理写入\n",
            "develop 子代理必须真实落盘"
        );
        assert!(
            workspace.join("child-ran.txt").is_file(),
            "develop 子代理必须真的执行了命令"
        );
        // 第 4 条调用是 bash + run_in_background：子代理没有被授予 job_start，
        // 执行器必须拒绝，并且不得留下任何后台任务。
        let background_result = state
            .live
            .get(&dev_child)
            .unwrap()
            .session
            .events()
            .iter()
            .filter_map(|event| match &event.event {
                SessionEvent::ToolResult {
                    content, is_error, ..
                } => Some((content.clone(), *is_error)),
                _ => None,
            })
            .nth(3)
            .expect("第 4 条工具结果存在");
        assert!(
            background_result.1 && background_result.0.contains("job_start"),
            "未授予 job_start 时 run_in_background 必须被拒：{background_result:?}"
        );
        assert!(
            state.runtime.jobs().list(&dev_child).is_empty(),
            "被拒的后台命令不得在 jobs 注册表里留下任务"
        );
        // 子代理不能派遣子代理：它的工具面里没有派遣入口。
        let dev_tools = state
            .live
            .get(&dev_child)
            .unwrap()
            .session
            .header()
            .subagent
            .clone()
            .unwrap()
            .effective_tools
            .unwrap();
        assert!(!dev_tools.iter().any(|name| name == "spawn_agent"));

        // —— explore：只读上限，幻觉调用也被拒且文件不变 ——
        let explore_parent = new_session(workspace.clone()).await;
        assert!(
            client
                .put(format!("{base}/api/sessions/{explore_parent}/permission"))
                .json(&json!({"mode": "full"}))
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
        assert!(
            client
                .post(format!("{base}/api/sessions/{explore_parent}/prompt"))
                .json(&json!({"prompt":"委派只读探索","provider":"runtime-test","model":"explore-parent"}))
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
        let explore_child = wait_for_child(&state, &explore_parent).await;
        wait_for_tool_results(&state, &explore_child, 2).await;
        let outcomes: Vec<(String, bool)> = state
            .live
            .get(&explore_child)
            .unwrap()
            .session
            .events()
            .iter()
            .filter_map(|event| match &event.event {
                SessionEvent::ToolResult {
                    content, is_error, ..
                } => Some((content.clone(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(outcomes.len(), 2, "幻觉调用必须各有一条结果为证");
        for (content, is_error) in &outcomes {
            assert!(*is_error, "explore 的写与命令必须被拒，实际结果：{content}");
        }
        assert!(
            outcomes
                .iter()
                .any(|(content, _)| content.contains("subagent/tool-not-granted")
                    || content.contains("硬禁用")),
            "拒绝理由必须可判定：{outcomes:?}"
        );
        assert!(
            !workspace.join("explore-wrote.txt").exists(),
            "只读子代理不得留下任何文件变化"
        );
    }

    /// §10.4/§10.5：子代理正常结束时清理它自己的后台任务并在结果里注明；
    /// 完成通知必须带 profile、终态、本次用量与结果引用、并明示截断。
    #[tokio::test]
    async fn settled_child_cancels_leftover_jobs_and_reports_a_rich_notice() {
        let (state, _) = setup().await;
        let state = Arc::new(state);
        let workspace = state.home.join("jobsfixture");
        std::fs::create_dir_all(&workspace).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = crate::api::router().with_state(state.clone());
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let client = reqwest::Client::new();
        let created: Value = client
            .post(format!("{base}/api/sessions"))
            .json(&json!({"cwd": workspace}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let parent = created["session"]["id"].as_str().unwrap().to_string();
        assert!(
            client
                .put(format!("{base}/api/sessions/{parent}/permission"))
                .json(&json!({"mode": "full"}))
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
        assert!(
            client
                .post(format!("{base}/api/sessions/{parent}/prompt"))
                .json(&json!({"prompt":"委派带后台任务的子任务","provider":"runtime-test","model":"jobs-parent"}))
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
        let child = wait_for_child(&state, &parent).await;
        // 等子代理结束，并且等它启动的后台任务被清理 + 通知送达父会话。
        let mut notice = String::new();
        for _ in 0..1500 {
            let delivered = state.live.get(&parent).map(|live| {
                live.session
                    .events()
                    .iter()
                    .filter_map(|event| match &event.event {
                        SessionEvent::AgentInbox { text, source, .. }
                        | SessionEvent::AgentDelivery { text, source, .. }
                            if source == "subagent-settled" && text.contains(&child) =>
                        {
                            Some(text.clone())
                        }
                        _ => None,
                    })
                    .next_back()
            });
            if let Some(Some(text)) = delivered {
                notice = text;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(!notice.is_empty(), "子代理结束通知必须送达父会话");
        assert!(notice.contains("子代理执行结束"), "{notice}");        assert!(
            notice.contains("inline:后台任务员"),
            "通知必须带 profile：{notice}"
        );
        assert!(
            notice.contains("结果：已完成"),
            "通知必须带明确终态：{notice}"
        );
        assert!(
            notice.contains("本次子代理用量："),
            "通知必须带子代理自身用量：{notice}"
        );
        assert!(
            notice.contains("结果引用："),
            "通知必须带结果引用：{notice}"
        );
        assert!(
            notice.contains("[已取消的子代理后台工作]"),
            "遗留的后台任务必须被取消并注明：{notice}"
        );
        // 清理与结果投递已解耦:通知到达不等于清理已完成——但清理必须有界
        // 完成,否则"只释放槽位、进程继续写文件"就是最坏结果。
        let mut cleaned = false;
        for _ in 0..300 {
            if state
                .runtime
                .jobs()
                .list(&child)
                .iter()
                .all(|job| job.finished_at.is_some())
            {
                cleaned = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(cleaned, "子代理结束后不得留下未完成的后台任务");
    }

    /// §10.1 崩溃窗口：派遣的每一步失败都必须回滚本次新增的会话、槽位与目录
    /// 条目；而"已入队"之后失败**不**回滚——child 的会话、首条 inbox 与完成
    /// 通知是持久事实，工具结果丢了也不能删掉正在运行的 child。
    #[tokio::test]
    async fn delegate_rolls_back_every_pre_enqueue_window_and_keeps_post_enqueue_state() {
        for point in ["create", "snapshot", "pre-enqueue", "enqueue"] {
            let (state, ctx) = setup().await;
            let parent = ctx.session_id.clone().unwrap();
            state.runtime.set_test_fault(point);
            let before = state.sessions.list().unwrap().len();
            let result = state
                .runtime
                .execute("spawn_agent", json!({"prompt":"注入故障"}), &ctx)
                .await;
            assert!(result.is_err(), "{point}：派遣必须失败");
            assert_eq!(
                state.sessions.list().unwrap().len(),
                before,
                "{point}：不得留下新会话"
            );
            assert_eq!(
                state.runtime.inner.admission.reserved_count(),
                0,
                "{point}：槽位必须释放"
            );
            assert!(
                state.runtime.list(&parent).as_array().unwrap().is_empty(),
                "{point}：目录里不得残留 child"
            );
        }

        // 返回 pending 之前失败：不回滚，且完成通知仍能送达（结果不丢）。
        let (state, ctx) = setup().await;
        let parent = ctx.session_id.clone().unwrap();
        state.runtime.set_test_fault("post-enqueue");
        let error = state
            .runtime
            .execute("spawn_agent", json!({"prompt":"注入故障"}), &ctx)
            .await
            .unwrap_err();
        assert!(error.contains("返回 pending 前"), "{error}");
        let child = wait_for_child(&state, &parent).await;
        assert!(
            state.sessions.exists(&child),
            "入队后失败不得删除已存在的 child 会话"
        );
        // child 的首条委派消息是持久事实。
        assert!(
            state
                .live
                .get(&child)
                .unwrap()
                .session
                .events()
                .iter()
                .any(|event| matches!(&event.event, SessionEvent::AgentInbox { source, .. } if source.starts_with("agent:"))),
            "委派消息必须已持久化"
        );
        for _ in 0..1000 {
            let delivered = state.live.get(&parent).is_some_and(|live| {
                live.session.events().iter().any(|event| {
                    matches!(&event.event, SessionEvent::AgentInbox { source, .. } if source == "subagent-settled")
                })
            });
            if delivered {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            state.live.get(&parent).unwrap().session.events().iter().any(
                |event| matches!(&event.event, SessionEvent::AgentInbox { source, .. } if source == "subagent-settled")
            ),
            "工具结果丢失不能导致完成通知丢失"
        );
    }

    /// 唤醒不得因"目标正在跑"而丢失。
    ///
    /// 这条锁死了主代理"卡在 wait_agent 等子代理、子代理此刻结束"的交错：
    /// 子代理 enqueue 触发 wake 时主代理 `running == true`，resume_pending 的
    /// CAS 失败。以前这里直接 return，唤醒意图被静默丢弃，且主代理那轮
    /// on_idle 的 inbox 扫描发生在投递之前的概率很大 —— 结果就是"子代理已经
    /// 完成，主代理一直不醒，必须用户再发一条消息"。
    #[tokio::test]
    async fn wake_is_deferred_instead_of_dropped_when_target_is_running() {
        let (state, ctx) = setup().await;
        let parent = ctx.session_id.clone().unwrap();
        let live = state.live.get(&parent).unwrap();

        // 主代理正在跑（模拟 wait_agent 那一轮）：CAS 失败才走挂起分支。
        live.running.store(true, Ordering::SeqCst);
        assert!(
            live.running.load(Ordering::SeqCst),
            "测试前提：会话处于运行中"
        );

        let runtime = state.runtime.clone();
        // resume_pending 从这里取模型上下文；不登记的话它会在取 selection
        // 时提前返回，根本走不到 running CAS 那条分支，测试就成了假阳性。
        runtime.inner.selections.lock().unwrap().insert(
            parent.clone(),
            ModelSelection {
                provider: "runtime-test".into(),
                model: "done".into(),
                reasoning_effort: None,
            },
        );
        // 投递一条消息：wake 会因 running 失败而挂起。
        runtime
            .enqueue(
                &parent,
                "m1".into(),
                "结果".into(),
                "subagent-settled".into(),
            )
            .await
            .unwrap();

        // wake 是 spawn 出去的异步任务，等它真正跑到 CAS 分支。
        let mut deferred = false;
        for _ in 0..200 {
            if runtime
                .inner
                .deferred_wakes
                .lock()
                .unwrap()
                .contains(&parent)
            {
                deferred = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(deferred, "目标正在跑时必须登记挂起唤醒，而不是静默丢弃");

        // 主代理这一轮结束 → on_idle 消费挂起项。
        // 消费是异步的(wake 在 spawn 的任务里真正推进):等它落地,而不是
        // 要求同步清表。
        live.running.store(false, Ordering::SeqCst);
        runtime.on_idle(&parent);
        let mut consumed = false;
        for _ in 0..200 {
            if !runtime
                .inner
                .deferred_wakes
                .lock()
                .unwrap()
                .contains(&parent)
            {
                consumed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(consumed, "会话空闲后必须消费自己的挂起唤醒");
    }

    /// 挂起唤醒必须在目标空闲后**真正跑起来**，而不只是被清掉标记。
    ///
    /// 只验标记会被"清表但没 wake"这种假修复骗过去，所以这里断言投递事件
    /// 真的进入了模型可见历史（AgentDelivery）。
    #[tokio::test]
    async fn deferred_wake_actually_delivers_pending_message_after_target_goes_idle() {
        let (state, ctx) = setup().await;
        let parent = ctx.session_id.clone().unwrap();
        let live = state.live.get(&parent).unwrap();
        live.running.store(true, Ordering::SeqCst);

        let runtime = state.runtime.clone();
        runtime.inner.selections.lock().unwrap().insert(
            parent.clone(),
            ModelSelection {
                provider: "runtime-test".into(),
                model: "done".into(),
                reasoning_effort: None,
            },
        );
        runtime
            .enqueue(
                &parent,
                "m1".into(),
                "子代理已完成".into(),
                "subagent-settled".into(),
            )
            .await
            .unwrap();

        live.running.store(false, Ordering::SeqCst);
        runtime.on_idle(&parent);

        // wake 是 spawn 出去的异步任务，给它落地时间。
        let mut delivered = false;
        for _ in 0..200 {
            if state.live.get(&parent).is_some_and(|live| {
                live.session
                    .events()
                    .iter()
                    .any(|event| matches!(event.event, SessionEvent::AgentDelivery { .. }))
            }) {
                delivered = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            delivered,
            "挂起唤醒必须在目标空闲后真正把待收消息投进模型历史，不能只是清掉标记"
        );
    }

    /// §8.2：子代理目录有**独立**字节预算，超限按 UTF-8 边界截断并明示；
    /// 未列出的定义仍可用 profile_id 指定（截断不等于禁用）。
    #[tokio::test]
    async fn subagent_catalog_respects_its_own_byte_budget() {
        use denia_tools::capabilities::AgentRuntime;
        let (state, ctx) = setup().await;
        let parent = ctx.session_id.clone().unwrap();
        let mut profile = denia_core::subagent::builtin_subagent_profiles()[0].clone();
        profile.id = "budget-probe".to_string();
        profile.name = "预算探针".to_string();
        profile.description = "用途".repeat(300);
        state
            .subagent_profiles
            .create(
                denia_core::subagent::ProfileWriteScope::User,
                None,
                profile,
                "角色".repeat(50),
            )
            .unwrap();
        state
            .settings
            .update("runtime", json!({"subagentCatalogMaxBytes": 1024}), None)
            .unwrap();

        let text = state
            .runtime
            .subagent_catalog(&parent)
            .await
            .unwrap()
            .expect("有可用定义时必须注入目录");
        assert!(
            text.contains("预算截断"),
            "超限必须明示截断而不是静默丢弃：{}",
            &text[..text.len().min(200)]
        );
        assert!(
            text.len() <= 1024 + 200,
            "截断后长度应贴近预算（含一段说明）：{}",
            text.len()
        );
        // 未列出的定义仍可指定：目录截断不影响派遣解析。
        assert!(
            state
                .subagent_profiles
                .resolve(None, "user:budget-probe")
                .is_ok()
        );

        // 提高预算后目录恢复完整（不是永久截断）。
        state
            .settings
            .update(
                "runtime",
                json!({"subagentCatalogMaxBytes": 1_048_576}),
                None,
            )
            .unwrap();
        let full = state
            .runtime
            .subagent_catalog(&parent)
            .await
            .unwrap()
            .unwrap();
        assert!(!full.contains("预算截断"));
        assert!(full.contains("user:budget-probe"));
    }

    /// §6.4/L04：停止 child 时只关闭**它自己**名下的浏览器 tab。
    #[tokio::test]
    async fn stopping_a_child_closes_only_its_own_browser_tabs() {
        use std::sync::Mutex as StdMutex;

        #[derive(Default)]
        struct RecordingHub {
            closed: StdMutex<Vec<String>>,
        }
        #[async_trait]
        impl denia_tools::BrowserExecute for RecordingHub {
            async fn execute(
                &self,
                _command: denia_browser::BrowserCommand,
            ) -> denia_browser::CommandOutcome {
                denia_browser::CommandOutcome::ok_value(Value::Null, 0)
            }
            async fn close_owned(&self, owner: &str) -> usize {
                self.closed.lock().unwrap().push(owner.to_string());
                1
            }
        }

        let (state, mut ctx) = setup().await;
        ctx.selection.as_mut().unwrap().model = "hold".into();
        let hub = Arc::new(RecordingHub::default());
        state.runtime.attach_browser(hub.clone());
        let parent = ctx.session_id.clone().unwrap();
        let child = state
            .runtime
            .execute("spawn_agent", json!({"prompt":"占用浏览器"}), &ctx)
            .await
            .unwrap()["childId"]
            .as_str()
            .unwrap()
            .to_string();
        wait_for_running(&state, &child).await;
        state.runtime.interrupt(&parent, &child).await.unwrap();
        assert_eq!(
            hub.closed.lock().unwrap().as_slice(),
            &[child.clone()],
            "只允许关闭该 child 名下的 tab"
        );
        assert!(
            !hub.closed.lock().unwrap().contains(&parent),
            "父代理的 tab 不得被顺手清掉"
        );
        settle(&state.runtime, &child).await;
    }

    /// §9.4：旧子代理首次继续前建立模型历史投影（在线与冷恢复一致），
    /// 不再只能"查看后重新派遣"。
    #[tokio::test]
    async fn legacy_child_is_projected_and_can_continue() {
        use denia_core::config::ModelSelection;
        use denia_core::session::{SessionEvent, SubagentDescriptor};

        let (state, ctx) = setup().await;
        let parent = ctx.session_id.clone().unwrap();
        let selection = ModelSelection {
            provider: "runtime-test".into(),
            model: "legacy-child".into(),
            reasoning_effort: None,
        };
        let child = state
            .sessions
            .create_subagent(
                &ctx.cwd,
                true,
                &parent,
                SubagentDescriptor::legacy("旧子代理", 1, "default", selection.clone(), None, None),
            )
            .unwrap();
        let child_id = child.id().to_string();
        // 旧日志：父系统提示 + 混入全局规则的自动注入 + 任务投递 + 一轮对话。
        child.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
        child
            .append(SessionEvent::SystemPrompt {
                turn: 1,
                step: 1,
                text: "父的系统提示".into(),
            })
            .unwrap();
        child
            .append(SessionEvent::UserMessage {
                text: "全局规则哨兵 GLOBAL-SENTINEL".into(),
                injected: true,
                images: Vec::new(),
                channel: Some("workspace-instructions".into()),
            })
            .unwrap();
        child
            .append(SessionEvent::AgentDelivery {
                id: "m1".into(),
                text: "[父代理 委派任务]\n调查登录流程".into(),
                source: format!("agent:{parent}"),
            })
            .unwrap();
        child
            .append(SessionEvent::AssistantMessage {
                turn: 1,
                step: 1,
                blocks: vec![denia_core::stream::ContentBlock::Text {
                    text: "先看登录入口".into(),
                }],
                usage: None,
                interrupted: false,
                source_event_seqs: Vec::new(),
                first_token_time: None,
            })
            .unwrap();
        child
            .append(SessionEvent::TurnEnd {
                turn: 1,
                reason: denia_core::session::TurnEndReason::Completed,
            })
            .unwrap();
        child.flush().unwrap();
        // 目录与父子关系：与真实派遣一致。
        state.runtime.inner.children.lock().unwrap().insert(
            child_id.clone(),
            Child {
                id: child_id.clone(),
                parent_id: parent.clone(),
                descriptor: child.header().subagent.clone().unwrap(),
            },
        );

        state.runtime.resume_pending(&child_id).await.unwrap();

        // 投影已建立并落盘；父代理不再收到"无法继续"的诊断。
        let live = state.live.get(&child_id).expect("child 已加载");
        assert!(
            live.session.history_projection().is_some(),
            "首次继续必须建立历史投影"
        );
        let projection_path = state
            .sessions
            .root()
            .join(&child_id)
            .join(denia_session::HISTORY_PROJECTION_FILE);
        assert!(projection_path.is_file(), "投影必须落盘");
        assert!(
            !state
                .live
                .get(&parent)
                .unwrap()
                .session
                .events()
                .iter()
                .any(|event| matches!(&event.event, SessionEvent::AgentInbox { source, .. } if source == "subagent-legacy")),
            "不再走'拒绝续跑'分支"
        );
        let model_view_has_sentinel = live.session.with_model_events(|events| {
            events.iter().any(|item| {
                matches!(
                    &item.event,
                    SessionEvent::UserMessage { text, .. } if text.contains("GLOBAL-SENTINEL")
                )
            })
        });
        assert!(!model_view_has_sentinel, "投影后旧注入退出模型历史");
        let migration = state.runtime.list(&parent);
        let entry = migration
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == child_id.as_str())
            .cloned()
            .unwrap();
        assert_eq!(entry["migration"]["needsRedispatch"], false);
        assert_eq!(entry["migration"]["projection"], "applied");

        // 真的继续一轮：模型请求里不能再出现旧全局规则。
        state
            .runtime
            .enqueue(
                &child_id,
                "continue-1".into(),
                "继续调查".into(),
                format!("agent:{parent}"),
            )
            .await
            .unwrap();
        for _ in 0..500 {
            let done = state.live.get(&child_id).is_some_and(|live| {
                live.session.events().iter().any(|event| {
                    matches!(&event.event, SessionEvent::AssistantMessage { blocks, .. }
                        if blocks.iter().any(|block| matches!(block, denia_core::stream::ContentBlock::Text { text } if text.contains("已按投影继续"))))
                })
            });
            if done {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let continued = state.live.get(&child_id).unwrap();
        assert!(
            continued.session.events().iter().any(|event| matches!(
                &event.event,
                SessionEvent::AssistantMessage { blocks, .. }
                    if blocks.iter().any(|block| matches!(block, denia_core::stream::ContentBlock::Text { text } if text.contains("已按投影继续")))
            )),
            "旧子代理必须能真的继续运行"
        );
    }

    /// T11：child 可以给**直接父代理**发消息，但发给兄弟或无关会话必须被拒。
    #[tokio::test]
    async fn child_messages_parent_but_not_siblings_or_strangers() {
        use denia_tools::capabilities::AgentRuntime;
        let (state, mut ctx) = setup().await;
        ctx.selection.as_mut().unwrap().model = "hold".into();
        let parent = ctx.session_id.clone().unwrap();
        let first = state
            .runtime
            .execute("spawn_agent", json!({"prompt":"第一个"}), &ctx)
            .await
            .unwrap()["childId"]
            .as_str()
            .unwrap()
            .to_string();
        wait_for_running(&state, &first).await;
        let second = state
            .runtime
            .execute("spawn_agent", json!({"prompt":"第二个"}), &ctx)
            .await
            .unwrap()["childId"]
            .as_str()
            .unwrap()
            .to_string();
        wait_for_running(&state, &second).await;

        // 子 → 直接父：允许。
        let mut child_ctx = ctx.clone();
        child_ctx.session_id = Some(first.clone());
        state
            .runtime
            .execute(
                "send_message",
                json!({"target": parent, "message": "汇报进度"}),
                &child_ctx,
            )
            .await
            .unwrap();
        // 子 → 兄弟：拒绝（只有直接父/子关系可通信）。
        let sibling = state
            .runtime
            .execute(
                "send_message",
                json!({"target": second, "message": "越级"}),
                &child_ctx,
            )
            .await;
        assert!(sibling.is_err(), "兄弟之间不得直接通信：{sibling:?}");
        // 子 → 无关会话：拒绝。
        let stranger = state
            .sessions
            .create(&ctx.cwd, true)
            .unwrap()
            .id()
            .to_string();
        let unrelated = state
            .runtime
            .execute(
                "send_message",
                json!({"target": stranger, "message": "无关"}),
                &child_ctx,
            )
            .await;
        assert!(unrelated.is_err(), "无关会话不得被子代理发消息");
        state.runtime.interrupt(&parent, &first).await.unwrap();
        state.runtime.interrupt(&parent, &second).await.unwrap();
        settle(&state.runtime, &first).await;
        settle(&state.runtime, &second).await;
    }

    /// T03：verify 轨道——授权是"只读+命令"（无写工具），角色正文要求如实
    /// 报告失败证据；子代理不能靠伪装成功收尾。
    #[tokio::test]
    async fn verify_track_grants_commands_without_write_tools() {
        use denia_tools::capabilities::AgentRuntime;
        let (state, mut ctx) = setup().await;
        ctx.selection.as_mut().unwrap().model = "hold".into();
        let parent = ctx.session_id.clone().unwrap();
        let child = state
            .runtime
            .execute(
                "spawn_agent",
                json!({"prompt":"运行测试并如实报告失败","profile_id":"builtin:verify"}),
                &ctx,
            )
            .await
            .unwrap()["childId"]
            .as_str()
            .unwrap()
            .to_string();
        wait_for_running(&state, &child).await;
        let prompt = state
            .runtime
            .subagent_prompt(&child)
            .await
            .unwrap()
            .expect("verify child 必须有运行快照");
        assert!(
            prompt.effective_tools.iter().any(|name| name == "bash"),
            "verify 必须能运行真实命令：{:?}",
            prompt.effective_tools
        );
        for forbidden in ["write_file", "edit", "job_start"] {
            assert!(
                !prompt.effective_tools.iter().any(|name| name == forbidden),
                "verify 不得拿到 {forbidden}：{:?}",
                prompt.effective_tools
            );
        }
        assert!(
            !prompt
                .effective_tools
                .iter()
                .any(|name| name.starts_with("mcp__")),
            "verify 默认不拿 MCP 工具"
        );
        assert!(prompt.instructions.contains("失败就报失败"));
        assert_eq!(prompt.permission_ceiling.as_deref(), Some("inherit"));
        state.runtime.interrupt(&parent, &child).await.unwrap();
        settle(&state.runtime, &child).await;
    }

    #[tokio::test]
    async fn admission_interrupt_and_owner_cleanup() {
        let (state, mut ctx) = setup().await;
        ctx.selection.as_mut().unwrap().model = "hold".into();
        state
            .settings
            .update(
                crate::subagents::SETTINGS_NS,
                json!({"maxConcurrentRuns":1}),
                None,
            )
            .unwrap();
        let result = state
            .runtime
            .execute("spawn_agent", json!({"prompt":"保持运行"}), &ctx)
            .await
            .unwrap();
        let id = result["childId"].as_str().unwrap();
        assert!(
            state
                .runtime
                .execute("spawn_agent", json!({"prompt":"超额"}), &ctx)
                .await
                .is_err()
        );
        assert!(
            state
                .runtime
                .remove_owner(ctx.session_id.as_deref().unwrap())
                .await
                .is_err()
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while state
                .live
                .get(id)
                .is_none_or(|l| l.cancel.lock().unwrap().is_none())
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        state
            .runtime
            .interrupt(ctx.session_id.as_deref().unwrap(), id)
            .await
            .unwrap();
        settle(&state.runtime, id).await;
        state
            .runtime
            .remove_owner(ctx.session_id.as_deref().unwrap())
            .await
            .unwrap();
        assert!(state.sessions.load(id).is_err());
    }

    /// goal 模式端到端:设置目标后空闲会话自动续跑,跑到轮次上限自动停;
    /// 状态转换 fail loud;清除后回归无目标。
    #[tokio::test]
    async fn goal_rounds_auto_continue_and_stop_at_max_rounds() {
        let (state, _) = setup().await;
        state
            .settings
            .update("goals", json!({"maxRounds": 2}), None)
            .unwrap();
        let state = Arc::new(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = crate::api::router().with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let client = reqwest::Client::new();
        let created: Value = client
            .post(format!("{base}/api/sessions"))
            .json(&json!({"cwd": state.home}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = created["session"]["id"].as_str().unwrap().to_string();
        let goal_url = format!("{base}/api/sessions/{id}/goal");

        // 预热一轮:goal 续跑的 selection 依赖请求头回推(先跑过普通轮)。
        let status = client
            .post(format!("{base}/api/sessions/{id}/prompt"))
            .json(&json!({"prompt":"预热","provider":"runtime-test","model":"done"}))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, axum::http::StatusCode::ACCEPTED);
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                let live = state.live.get(&id).unwrap();
                if !live.running.load(Ordering::SeqCst) && live.session.next_turn_number() > 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap();

        // 初始:无目标;非法转换 fail loud。
        let view: Value = client
            .get(&goal_url)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(view["goal"].is_null());
        assert_eq!(
            client
                .post(&goal_url)
                .json(&json!({"action":"pause"}))
                .send()
                .await
                .unwrap()
                .status(),
            axum::http::StatusCode::BAD_REQUEST
        );

        // 设置目标:空闲会话立即自动续跑,跑到轮次上限(maxRounds=2)停止。
        // token 预算给得足够宽:本测试钉的是**轮次上限**(跑满两轮后不再
        // 自动续跑)。“缺 usage 转保守估算”之后,一轮 mock 轮的估算用量会
        // 达到数万 token——原先那个 123 的小预算在任何真实计费下都撑不过
        // 一轮,会把这条断言挟持成预算耗尽路径。
        client
            .post(&goal_url)
            .json(&json!({"action":"set","objective":"验证 goal 续跑","tokenBudget":10_000_000}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                let live = state.live.get(&id).unwrap();
                let rounds = live.session.goal().map(|g| g.rounds_started).unwrap_or(0);
                if rounds >= 2 && !live.running.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap();
        let live = state.live.get(&id).unwrap();
        let goal = live.session.goal().unwrap();
        assert_eq!(goal.rounds_started, 2, "跑满两轮后必须停止");
        assert_eq!(
            goal.status,
            denia_core::session::GoalStatus::Active,
            "轮次耗尽保持 active(不自动续,但状态不谎报)"
        );
        let events = live.session.events();
        // 目标状态块(channel=goal)在每轮内容变化时注入一次:轮次与
        // 用量随轮推进,状态块逐轮更新;不再有单独的 goal-round 消息。
        let injected = events
            .iter()
            .filter(|e| {
                matches!(
                    &e.event,
                    SessionEvent::UserMessage {
                        channel: Some(c),
                        injected: true,
                        ..
                    } if c == "goal"
                )
            })
            .count();
        assert_eq!(injected, 2, "每轮一条 goal 状态注入");
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(&e.event, SessionEvent::Goal { op: GoalOp::Round }))
                .count(),
            2
        );
        assert!(events.iter().any(|e| matches!(
            &e.event,
            SessionEvent::UserMessage { text, .. }
                if text.contains("[denia 目标]") && text.contains("已发起续跑轮数:1")
        )));
        assert!(events.iter().any(|e| matches!(
            &e.event,
            SessionEvent::UserMessage { text, .. } if text.contains("已发起续跑轮数:2")
        )));

        // 暂停/恢复/清除的状态转换。
        let view: Value = client
            .post(&goal_url)
            .json(&json!({"action":"pause"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(view["goal"]["status"], "paused");
        assert_eq!(
            client
                .post(&goal_url)
                .json(&json!({"action":"pause"}))
                .send()
                .await
                .unwrap()
                .status(),
            axum::http::StatusCode::BAD_REQUEST
        );
        let view: Value = client
            .post(&goal_url)
            .json(&json!({"action":"resume"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(view["goal"]["status"], "active");
        let view: Value = client
            .post(&goal_url)
            .json(&json!({"action":"clear"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(view["goal"].is_null());
        // 预算超出全局上限:拒绝(fail loud)。
        client
            .post(&goal_url)
            .json(&json!({"action":"set","objective":"预算校验","tokenBudget":999_999_999}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .expect_err("预算超上限必须失败");
        server.abort();
    }
    /// 暂停后恢复不得被同一条旧 `TurnEnd{Aborted}` 打回 paused。
    ///
    /// 缺陷形状:判定"上一轮结局"取整份日志里最后一条 `TurnEnd`,没有水位
    /// 也没有已处理标记;日志 append-only,暂停前那条 `Aborted` 一直留在那里,
    /// 于是 resume 之后 `continue_goal` 读到的还是它,又落一次 Pause——判据
    /// 不变,恢复必然立刻失效。
    ///
    /// 反向语义同时覆盖:水位之后的**新** Aborted 必须仍然暂停目标(连续
    /// 两次手动停止都要生效)。
    #[tokio::test]
    async fn goal_resume_is_not_reverted_by_stale_abort() {
        let (state, _) = setup().await;
        let state = Arc::new(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = crate::api::router().with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let client = reqwest::Client::new();

        // 轮询到条件成立;超时即失败,并把当前目标状态带进报错。
        async fn wait_until(
            state: &crate::state::AppState,
            id: &str,
            label: &str,
            check: impl Fn(&crate::state::LiveSession) -> bool,
        ) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                let live = state.live.get(id).unwrap();
                if check(&live) {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "等待超时:{label};当前目标 = {:?}",
                    live.session.goal()
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }

        let created: Value = client
            .post(format!("{base}/api/sessions"))
            .json(&json!({"cwd": state.home}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = created["session"]["id"].as_str().unwrap().to_string();
        let goal_url = format!("{base}/api/sessions/{id}/goal");

        // 目标轮用 hold 模型(流永不产出):轮次挂住,便于手动停止出一个
        // 真实的 `TurnEnd{Aborted}`。
        client
            .post(&goal_url)
            .json(&json!({
                "action": "set",
                "objective": "验证暂停恢复不被旧结局打回",
                "provider": "runtime-test",
                "model": "hold"
            }))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        wait_until(&state, &id, "目标设置后自动开跑第一轮", |live| {
            live.running.load(Ordering::SeqCst)
                && live.session.goal().is_some_and(|g| g.rounds_started == 1)
        })
        .await;

        // 第一次手动停止:水位之后的 Aborted → 目标自动暂停(原有语义)。
        client
            .post(format!("{base}/api/sessions/{id}/cancel"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        wait_until(&state, &id, "手动停止后目标自动暂停", |live| {
            live.session
                .goal()
                .is_some_and(|g| matches!(g.status, denia_core::session::GoalStatus::Paused))
        })
        .await;

        // 恢复:旧 Aborted 不得再把目标打回 paused——恢复必须真的开新轮。
        let view: Value = client
            .post(&goal_url)
            .json(&json!({"action":"resume"}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(view["goal"]["status"], "active");
        wait_until(&state, &id, "恢复后开新轮", |live| {
            live.session.goal().is_some_and(|g| g.rounds_started == 2)
        })
        .await;
        let view: Value = client
            .get(&goal_url)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            view["goal"]["status"], "active",
            "恢复之后不得再被同一条旧 Aborted 暂停:{}",
            view["goal"]
        );

        // 第二次手动停止:水位之后的新 Aborted 仍必须暂停目标。
        client
            .post(format!("{base}/api/sessions/{id}/cancel"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        wait_until(&state, &id, "第二次手动停止仍暂停目标", |live| {
            live.session
                .goal()
                .is_some_and(|g| matches!(g.status, denia_core::session::GoalStatus::Paused))
        })
        .await;
        let live = state.live.get(&id).unwrap();
        assert_eq!(
            live.session.goal().map(|g| g.rounds_started),
            Some(2),
            "暂停态不得再推进轮次"
        );
        assert_eq!(
            live.session
                .events()
                .iter()
                .filter(|e| matches!(
                    &e.event,
                    SessionEvent::TurnEnd {
                        reason: TurnEndReason::Aborted { .. },
                        ..
                    }
                ))
                .count(),
            2,
            "两次手动停止各留一条 Aborted"
        );
        server.abort();
    }
    /// `on_idle` 的 paused 判定必须与续跑分类共用同一水位:resume 走的是
    /// 自动续跑路径(不经过 `human_turn`,清不掉 paused),若仍按"整份日志
    /// 最后一条 TurnEnd"判定,暂停前那条 Aborted 会把已恢复的会话一直标成
    /// "用户刚停过",新轮次的唤醒全被挡住。
    #[tokio::test]
    async fn on_idle_pause_flag_ignores_turn_ends_before_last_goal_op() {
        let (state, ctx) = setup().await;
        let owner = ctx.session_id.as_deref().unwrap();
        let live = state.live.get(owner).unwrap();
        let status = || live.session.goal().map(|goal| goal.status);

        // 目标 active → 一轮被用户手动停止 → 目标暂停(Aborted 留在水位之前)。
        live.session
            .apply_goal(GoalOp::Set {
                objective: "验证 paused 水位".into(),
                token_budget: None,
            })
            .unwrap();
        live.session
            .append(SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Aborted { cause: None },
            })
            .unwrap();
        live.session.apply_goal(GoalOp::Pause).unwrap();
        assert_eq!(status(), Some(denia_core::session::GoalStatus::Paused));

        // resume 落成新水位:它之前那条 Aborted 不得再设置 paused 标记。
        live.session.apply_goal(GoalOp::Resume).unwrap();
        assert_eq!(status(), Some(denia_core::session::GoalStatus::Active));
        state.runtime.on_idle(owner);
        assert!(
            !state.runtime.inner.paused.lock().unwrap().contains(owner),
            "水位之前的旧 Aborted 不得再设置 paused 标记"
        );

        // 水位之后的新 Aborted 仍然生效:连续两次手动停止都要能暂停。
        live.session
            .append(SessionEvent::TurnEnd {
                turn: 2,
                reason: TurnEndReason::Aborted { cause: None },
            })
            .unwrap();
        state.runtime.on_idle(owner);
        assert!(
            state.runtime.inner.paused.lock().unwrap().contains(owner),
            "水位之后的新 Aborted 必须仍然设置 paused 标记"
        );
    }
    #[tokio::test]
    async fn cancelled_start_leaves_no_child_and_config_rejects_invalid() {
        let (state, ctx) = setup().await;
        ctx.cancel.cancel();
        assert!(
            state
                .runtime
                .execute("spawn_agent", json!({"prompt":"不应创建"}), &ctx)
                .await
                .is_err()
        );
        assert!(
            state
                .runtime
                .list(ctx.session_id.as_deref().unwrap())
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            state
                .settings
                .update("runtime", json!({"maxDepth":0}), None)
                .is_err()
        );
        // 项目记忆预算:越界拒绝;布尔开关与合法预算放行。
        assert!(
            state
                .settings
                .update("runtime", json!({"projectMemoryMaxBytes":0}), None)
                .is_err()
        );
        assert!(
            state
                .settings
                .update(
                    "runtime",
                    json!({"projectMemoryMaxBytes":25600,"memoryEnabled":false}),
                    None
                )
                .is_ok()
        );
    }

    #[tokio::test]
    async fn http_prompt_tools_jobs_skills_and_child_controls() {
        let (state, _) = setup().await;
        std::fs::create_dir_all(state.home.join("skills/runtime-test")).unwrap();
        std::fs::write(
            state.home.join("skills/runtime-test/SKILL.md"),
            "---\nname: runtime-test\ndescription: 接口验证技能\n---\n读取并验证运行时结果。",
        )
        .unwrap();
        let state = Arc::new(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = crate::api::router().with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let client = reqwest::Client::new();
        let created: Value = client
            .post(format!("{base}/api/sessions"))
            .json(&json!({"cwd":state.home}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = created["session"]["id"].as_str().unwrap();
        assert_eq!(client.post(format!("{base}/api/sessions/{id}/prompt")).json(&json!({"prompt":"验证三个系统","provider":"runtime-test","model":"orchestrator","skills":["runtime-test"]})).send().await.unwrap().status(),axum::http::StatusCode::ACCEPTED);
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                let live = state.live.get(id).unwrap();
                let count = live
                    .session
                    .events()
                    .iter()
                    .filter(|e| matches!(e.event, SessionEvent::ToolResult { .. }))
                    .count();
                if count >= 4 && !live.running.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap();
        let live = state.live.get(id).unwrap();
        let events = live.session.events();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.event, SessionEvent::ToolResult { is_error: true, .. })),
            "工具链不得出错：{events:?}"
        );
        let children: Value = client
            .get(format!("{base}/api/sessions/{id}/agents"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let child = children["agents"][0]["id"].as_str().unwrap();
        assert!(
            !client
                .post(format!("{base}/api/sessions/{child}/prompt"))
                .json(&json!({"prompt":"直接写入"}))
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
        let jobs: Value = client
            .get(format!("{base}/api/sessions/{id}/jobs"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let job = jobs["jobs"][0]["id"].as_str().unwrap();
        state
            .runtime
            .jobs()
            .wait(job, id, 10_000, &CancellationToken::new())
            .await
            .unwrap();
        let output: Value = client
            .get(format!("{base}/api/sessions/{id}/jobs/{job}/output"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(output["output"].as_str().unwrap().contains("runtime-e2e"));
        assert!(
            !client
                .get(format!("{base}/api/sessions/{child}/jobs/{job}/output"))
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
        let skills: Value = client
            .get(format!("{base}/api/sessions/{id}/skills"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            skills["skills"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["name"] == "runtime-test")
        );
        // dsh 对齐：skill load 的 SKILL.md 全文直接随工具结果返回（一次进历史），
        // 能力上下文不再携带已加载正文。
        let live = state.live.get(id).unwrap();
        let events = live.session.events();
        let skill_call = events
            .iter()
            .rev()
            .find_map(|e| match &e.event {
                SessionEvent::ToolCall { call_id, name, .. } if name == "skill" => {
                    Some(call_id.clone())
                }
                _ => None,
            })
            .expect("skill load 工具调用应已落库");
        let load_result = events
            .iter()
            .rev()
            .find_map(|e| match &e.event {
                SessionEvent::ToolResult {
                    call_id,
                    content,
                    is_error: false,
                    ..
                } if *call_id == skill_call => Some(content.clone()),
                _ => None,
            })
            .expect("skill load 工具结果应已落库");
        assert!(
            load_result.contains("读取并验证运行时结果。"),
            "SKILL.md 正文应随工具结果返回：{load_result}"
        );
        let context = state
            .runtime
            .context(id, &state.home)
            .await
            .unwrap()
            .join("\n");
        assert!(
            !context.contains("读取并验证运行时结果。"),
            "能力上下文不应再携带技能正文：{context}"
        );
        server.abort();
    }

    /// 项目记忆索引注入(运行时接线):选择层按触碰路径与最近用户消息裁到
    /// 相关条目,被省略的条目在渲染层留全文指针;空桶不注入。
    #[tokio::test]
    async fn memory_index_injection_selects_by_touched_path_and_prompt() {
        let home =
            std::env::temp_dir().join(format!("denia-memory-inject-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let state = crate::state::build_state(&home, false, 3600).await.unwrap();
        let cwd = home.join("ws");
        std::fs::create_dir_all(&cwd).unwrap();
        let root = crate::project_memory::memory_root(&home, &cwd);
        std::fs::create_dir_all(&root).unwrap();

        // 空桶不注入——目录路径已由装配阶段内联进「# 项目记忆」段。
        assert!(
            state
                .runtime
                .project_memory_index(&cwd, &[], None)
                .await
                .unwrap()
                .is_none()
        );

        std::fs::write(
            root.join(crate::project_memory::MEMORY_INDEX_FILE),
            "# 项目记忆\n- [注入通道](injections.md) — 每步注入与幂等基准\n- [面板配色](frontend-css.md) — 设置页样式\n",
        )
        .unwrap();
        // 触碰路径命中一条:另一条省略,并附全文指针。
        let block = state
            .runtime
            .project_memory_index(&cwd, &[cwd.join("src/injections.rs")], None)
            .await
            .unwrap()
            .expect("有索引时必须注入");
        assert!(block.contains("injections.md"), "{block}");
        assert!(!block.contains("frontend-css.md"), "{block}");
        assert!(block.contains("另有 1 条未列出"), "{block}");
        assert!(block.contains("/MEMORY.md"), "{block}");

        // 最近用户消息同样是选择依据(关键词命中另一条)。
        let block = state
            .runtime
            .project_memory_index(&cwd, &[], Some("设置页样式调一下"))
            .await
            .unwrap()
            .unwrap();
        assert!(block.contains("frontend-css.md"), "{block}");
        assert!(!block.contains("injections.md"), "{block}");

        std::fs::remove_dir_all(home).unwrap();
    }

    /// 交接信息的**单一来源**:同一个子代理上,`settle` 注入父会话的通知文本与
    /// `wait_agent` 的返回对象必须由同一份结构渲染(逐项对得上);并且"子代理
    /// 自称完成"与"宿主验证"必须能分开读出来 —— 默认 develop 子代理没有账本
    /// 工具,`hostVerified` 必须是 false,未验证提醒必须落在注入文本里。
    #[tokio::test]
    async fn settlement_notice_and_wait_agent_agree_on_the_same_handoff() {
        use denia_tools::capabilities::AgentRuntime;

        let (state, base_ctx) = setup().await;
        let state = Arc::new(state);
        let workspace = state.home.join("handofffixture");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("existing.txt"), "旧内容\n").unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = crate::api::router().with_state(state.clone());
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let client = reqwest::Client::new();
        let created: Value = client
            .post(format!("{base}/api/sessions"))
            .json(&json!({"cwd": workspace}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let parent = created["session"]["id"].as_str().unwrap().to_string();
        assert!(
            client
                .put(format!("{base}/api/sessions/{parent}/permission"))
                .json(&json!({"mode": "full"}))
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
        assert!(
            client
                .post(format!("{base}/api/sessions/{parent}/prompt"))
                .json(&json!({"prompt":"委派开发任务","provider":"runtime-test","model":"exec-parent"}))
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
        let child = wait_for_child(&state, &parent).await;
        wait_for_tool_results(&state, &child, 4).await;

        // 等完成通知真的落进父会话:AgentInbox / AgentDelivery 里的那段文本就是
        // 被注入父上下文的那份(注入通道不另改)。
        let mut notice = String::new();
        for _ in 0..1500 {
            let delivered = state.live.get(&parent).map(|live| {
                live.session
                    .events()
                    .iter()
                    .filter_map(|event| match &event.event {
                        SessionEvent::AgentInbox { text, source, .. }
                        | SessionEvent::AgentDelivery { text, source, .. }
                            if source == "subagent-settled" && text.contains(&child) =>
                        {
                            Some(text.clone())
                        }
                        _ => None,
                    })
                    .next_back()
            });
            if let Some(Some(text)) = delivered {
                notice = text;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(!notice.is_empty(), "子代理结束通知必须送达父会话");

        let mut ctx = base_ctx.clone();
        ctx.session_id = Some(parent.clone());
        let ready = state
            .runtime
            .execute("wait_agent", json!({"target":child}), &ctx)
            .await
            .unwrap();
        // 既有契约不变(向后兼容:只加字段,不改既有字段的取值)。
        assert_eq!(ready["status"], "ready");
        assert_eq!(ready["running"], false);
        assert_eq!(ready["id"], json!(child));

        // 终态枚举能说出这是子代理自己说的完成,而不是宿主验收过了。
        assert_eq!(ready["terminal"]["outcome"], "completed");
        assert_eq!(ready["terminal"]["basis"], "self_reported");
        assert_eq!(ready["terminal"]["hostVerified"], false);
        assert_eq!(ready["terminal"]["finalized"], true);
        assert_eq!(ready["terminal"]["reason"], "已完成");

        // 用量是结构化字段,不是拼进文本的字符串。
        // 用量是结构化字段,不是拼进文本的字符串:五个分量 + 总数都在。
        for key in [
            "uncachedInputTokens",
            "outputTokens",
            "cacheReadTokens",
            "cacheWriteTokens",
            "reasoningTokens",
            "totalTokens",
        ] {
            assert!(
                ready["usage"][key].is_u64(),
                "usage.{key} 必须是数字字段:{ready}"
            );
        }
        let total = ready["usage"]["totalTokens"].as_u64().unwrap();

        // 变更文件:真写过的在里面;只读的文件不在;口径写清楚。
        let changed: Vec<&str> = ready["changedPaths"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(changed.contains(&"child-wrote.txt"), "{ready}");
        assert!(
            !changed.contains(&"existing.txt"),
            "只读没改的文件不得计入:{ready}"
        );
        assert!(
            ready["changedPathsNote"].as_str().unwrap().contains("bash"),
            "{ready}"
        );
        // 没有产物时给空数组,不报错、不给 null。
        assert!(ready["artifacts"].is_array(), "{ready}");

        // 默认 develop 子代理没有账本工具:拿不到结论给 null,原因与提醒都写明。
        assert!(ready["verification"].is_null(), "{ready}");
        let note = ready["verificationNote"].as_str().unwrap();
        assert!(note.starts_with('无'), "{ready}");
        let advisories: Vec<&str> = ready["advisories"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(
            advisories.iter().any(|line| line.contains("[未验证]")),
            "{ready}"
        );

        // —— 两处同源:通知文本与返回对象逐项对得上 ——
        assert!(
            notice.contains(&format!(
                "子代理：{child}（{}）",
                ready["profile"].as_str().unwrap()
            )),
            "{notice}"
        );
        assert!(
            notice.contains(&format!(
                "终态：{}",
                ready["terminal"]["outcome"].as_str().unwrap()
            )),
            "{notice}"
        );
        assert!(
            notice.contains(&format!("本次子代理用量：{total} tokens")),
            "通知里的用量必须与 wait_agent 的 usage 同源:{notice}"
        );
        assert!(notice.contains("child-wrote.txt"), "{notice}");
        for line in &advisories {
            assert!(
                notice.contains(line),
                "父侧注入文本必须带上这条标注:{line}\n{notice}"
            );
        }
        std::fs::remove_dir_all(state.home.clone()).ok();
    }

    /// 有账本且验收通过时,"宿主验证"必须为真 —— 与"自称完成"分开。
    ///
    /// 这里直接给子会话塞账本事件(它们本就是宿主执行链的产物)再算交接信息,
    /// 断言终态依据从 `self_reported` 变成 `host_verified`,且不再出现未验证提醒。
    #[tokio::test]
    async fn handoff_marks_host_verified_when_the_ledger_concludes_passed() {
        use denia_core::config::ModelSelection;
        use denia_core::session::{SessionEvent, SubagentDescriptor};
        use denia_core::task::{
            CheckRun, FileFingerprint, RequirementClaim, RequirementId, RequirementKind,
            RevisionId, SourceRef, TaskId, TaskOp, ValidationResult,
        };

        let (state, ctx) = setup().await;
        let parent = ctx.session_id.clone().unwrap();
        let selection = ModelSelection {
            provider: "runtime-test".into(),
            model: "ledger-child".into(),
            reasoning_effort: None,
        };
        let session = state
            .sessions
            .create_subagent(
                &ctx.cwd,
                true,
                &parent,
                SubagentDescriptor::legacy("账本子代理", 1, "default", selection, None, None),
            )
            .unwrap();
        let child_id = session.id().to_string();
        let fingerprint = FileFingerprint {
            path: "src/lib.rs".into(),
            digest: "d1".into(),
        };
        let user = session
            .append(SessionEvent::UserMessage {
                text: "改完跑通测试再说完事".into(),
                injected: false,
                images: Vec::new(),
                channel: None,
            })
            .unwrap();
        session
            .append(SessionEvent::Task {
                op: TaskOp::Open {
                    task_id: TaskId::new("task-1"),
                    revision: RevisionId::new("rev-a"),
                    goal: "改好解析器".into(),
                    requirements: vec![RequirementClaim {
                        id: RequirementId::new("req-1"),
                        kind: RequirementKind::Acceptance,
                        text: "cargo test 全绿".into(),
                        source: SourceRef {
                            session: None,
                            seq: user.seq,
                            time_ms: user.time,
                            quote: Some("跑通测试".into()),
                        },
                    }],
                },
            })
            .unwrap();
        session
            .append(SessionEvent::Task {
                op: TaskOp::RecordChange {
                    summary: "改了 src/lib.rs".into(),
                    fingerprints: vec![fingerprint.clone()],
                },
            })
            .unwrap();
        let result = ValidationResult::from_check_runs(
            RevisionId::new("rev-a"),
            vec![CheckRun {
                command: "cargo test".into(),
                exit_code: Some(0),
                expect_exit_code: 0,
                workdir: ".".into(),
                fingerprints: vec![fingerprint],
            }],
            vec![RequirementId::new("req-1")],
            Some(child_id.clone()),
            1,
        )
        .expect("有检查运行才构成结论");
        session
            .append(SessionEvent::Task {
                op: TaskOp::RecordValidation { result },
            })
            .unwrap();
        session
            .append(SessionEvent::Task { op: TaskOp::Close })
            .unwrap();
        session
            .append(SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            })
            .unwrap();
        session.flush().unwrap();

        let descriptor = session.header().subagent.clone().unwrap();
        let child = Child {
            id: child_id.clone(),
            parent_id: parent.clone(),
            descriptor,
        };
        // 冷加载的实例(事件不驻留)也要能算出同一份交接信息:日志走磁盘回放,
        // 不能把"读不到事件"当成"什么都没做"。
        let live = state.live.get_or_load(&state.sessions, &child_id).unwrap();
        let handoff = subagent_handoff(Some(&child), &live.session, false);

        assert_eq!(
            handoff.terminal.outcome,
            TerminalOutcome::Completed,
            "{:?}",
            handoff.terminal
        );
        assert_eq!(handoff.terminal.basis, TerminalBasis::HostVerified);
        assert!(handoff.terminal.host_verified, "{:?}", handoff.terminal);
        assert!(matches!(
            handoff.verification,
            Some(VerificationState::Verified {
                verdict: ValidationVerdict::Passed,
                ..
            })
        ));
        assert!(
            handoff.verification_note.contains("验收通过"),
            "{}",
            handoff.verification_note
        );
        assert!(handoff.advisories.is_empty(), "{:?}", handoff.advisories);
        // 账本里记录的变更范围也并入 changedPaths,并在口径里注明来源。
        assert!(handoff.changed_paths.contains(&"src/lib.rs".to_string()));
        assert!(
            handoff.changed_paths_note.contains("任务账本"),
            "{}",
            handoff.changed_paths_note
        );
        let notice = handoff.notice_text("正文", "", "");
        assert!(notice.contains("宿主验证 有"), "{notice}");
        assert!(notice.contains("验收通过"), "{notice}");
        assert!(!notice.contains("[未验证]"), "{notice}");
        std::fs::remove_dir_all(state.home.clone()).ok();
    }

    /// 日志归集的三条边界:执行成功的写算改动、**被拒的写不算改动**、产物引用
    /// 按 `outputArtifact` 读出来;无内容时给空数组 / `None` + 口径,不报错。
    #[tokio::test]
    async fn changed_paths_and_artifacts_follow_the_session_log() {
        use denia_core::config::ModelSelection;
        use denia_core::session::{SessionEvent, SubagentDescriptor};

        let (state, ctx) = setup().await;
        let parent = ctx.session_id.clone().unwrap();
        let selection = ModelSelection {
            provider: "runtime-test".into(),
            model: "log-child".into(),
            reasoning_effort: None,
        };
        let session = state
            .sessions
            .create_subagent(
                &ctx.cwd,
                true,
                &parent,
                SubagentDescriptor::legacy("日志子代理", 1, "default", selection, None, None),
            )
            .unwrap();
        let child_id = session.id().to_string();
        let call = |call_id: &str, name: &str, arguments: Value| SessionEvent::ToolCall {
            turn: 1,
            step: 1,
            call_id: call_id.into(),
            name: name.into(),
            arguments: arguments.to_string(),
        };
        session
            .append(call(
                "c1",
                "write_file",
                json!({"path": "child-a.txt", "content": "x"}),
            ))
            .unwrap();
        session
            .append(SessionEvent::ToolResult {
                turn: 1,
                step: 1,
                call_id: "c1".into(),
                content: "已写入 child-a.txt".into(),
                is_error: false,
                error: None,
                error_identity: None,
                meta: None,
                truncation: None,
                replaces: None,
            })
            .unwrap();
        // 被拒的写:调用在日志里,但它没有改动任何东西。
        session
            .append(call(
                "c2",
                "write_file",
                json!({"path": "refused.txt", "content": "x"}),
            ))
            .unwrap();
        session
            .append(SessionEvent::ToolResult {
                turn: 1,
                step: 1,
                call_id: "c2".into(),
                content: "子代理不能使用 write_file".into(),
                is_error: true,
                error: Some("tool-not-granted".into()),
                error_identity: None,
                meta: None,
                truncation: None,
                replaces: None,
            })
            .unwrap();
        // bash:命令间接改动推断不出来(不计入 changedPaths),但产物引用在 meta 里。
        session
            .append(call("c3", "bash", json!({"command": "echo hi"})))
            .unwrap();
        session
            .append(SessionEvent::ToolResult {
                turn: 1,
                step: 1,
                call_id: "c3".into(),
                content: "退出码: 0\nhi\n".into(),
                is_error: false,
                error: None,
                error_identity: None,
                meta: Some(json!({
                    "exit_code": 0,
                    "end_reason": "completed",
                    "outputArtifact": {
                        "output_id": "art-7",
                        "streams": {},
                        "complete": true,
                        "storage_error": null,
                    },
                })),
                truncation: None,
                replaces: None,
            })
            .unwrap();
        session
            .append(SessionEvent::AssistantMessage {
                turn: 1,
                step: 1,
                blocks: vec![denia_core::stream::ContentBlock::Text {
                    text: "写完了".into(),
                }],
                usage: None,
                interrupted: false,
                source_event_seqs: Vec::new(),
                first_token_time: None,
            })
            .unwrap();
        session
            .append(SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            })
            .unwrap();
        session.flush().unwrap();

        let child = Child {
            id: child_id.clone(),
            parent_id: parent.clone(),
            descriptor: session.header().subagent.clone().unwrap(),
        };
        let live = state.live.get_or_load(&state.sessions, &child_id).unwrap();
        let handoff = subagent_handoff(Some(&child), &live.session, false);

        assert_eq!(handoff.changed_paths, vec!["child-a.txt".to_string()]);
        assert!(
            !handoff.changed_paths.contains(&"refused.txt".to_string()),
            "被拒的写不算改动"
        );
        assert_eq!(handoff.artifacts.len(), 1, "{:?}", handoff.artifacts);
        assert_eq!(handoff.artifacts[0].output_id, "art-7");
        assert_eq!(handoff.artifacts[0].tool, "bash");
        assert_eq!(handoff.artifacts[0].complete, Some(true));
        // 没有账本:验证字段给 `None` + 原因,而不是报错。
        assert!(handoff.verification.is_none());
        assert!(!handoff.verification_note.is_empty());
        // 判定沿用子代理自己的终态,但明确标出宿主没有验证结论。
        assert_eq!(handoff.terminal.outcome, TerminalOutcome::Completed);
        assert!(!handoff.terminal.host_verified);
        std::fs::remove_dir_all(state.home.clone()).ok();
    }

    /// 空值的那一侧:`changedPaths` / `artifacts` 在"确实什么都没发生"时必须是
    /// **空数组 + 口径说明**,不是 `null`、不报错 —— "没有改动"与"读不到改动"
    /// 必须能分开读,否则父代理会把"读不到"当成"没改"。
    #[tokio::test]
    async fn handoff_reports_empty_changes_and_artifacts_without_failing() {
        use denia_core::config::ModelSelection;
        use denia_core::session::{SessionEvent, SubagentDescriptor};

        let (state, ctx) = setup().await;
        let parent = ctx.session_id.clone().unwrap();
        let selection = ModelSelection {
            provider: "runtime-test".into(),
            model: "quiet-child".into(),
            reasoning_effort: None,
        };
        let session = state
            .sessions
            .create_subagent(
                &ctx.cwd,
                true,
                &parent,
                SubagentDescriptor::legacy("只读子代理", 1, "default", selection, None, None),
            )
            .unwrap();
        let child_id = session.id().to_string();
        // 只读的一轮:没有写调用、没有产物、没有账本。
        session
            .append(SessionEvent::AssistantMessage {
                turn: 1,
                step: 1,
                blocks: vec![denia_core::stream::ContentBlock::Text {
                    text: "看完了,没问题".into(),
                }],
                usage: None,
                interrupted: false,
                source_event_seqs: Vec::new(),
                first_token_time: None,
            })
            .unwrap();
        session
            .append(SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            })
            .unwrap();
        session.flush().unwrap();

        let child = Child {
            id: child_id.clone(),
            parent_id: parent.clone(),
            descriptor: session.header().subagent.clone().unwrap(),
        };
        let live = state.live.get_or_load(&state.sessions, &child_id).unwrap();
        let handoff = subagent_handoff(Some(&child), &live.session, false);

        assert!(
            handoff.changed_paths.is_empty(),
            "{:?}",
            handoff.changed_paths
        );
        assert!(handoff.artifacts.is_empty(), "{:?}", handoff.artifacts);
        assert!(
            handoff.changed_paths_note.contains("没有可识别的落盘改动"),
            "{}",
            handoff.changed_paths_note
        );
        assert!(
            handoff.changed_paths_note.contains("bash"),
            "口径必须说清哪些改动推断不出来:{}",
            handoff.changed_paths_note
        );
        assert!(handoff.verification.is_none());
        let notice = handoff.notice_text("正文", "", "");
        assert!(notice.contains("变更文件：（无）"), "{notice}");
        assert!(notice.contains("产物：（无）"), "{notice}");
        assert!(notice.contains("没有可识别的落盘改动"), "{notice}");
        // 顺手钉住 wire 形状:空就是空数组,验证拿不到就是 null。
        let fields = handoff.fields();
        assert_eq!(fields["changedPaths"], json!([]));
        assert_eq!(fields["artifacts"], json!([]));
        assert!(fields["verification"].is_null());
        std::fs::remove_dir_all(state.home.clone()).ok();
    }
    #[test]
    fn execution_budget_config_keeps_the_protection_baseline() {
        let defaults = GoalsConfig::default();
        assert_eq!(defaults.max_execution_steps, 128);
        assert_eq!(defaults.max_execution_requests, 256);
        assert_eq!(defaults.max_execution_active_ms, 60 * 60 * 1_000);
        assert!(validate_goals_config(json!({})).is_ok());
        // 0 = 显式关闭某一道边界(其余两道仍生效),允许。
        assert!(validate_goals_config(json!({"maxExecutionSteps": 0})).is_ok());
        assert!(validate_goals_config(json!({"maxExecutionSteps": 100_001})).is_err());
        assert!(validate_goals_config(json!({"maxExecutionRequests": 1_000_001})).is_err());
        assert!(validate_goals_config(json!({"maxExecutionActiveMs": 86_400_001})).is_err());
        assert!(validate_goals_config(json!({"maxExecutionSteps": "128"})).is_err());
    }

    #[tokio::test]
    async fn driver_reads_the_execution_budget_from_runtime_config() {
        let (state, _ctx) = setup().await;
        // 上限来源已经装在驱动器上:默认值就是防护基线。
        assert_eq!(
            state.driver.execution_limits(),
            state.runtime.execution_limits()
        );
        assert_eq!(state.driver.execution_limits().max_steps, 128);
        // 改配置 → 下一次取快照就是新值(在途轮次保持自己的快照,由
        // `turn::budget_tests` 的边界测试钉住)。
        state
            .settings
            .update("goals", json!({"maxExecutionSteps": 7}), None)
            .unwrap();
        assert_eq!(state.driver.execution_limits().max_steps, 7);
        assert_eq!(state.runtime.execution_limits().max_requests, 256);
        std::fs::remove_dir_all(state.home.clone()).ok();
    }

    /// 某个子代理在父会话里的完成通知正文(收件箱里的最后一条)。
    fn last_settled_notice(state: &crate::state::AppState, parent: &str, child: &str) -> Option<String> {
        state.live.get(parent).and_then(|live| {
            live.session
                .events()
                .iter()
                .filter_map(|event| match &event.event {
                    SessionEvent::AgentInbox { text, source, .. }
                        if source == "subagent-settled" && text.contains(child) =>
                    {
                        Some(text.clone())
                    }
                    _ => None,
                })
                .next_back()
        })
    }

    /// 某个子代理在父会话里的完成通知条数(收件箱;已领取的也计)。
    fn settled_notice_count(state: &crate::state::AppState, parent: &str, child: &str) -> usize {
        state
            .live
            .get(parent)
            .map(|live| {
                live.session
                    .events()
                    .iter()
                    .filter(|event| {
                        matches!(&event.event, SessionEvent::AgentInbox { text, source, .. }
                            if source == "subagent-settled" && text.contains(child))
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    async fn wait_for_settled_notice(
        state: &crate::state::AppState,
        parent: &str,
        child: &str,
        needle: &str,
    ) -> String {
        for _ in 0..500 {
            if let Some(text) = last_settled_notice(state, parent, child)
                && text.contains(needle)
            {
                return text;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("父代理未在期限内收到包含 {needle:?} 的子代理完成通知");
    }

    /// 完成通知不受普通消息"连续唤醒上限"的限制:额度耗尽也必须把结果
    /// 送给主代理。
    ///
    /// 锁死现象:同一用户轮次后分批完成超过 `maxConsecutiveWakes` 次,
    /// 第 N+1 条完成消息只入队不唤醒,必须用户再发一条消息才收到。
    #[tokio::test]
    async fn completion_wake_bypasses_message_loop_guard() {
        let (state, ctx) = setup().await;
        let parent = ctx.session_id.clone().unwrap();
        let live = state.live.get(&parent).unwrap();
        live.running.store(false, Ordering::SeqCst);
        let runtime = state.runtime.clone();
        let selection = ModelSelection {
            provider: "runtime-test".into(),
            model: "done".into(),
            reasoning_effort: None,
        };
        runtime
            .inner
            .selections
            .lock()
            .unwrap()
            .insert(parent.clone(), selection.clone());
        // 普通消息的唤醒额度已经耗尽。
        runtime
            .inner
            .wakes
            .lock()
            .unwrap()
            .insert(parent.clone(), runtime.config().max_consecutive_wakes);

        let child = state
            .sessions
            .create_subagent(
                &ctx.cwd,
                true,
                &parent,
                SubagentDescriptor::legacy("回调子代理", 1, "default", selection, None, None),
            )
            .unwrap();
        let child_id = child.id().to_string();
        let notice_id = format!("settled:{child_id}:turn:1:seq:9");
        runtime.inner.delivery.push(
            Notice {
                child: child_id.clone(),
                parent: parent.clone(),
                id: notice_id.clone(),
                legacy_id: format!("settled:{child_id}:2"),
                text: "[子代理执行结束] 分批完成".into(),
            },
            false,
        );
        runtime.retry_deliveries().await;

        let mut consumed = false;
        for _ in 0..300 {
            let live = state.live.get(&parent).unwrap();
            let events = live.session.events();
            let delivered = events.iter().any(|event| {
                matches!(&event.event, SessionEvent::AgentDelivery { id, .. } if id == &notice_id)
            });
            let turns = events
                .iter()
                .filter(|event| matches!(event.event, SessionEvent::TurnStart { .. }))
                .count();
            if delivered && turns == 1 && !live.running.load(Ordering::SeqCst) {
                consumed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            consumed,
            "唤醒额度耗尽后,子代理完成通知仍必须自动唤醒主代理并被消费"
        );
    }

    /// 普通代理消息的防循环护栏仍然有效:额度耗尽后不再自动开轮。
    #[tokio::test]
    async fn ordinary_message_wake_still_respects_message_loop_guard() {
        let (state, ctx) = setup().await;
        let parent = ctx.session_id.clone().unwrap();
        let live = state.live.get(&parent).unwrap();
        live.running.store(false, Ordering::SeqCst);
        let runtime = state.runtime.clone();
        runtime.inner.selections.lock().unwrap().insert(
            parent.clone(),
            ModelSelection {
                provider: "runtime-test".into(),
                model: "done".into(),
                reasoning_effort: None,
            },
        );
        runtime
            .inner
            .wakes
            .lock()
            .unwrap()
            .insert(parent.clone(), runtime.config().max_consecutive_wakes);
        runtime
            .enqueue(&parent, "m1".into(), "普通消息".into(), "job-completed".into())
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !live.running.load(Ordering::SeqCst),
            "额度耗尽时普通消息不得再开轮"
        );
        assert!(
            live.session
                .events()
                .iter()
                .all(|event| !matches!(event.event, SessionEvent::TurnStart { .. })),
            "普通消息的防循环护栏不得被完成通知的旁路改掉"
        );
    }

    /// 完成通知的瞬时投递失败必须自动补投,且不得重复入队。
    #[tokio::test]
    async fn completion_notice_retries_after_transient_enqueue_failure() {
        let (state, ctx) = setup().await;
        let parent = ctx.session_id.clone().unwrap();
        let runtime = state.runtime.clone();
        let child = state
            .sessions
            .create_subagent(
                &ctx.cwd,
                true,
                &parent,
                SubagentDescriptor::legacy(
                    "重试子代理",
                    1,
                    "default",
                    ctx.selection.clone().unwrap(),
                    None,
                    None,
                ),
            )
            .unwrap();
        let child_id = child.id().to_string();
        let notice_id = format!("settled:{child_id}:turn:1:seq:5");
        runtime.set_test_fault("completion-enqueue");
        runtime.inner.delivery.push(
            Notice {
                child: child_id.clone(),
                parent: parent.clone(),
                id: notice_id.clone(),
                legacy_id: format!("settled:{child_id}:2"),
                text: format!("[子代理执行结束] {child_id}"),
            },
            false,
        );
        runtime.retry_deliveries().await;
        assert_eq!(
            settled_notice_count(&state, &parent, &child_id),
            0,
            "首次投递失败时不得留下半截状态"
        );

        runtime
            .inner
            .faults
            .lock()
            .unwrap()
            .remove("completion-enqueue");
        let mut delivered = false;
        for _ in 0..300 {
            runtime.retry_deliveries().await;
            if settled_notice_count(&state, &parent, &child_id) == 1 {
                delivered = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
        assert!(delivered, "瞬时失败后必须自动补投");
        // 幂等:已入队的通知再次补投不得重复。
        runtime.retry_deliveries().await;
        assert_eq!(settled_notice_count(&state, &parent, &child_id), 1);
    }

    /// 真实冷启动恢复:子代理终态已落盘、完成通知还没入队(崩溃窗口),
    /// 新的 Runtime 必须自己发现并补投,不能等用户发消息。
    #[tokio::test]
    async fn restart_recovery_delivers_unnotified_completion() {
        let (state, ctx) = setup().await;
        let parent = ctx.session_id.clone().unwrap();
        let selection = ctx.selection.clone().unwrap();
        let child = state
            .sessions
            .create_subagent(
                &ctx.cwd,
                true,
                &parent,
                SubagentDescriptor::legacy("崩溃子代理", 1, "default", selection.clone(), None, None),
            )
            .unwrap();
        let child_id = child.id().to_string();
        child
            .append(SessionEvent::AgentInbox {
                id: "m1".into(),
                text: "[父代理 委派任务]\n调查".into(),
                source: format!("agent:{parent}"),
            })
            .unwrap();
        child
            .append(SessionEvent::AgentDelivery {
                id: "m1".into(),
                text: "[父代理 委派任务]\n调查".into(),
                source: format!("agent:{parent}"),
            })
            .unwrap();
        child.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
        child
            .append(SessionEvent::AssistantMessage {
                turn: 1,
                step: 1,
                blocks: vec![ContentBlock::Text {
                    text: "已查完".into(),
                }],
                usage: None,
                interrupted: false,
                source_event_seqs: Vec::new(),
                first_token_time: None,
            })
            .unwrap();
        child
            .append(SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            })
            .unwrap();
        child.flush().unwrap();

        // 新的运行时(等价于进程重启):目录从 header 重建,收件箱为空。
        let restored = Runtime::new(
            &state.home,
            state.sessions.clone(),
            state.live.clone(),
            state.settings.clone(),
            state.events.clone(),
            state.registry.clone(),
            state.workspaces.clone(),
            state.subagent_profiles.clone(),
        )
        .unwrap();
        restored
            .inner
            .selections
            .lock()
            .unwrap()
            .insert(parent.clone(), selection);
        state.live.get(&parent).unwrap().running.store(false, Ordering::SeqCst);
        restored.attach(&state.driver);

        let notice = wait_for_settled_notice(&state, &parent, &child_id, "子代理执行结束").await;
        assert!(notice.contains("结果：已完成"), "{notice}");
        // 幂等:重跑恢复不得重复通知同一次终态。
        restored.recover_deliveries().await.unwrap();
        assert_eq!(settled_notice_count(&state, &parent, &child_id), 1);
    }

    /// 崩溃窗口的另一半:收件箱已落盘、唤醒还没发生,重启后必须自动唤醒
    /// 父代理并把消息投进模型历史。
    #[tokio::test]
    async fn restart_recovery_wakes_parent_for_pending_inbox() {
        let (state, ctx) = setup().await;
        let parent = ctx.session_id.clone().unwrap();
        let selection = ctx.selection.clone().unwrap();
        let notice_id = "settled:orphan:turn:2:seq:7";
        let live = state.live.get(&parent).unwrap();
        live.session
            .append(SessionEvent::AgentInbox {
                id: notice_id.into(),
                text: "[子代理执行结束] 补投路径".into(),
                source: "subagent-settled".into(),
            })
            .unwrap();
        live.session.flush().unwrap();
        live.running.store(false, Ordering::SeqCst);

        let restored = Runtime::new(
            &state.home,
            state.sessions.clone(),
            state.live.clone(),
            state.settings.clone(),
            state.events.clone(),
            state.registry.clone(),
            state.workspaces.clone(),
            state.subagent_profiles.clone(),
        )
        .unwrap();
        restored
            .inner
            .selections
            .lock()
            .unwrap()
            .insert(parent.clone(), selection);
        restored.attach(&state.driver);

        let mut delivered = false;
        for _ in 0..300 {
            if state.live.get(&parent).unwrap().session.events().iter().any(
                |event| matches!(&event.event, SessionEvent::AgentDelivery { id, .. } if id == notice_id),
            ) {
                delivered = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(delivered, "重启后必须自动唤醒父代理并领取待收消息");
    }

    /// 启动快照不可用 → 拒绝启动 → 失败终态必须通知父代理,且不得因为
    /// 派遣消息还挂在收件箱而被当成"仍在运行"。
    #[tokio::test]
    async fn snapshot_failure_settles_child_and_notifies_parent_once() {
        let (state, ctx) = setup().await;
        let parent = ctx.session_id.clone().unwrap();
        let runtime = state.runtime.clone();
        let child = runtime
            .execute("spawn_agent", json!({"prompt":"占用","model":"hold"}), &ctx)
            .await
            .unwrap()["childId"]
            .as_str()
            .unwrap()
            .to_string();
        wait_for_running(&state, &child).await;
        // 先让子代理正常收场,再破坏它的运行快照。
        runtime.interrupt(&parent, &child).await.unwrap();
        settle(&runtime, &child).await;
        let before = settled_notice_count(&state, &parent, &child);
        std::fs::write(
            state.sessions.root().join(&child).join("subagent.json"),
            "{\"broken\":true}",
        )
        .unwrap();

        runtime
            .execute("send_message", json!({"target":child,"message":"继续"}), &ctx)
            .await
            .unwrap();
        let notice = wait_for_settled_notice(&state, &parent, &child, "拒绝启动").await;
        assert!(notice.contains("子代理执行结束"), "{notice}");
        assert!(
            notice.contains("失败") || notice.contains("错误"),
            "必须有明确失败终态:{notice}"
        );
        // 明确结算:不再停在"待处理",查询立即给一致快照。
        let ready = runtime
            .execute("wait_agent", json!({"target":child}), &ctx)
            .await
            .unwrap();
        assert_eq!(ready["status"], "ready", "{ready}");
        assert_eq!(ready["running"], false);
        assert_eq!(
            settled_notice_count(&state, &parent, &child),
            before + 1,
            "同一轮失败只允许一条完成通知"
        );
    }
}