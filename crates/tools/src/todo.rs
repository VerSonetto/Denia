//! The `todo_write` tool: whole-list replacement over the session log.
//!
//! The model resends the ENTIRE list on every call; the snapshot lands on the
//! event-sourced session log as a `todo-write` event (last-write-wins on
//! replay), so durability and resume come from the log, not a service. The
//! list belongs to the one calling session — a caller with no session sink is
//! rejected rather than silently writing nowhere.

use async_trait::async_trait;
use denia_core::session::{SessionEvent, TodoItem, TodoStatus};
use denia_core::tool::ToolSchema;
use serde::Deserialize;

use crate::support::{parse_tool_args, tool_error};
use crate::{Tool, ToolContext, ToolOutput};

/// Deployment policy: may several todos be `in_progress` at once?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TodoPolicy {
    pub allow_parallel_in_progress: bool,
}

impl Default for TodoPolicy {
    /// Single-active discipline: one in_progress at a time.
    fn default() -> Self {
        Self {
            allow_parallel_in_progress: false,
        }
    }
}

const DESCRIPTION_HEAD: &str = "记录并更新当前工作的结构化任务清单。每次调用必须发送完整列表——本调用整体替换旧列表(没有部分更新、没有单条编辑)。用于规划多步工作并展示进度:开始前把每个具体步骤拆成一条 todo。";

const DESCRIPTION_PARALLEL: &str = "正在做的任务标记 in_progress:工作真在并行时(并发子代理、后台命令)可以多条同时 in_progress;只要还有未完成的工作,至少保持一条 in_progress。";

const DESCRIPTION_SINGLE: &str = "同一时间最多一条 todo 处于 in_progress;只要还有未完成的工作,应恰有一条进行中。";

const DESCRIPTION_TAIL: &str = "任务完成的当下就标记 completed(不要攒到最后批量标);全部工作完成才允许没有 in_progress 项。琐碎的单步任务不用建清单。状态:pending(未开始)、in_progress(进行中)、completed(已完成)。";

fn describe(allow_parallel: bool) -> String {
    let mut text = String::from(DESCRIPTION_HEAD);
    text.push_str(if allow_parallel {
        DESCRIPTION_PARALLEL
    } else {
        DESCRIPTION_SINGLE
    });
    text.push_str(DESCRIPTION_TAIL);
    text
}

#[derive(Deserialize)]
struct TodoArgs {
    todos: Vec<TodoArg>,
}

#[derive(Deserialize)]
struct TodoArg {
    content: String,
    status: String,
}

/// Validate value constraints and build the canonical list: trimmed non-empty
/// unique content, and at most one `in_progress` unless the policy allows
/// parallel work.
fn to_todo_list(raw: Vec<TodoArg>, allow_parallel: bool) -> Result<Vec<TodoItem>, String> {
    let mut todos = Vec::with_capacity(raw.len());
    let mut seen = std::collections::HashSet::new();
    let mut active = 0usize;
    for item in raw {
        let content = item.content.trim().to_string();
        if content.is_empty() {
            return Err("todo 无效:content 必须是非空字符串".to_string());
        }
        if !seen.insert(content.clone()) {
            return Err(format!("todo 无效:content 重复:{content:?}"));
        }
        let status = match item.status.as_str() {
            "pending" => TodoStatus::Pending,
            "in_progress" => {
                active += 1;
                TodoStatus::InProgress
            }
            "completed" => TodoStatus::Completed,
            other => {
                return Err(format!(
                    "todo 状态无效:{other:?}(可用:pending/in_progress/completed)"
                ))
            }
        };
        todos.push(TodoItem { content, status });
    }
    if !allow_parallel && active > 1 {
        return Err(format!(
            "todo 无效:同时最多一条 in_progress(当前 {active} 条)"
        ));
    }
    Ok(todos)
}

/// The `todo_write` tool: records the session's plan on the durable log.
pub struct TodoWriteTool {
    schema: ToolSchema,
    policy: TodoPolicy,
}

impl TodoWriteTool {
    pub fn new(policy: TodoPolicy) -> Self {
        Self {
            schema: ToolSchema {
                name: "todo_write".to_string(),
                description: describe(policy.allow_parallel_in_progress),
                parameters: serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "todos": {
                            "type": "array",
                            "description": "完整任务列表,整体替换之前的列表。",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {
                                    "content": {
                                        "type": "string",
                                        "description": "任务内容——一句简短的祈使句。"
                                    },
                                    "status": {
                                        "type": "string",
                                        "enum": ["pending", "in_progress", "completed"],
                                        "description": "pending(未开始)| in_progress(进行中)| completed(已完成)。"
                                    }
                                },
                                "required": ["content", "status"]
                            }
                        }
                    },
                    "required": ["todos"]
                }),
            },
            policy,
        }
    }
}

impl Default for TodoWriteTool {
    fn default() -> Self {
        Self::new(TodoPolicy::default())
    }
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn execute(&self, arguments: &str, ctx: &ToolContext) -> ToolOutput {
        let args: TodoArgs = match parse_tool_args(arguments) {
            Ok(args) => args,
            Err(error) => {
                return tool_error(
                    format!("参数解析失败:{error}"),
                    "参数必须是 JSON 对象,必填字段为 todos(数组,每项含 content 与 status)",
                );
            }
        };
        let todos = match to_todo_list(args.todos, self.policy.allow_parallel_in_progress) {
            Ok(todos) => todos,
            Err(error) => {
                return tool_error(error, "修正 todos 后重发完整列表");
            }
        };
        // The list is per-session state; a caller with no owning session has
        // nowhere to write it. Reject rather than silently no-op.
        let Some(emit) = &ctx.emit_event else {
            return tool_error(
                "todo_write 需要归属的代理会话(当前调用没有会话事件通道)",
                "该工具只能在代理会话内使用",
            );
        };
        let count = |status: TodoStatus| todos.iter().filter(|t| t.status == status).count();
        let (pending, in_progress, completed) = (
            count(TodoStatus::Pending),
            count(TodoStatus::InProgress),
            count(TodoStatus::Completed),
        );
        emit(SessionEvent::TodoWrite { todos });
        ToolOutput::text(format!(
            "已更新任务列表:{pending} 个待办、{in_progress} 个进行中、{completed} 个已完成。"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::session::PermissionMode;
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    fn ctx_with_sink(collected: Arc<Mutex<Vec<SessionEvent>>>) -> ToolContext {
        ToolContext {
            session_id: None,
            selection: None,
            cwd: std::env::temp_dir(),
            cancel: CancellationToken::new(),
            confined: true,
            vision_supported: true,
            emit_event: Some(Arc::new(move |event| {
                collected.lock().unwrap().push(event);
            })),
            file_history: None,
            permission_mode: PermissionMode::AutoEdit,
            ask: None,
            call_id: None,
            goal_reader: None,
            read_state: None,
        }
    }

    fn ctx_without_sink() -> ToolContext {
        ToolContext {
            session_id: None,
            selection: None,
            cwd: std::env::temp_dir(),
            cancel: CancellationToken::new(),
            confined: true,
            vision_supported: true,
            emit_event: None,
            file_history: None,
            permission_mode: PermissionMode::AutoEdit,
            ask: None,
            call_id: None,
            goal_reader: None,
            read_state: None,
        }
    }

    #[tokio::test]
    async fn writes_snapshot_and_reports_counts() {
        let collected = Arc::new(Mutex::new(Vec::new()));
        let tool = TodoWriteTool::default();
        let out = tool
            .execute(
                r#"{"todos":[
                    {"content":"a","status":"completed"},
                    {"content":"b","status":"in_progress"},
                    {"content":"c","status":"pending"}
                ]}"#,
                &ctx_with_sink(collected.clone()),
            )
            .await;
        assert!(!out.is_error);
        assert_eq!(
            out.content,
            "已更新任务列表:1 个待办、1 个进行中、1 个已完成。"
        );
        let events = collected.lock().unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            SessionEvent::TodoWrite { todos } => {
                assert_eq!(todos.len(), 3);
                assert_eq!(todos[1].status, TodoStatus::InProgress);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_empty_duplicate_and_overactive() {
        let tool = TodoWriteTool::default();
        let sink = || ctx_with_sink(Arc::new(Mutex::new(Vec::new())));
        let empty = tool
            .execute(
                r#"{"todos":[{"content":"  ","status":"pending"}]}"#,
                &sink(),
            )
            .await;
        assert!(empty.is_error && empty.content.contains("非空字符串"));
        let dup = tool
            .execute(
                r#"{"todos":[{"content":"a","status":"pending"},{"content":"a","status":"pending"}]}"#,
                &sink(),
            )
            .await;
        assert!(dup.is_error && dup.content.contains("重复"));
        let over = tool
            .execute(
                r#"{"todos":[{"content":"a","status":"in_progress"},{"content":"b","status":"in_progress"}]}"#,
                &sink(),
            )
            .await;
        assert!(over.is_error && over.content.contains("最多一条 in_progress"));
    }

    #[tokio::test]
    async fn parallel_policy_allows_many_in_progress() {
        let tool = TodoWriteTool::new(TodoPolicy {
            allow_parallel_in_progress: true,
        });
        let out = tool
            .execute(
                r#"{"todos":[{"content":"a","status":"in_progress"},{"content":"b","status":"in_progress"}]}"#,
                &ctx_with_sink(Arc::new(Mutex::new(Vec::new()))),
            )
            .await;
        assert!(!out.is_error);
    }

    #[tokio::test]
    async fn rejects_caller_without_session() {
        let tool = TodoWriteTool::default();
        let out = tool
            .execute(
                r#"{"todos":[{"content":"a","status":"pending"}]}"#,
                &ctx_without_sink(),
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("归属的代理会话"));
    }

    #[tokio::test]
    async fn description_matches_policy() {
        let single = TodoWriteTool::default();
        assert!(single.schema().description.contains("最多一条"));
        let parallel = TodoWriteTool::new(TodoPolicy {
            allow_parallel_in_progress: true,
        });
        assert!(parallel.schema().description.contains("多条同时"));
    }
}
