//! The event-sourced session vocabulary and its model-history projection.
//!
//! The append-only log is the source of truth; [`derive_messages`] projects
//! the wire history from it. Envelopes serialize flat and internally tagged:
//! `{ "seq", "time", "type", ...payload }`.

use serde::{Deserialize, Serialize};

use crate::config::LlmCallConfig;
use crate::error::LlmFailure;
use crate::message::{ChatMessage, ToolCallRef};
use crate::stream::{ContentBlock, StreamChunk, TokenUsage};
use crate::tool::ToolSchema;

/// On-disk and wire format version; pinned while unreleased.
pub const SESSION_FORMAT_VERSION: u32 = 0;

/// First JSONL line of every session file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionHeader {
    #[serde(rename = "type")]
    pub kind: SessionHeaderKind,
    pub version: u32,
    pub id: String,
    pub created_at: u64,
    pub cwd: String,
    /// Confines file tools to `cwd`; orthogonal to the directory choice.
    #[serde(default = "default_true")]
    pub sandbox: bool,
    /// 分支来源:本会话由哪个父会话 fork 而来(dsh parentSession 血缘)。
    /// 旧日志/普通会话无此字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<SubagentDescriptor>,
}

/// 子代理身份与恢复配置随会话头持久化；普通用户分支没有此描述符。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentDescriptor {
    pub label: String,
    pub depth: usize,
    pub mode: String,
    pub selection: crate::config::ModelSelection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionHeaderKind {
    Session,
}

/// Why one turn closed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TurnEndReason {
    Completed,
    /// 取消中断(对齐 dsh `aborted` + `AgentCancelCause`);cause 缺失 = 旧日志。
    Aborted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cause: Option<AbortCause>,
    },
    MaxTokens,
    Error {
        failure: LlmFailure,
    },
    /// 死循环保护:模型连续输出完全相同的内容(回复文本或工具调用序列)
    /// 达到阈值,driver 强制中断本轮。`repeats` 是触发时的连续重复次数。
    LoopDetected {
        repeats: u32,
    },
    /// 崩溃孤儿轮次的合成闭合(对齐 dsh `interrupted`;仅加载时生成,loop 不发射)。
    Interrupted,
}

/// 取消的发起方(对齐 dsh `AgentCancelCause`;`Legacy` 兼容旧日志无 cause 记录)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum AbortCause {
    User,
    Parent,
    Hook { reason: String },
    Disposed,
    Legacy,
}

/// 会话当前权限模式(四档,自设计策略引擎)。
///
/// serde `alias` 承担旧日志兼容:历史 JSONL 里的三档值在反序列化时
/// 落到语义等价的新档位,新事件只写新值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    /// 只读:一切写类操作(文件写、有写副作用的命令)被策略拒绝。
    ReadOnly,
    /// 自动编辑:工作区内文件编辑与命令自动放行,越界写文件走审批。
    #[serde(alias = "workspace-write")]
    AutoEdit,
    /// 计划:只读执行面 + `exit_plan` 提交计划等用户审批。
    Plan,
    /// 完全访问:全部自动放行,路径可出工作区,不弹审批。
    #[serde(alias = "danger-full-access")]
    Full,
}

impl PermissionMode {
    /// 是否完全访问档(权限最高,无审批、路径不受限)。
    pub fn is_full(self) -> bool {
        matches!(self, Self::Full)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::AutoEdit => "auto-edit",
            Self::Plan => "plan",
            Self::Full => "full",
        }
    }

    /// 从字符串解析权限模式;同时接受新值与旧日志三档别名。
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "read-only" => Some(Self::ReadOnly),
            "auto-edit" | "workspace-write" => Some(Self::AutoEdit),
            "plan" => Some(Self::Plan),
            "full" | "danger-full-access" => Some(Self::Full),
            _ => None,
        }
    }
}

/// 审批策略(遗留词汇):仅旧日志的 `approval-policy` 事件仍携带该值;
/// 新事件不再发射,折叠时按 no-op 处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalPolicy {
    Ask,
    Never,
}

impl ApprovalPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Never => "never",
        }
    }
}

/// 一次审批请求的闭合结果(抄 dsh ApprovalOutcome)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalOutcome {
    AllowedOnce,
    Rejected,
    Cancelled,
    Unavailable,
}

/// 计划审批(`exit_plan`)的闭合决策:批准可携带执行档位与模型选择,
/// 拒绝可携带补充建议(驱动模型同轮修订重提)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanReviewDecision {
    pub outcome: ApprovalOutcome,
    /// 批准时用户选定的执行档位(仅 auto-edit / full);缺省 auto-edit。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execute_mode: Option<PermissionMode>,
    /// 批准时用户选定的执行模型;缺省沿用当前 selection。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<crate::config::ModelSelection>,
    /// 所选模型是否可识图(宿主在应答时解析目录后回填,供 driver 同步
    /// 视觉注入开关);缺省沿用当前值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision_supported: Option<bool>,
    /// 用户补充建议:批准时随执行参考,拒绝时驱动重写。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<String>,
}

/// 向用户提问时可选项(denia `ask` 工具,对照 dsh `AskUserQuestionOption`)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskOption {
    /// 用户可见的选项标签,也是回答里回传的取值。
    pub label: String,
    /// 一句话说明取舍/影响;UI 渲染在标签下方。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 模型推荐的选项。结构化字段,不靠标签后缀约定(对照 dsh 的
    /// "(Recommended)" 字符串约定:多语言/全半角括号都会失配)。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recommended: bool,
}

/// 一个待回答的问题。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskQuestion {
    /// 调用方给的稳定 id,原样回显在回答里(配对答案与问题)。
    pub id: String,
    /// 问题正文。
    pub question: String,
    /// 可选短标题(如"确认""选择模式")。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// 可选补充说明(可含 markdown);渲染在问题与选项之间,不混进选项标签。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// 可选选项;为空表示纯自由文本回答。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<AskOption>,
    /// 是否允许多选。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub multi_select: bool,
    /// 是否允许自由文本补充(缺省 true)。
    #[serde(default = "default_true")]
    pub allow_custom: bool,
}

/// 一个问题的回答。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskAnswer {
    /// 对应的 `AskQuestion::id`。
    pub id: String,
    /// 选中的选项标签(多选可多项;单选 + custom 时为空)。
    #[serde(default)]
    pub selected: Vec<String>,
    /// 自由文本补充。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<String>,
    /// 用户显式跳过本题(区别于"空回答":模型据此知道是没答还是没看到)。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skipped: bool,
}

/// 一次提问的结局(denia 扩展:dsh 只有 answered/cancelled 两种,
/// 无超时、无"通道不可用",模型无法区分"没人答"和"答了空")。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AskOutcome {
    /// 用户已作答(可能含跳过项)。
    Answered,
    /// 等待超时:用户未在预算内作答,模型应据现有信息继续或改道。
    TimedOut,
    /// 用户主动取消整组提问。
    Cancelled,
    /// 无可用应答通道(如子代理会话、无 UI 的部署)。
    Unavailable,
}

/// 一次提问的闭合结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskResolution {
    pub outcome: AskOutcome,
    /// 已作答/已跳过的条目;超时与取消时保留已收到的部分。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answers: Vec<AskAnswer>,
    /// `Unavailable` 时的原因(可执行提示)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One entry in the session's todo list — the unit of the `todo-write`
/// whole-list snapshot.
///
/// Deliberately minimal: a human-readable `content` line and a three-state
/// `status`. No id, priority, or ordering field — the list is replaced
/// wholesale on every write (last-write-wins), so entries need no identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    /// What the task is — a short imperative line shown in the UI.
    pub content: String,
    /// Lifecycle state; `in_progress` marks work being done right now.
    pub status: TodoStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// 会话目标的生命周期状态(工作逻辑对照 codex thread goals;codex 的
/// `usage_limited` 是账号配额概念,denia 走 API key 无此反馈,不设)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GoalStatus {
    /// 进行中:harness 会在会话空闲时自动续跑(goal 轮)。
    Active,
    /// 已暂停:用户手动暂停或中止了运行中的轮次,等待恢复。
    Paused,
    /// 受阻:模型标记无法推进,或执行连续失败达到上限。
    Blocked,
    /// 预算耗尽:tokens_used 达到 token_budget;只允许一次收尾轮。
    BudgetLimited,
    /// 已完成:模型标记目标达成;终态,只能清除后重建。
    Complete,
}

impl GoalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::BudgetLimited => "budget-limited",
            Self::Complete => "complete",
        }
    }

    /// 中文状态标签(模型侧注入与工具结果的统一口径)。
    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "进行中",
            Self::Paused => "已暂停",
            Self::Blocked => "受阻",
            Self::BudgetLimited => "预算已用尽",
            Self::Complete => "已完成",
        }
    }
}

/// 会话目标的持久化快照(`goal` 事件折叠后的当前值)。
///
/// 记账采用减法:`tokens_used = 会话累计 token - base_tokens`,因此
/// `base_tokens` 记录目标激活时刻的会话累计;pause/resume 不重置总账,
/// 一个目标生命周期内的花费连续累计。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalState {
    pub objective: String,
    pub status: GoalStatus,
    /// token 预算(可选);`None` = 不限,续跑只受轮数上限约束。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    /// 受阻原因(blocked 时的 hover 提示与模型侧上下文)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
    /// 目标激活时刻的会话累计 token 快照(减法记账基数)。
    #[serde(default)]
    pub base_tokens: u64,
    /// 已发起的 goal 续跑轮数(含预算收尾轮;用户手动轮次不计)。
    #[serde(default)]
    pub rounds_started: u32,
    pub created_at: u64,
    pub updated_at: u64,
}

/// 目标操作(事件负载)。状态机转换见 [`apply_goal_op`]:合法转换生效,
/// 非法转换在折叠时忽略(回放健壮性);调用方(API/工具)应在写事件前
/// 校验转换合法性并显式报错,两层各司其职。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum GoalOp {
    /// 创建目标(仅当当前无目标;已存在时忽略——用户先清除再重建)。
    Set {
        objective: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_budget: Option<u64>,
    },
    /// 编辑目标文本与/或预算。blocked/budget-limited 下编辑视为用户重新
    /// 出发,状态回到 active;paused 保持暂停;complete 忽略(终态)。
    Edit {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        objective: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_budget: Option<u64>,
    },
    /// 暂停(仅 active 生效)。
    Pause,
    /// 恢复(paused/blocked → active;budget-limited/complete 拒绝)。
    Resume,
    /// goal 续跑轮 +1(由续跑发起方在 turn 开始前写)。
    Round,
    /// 模型标记目标达成。
    Complete,
    /// 标记受阻(模型无法推进;连续执行失败达到上限时也走这里)。
    Block {
        #[serde(default)]
        reason: String,
    },
    /// token 预算耗尽(active → budget-limited);随后至多发起一轮收尾。
    BudgetLimit,
    /// 清除目标(唯一回到"无目标"的操作)。
    Clear,
}

/// 目标状态机的集中折叠:返回操作后的新状态;`None` 输入仅接受 `Set`,
/// `None` 输出仅由 `Clear` 产生。`activation_tokens` 是 `Set` 时刻的会话
/// 累计 token(减法记账基数),由持有 meter 的折叠方传入。
pub fn apply_goal_op(
    current: Option<GoalState>,
    op: &GoalOp,
    now: u64,
    activation_tokens: u64,
) -> Option<GoalState> {
    let touch = |mut goal: GoalState| {
        goal.updated_at = now;
        goal
    };
    match op {
        GoalOp::Set {
            objective,
            token_budget,
        } => {
            // 已有目标时忽略——例外:complete 是终态,用户重新设置目标
            // (GoalBar 已不可见)视为直接重开,替换旧目标。
            if current
                .as_ref()
                .is_some_and(|goal| goal.status != GoalStatus::Complete)
            {
                return current;
            }
            Some(GoalState {
                objective: objective.clone(),
                status: GoalStatus::Active,
                // 0 与缺省同义:不设预算(否则 0 预算会立刻耗尽)。
                token_budget: token_budget.filter(|budget| *budget > 0),
                blocked_reason: None,
                base_tokens: activation_tokens,
                rounds_started: 0,
                created_at: now,
                updated_at: now,
            })
        }
        GoalOp::Edit {
            objective,
            token_budget,
        } => {
            let Some(mut goal) = current.clone() else {
                return None;
            };
            if goal.status == GoalStatus::Complete {
                return Some(goal);
            }
            if let Some(text) = objective {
                if text.trim().is_empty() {
                    return Some(goal);
                }
                goal.objective = text.trim().to_string();
            }
            // Some(0) 显式清除预算;其余非零值更新。
            if let Some(budget) = token_budget {
                goal.token_budget = if *budget == 0 { None } else { Some(*budget) };
            }
            // 用户编辑 = 重新出发:受阻/预算耗尽回到进行中;暂停保持暂停。
            if matches!(
                goal.status,
                GoalStatus::Blocked | GoalStatus::BudgetLimited
            ) {
                goal.status = GoalStatus::Active;
                goal.blocked_reason = None;
            }
            Some(touch(goal))
        }
        GoalOp::Pause => {
            let Some(mut goal) = current.clone() else {
                return None;
            };
            if goal.status == GoalStatus::Active {
                goal.status = GoalStatus::Paused;
            }
            Some(touch(goal))
        }
        GoalOp::Resume => {
            let Some(mut goal) = current.clone() else {
                return None;
            };
            if matches!(goal.status, GoalStatus::Paused | GoalStatus::Blocked) {
                goal.status = GoalStatus::Active;
                goal.blocked_reason = None;
            }
            Some(touch(goal))
        }
        GoalOp::Round => {
            let Some(mut goal) = current.clone() else {
                return None;
            };
            goal.rounds_started = goal.rounds_started.saturating_add(1);
            Some(touch(goal))
        }
        GoalOp::Complete => {
            let Some(mut goal) = current.clone() else {
                return None;
            };
            if goal.status != GoalStatus::Complete {
                goal.status = GoalStatus::Complete;
                goal.blocked_reason = None;
            }
            Some(touch(goal))
        }
        GoalOp::Block { reason } => {
            let Some(mut goal) = current.clone() else {
                return None;
            };
            if goal.status != GoalStatus::Complete {
                goal.status = GoalStatus::Blocked;
                goal.blocked_reason =
                    (!reason.trim().is_empty()).then(|| reason.trim().to_string());
            }
            Some(touch(goal))
        }
        GoalOp::BudgetLimit => {
            let Some(mut goal) = current.clone() else {
                return None;
            };
            if goal.status == GoalStatus::Active {
                goal.status = GoalStatus::BudgetLimited;
            }
            Some(touch(goal))
        }
        GoalOp::Clear => None,
    }
}

/// 为何写入 `request-header` 快照(对齐 dsh `RequestHeaderReason`)。
///
/// - `initial` — 日志里第一条 header(全新会话的第一次请求);
/// - `resume` — 日志已有 header,本次 loop 实例的第一次请求(重启/续开/fork 种子);
/// - `change` — 后续请求的 header 与上次不同(同时开启新消息列);
/// - `series` — 头未变但开始显式独立消息列(本仓暂不产出,保留位对齐)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RequestHeaderReason {
    Initial,
    Resume,
    Change,
    Series,
}

/// 一次模型请求的完整头部快照(对齐 dsh `EpochHeader`):调用配置 +
/// 模型实际收到的系统提示 + 工具 schema。日志专用,不进入派生历史;
/// 最近的快照即可重建一次请求的形态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestHeaderSnapshot {
    /// 调用配置(provider/model/推理强度/采样参数)。
    pub config: LlmCallConfig,
    /// 渲染后的完整系统提示(含模型框架,模型实际收到的原文);无 system 请求时缺省。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// 组装好的工具 schema 列表;无工具请求时缺省。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSchema>,
}

/// 工具调用的内部失败身份(对齐 dsh `tool/result.error { name, code }`)。
/// 与 `ToolResult.error`(模型可见错误文本)互补:这里记的是工具内部的
/// 失败种类与码,供诊断聚合,不进入派生历史。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolFailureIdentity {
    pub name: String,
    pub code: String,
}

/// 一次工具输出被截断的事实(落盘层统一输出预算产出);模型可见的截断
/// 提示已并入 `ToolResult.content` 尾部,这里是给 UI 徽标用的结构化数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TruncationInfo {
    /// 截断前的总字符数。
    pub total_chars: u64,
    /// 实际保留(展示给模型)的字符数,不含截断提示本身。
    pub shown_chars: u64,
}

/// The durable event vocabulary, internally tagged on `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum SessionEvent {
    /// 已准入但尚未在模型步边界领取的代理/任务通知。
    AgentInbox {
        id: String,
        text: String,
        source: String,
    },
    /// 与文本投影一起原子落盘的领取记录，重启不会重复领取。
    AgentDelivery {
        id: String,
        text: String,
        source: String,
    },
    TurnStart {
        turn: u32,
    },
    TurnEnd {
        turn: u32,
        reason: TurnEndReason,
    },
    StepStart {
        turn: u32,
        step: u32,
    },
    StepEnd {
        turn: u32,
        step: u32,
    },
    UserMessage {
        text: String,
        /// harness 注入的纠错/上下文消息,非用户手打;UI 弱化渲染。
        #[serde(default)]
        injected: bool,
        /// 注入通道名(workspace-instructions / capability / skill-catalog /
        /// feedback / gesture-skill / file-notice / quote / runtime-context /
        /// image 等);None = 真实用户消息或旧日志。通道识别以本字段优先,
        /// 旧日志回退到文本前缀判断。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
        /// 用户粘贴/上传的内联图片(仅 vision 模型;旧日志无此字段)。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<crate::message::ImageData>,
    },
    /// 模型请求使用的系统提示词(用户可见副本,不含优先级框架)。
    SystemPrompt {
        turn: u32,
        step: u32,
        text: String,
    },
    AssistantChunk {
        turn: u32,
        step: u32,
        chunk: StreamChunk,
    },
    AssistantMessage {
        turn: u32,
        step: u32,
        blocks: Vec<ContentBlock>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        interrupted: bool,
        /// 构建本条消息的 chunk 事件 seq 引用(对齐 dsh `sourceEventSeqs`)。
        /// 空流(无任何 chunk)时为缺省,不记录。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        source_event_seqs: Vec<u64>,
    },
    ToolCall {
        turn: u32,
        step: u32,
        call_id: String,
        name: String,
        arguments: String,
    },
    ToolResult {
        turn: u32,
        step: u32,
        call_id: String,
        content: String,
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// 工具内部失败身份(对齐 dsh `tool/result.error{name,code}`);None = 无内部标识。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_identity: Option<ToolFailureIdentity>,
        /// 工具私有展示载荷(对齐 dsh `tool/result.meta`);核心不解释其形状。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<serde_json::Value>,
        /// 输出截断事实(统一输出预算产出);None = 未截断。旧日志无此字段。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        truncation: Option<TruncationInfo>,
        /// 工具结果剪枝替换:本事件是对旧 `ToolResult` 事件(seq)的 surface
        /// 替换,旧节点不再进入模型历史与 token-meter 表面(对齐 dsh
        /// `tool/result` 的 `surfaceOp.replace`)。None = 普通追加。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replaces: Option<u64>,
    },
    /// LLM 总结压缩(对齐 Claude Code compact 设计):把 `replaces_from..=
    /// replaces_to` 的事件区间折叠成一条摘要消息,保留窗口从 `keep_from`
    /// 开始。日志保持 append-only:区间内旧事件仍在磁盘上,只是不再进入
    /// 派生历史与 token-meter 表面。
    CompactionSummary {
        turn: u32,
        step: u32,
        /// 压缩后插入的摘要文本(模型可见,user role)。
        summary: String,
        /// 被压缩的事件 seq 区间(含)。区间内的消息不再进入派生历史。
        replaces_from: u64,
        replaces_to: u64,
        /// 压缩后保留窗口的起始事件 seq。
        keep_from: u64,
        /// 压缩前/后的启发式 token 估算(调试与 UI 展示用)。
        #[serde(default)]
        pre_tokens: u64,
        #[serde(default)]
        post_tokens: u64,
    },
/// Whole-list todo snapshot; latest write wins on replay. Log-only UI
/// state — never part of the derived model history.
TodoWrite {
    todos: Vec<TodoItem>,
},
/// 会话目标(goal 模式)的一次操作。日志持久、可回放、fork 随种子继承,
/// 不进入模型历史;模型侧的目标感知走 `channel: "goal"` 注入消息。
/// 投影语义见 [`apply_goal_op`]:操作即意图,状态机集中折叠。
Goal {
    op: GoalOp,
},
/// 本地斜杠命令的回显(如 `/goal <目标>`):落日志获得真实 seq,前端
/// 按序渲染为右对齐命令气泡——命令的"发送"必须有可见且排序正确的
/// 结果。仅日志;不进入模型历史,也不携带任何目标状态语义。
CommandRun {
    /// 命令名(如 `goal`)。
    name: String,
    /// 用户输入的完整原文(含 `/name` 前缀)。
    text: String,
},
    /// 会话权限模式切换(抄 dsh sandbox/mode):日志持久、可回放,
    /// 不进入模型历史;driver 读 fold 后的当前值做策略判断。
    PermissionMode {
        mode: PermissionMode,
    },
    /// 会话审批策略切换(遗留事件):新代码不再发射;变体保留以解析
    /// 旧日志,折叠为 no-op。
    ApprovalPolicy {
        policy: ApprovalPolicy,
    },
    /// 一次待审批的工具调用(抄 dsh approval/asked):driver 阻塞等待
    /// 用户通过 REST 决策,审批期间 UI 依据该事件弹窗。
    ApprovalAsked {
        request_id: String,
        call_id: String,
        tool: String,
        args_preview: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// 一次审批请求的闭合结果(抄 dsh approval/decided)。
    ApprovalDecided {
        request_id: String,
        outcome: ApprovalOutcome,
    },
    /// 模型向用户发起一组提问(denia `ask` 工具):工具调用阻塞等待应答,
    /// UI 依据该事件渲染问答卡片;落盘后刷新页面也能恢复挂起态。
    AskRequested {
        request_id: String,
        /// 发起本次提问的工具调用 id(UI 把卡片挂在对应工具行上)。
        call_id: String,
        questions: Vec<AskQuestion>,
        /// 等待预算(毫秒);到点未答按 `TimedOut` 结算。
        timeout_ms: u64,
    },
    /// 一次提问的闭合结果。
    AskResolved {
        request_id: String,
        resolution: AskResolution,
    },
    /// 下一个模型请求的完整头部快照(对齐 dsh `request/header`),在其 step
    /// 内、请求 dispatch 之前落盘。仅日志;最近的快照重建请求形态。
    /// 按 dsh 语义按需写入:日志无 header 时写 `initial`,loop 恢复时写
    /// `resume`,header 与上次不同时写 `change`(带 `starts_series`),
    /// 相同则不重复写。
    RequestHeader {
        turn: u32,
        step: u32,
        header: RequestHeaderSnapshot,
        reason: RequestHeaderReason,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        starts_series: bool,
    },
    /// 下一个请求的路由元数据(对齐 dsh `request/context`):仅在路由或
    /// 容量(上下文窗口)变化时记录;不参与请求重建与 header 相等性判断。
    RequestContext {
        turn: u32,
        step: u32,
        provider: String,
        model: String,
        /// 路由通告的最大上下文(输入+输出,token);未通告时缺省。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_window: Option<u64>,
    },
    /// 一次模型请求重试尝试的轨迹(对齐 dsh llm-retry 的事件化重试):
    /// 提供方抖动时 harness 按退避重发,每次尝试失败落一条。仅日志,
    /// 不进入模型历史;`delay_ms` 是本次失败后的退避(下一次尝试前等待)。
    RetryAttempt {
        turn: u32,
        step: u32,
        attempt: u32,
        code: String,
        message: String,
        delay_ms: u64,
    },
}

/// One log entry: monotonic coordinates plus the event payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEnvelope {
    /// Contiguous, starts at 1.
    pub seq: u64,
    /// Unix epoch milliseconds.
    pub time: u64,
    #[serde(flatten)]
    pub event: SessionEvent,
}

/// Content of the synthetic result appended for tool calls the log never
/// answered (aborts, crashes) so the derived wire history stays valid.
pub const INTERRUPTED_TOOL_RESULT: &str = "[tool execution was interrupted]";

/// 会话分支(抄 dsh `session.fork` 的边界切割语义)在日志前缀上的纯函数:
/// 返回种子事件数量(切片右端,不含)。
///
/// - `at_seq` 给定且未越过日志末尾:锚定到第一个 `seq >= at_seq` 的
///   `turn-end`(锚点落在某个已完成轮次内,切点绝不回退到上一轮)。
/// - `at_seq` 未给或越过末尾:锚定到最后一个 `turn-end`。
/// - 找不到边界(尚无任何已完成轮次)→ `None`,调用方报 fork-unavailable。
///
/// 与 dsh 的差异:dsh 的 turn/end 后可能残留属于该轮的冻结节点(需要
/// 延伸到下一个 turn/start);本仓驱动器把下一轮的用户消息写在 turn-end
/// 与下一轮 turn-start 之间,那些事件属于下一轮,必须排除——切点严格
/// 落在 `turn-end` 之后。
pub fn fork_cut_index(events: &[SessionEnvelope], at_seq: Option<u64>) -> Option<usize> {
    let last_seq = events.last()?.seq;
    let boundary_idx = match at_seq {
        Some(anchor) if anchor <= last_seq => events.iter().position(|envelope| {
            envelope.seq >= anchor && matches!(envelope.event, SessionEvent::TurnEnd { .. })
        }),
        _ => events
            .iter()
            .rposition(|envelope| matches!(envelope.event, SessionEvent::TurnEnd { .. })),
    }?;
    Some(boundary_idx + 1)
}

/// Projects the model-facing history from the log.
///
/// `user-message` becomes a user message; `assistant-message` becomes an
/// assistant message (skipped when it carries neither text nor tool calls);
/// `tool-result` becomes a tool-role message. Chunks and boundary events
/// project nothing. Tool calls left unanswered by the end of the log get a
/// synthetic interrupted result, in call order, after everything else.
///
/// Tool-result pruning replacements (`tool-result` with `replaces`) fold the
/// surface: the replacement takes the original node's position and the old
/// content no longer reaches the model (对齐 dsh `surfaceOp.replace`)。
pub fn derive_messages(events: &[SessionEnvelope]) -> Vec<ChatMessage> {
    derive_surface(events)
        .into_iter()
        .map(|item| item.message)
        .collect()
}

/// 一条派生 surface 消息及其来源事件 seq。压缩器用它把消息切回事件区间。
#[derive(Debug, Clone)]
pub struct SurfaceMessage {
    pub seq: u64,
    pub message: ChatMessage,
}

/// 派生模型可见历史,带事件 seq(压缩折叠同 [`derive_messages`])。
pub fn derive_surface(events: &[SessionEnvelope]) -> Vec<SurfaceMessage> {
    derive_surface_inner(events)
}

/// [`derive_surface`] 的实现。
fn derive_surface_inner(events: &[SessionEnvelope]) -> Vec<SurfaceMessage> {
    /// 当前模型可见 surface 节点(仅 user/assistant/tool-result 三类有消息)。
    struct SurfaceItem {
        seq: u64,
        message: ChatMessage,
    }

    let mut surface: Vec<SurfaceItem> = Vec::new();
    let mut unanswered: Vec<String> = Vec::new();
    // 压缩折叠:被压缩的区间变成摘要消息,插在保留窗口之前。
    #[derive(Clone)]
    struct PendingSummary {
        keep_from: u64,
        text: String,
    }
    let mut summaries: Vec<PendingSummary> = Vec::new();
    // 在等 tool-result 期间注入的带图用户消息(如 browser 截图):不能插在
    // tool-call 与 tool-result 中间——多数 provider 校验二者必须相邻
    // (MiniMax: "tool call result does not follow tool call")。图片暂存,
    // 合并进紧随的 tool-result(wire 层发多模态 content)。
    let mut pending_tool_images: Vec<crate::message::ImageData> = Vec::new();

    for envelope in events {
        let seq = envelope.seq;
        match &envelope.event {
            SessionEvent::AgentDelivery { text, .. } => {
                surface.push(SurfaceItem {
                    seq,
                    message: ChatMessage::user(text),
                });
            }
            SessionEvent::UserMessage { text, images, .. } => {
                if !unanswered.is_empty() && !images.is_empty() {
                    pending_tool_images.extend(images.clone());
                    // 事件不进 surface:图片由下一条 tool-result 携带。
                    continue;
                }
                let message = if images.is_empty() {
                    ChatMessage::user(text)
                } else {
                    ChatMessage::user_with_images(text, images.clone())
                };
                surface.push(SurfaceItem { seq, message });
            }
            SessionEvent::CompactionSummary {
                summary,
                replaces_from,
                replaces_to,
                keep_from,
                ..
            } => {
                // 区间内的事件不再进入模型历史(日志保持 append-only)。
                surface.retain(|item| item.seq < *replaces_from || item.seq > *replaces_to);
                summaries.push(PendingSummary {
                    keep_from: *keep_from,
                    text: summary.clone(),
                });
            }
            SessionEvent::AssistantMessage { blocks, .. } => {
                let text: String = blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                let calls: Vec<ToolCallRef> = blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                        } => Some(ToolCallRef {
                            id: id.clone(),
                            name: name.clone(),
                            arguments: arguments.clone(),
                        }),
                        _ => None,
                    })
                    .collect();
                if text.is_empty() && calls.is_empty() {
                    continue;
                }
                for call in &calls {
                    unanswered.push(call.id.clone());
                }
                surface.push(SurfaceItem {
                    seq,
                    // 模型历史剥离 reasoning_content:思考过程不回传 provider,
                    // 省 token 且不影响后续决策;UI/日志仍保留完整 blocks。
                    message: ChatMessage::assistant(text, None, calls),
                });
            }
            SessionEvent::ToolResult {
                call_id,
                content,
                replaces,
                ..
            } => {
                unanswered.retain(|id| id != call_id);
                let content = content.clone();
                if let Some(replaced_seq) = replaces {
                    if let Some(item) = surface.iter_mut().find(|item| item.seq == *replaced_seq) {
                        item.message = ChatMessage::tool_result(call_id.clone(), content);
                        continue;
                    }
                }
                if pending_tool_images.is_empty() {
                    surface.push(SurfaceItem {
                        seq,
                        message: ChatMessage::tool_result(call_id, content),
                    });
                } else {
                    // 带图合并:截图等工具图片挂到本条 tool-result 上,
                    // wire 层把 content 升级为 text+image_url 多模态数组。
                    let images = std::mem::take(&mut pending_tool_images);
                    let mut merged = content;
                    merged.push_str("\n[附:工具产生的截图已附在本条结果]");
                    let mut message = ChatMessage::tool_result(call_id, merged);
                    message.images = images;
                    surface.push(SurfaceItem { seq, message });
                }
            }
            _ => {}
        }
    }

    // 摘要消息插到保留窗口之前(对齐 compact boundary 语义:摘要代表被
    // 压缩的旧历史,随后是保留窗口,最后是压缩后新追加的消息)。
    // 摘要项 seq 取 keep_from-1:保证排在保留窗口之前,且后续摘要的
    // keep_from 单调不减时不会被 position 查找误命中。
    for summary in summaries {
        let pos = surface
            .iter()
            .position(|item| item.seq >= summary.keep_from)
            .unwrap_or(surface.len());
        surface.insert(
            pos,
            SurfaceItem {
                seq: summary.keep_from.saturating_sub(1),
                message: ChatMessage::user(summary.text),
            },
        );
    }

    let mut messages: Vec<SurfaceMessage> = surface
        .into_iter()
        .map(|item| SurfaceMessage {
            seq: item.seq,
            message: item.message,
        })
        .collect();
    for call_id in unanswered {
        messages.push(SurfaceMessage {
            seq: u64::MAX - messages.len() as u64,
            message: ChatMessage::tool_result(call_id, INTERRUPTED_TOOL_RESULT),
        });
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::BlockType;

    fn envelope(seq: u64, event: SessionEvent) -> SessionEnvelope {
        SessionEnvelope {
            seq,
            time: 1_700_000_000_000 + seq,
            event,
        }
    }

    #[test]
    fn header_round_trips_flat_json() {
        let header = SessionHeader {
            kind: SessionHeaderKind::Session,
            version: SESSION_FORMAT_VERSION,
            id: "0f8c".to_string(),
            created_at: 1_700_000_000_000,
            cwd: "/tmp/work".to_string(),
            sandbox: true,
            parent_session: None,
            subagent: None,
        };
        let json = serde_json::to_string(&header).unwrap();
        assert_eq!(
            json,
            r#"{"type":"session","version":0,"id":"0f8c","created_at":1700000000000,"cwd":"/tmp/work","sandbox":true}"#
        );
        assert_eq!(
            serde_json::from_str::<SessionHeader>(&json).unwrap(),
            header
        );
    }

    #[test]
    fn system_prompt_event_round_trips() {
        let env = envelope(
            3,
            SessionEvent::SystemPrompt {
                turn: 1,
                step: 1,
                text: "你是 agent".into(),
            },
        );
        assert_eq!(
            serde_json::to_string(&env).unwrap(),
            r#"{"seq":3,"time":1700000000003,"type":"system-prompt","turn":1,"step":1,"text":"你是 agent"}"#
        );
    }

    #[test]
    fn derive_messages_merges_tool_time_images_into_tool_result() {
        let events = vec![
            envelope(
                1,
                SessionEvent::UserMessage {
                    text: "去截图".into(),
                    injected: false,
                    images: Vec::new(),
                    channel: None,
                },
            ),
            envelope(
                2,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 1,
                    blocks: vec![ContentBlock::ToolCall {
                        id: "c1".into(),
                        name: "browser".into(),
                        arguments: "{}".into(),
                    }],
                    usage: None,
                    interrupted: false,
                    source_event_seqs: Vec::new(),
                },
            ),
            // 截图注入:插在 tool-call 与 tool-result 之间(原 MiniMax 2013 的时序)
            envelope(
                3,
                SessionEvent::UserMessage {
                    text: "[harness] 浏览器截图已注入".into(),
                    injected: true,
                    images: vec![crate::message::ImageData {
                        mime: "image/png".into(),
                        data: "AAAA".into(),
                    }],
                    channel: None,
                },
            ),
            envelope(
                4,
                SessionEvent::ToolResult {
                    turn: 1,
                    step: 1,
                    call_id: "c1".into(),
                    content: "ok".into(),
                    is_error: false,
                    error: None,
                    error_identity: None,
                    meta: None,
                    replaces: None,
                    truncation: None,
                },
            ),
        ];
        let messages = derive_messages(&events);
        // 期望: user → assistant(tool_call) → tool(带图) — injected 消息不再插队
        assert_eq!(messages.len(), 3, "{messages:?}");
        assert_eq!(messages[2].role, crate::message::ChatRole::Tool);
        assert_eq!(
            messages[2].images.len(),
            1,
            "截图应并入 tool-result: {messages:?}"
        );
        assert!(messages[2].content.contains("截图"));
    }

    #[test]
    fn envelope_serializes_type_tagged_flat() {
        let env = envelope(1, SessionEvent::TurnStart { turn: 1 });
        assert_eq!(
            serde_json::to_string(&env).unwrap(),
            r#"{"seq":1,"time":1700000000001,"type":"turn-start","turn":1}"#
        );
        let chunk_env = envelope(
            2,
            SessionEvent::AssistantChunk {
                turn: 1,
                step: 1,
                chunk: StreamChunk::BlockStart {
                    index: 0,
                    block_type: BlockType::Text,
                },
            },
        );
        let json = serde_json::to_string(&chunk_env).unwrap();
        assert!(json.contains(r#""type":"assistant-chunk""#));
        assert!(json.contains(r#""chunk":{"type":"block-start""#));
        assert_eq!(
            serde_json::from_str::<SessionEnvelope>(&json).unwrap(),
            chunk_env
        );
    }

    #[test]
    fn tool_events_round_trip() {
        let call = envelope(
            3,
            SessionEvent::ToolCall {
                turn: 1,
                step: 1,
                call_id: "call_1".to_string(),
                name: "bash".to_string(),
                arguments: r#"{"command":"echo hi"}"#.to_string(),
            },
        );
        let json = serde_json::to_string(&call).unwrap();
        assert_eq!(
            serde_json::from_str::<SessionEnvelope>(&json).unwrap(),
            call
        );
    }

    #[test]
    fn derive_projects_user_assistant_and_tool() {
        let events = vec![
            envelope(1, SessionEvent::TurnStart { turn: 1 }),
            envelope(
                2,
                SessionEvent::UserMessage {
                    text: "hi".into(),
                    injected: false,
                    images: Vec::new(),
                    channel: None,
                },
            ),
            envelope(3, SessionEvent::StepStart { turn: 1, step: 1 }),
            envelope(
                4,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 1,
                    blocks: vec![
                        ContentBlock::Reasoning { text: "hmm".into() },
                        ContentBlock::Text {
                            text: "running".into(),
                        },
                        ContentBlock::ToolCall {
                            id: "c1".into(),
                            name: "bash".into(),
                            arguments: "{}".into(),
                        },
                    ],
                    usage: None,
                    interrupted: false,
                    source_event_seqs: vec![4],
                },
            ),
            envelope(
                5,
                SessionEvent::ToolCall {
                    turn: 1,
                    step: 1,
                    call_id: "c1".into(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                },
            ),
            envelope(
                6,
                SessionEvent::ToolResult {
                    turn: 1,
                    step: 1,
                    call_id: "c1".into(),
                    content: "exit code: 0".into(),
                    is_error: false,
                    error: None,
                    error_identity: None,
                    meta: None,
                    replaces: None,
                    truncation: None,
                },
            ),
            envelope(
                7,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 2,
                    blocks: vec![ContentBlock::Text {
                        text: "done".into(),
                    }],
                    usage: Some(TokenUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                        cache_read_tokens: None,
                        reasoning_tokens: None,
                    }),
                    interrupted: false,
                    source_event_seqs: Vec::new(),
                },
            ),
            envelope(
                8,
                SessionEvent::TurnEnd {
                    turn: 1,
                    reason: TurnEndReason::Completed,
                },
            ),
        ];
        let messages = derive_messages(&events);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0], ChatMessage::user("hi"));
        let assistant = &messages[1];
        assert_eq!(assistant.content, "running");
        assert_eq!(assistant.reasoning_content.as_deref(), None);
        assert_eq!(assistant.tool_calls.len(), 1);
        assert_eq!(messages[2], ChatMessage::tool_result("c1", "exit code: 0"));
        assert_eq!(messages[3].content, "done");
    }

    #[test]
    fn derive_folds_tool_result_replacement() {
        let events = vec![
            envelope(1, SessionEvent::TurnStart { turn: 1 }),
            envelope(
                2,
                SessionEvent::UserMessage {
                    text: "hi".into(),
                    injected: false,
                    images: Vec::new(),
                    channel: None,
                },
            ),
            envelope(
                3,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 1,
                    blocks: vec![ContentBlock::ToolCall {
                        id: "c1".into(),
                        name: "bash".into(),
                        arguments: "{}".into(),
                    }],
                    usage: None,
                    interrupted: false,
                    source_event_seqs: Vec::new(),
                },
            ),
            envelope(
                4,
                SessionEvent::ToolResult {
                    turn: 1,
                    step: 1,
                    call_id: "c1".into(),
                    content: "x".repeat(100),
                    is_error: false,
                    error: None,
                    error_identity: None,
                    meta: None,
                    replaces: None,
                    truncation: None,
                },
            ),
            envelope(
                5,
                SessionEvent::ToolResult {
                    turn: 1,
                    step: 1,
                    call_id: "c1".into(),
                    content: "pruned".into(),
                    is_error: false,
                    error: None,
                    error_identity: None,
                    meta: None,
                    replaces: Some(4),
                    truncation: None,
                },
            ),
            envelope(
                6,
                SessionEvent::TurnEnd {
                    turn: 1,
                    reason: TurnEndReason::Completed,
                },
            ),
        ];
        let messages = derive_messages(&events);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0], ChatMessage::user("hi"));
        assert_eq!(messages[1].tool_calls.len(), 1);
        assert_eq!(messages[2], ChatMessage::tool_result("c1", "pruned"));
        assert!(!messages[2].content.contains("xxx"));
    }

    #[test]
    fn derive_skips_empty_assistant_and_synthesizes_missing_results() {
        let events = vec![
            envelope(
                1,
                SessionEvent::UserMessage {
                    text: "go".into(),
                    injected: false,
                    images: Vec::new(),
                    channel: None,
                },
            ),
            envelope(
                2,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 1,
                    blocks: vec![],
                    usage: Some(TokenUsage::default()),
                    interrupted: false,
                    source_event_seqs: Vec::new(),
                },
            ),
            envelope(
                3,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 2,
                    blocks: vec![ContentBlock::ToolCall {
                        id: "cX".into(),
                        name: "bash".into(),
                        arguments: "{}".into(),
                    }],
                    usage: None,
                    interrupted: true,
                    source_event_seqs: Vec::new(),
                },
            ),
        ];
        let messages = derive_messages(&events);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1].tool_calls[0].id, "cX");
        assert_eq!(
            messages[2],
            ChatMessage::tool_result("cX", INTERRUPTED_TOOL_RESULT)
        );
    }

    #[test]
    fn finish_reason_error_round_trips() {
        let reason = TurnEndReason::Error {
            failure: LlmFailure::new(crate::error::codes::STEP_LIMIT, "too many steps"),
        };
        let json = serde_json::to_string(&reason).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"error","failure":{"message":"too many steps","code":"STEP_LIMIT"}}"#
        );
    }

    #[test]
    fn aborted_cause_round_trips_and_legacy_json_still_loads() {
        // 新版:带取消来源。
        let with_cause = TurnEndReason::Aborted {
            cause: Some(AbortCause::User),
        };
        let json = serde_json::to_string(&with_cause).unwrap();
        assert_eq!(json, r#"{"kind":"aborted","cause":{"kind":"user"}}"#);
        assert_eq!(
            serde_json::from_str::<TurnEndReason>(&json).unwrap(),
            with_cause
        );
        // 旧日志:{"kind":"aborted"} 无 cause → 兼容加载为 None。
        let legacy = serde_json::from_str::<TurnEndReason>(r#"{"kind":"aborted"}"#).unwrap();
        assert_eq!(legacy, TurnEndReason::Aborted { cause: None });
        // 输出保持旧形状(无 cause 不写字段)。
        assert_eq!(
            serde_json::to_string(&legacy).unwrap(),
            r#"{"kind":"aborted"}"#
        );
    }

    #[test]
    fn interrupted_orphan_close_round_trips() {
        let reason = TurnEndReason::Interrupted;
        let json = serde_json::to_string(&reason).unwrap();
        assert_eq!(json, r#"{"kind":"interrupted"}"#);
        assert_eq!(
            serde_json::from_str::<TurnEndReason>(&json).unwrap(),
            reason
        );
    }

    /// goal 状态机转换矩阵 + 事件 round-trip。
    #[test]
    fn goal_op_state_machine_and_round_trip() {
        let base = |status| GoalState {
            objective: "修完回归".into(),
            status,
            token_budget: None,
            blocked_reason: None,
            base_tokens: 100,
            rounds_started: 2,
            created_at: 1,
            updated_at: 1,
        };
        // Set 仅在无目标时生效。
        assert_eq!(
            apply_goal_op(None, &GoalOp::Set { objective: "x".into(), token_budget: Some(5) }, 7, 42)
                .unwrap()
                .objective,
            "x"
        );
        let seeded = apply_goal_op(None, &GoalOp::Set { objective: "x".into(), token_budget: Some(5) }, 7, 42).unwrap();
        assert_eq!(seeded.base_tokens, 42);
        assert_eq!(seeded.status, GoalStatus::Active);
        assert_eq!(
            apply_goal_op(Some(seeded.clone()), &GoalOp::Set { objective: "y".into(), token_budget: None }, 8, 9)
                .unwrap()
                .objective,
            "x",
            "已有目标时 Set 忽略"
        );
        // Round 累计;Pause/Resume 矩阵。
        assert_eq!(
            apply_goal_op(Some(base(GoalStatus::Active)), &GoalOp::Round, 3, 0).unwrap().rounds_started,
            3
        );
        assert_eq!(
            apply_goal_op(Some(base(GoalStatus::Active)), &GoalOp::Pause, 3, 0).unwrap().status,
            GoalStatus::Paused
        );
        assert_eq!(
            apply_goal_op(Some(base(GoalStatus::Paused)), &GoalOp::Resume, 3, 0).unwrap().status,
            GoalStatus::Active
        );
        assert_eq!(
            apply_goal_op(Some(base(GoalStatus::BudgetLimited)), &GoalOp::Resume, 3, 0).unwrap().status,
            GoalStatus::BudgetLimited,
            "预算耗尽状态 resume 拒绝"
        );
        // Edit:受阻/预算耗尽回 active;complete 终态忽略;暂停保持。
        let edited = apply_goal_op(
            Some(base(GoalStatus::Blocked)),
            &GoalOp::Edit { objective: Some("新目标".into()), token_budget: Some(10) },
            4,
            0,
        )
        .unwrap();
        assert_eq!((edited.status, edited.objective.as_str(), edited.token_budget),
            (GoalStatus::Active, "新目标", Some(10)));
        assert_eq!(
            apply_goal_op(Some(base(GoalStatus::Paused)), &GoalOp::Edit { objective: None, token_budget: None }, 4, 0)
                .unwrap()
                .status,
            GoalStatus::Paused,
            "暂停中编辑保持暂停"
        );
        assert_eq!(
            apply_goal_op(Some(base(GoalStatus::Complete)), &GoalOp::Edit { objective: Some("z".into()), token_budget: None }, 4, 0)
                .unwrap()
                .objective,
            "修完回归",
            "complete 终态忽略编辑"
        );
        // BudgetLimit 仅 active 生效;Clear 回 None。
        assert_eq!(
            apply_goal_op(Some(base(GoalStatus::Paused)), &GoalOp::BudgetLimit, 5, 0).unwrap().status,
            GoalStatus::Paused
        );
        assert_eq!(
            apply_goal_op(Some(base(GoalStatus::Active)), &GoalOp::BudgetLimit, 5, 0).unwrap().status,
            GoalStatus::BudgetLimited
        );
        assert_eq!(apply_goal_op(Some(base(GoalStatus::Active)), &GoalOp::Clear, 6, 0), None);
        assert_eq!(apply_goal_op(None, &GoalOp::Clear, 6, 0), None);
        // 事件序列化:tag + kebab-case。
        let env = envelope(9, SessionEvent::Goal { op: GoalOp::Block { reason: "卡住了".into() } });
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains(r#""type":"goal""#), "{json}");
        assert!(json.contains(r#""kind":"block""#), "{json}");
        assert_eq!(serde_json::from_str::<SessionEnvelope>(&json).unwrap(), env);
        let round = envelope(10, SessionEvent::Goal { op: GoalOp::Round });
        assert!(serde_json::to_string(&round).unwrap().contains(r#""kind":"round""#));
    }

    #[test]
    fn request_header_and_context_round_trip() {
        let header = RequestHeaderSnapshot {
            config: LlmCallConfig {
                provider: "cat".into(),
                model: "glm-5.3-flash".into(),
                reasoning_effort: Some("xhigh".into()),
                temperature: None,
                max_tokens: None,
                stop: Vec::new(),
            },
            system: Some("你是 agent".into()),
            tools: vec![ToolSchema {
                name: "bash".into(),
                description: "run a command".into(),
                parameters: serde_json::json!({ "type": "object" }),
            }],
        };
        let env = envelope(
            3,
            SessionEvent::RequestHeader {
                turn: 1,
                step: 1,
                header: header.clone(),
                reason: RequestHeaderReason::Initial,
                starts_series: false,
            },
        );
        let json = serde_json::to_string(&env).unwrap();
        // 类型标签与关键信息必须可见。
        assert!(json.contains(r#""type":"request-header""#));
        assert!(json.contains(r#""provider":"cat""#));
        assert!(json.contains(r#""reason":"initial""#));
        assert!(!json.contains("startsSeries"));
        assert_eq!(serde_json::from_str::<SessionEnvelope>(&json).unwrap(), env);

        let ctx = envelope(
            4,
            SessionEvent::RequestContext {
                turn: 1,
                step: 1,
                provider: "cat".into(),
                model: "glm-5.3-flash".into(),
                context_window: Some(1_000_000),
            },
        );
        let ctx_json = serde_json::to_string(&ctx).unwrap();
        assert!(ctx_json.contains(r#""type":"request-context""#));
        assert_eq!(
            serde_json::from_str::<SessionEnvelope>(&ctx_json).unwrap(),
            ctx
        );
    }

    #[test]
    fn tool_result_identity_and_meta_round_trip() {
        let env = envelope(
            7,
            SessionEvent::ToolResult {
                turn: 1,
                step: 1,
                call_id: "c1".into(),
                content: "exit code: 0".into(),
                is_error: false,
                error: None,
                error_identity: Some(ToolFailureIdentity {
                    name: "bash".into(),
                    code: "SANDBOX_DENIED".into(),
                }),
                meta: Some(serde_json::json!({ "diff": "…" })),
                replaces: None,
                truncation: None,
            },
        );
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains(r#""error_identity":{"name":"bash","code":"SANDBOX_DENIED"}"#));
        assert!(json.contains(r#""meta":{"diff":"…"}"#));
        assert_eq!(serde_json::from_str::<SessionEnvelope>(&json).unwrap(), env);
    }

    /// 两轮完整对话:1 turn-start, 2 user, 3 assistant, 4 turn-end,
    /// 5 turn-start, 6 user, 7 assistant, 8 turn-end。
    fn two_turn_log() -> Vec<SessionEnvelope> {
        let mut events = vec![
            envelope(1, SessionEvent::TurnStart { turn: 1 }),
            envelope(
                2,
                SessionEvent::UserMessage {
                    text: "first".into(),
                    injected: false,
                    images: Vec::new(),
                    channel: None,
                },
            ),
            envelope(
                3,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 1,
                    blocks: vec![ContentBlock::Text { text: "hi".into() }],
                    usage: None,
                    interrupted: false,
                    source_event_seqs: Vec::new(),
                },
            ),
            envelope(
                4,
                SessionEvent::TurnEnd {
                    turn: 1,
                    reason: TurnEndReason::Completed,
                },
            ),
            envelope(5, SessionEvent::TurnStart { turn: 2 }),
            envelope(
                6,
                SessionEvent::UserMessage {
                    text: "second".into(),
                    injected: false,
                    images: Vec::new(),
                    channel: None,
                },
            ),
            envelope(
                7,
                SessionEvent::AssistantMessage {
                    turn: 2,
                    step: 1,
                    blocks: vec![ContentBlock::Text {
                        text: "done".into(),
                    }],
                    usage: None,
                    interrupted: false,
                    source_event_seqs: Vec::new(),
                },
            ),
            envelope(
                8,
                SessionEvent::TurnEnd {
                    turn: 2,
                    reason: TurnEndReason::Completed,
                },
            ),
        ];
        for (index, envelope) in events.iter_mut().enumerate() {
            envelope.seq = index as u64 + 1;
        }
        events
    }

    #[test]
    fn fork_cut_without_anchor_ends_on_last_completed_turn() {
        let events = two_turn_log();
        // 无锚点:切到最后一个 turn-end(含)= 8 条种子。
        assert_eq!(fork_cut_index(&events, None), Some(8));
    }

    #[test]
    fn fork_cut_anchor_clamps_forward_to_turn_end() {
        let events = two_turn_log();
        // 锚点落在第 1 轮中间:向前找到本轮 turn-end(seq 4),不回退。
        assert_eq!(fork_cut_index(&events, Some(2)), Some(4));
        assert_eq!(fork_cut_index(&events, Some(1)), Some(4));
        // 锚点正好是 turn-end 本身:切在本轮之后。
        assert_eq!(fork_cut_index(&events, Some(4)), Some(4));
        // 锚点指向下一轮的 turn-start:归入第 2 轮之后。
        assert_eq!(fork_cut_index(&events, Some(5)), Some(8));
    }

    #[test]
    fn fork_cut_anchor_beyond_end_falls_back_to_last_turn_end() {
        let events = two_turn_log();
        assert_eq!(fork_cut_index(&events, Some(99)), Some(8));
    }

    #[test]
    fn fork_cut_excludes_next_turn_prelude() {
        // 本仓布局:下一轮的用户消息写在 turn-end 与下一轮 turn-start 之间,
        // 属于下一轮,不进种子(切点严格落在 turn-end 之后)。
        let mut events = two_turn_log();
        events.insert(
            4,
            envelope(
                0,
                SessionEvent::UserMessage {
                    text: "next turn prompt".into(),
                    injected: false,
                    images: Vec::new(),
                    channel: None,
                },
            ),
        );
        for (index, envelope) in events.iter_mut().enumerate() {
            envelope.seq = index as u64 + 1;
        }
        // 锚定第 1 轮:boundary seq 4 → 种子 4 条;下一轮预置消息(seq 5)被排除。
        assert_eq!(fork_cut_index(&events, Some(1)), Some(4));
        // 无锚点:切到最后一个 turn-end(seq 9 → index 8 → 9 条)。
        assert_eq!(fork_cut_index(&events, None), Some(9));
    }

    #[test]
    fn fork_cut_without_any_completed_turn_is_unavailable() {
        let events = vec![
            envelope(1, SessionEvent::TurnStart { turn: 1 }),
            envelope(
                2,
                SessionEvent::UserMessage {
                    text: "hi".into(),
                    injected: false,
                    images: Vec::new(),
                    channel: None,
                },
            ),
        ];
        assert_eq!(fork_cut_index(&events, None), None);
        // 锚点在未完成轮次内且不越过末尾:无边界 → None(dsh fork-unavailable)。
        assert_eq!(fork_cut_index(&events, Some(2)), None);
        // 锚点越过末尾:回退到最后一个 turn-end,仍没有 → None。
        assert_eq!(fork_cut_index(&events, Some(99)), None);
        assert_eq!(fork_cut_index(&[], None), None);
    }

    #[test]
    fn derive_folds_compaction_summary_into_keep_window_order() {
        let events = vec![
            envelope(
                1,
                SessionEvent::UserMessage {
                    text: "old request".into(),
                    injected: false,
                    images: Vec::new(),
                    channel: None,
                },
            ),
            envelope(
                2,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 1,
                    blocks: vec![ContentBlock::Text {
                        text: "old work".into(),
                    }],
                    usage: None,
                    interrupted: false,
                    source_event_seqs: Vec::new(),
                },
            ),
            // 压缩:事件 1..=2 被折叠成摘要,保留窗口从 3 开始。
            envelope(
                3,
                SessionEvent::CompactionSummary {
                    turn: 1,
                    step: 2,
                    summary: "previous work summarized".into(),
                    replaces_from: 1,
                    replaces_to: 2,
                    keep_from: 4,
                    pre_tokens: 100,
                    post_tokens: 10,
                },
            ),
            envelope(
                4,
                SessionEvent::UserMessage {
                    text: "now this".into(),
                    injected: false,
                    images: Vec::new(),
                    channel: None,
                },
            ),
            envelope(
                5,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 3,
                    blocks: vec![ContentBlock::Text {
                        text: "fresh reply".into(),
                    }],
                    usage: None,
                    interrupted: false,
                    source_event_seqs: Vec::new(),
                },
            ),
        ];
        let messages = derive_messages(&events);
        // 摘要、保留窗口、新消息,顺序正确;被压缩的旧消息不再出现。
        assert_eq!(messages.len(), 3);
        assert!(messages[0].content.contains("previous work summarized"));
        assert_eq!(messages[1].content, "now this");
        assert_eq!(messages[2].content, "fresh reply");
    }
}
