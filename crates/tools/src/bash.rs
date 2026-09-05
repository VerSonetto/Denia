//! The `bash` tool: one shell command per call.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use denia_core::session::PermissionMode;
use denia_core::tool::ToolSchema;
use serde::Deserialize;

use crate::permission::{bash_may_write, denial_marker, escalation_hint};
use crate::{Tool, ToolContext, ToolOutput, parse_args_lenient, shell, truncate};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const STREAM_CAP: usize = 32_000;

#[derive(Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// Runs one shell command in the session workspace.
pub struct BashTool {
    schema: ToolSchema,
    runtime: Option<std::sync::Arc<dyn crate::capabilities::AgentRuntime>>,
}

impl BashTool {
    pub fn new() -> Self {
        let runtime = shell::shell_runtime();
        let command_description = shell::bash_command_param_description(&runtime);
        Self {
            runtime: None,
            schema: ToolSchema {
                name: "bash".to_string(),
                description: shell::bash_tool_description(&runtime),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": command_description,
                        },
                        "timeout_ms": { "type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_MS },
                        "sandbox_permissions": {
                            "type": "string",
                            "enum": ["workspace-write", "danger-full-access"],
                            "description": "The wider sandbox mode this command needs. Only valid as a one-shot retry of a command the sandbox just denied; requires justification and user approval."
                        },
                        "justification": {
                            "type": "string",
                            "description": "Required with sandbox_permissions: one sentence for the user explaining why this exact command needs the wider access."
                        }
                    },
                    "required": ["command"]
                }),
            },
        }
    }

    pub fn with_runtime(
        mut self,
        runtime: std::sync::Arc<dyn crate::capabilities::AgentRuntime>,
    ) -> Self {
        self.runtime = Some(runtime);
        self.schema.parameters["properties"]["run_in_background"] = serde_json::json!({"type":"boolean","description":"后台执行并立即返回任务 id；用 job_output 读取输出、job_kill 停止。"});
        self
    }
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for BashTool {
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn execute(&self, arguments: &str, ctx: &ToolContext) -> ToolOutput {
        if let Ok(value) = parse_args_lenient::<serde_json::Value>(arguments) {
            if value["run_in_background"].as_bool() == Some(true) {
                let result = match &self.runtime {
                    Some(runtime) => runtime.execute("job_start", value, ctx).await,
                    None => Err("当前部署未启用后台任务".into()),
                };
                return match result {
                    Ok(value) => ToolOutput {
                        content: value.to_string(),
                        is_error: false,
                    },
                    Err(content) => ToolOutput {
                        content,
                        is_error: true,
                    },
                };
            }
        }
        let args: BashArgs = match parse_args_lenient(arguments) {
            Ok(args) => args,
            Err(error) => {
                return ToolOutput {
                    content: format!("invalid arguments: {error}"),
                    is_error: true,
                };
            }
        };
        let timeout = Duration::from_millis(
            args.timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS),
        );

        let effective = ctx.effective_permission();
        if effective == PermissionMode::ReadOnly && bash_may_write(&args.command) {
            return ToolOutput {
                content: format!(
                    "{}\n{}",
                    denial_marker(effective),
                    escalation_hint("command")
                ),
                is_error: true,
            };
        }

        let mut command = shell::shell_command(&args.command);
        command
            .current_dir(&ctx.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return ToolOutput {
                    content: format!("spawn failed: {error}"),
                    is_error: true,
                };
            }
        };

        let call_cancel = ctx.cancel.child_token();
        let waited = tokio::select! {
            biased;
            _ = call_cancel.cancelled() => {
                let _ = child.kill().await;
                return ToolOutput {
                    content: "command aborted".to_string(),
                    is_error: true,
                };
            }
            waited = tokio::time::timeout(timeout, child.wait()) => waited,
        };
        match waited {
            // Drop kills the child via kill_on_drop.
            Err(_) => ToolOutput {
                content: format!("command timed out after {}ms", timeout.as_millis()),
                is_error: true,
            },
            Ok(Err(error)) => ToolOutput {
                content: format!("wait failed: {error}"),
                is_error: true,
            },
            Ok(Ok(_)) => match child.wait_with_output().await {
                Err(error) => ToolOutput {
                    content: format!("output capture failed: {error}"),
                    is_error: true,
                },
                Ok(output) => {
                    // A non-zero exit code is data, not a tool failure.
                    let code = output.status.code().unwrap_or(-1);
                    let stdout = truncate(&String::from_utf8_lossy(&output.stdout), STREAM_CAP);
                    let stderr = truncate(&String::from_utf8_lossy(&output.stderr), STREAM_CAP);
                    let mut content = format!("exit code: {code}\n{stdout}");
                    if !stderr.trim().is_empty() {
                        content.push_str(&format!("\n--- stderr ---\n{stderr}"));
                    }
                    ToolOutput {
                        content,
                        is_error: false,
                    }
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            session_id: None,
            selection: None,
            cwd: dir.to_path_buf(),
            cancel: CancellationToken::new(),
            confined: true,
            vision_supported: true,
            emit_event: None,
            file_history: None,
            permission_mode: PermissionMode::WorkspaceWrite,
            permission_override: None,
        }
    }

    #[tokio::test]
    async fn schema_describes_host_shell() {
        let tool = BashTool::new();
        let schema = tool.schema();
        assert!(schema.description.contains(std::env::consts::OS));
        assert!(schema.description.contains(std::env::consts::ARCH));
        let command = schema
            .parameters
            .get("properties")
            .and_then(|props| props.get("command"))
            .and_then(|field| field.get("description"))
            .and_then(|value| value.as_str())
            .expect("command description");
        assert!(command.contains(std::env::consts::OS));
    }

    #[tokio::test]
    async fn read_only_denies_obvious_write_command() {
        let dir = std::env::temp_dir();
        let mut context = ctx(&dir);
        context.permission_mode = PermissionMode::ReadOnly;
        let tool = BashTool::new();
        let denied = tool
            .execute(r#"{"command":"echo hi > out.txt"}"#, &context)
            .await;
        assert!(denied.is_error, "{}", denied.content);
        assert!(denied.content.contains("read-only"), "{}", denied.content);

        let read_only = tool.execute(r#"{"command":"echo hi"}"#, &context).await;
        assert!(!read_only.is_error, "{}", read_only.content);
    }

    #[tokio::test]
    async fn echoes_and_reports_exit_code() {
        let dir = std::env::temp_dir();
        let tool = BashTool::new();
        let ok = tool.execute(r#"{"command":"echo hi"}"#, &ctx(&dir)).await;
        assert!(!ok.is_error);
        assert!(ok.content.starts_with("exit code: 0"));
        assert!(ok.content.contains("hi"));

        let failing = if cfg!(windows) {
            r#"{"command":"exit 3"}"#
        } else {
            r#"{"command":"exit 3"}"#
        };
        let bad = tool.execute(failing, &ctx(&dir)).await;
        assert!(!bad.is_error, "non-zero exit is data");
        assert!(bad.content.starts_with("exit code: 3"));
    }

    #[tokio::test]
    async fn times_out() {
        let dir = std::env::temp_dir();
        let tool = BashTool::new();
        let sleeping = if cfg!(windows) {
            r#"{"command":"ping -n 10 127.0.0.1 >nul","timeout_ms":200}"#
        } else {
            r#"{"command":"sleep 10","timeout_ms":200}"#
        };
        let out = tool.execute(sleeping, &ctx(&dir)).await;
        assert!(out.is_error);
        assert!(out.content.contains("timed out"));
    }

    #[tokio::test]
    async fn bad_arguments_are_errors() {
        let dir = std::env::temp_dir();
        let tool = BashTool::new();
        let out = tool.execute("not json", &ctx(&dir)).await;
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn cancel_aborts() {
        let dir = std::env::temp_dir();
        let tool = BashTool::new();
        let cancel = CancellationToken::new();
        let context = ToolContext {
            session_id: None,
            selection: None,
            cwd: dir.clone(),
            cancel: cancel.clone(),
            confined: true,
            vision_supported: true,
            emit_event: None,
            file_history: None,
            permission_mode: PermissionMode::WorkspaceWrite,
            permission_override: None,
        };
        let command = if cfg!(windows) {
            r#"{"command":"ping -n 10 127.0.0.1 >nul"}"#
        } else {
            r#"{"command":"sleep 10"}"#
        };
        let handle = tokio::spawn(async move { tool.execute(command, &context).await });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        cancel.cancel();
        let out = handle.await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("aborted"));
    }
}
