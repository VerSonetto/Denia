//! Rust 原生子代理编排；复用 SessionDriver，只有一份会话日志与运行状态。
use crate::{
    jobs::Jobs,
    lifecycle::{Completion, CompletionQueue},
    state::{LiveSessions, ModelActivity, RunningGuard, ServerEvent},
};
use async_trait::async_trait;
use denia_core::{
    config::ModelSelection,
    session::{GoalOp, SessionEnvelope, SessionEvent, SubagentDescriptor, TurnEndReason},
    task::{StaleReason, TaskState, ValidationVerdict, VerificationState},
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

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Child {
    id: String,
    parent_id: String,
    descriptor: SubagentDescriptor,
}

#[derive(Default)]
struct Inbox {
    seq: u64,
    pending: BTreeMap<u64, (String, String, String)>,
    seen: HashSet<String>,
}

/// 一条排队中的「引导」消息(运行时内存态)。
#[derive(Debug, Clone)]
pub struct SteeringMessage {
    /// 客户端生成的稳定 id:送达事件与队列项一一对应。
    pub id: String,
    pub text: String,
    pub images: Vec<denia_core::message::ImageData>,
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
    delivery: Arc<CompletionQueue>,
    paused: Mutex<HashSet<String>>,
    /// 用户「引导」消息:运行中排在这里,由驱动器在 step 边界取走并落成
    /// 真实用户消息。只驻内存 —— 进程重启时轮次已中止,前端持久化的队列
    /// 会在下一次空闲时按普通消息重发,不会丢失。
    steering: Mutex<HashMap<String, std::collections::VecDeque<SteeringMessage>>>,
    /// 会话轨迹/日志查询的辅助记录（队列发送、子代理定义等不落在
    /// SessionEvent 里的动作）。主轨迹仍直接投影会话日志。
    trace: crate::trajectory::TraceStore,
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
                delivery: Arc::new(CompletionQueue::default()),
                paused: Mutex::new(HashSet::new()),
                steering: Mutex::new(HashMap::new()),
                trace: crate::trajectory::TraceStore::new(home),
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
                        // 结果先进入统一生命周期队列；是否立刻交给模型由
                        // 队列按“模型是否正在输出/执行工具”裁决。
                        if let Some(result) = runtime.inner.jobs.claim_notice(&job.id, &job.owner) {
                            let max = runtime.config().output_bytes * 2;
                            let mut text = format!("[后台任务完成]\n{result}");
                            if text.len() > max {
                                let suffix =
                                    "\n（后台任务结果已截断，完整输出请用 job_output 读取。）";
                                let cut = truncate_utf8(&text, max.saturating_sub(suffix.len()))
                                    .to_string();
                                text = format!("{cut}{suffix}");
                            }
                            let id = format!("job:{}", job.id);
                            runtime.inner.delivery.push(
                                Completion {
                                    child: job.owner.clone(),
                                    target: job.owner.clone(),
                                    id: id.clone(),
                                    legacy_id: id,
                                    text,
                                    source: "job-completed".into(),
                                },
                                false,
                            );
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
    /// 会话回退后的运行时内存态同步。
    ///
    /// rewind 是**物理截断**：被截掉那支连同它的 seq 一起消失，重建的分支
    /// 从 `toSeq` 重新编号。任何按 seq 记账的每会话状态在回退后都会指向旧
    /// 分支——最要命的是收件箱水位：水位落在新日志里更靠前的位置，水位之下
    /// 新追加的 `AgentInbox` 永远不会被扫描。表现就是子代理完成通知既不落
    /// `AgentDelivery`（模型看不到结果），收件箱也被判成空的（`resume_pending`
    /// 不唤醒），必须等用户再发一条消息才可能收到。
    ///
    /// 摘除整条记录而不是回拨水位：重新扫描截断后的日志才是“新分支还有哪
    /// 些待收消息”的唯一来源——旧分支那几条 `AgentInbox` 已经不在了。
    pub fn on_session_rewound(&self, id: &str) {
        self.inner.inboxes.lock().unwrap().remove(id);
        self.inner.completion_marks.lock().unwrap().remove(id);
        self.inner.deferred_wakes.lock().unwrap().remove(id);
        self.inner.wakes.lock().unwrap().remove(id);
        // 回退是用户的重定向动作：旧分支留下的暂停标记不再作数（与
        // `human_turn` 清暂停同一口径），否则自动唤醒会被永久挡住。
        self.inner.paused.lock().unwrap().remove(id);
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
}

mod artifacts;
mod capabilities;
mod completion;
mod config;
mod definition;
mod delegate;
mod handoff;
mod inbox;
mod registry;
mod steering;
#[cfg(test)]
mod tests;
mod trace;
mod turn;

use artifacts::{ArtifactRef, artifacts, display_path, push_unique, tool_path_argument};
use capabilities::background_result;
use config::execution_limits_from;
pub use config::{GoalsConfig, RuntimeConfig, validate_config, validate_goals_config};
pub(crate) use delegate::read_subagent_snapshot;
use handoff::{subagent_handoff, subagent_handoff_at, truncate_utf8, turn_end_label};
use inbox::pending_interactions;
