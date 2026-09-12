//! The `exit_plan` tool:计划模式的计划呈交入口。
//!
//! 审批流程由 agent-loop 派发处统一编排(分类 → 审批桥 → 决策落事件),
//! 工具结构体只承担三件事:模型可见 schema、与 `tool:plan` 纪律段同步
//! 进退、非审批路径的执行兜底。

use async_trait::async_trait;
use denia_core::tool::ToolSchema;
use serde::Deserialize;

use crate::support::{parse_tool_args, tool_error};
use crate::{Tool, ToolContext, ToolOutput};

/// 计划呈交参数。
#[derive(Debug, Deserialize)]
pub struct ExitPlanArgs {
    /// 计划正文(markdown)。
    pub plan: String,
    /// 可选短标题。
    #[serde(default)]
    pub title: Option<String>,
}

impl ExitPlanArgs {
    /// 从模型原始参数解析;计划正文必须是非空字符串。
    pub fn from_raw(raw: &str) -> Result<Self, String> {
        let args: Self = parse_tool_args(raw)?;
        let plan = args.plan.trim().to_string();
        if plan.is_empty() {
            return Err("plan 必须是非空的计划正文(markdown)".to_string());
        }
        Ok(Self {
            plan,
            title: args.title,
        })
    }
}

/// 计划呈交工具:仅计划模式注册,批准后由宿主切换执行档位并继续。
#[derive(Default)]
pub struct ExitPlanTool;

impl ExitPlanTool {
    fn build_schema() -> ToolSchema {
        ToolSchema {
            name: "exit_plan".to_string(),
            description: "计划模式专用:把完整执行计划呈交给用户审批。仅当当前处于计划模式且调研完成后调用;提交后阻塞等待用户决策,不要在等待期间继续其他操作。".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "可选的计划短标题(一句话概括要做什么)。"
                    },
                    "plan": {
                        "type": "string",
                        "description": "计划正文(markdown):目标、分步方案、涉及文件、风险与验证方式。"
                    }
                },
                "required": ["plan"]
            }),
        }
    }
}

#[async_trait]
impl Tool for ExitPlanTool {
    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(Self::build_schema)
    }

    /// 执行兜底:正常路径下派发处在审批决策后直接合成结果;走到这里
    /// 说明宿主没有接审批编排(如脱离 agent-loop 的裸注册),调用不生效。
    async fn execute(&self, arguments: &str, _ctx: &ToolContext) -> ToolOutput {
        if let Err(error) = ExitPlanArgs::from_raw(arguments) {
            return tool_error(
                format!("参数解析失败:{error}"),
                "plan 必填,为非空的 markdown 计划正文;title 可选",
            );
        }
        ToolOutput::error(
            "计划审批需要宿主审批通道;当前部署未接入 exit_plan 的审批编排,本次调用未生效。".to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::{decide, ActionClass, Decision};
    use denia_core::session::PermissionMode;
    use std::path::PathBuf;
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolContext {
        ToolContext {
            session_id: None,
            selection: None,
            cwd: PathBuf::from("."),
            cancel: CancellationToken::new(),
            confined: true,
            vision_supported: false,
            emit_event: None,
            file_history: None,
            permission_mode: PermissionMode::Plan,
            ask: None,
            call_id: None,
            goal_reader: None,
            read_state: None,
        }
    }

    #[tokio::test]
    async fn execute_without_host_bridge_is_error() {
        let raw = r##"{"plan": "# 做点什么\n\n- 步骤一"}"##;
        let output = ExitPlanTool.execute(raw, &ctx()).await;
        assert!(output.is_error);
        assert!(output.content.contains("审批通道"));
    }

    #[tokio::test]
    async fn empty_plan_is_rejected_before_bridge() {
        let output = ExitPlanTool.execute(r#"{"plan":"  "}"#, &ctx()).await;
        assert!(output.is_error);
        assert!(output.content.contains("plan"));
    }

    #[test]
    fn plan_submit_requires_approval_in_plan_mode() {
        // 计划提交在计划档走 Ask(审批),其余档直接拒绝。
        assert_eq!(
            decide(PermissionMode::Plan, ActionClass::PlanSubmit),
            Decision::Ask
        );
        assert!(matches!(
            decide(PermissionMode::AutoEdit, ActionClass::PlanSubmit),
            Decision::Deny(_)
        ));
    }
}
