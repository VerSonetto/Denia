//! The `get_goal` / `update_goal` tools:模型侧的会话目标读写入口。
//!
//! 状态的真值在会话日志的 `goal` 事件流(`apply_goal_op` 集中折叠);
//! 本模块只做两件事:把当前投影读给模型(`get_goal`,经
//! [`ToolContext::goal_reader`]),把模型的状态变更以操作事件落盘
//! (`update_goal`,经 `emit_event`)。模型不能创建目标(用户专属,
//! `/goal` 命令或控制台 GoalBar),也不能修改 token 预算(资源决策
//! 留给用户);状态转换合法性在这里预校验,折叠层兜底忽略非法转换。

use async_trait::async_trait;
use denia_core::session::{GoalOp, GoalStatus, SessionEvent};
use denia_core::tool::ToolSchema;
use serde::Deserialize;

use crate::support::{parse_tool_args, tool_error};
use crate::{Tool, ToolContext, ToolOutput};

/// 把当前目标投影渲染成模型可读文本;`None` = 会话没有目标。
fn render_snapshot(goal: denia_core::session::GoalState, tokens_used: u64) -> String {
    let budget = match goal.token_budget {
        Some(budget) => format!("{tokens_used}/{budget} tokens"),
        None => format!("{tokens_used} tokens(未设预算)"),
    };
    let extra = match (&goal.status, &goal.blocked_reason) {
        (GoalStatus::Blocked, Some(reason)) => format!("\n受阻原因:{reason}"),
        (GoalStatus::Blocked, None) => String::new(),
        _ => String::new(),
    };
    format!(
        "当前目标:\n目标:{}\n状态:{}\n预算用量:{}\n已发起续跑轮数:{}{}",
        goal.objective,
        goal.status.label(),
        budget,
        goal.rounds_started,
        extra,
    )
}

#[derive(Deserialize)]
struct GetGoalArgs {}

/// `get_goal`:读取当前会话目标的状态与用量。
#[derive(Default)]
pub struct GetGoalTool;

impl GetGoalTool {
    fn build_schema() -> ToolSchema {
        ToolSchema {
            name: "get_goal".to_string(),
            description: "读取当前会话目标(objective)的状态、预算用量与续跑轮数。会话没有目标时返回明确说明。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }
}

#[async_trait]
impl Tool for GetGoalTool {
    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(Self::build_schema)
    }

    async fn execute(&self, arguments: &str, ctx: &ToolContext) -> ToolOutput {
        // 宽容解析:空参数/空对象都放行(本工具无参数)。
        if !arguments.trim().is_empty() {
            let _: GetGoalArgs = match parse_tool_args(arguments) {
                Ok(args) => args,
                Err(error) => {
                    return tool_error(format!("参数解析失败:{error}"), "get_goal 不接受任何参数");
                }
            };
        }
        let Some(reader) = &ctx.goal_reader else {
            return tool_error(
                "get_goal 需要归属的代理会话(当前调用无法读取会话状态)",
                "该工具只能在代理会话内使用",
            );
        };
        match reader() {
            Some((goal, tokens_used)) => ToolOutput::text(render_snapshot(goal, tokens_used)),
            None => ToolOutput::text("当前会话没有设置目标。".to_string()),
        }
    }
}

#[derive(Deserialize)]
struct UpdateGoalArgs {
    action: String,
    #[serde(default)]
    objective: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

/// `update_goal`:模型侧的目标状态变更入口。
#[derive(Default)]
pub struct UpdateGoalTool;

impl UpdateGoalTool {
    fn build_schema() -> ToolSchema {
        ToolSchema {
            name: "update_goal".to_string(),
            description: "更新当前会话目标的状态。action 取值:edit(修改目标文本,需 objective)、pause(暂停)、resume(恢复)、complete(标记目标已达成)、blocked(标记受阻无法推进,需 reason 说明阻塞条件)。会话没有目标时调用无效;token 预算由用户设置,不接受修改。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["edit", "pause", "resume", "complete", "blocked"],
                        "description": "状态变更动作。"
                    },
                    "objective": {
                        "type": "string",
                        "description": "新的目标文本(action=edit 时必填;一句话描述要达成什么)。"
                    },
                    "reason": {
                        "type": "string",
                        "description": "受阻原因(action=blocked 时必填;说明什么条件阻塞了推进)。"
                    }
                },
                "required": ["action"]
            }),
        }
    }
}

#[async_trait]
impl Tool for UpdateGoalTool {
    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(Self::build_schema)
    }

    async fn execute(&self, arguments: &str, ctx: &ToolContext) -> ToolOutput {
        let args: UpdateGoalArgs = match parse_tool_args(arguments) {
            Ok(args) => args,
            Err(error) => {
                return tool_error(
                    format!("参数解析失败:{error}"),
                    "参数必须是 JSON 对象,必填 action,可选 objective/reason",
                );
            }
        };
        let Some(emit) = &ctx.emit_event else {
            return tool_error(
                "update_goal 需要归属的代理会话(当前调用没有会话事件通道)",
                "该工具只能在代理会话内使用",
            );
        };
        let action = args.action.as_str();
        // 预校验(折叠层另有兜底):转换合法性 + 必填参数。
        let reader = ctx.goal_reader.as_ref();
        let current = reader.and_then(|read| read());
        let op = match action {
            "edit" => {
                let Some(objective) = args.objective.as_ref().map(|t| t.trim().to_string()) else {
                    return tool_error("action=edit 需要 objective 参数", "补上目标文本后重试");
                };
                if objective.is_empty() {
                    return tool_error("objective 必须是非空文本", "补上目标文本后重试");
                }
                GoalOp::Edit {
                    objective: Some(objective),
                    token_budget: None,
                }
            }
            "pause" => GoalOp::Pause,
            "resume" => GoalOp::Resume,
            "complete" => GoalOp::Complete,
            "blocked" => {
                let reason = args
                    .reason
                    .as_ref()
                    .map(|t| t.trim().to_string())
                    .unwrap_or_default();
                if reason.is_empty() {
                    return tool_error(
                        "action=blocked 需要 reason 参数",
                        "说明阻塞条件(什么被卡住、需要什么才能继续)后重试",
                    );
                }
                GoalOp::Block { reason }
            }
            other => {
                return tool_error(
                    format!("未知 action:{other:?}"),
                    "可用:edit/pause/resume/complete/blocked",
                );
            }
        };
        // 显式报错的转换(与 apply_goal_op 的忽略语义一一对应):无目标、
        // 预算耗尽/完成后 resume、无目标时除 resume 外的一切动作。
        if let Some((goal, _)) = &current {
            if matches!(action, "resume")
                && matches!(
                    goal.status,
                    GoalStatus::BudgetLimited | GoalStatus::Complete
                )
            {
                return tool_error(
                    format!(
                        "当前状态({})不允许恢复;目标文本或预算调整请让用户处理",
                        goal.status.label()
                    ),
                    "向用户说明情况",
                );
            }
        } else {
            return tool_error("当前会话没有设置目标", "调用 get_goal 确认后再操作");
        }
        emit(SessionEvent::Goal { op });
        ToolOutput::text(match action {
            "edit" => "已更新目标文本。".to_string(),
            "pause" => "目标已暂停,自动续跑停止。".to_string(),
            "resume" => "目标已恢复,空闲时会继续自动续跑。".to_string(),
            "complete" => "目标已标记完成。".to_string(),
            "blocked" => "目标已标记受阻,自动续跑停止;等待用户处理。".to_string(),
            _ => unreachable!("action 已在上面校验"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolContext;
    use denia_core::session::{GoalState, PermissionMode};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    fn ctx_with(
        events: Arc<Mutex<Vec<SessionEvent>>>,
        goal: Option<(GoalState, u64)>,
    ) -> ToolContext {
        ToolContext {
            session_id: None,
            selection: None,
            cwd: PathBuf::from("."),
            cancel: CancellationToken::new(),
            confined: true,
            vision_supported: false,
            emit_event: Some(Arc::new(move |event| {
                events.lock().unwrap().push(event);
            })),
            file_history: None,
            permission_mode: PermissionMode::AutoEdit,
            ask: None,
            call_id: None,
            goal_reader: Some(Arc::new(move || goal.clone())),
            read_state: None,
        }
    }

    fn active_goal() -> (GoalState, u64) {
        (
            GoalState {
                objective: "修完回归".into(),
                status: GoalStatus::Active,
                token_budget: Some(10_000),
                blocked_reason: None,
                base_tokens: 100,
                rounds_started: 2,
                created_at: 1,
                updated_at: 1,
            },
            900,
        )
    }

    #[tokio::test]
    async fn get_goal_renders_snapshot_and_missing_case() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let out = GetGoalTool
            .execute("{}", &ctx_with(sink.clone(), Some(active_goal())))
            .await;
        assert!(!out.is_error);
        assert!(out.content.contains("修完回归"));
        assert!(out.content.contains("进行中"));
        assert!(out.content.contains("900/10000 tokens"));
        assert!(out.content.contains("续跑轮数:2"));
        let empty = GetGoalTool
            .execute("", &ctx_with(sink.clone(), None))
            .await;
        assert!(!empty.is_error);
        assert!(empty.content.contains("没有设置目标"));
        assert!(sink.lock().unwrap().is_empty(), "get_goal 不写事件");
    }

    #[tokio::test]
    async fn update_goal_validates_actions_against_state() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let ok = UpdateGoalTool
            .execute(r#"{"action":"complete"}"#, &ctx_with(sink.clone(), Some(active_goal())))
            .await;
        assert!(!ok.is_error, "{}", ok.content);
        assert_eq!(
            sink.lock().unwrap().pop(),
            Some(SessionEvent::Goal { op: GoalOp::Complete })
        );
        // blocked 缺 reason 拒绝;未知 action 拒绝。
        assert!(
            UpdateGoalTool
                .execute(r#"{"action":"blocked"}"#, &ctx_with(sink.clone(), Some(active_goal())))
                .await
                .is_error
        );
        assert!(
            UpdateGoalTool
                .execute(r#"{"action":"destroy"}"#, &ctx_with(sink.clone(), Some(active_goal())))
                .await
                .is_error
        );
        // 预算耗尽后 resume 显式拒绝。
        let mut limited = active_goal().0;
        limited.status = GoalStatus::BudgetLimited;
        assert!(
            UpdateGoalTool
                .execute(r#"{"action":"resume"}"#, &ctx_with(sink.clone(), Some((limited, 10_000))))
                .await
                .is_error
        );
        // 无目标时动作显式拒绝。
        assert!(
            UpdateGoalTool
                .execute(r#"{"action":"complete"}"#, &ctx_with(sink, None))
                .await
                .is_error
        );
    }

    #[tokio::test]
    async fn edit_requires_objective_and_writes_event() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        assert!(
            UpdateGoalTool
                .execute(r#"{"action":"edit"}"#, &ctx_with(sink.clone(), Some(active_goal())))
                .await
                .is_error
        );
        let ok = UpdateGoalTool
            .execute(
                r#"{"action":"edit","objective":" 改成新目标 "}"#,
                &ctx_with(sink.clone(), Some(active_goal())),
            )
            .await;
        assert!(!ok.is_error, "{}", ok.content);
        assert_eq!(
            sink.lock().unwrap().pop(),
            Some(SessionEvent::Goal {
                op: GoalOp::Edit {
                    objective: Some("改成新目标".into()),
                    token_budget: None,
                }
            })
        );
    }

    #[tokio::test]
    async fn tools_reject_callers_without_session() {
        let ctx = ToolContext {
            session_id: None,
            selection: None,
            cwd: PathBuf::from("."),
            cancel: CancellationToken::new(),
            confined: true,
            vision_supported: false,
            emit_event: None,
            file_history: None,
            permission_mode: PermissionMode::AutoEdit,
            ask: None,
            call_id: None,
            goal_reader: None,
            read_state: None,
        };
        assert!(GetGoalTool.execute("{}", &ctx).await.is_error);
        assert!(
            UpdateGoalTool
                .execute(r#"{"action":"complete"}"#, &ctx)
                .await
                .is_error
        );
    }
}
