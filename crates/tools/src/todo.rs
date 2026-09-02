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

use crate::{Tool, ToolContext, ToolOutput, parse_args_lenient};

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

const DESCRIPTION_HEAD: &str = "Record and update a structured task list for the current work. \
Send the ENTIRE list every call — it REPLACES the previous list (there are no \
partial updates, no per-item edits). Use it to plan multi-step work and show \
progress: add one todo per concrete step before you start. ";

const DESCRIPTION_PARALLEL: &str = "Mark every todo being actively worked on \
`in_progress` — several at once when work genuinely runs in parallel (e.g. \
concurrent subagents or background commands), one for sequential work; while \
work remains, at least one task should be `in_progress`. ";

const DESCRIPTION_SINGLE: &str = "Keep AT MOST ONE todo `in_progress` at a \
time; while work remains, exactly one active task should be `in_progress`. ";

const DESCRIPTION_TAIL: &str = "Mark a todo `completed` the moment it is done \
(do not batch completions), and allow no `in_progress` item only once all work \
is complete. Skip the list for trivial single-step tasks. Statuses: `pending` \
(not started), `in_progress` (being worked on now), `completed` (finished).";

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
            return Err("invalid todo: `content` must be a non-empty string".to_string());
        }
        if !seen.insert(content.clone()) {
            return Err(format!("invalid todos: duplicate content {content:?}"));
        }
        let status = match item.status.as_str() {
            "pending" => TodoStatus::Pending,
            "in_progress" => {
                active += 1;
                TodoStatus::InProgress
            }
            "completed" => TodoStatus::Completed,
            other => return Err(format!("invalid todo status: {other:?}")),
        };
        todos.push(TodoItem { content, status });
    }
    if !allow_parallel && active > 1 {
        return Err(format!(
            "invalid todos: at most one task may be in_progress (got {active})"
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
                            "description": "The COMPLETE task list, replacing any previous list.",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {
                                    "content": {
                                        "type": "string",
                                        "description": "What the task is — a short imperative line."
                                    },
                                    "status": {
                                        "type": "string",
                                        "enum": ["pending", "in_progress", "completed"],
                                        "description": "pending (not started) | in_progress (now) | completed (done)."
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
        let args: TodoArgs = match parse_args_lenient(arguments) {
            Ok(args) => args,
            Err(error) => {
                return ToolOutput {
                    content: format!("invalid arguments: {error}"),
                    is_error: true,
                };
            }
        };
        let todos = match to_todo_list(args.todos, self.policy.allow_parallel_in_progress) {
            Ok(todos) => todos,
            Err(error) => {
                return ToolOutput {
                    content: error,
                    is_error: true,
                };
            }
        };
        // The list is per-session state; a caller with no owning session has
        // nowhere to write it. Reject rather than silently no-op.
        let Some(emit) = &ctx.emit_event else {
            return ToolOutput {
                content: "todo_write requires an owning agent session".to_string(),
                is_error: true,
            };
        };
        let count = |status: TodoStatus| todos.iter().filter(|t| t.status == status).count();
        let (pending, in_progress, completed) = (
            count(TodoStatus::Pending),
            count(TodoStatus::InProgress),
            count(TodoStatus::Completed),
        );
        emit(SessionEvent::TodoWrite { todos });
        ToolOutput {
            content: format!(
                "Updated todo list: {pending} pending, {in_progress} in progress, {completed} completed."
            ),
            is_error: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    fn ctx_with_sink(
        collected: Arc<Mutex<Vec<SessionEvent>>>,
    ) -> ToolContext {
        ToolContext {
            cwd: std::env::temp_dir(),
            cancel: CancellationToken::new(),
            confined: true,
        vision_supported: true,
            emit_event: Some(Arc::new(move |event| {
                collected.lock().unwrap().push(event);
            })),
        }
    }

    fn ctx_without_sink() -> ToolContext {
        ToolContext {
            cwd: std::env::temp_dir(),
            cancel: CancellationToken::new(),
            confined: true,
        vision_supported: true,
            emit_event: None,
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
            "Updated todo list: 1 pending, 1 in progress, 1 completed."
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
            .execute(r#"{"todos":[{"content":"  ","status":"pending"}]}"#, &sink())
            .await;
        assert!(empty.is_error && empty.content.contains("non-empty"));
        let dup = tool
            .execute(
                r#"{"todos":[{"content":"a","status":"pending"},{"content":"a","status":"pending"}]}"#,
                &sink(),
            )
            .await;
        assert!(dup.is_error && dup.content.contains("duplicate"));
        let over = tool
            .execute(
                r#"{"todos":[{"content":"a","status":"in_progress"},{"content":"b","status":"in_progress"}]}"#,
                &sink(),
            )
            .await;
        assert!(over.is_error && over.content.contains("at most one"));
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
            .execute(r#"{"todos":[{"content":"a","status":"pending"}]}"#, &ctx_without_sink())
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("owning agent session"));
    }

    #[tokio::test]
    async fn description_matches_policy() {
        let single = TodoWriteTool::default();
        assert!(single.schema().description.contains("AT MOST ONE"));
        let parallel = TodoWriteTool::new(TodoPolicy {
            allow_parallel_in_progress: true,
        });
        assert!(parallel.schema().description.contains("several at once"));
    }
}
