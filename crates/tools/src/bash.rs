//! The `bash` tool: one shell command per call.
//!
//! 有 [`ShellHub`] 的部署走**常驻 shell**(同一会话的多次调用共享工作目录、
//! 变量与环境),没有则退回一次性进程。形态差异的取舍见
//! [`crate::shell_session`] 的模块文档。

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use denia_core::tool::ToolSchema;
use serde::Deserialize;

use crate::shell_session::ShellHub;
use crate::support::{parse_tool_args, tool_error};
use crate::{Tool, ToolContext, ToolOutput, shell};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;

#[derive(Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// Runs one shell command in the session workspace.
pub struct BashTool {
    schema: ToolSchema,
    runtime: Option<Arc<dyn crate::capabilities::AgentRuntime>>,
    hub: Option<ShellHub>,
}

impl BashTool {
    pub fn new() -> Self {
        let runtime = shell::shell_runtime();
        let command_description = shell::bash_command_param_description(&runtime);
        Self {
            runtime: None,
            hub: None,
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
                        "timeout_ms": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": MAX_TIMEOUT_MS,
                            "description": "超时时间(毫秒),默认 120000,最大 600000;长任务请用 run_in_background。"
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
        self.schema.parameters["properties"]["run_in_background"] = serde_json::json!({"type":"boolean","description":"后台执行并立即返回任务 id;用 job_output 读取输出、job_kill 停止。"});
        self
    }

    /// 挂上常驻 shell 注册表;不挂则每次调用都新起一个一次性进程。
    ///
    /// 描述随形态一起换:`常驻`与`每次新进程`是两句互斥的话,模型看到的必须
    /// 与真实执行方式一致(项目铁律:模型可见 == 模型可执行)。
    pub fn with_shell_hub(mut self, hub: ShellHub) -> Self {
        self.schema.description = shell::persistent_bash_tool_description(&shell::shell_runtime());
        self.hub = Some(hub);
        self
    }

    /// 常驻 shell 路径:同一会话复用同一个进程,状态跨调用保留。
    async fn execute_persistent(
        &self,
        hub: &ShellHub,
        session_id: &str,
        command: &str,
        timeout_ms: u64,
        ctx: &ToolContext,
    ) -> ToolOutput {
        // 起进程是阻塞操作,丢进 blocking 池;已有 shell 时这里只是查表。
        let spawned = {
            let hub = hub.clone();
            let key = session_id.to_string();
            let cwd = ctx.cwd.clone();
            tokio::task::spawn_blocking(move || hub.get_or_spawn(&key, &cwd)).await
        };
        let shell = match spawned {
            Ok(Ok(shell)) => shell,
            Ok(Err(message)) => {
                return tool_error(
                    message,
                    "确认宿主 shell 可执行文件可用;工作目录是会话工作区",
                );
            }
            Err(join_error) => {
                return tool_error(format!("持久 shell 启动任务失败:{join_error}"), "请重试一次");
            }
        };

        let runner = {
            let shell = Arc::clone(&shell);
            let command = command.to_string();
            let timeout = Duration::from_millis(timeout_ms);
            tokio::task::spawn_blocking(move || shell.run(&command, timeout))
        };

        tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => {
                // 阻塞在读取上没法协作式取消,直接把 shell 杀掉:等它的那次
                // 读取会以"进程已退出"收尾,不会留下半条命令的脏状态。
                // 读取任务本身随即自然结束,这里不必再 await(也无法取消)。
                shell.kill();
                tool_error(
                    "命令被用户中断",
                    "该会话的持久 shell 已终止,下一条命令会重开一个干净 shell",
                )
            }
            result = runner => match result {
                Ok(Ok(captured)) => ToolOutput::text(format!(
                    "退出码: {}\n{}",
                    captured.exit_code.unwrap_or(-1),
                    captured.text
                )),
                Ok(Err(message)) => ToolOutput::error(format!("[工具错误] {message}")),
                Err(join_error) => {
                    tool_error(format!("命令执行任务失败:{join_error}"), "请重试一次")
                }
            },
        }
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
        if let Ok(value) = parse_tool_args::<serde_json::Value>(arguments)
            && value["run_in_background"].as_bool() == Some(true)
        {
            let result = match &self.runtime {
                Some(runtime) => runtime.execute("job_start", value, ctx).await,
                None => Err("当前部署未启用后台任务".into()),
            };
            return match result {
                Ok(value) => ToolOutput::text(value.to_string()),
                Err(content) => ToolOutput::error(content),
            };
        }
        let args: BashArgs = match parse_tool_args(arguments) {
            Ok(args) => args,
            Err(error) => {
                return tool_error(
                    format!("参数解析失败:{error}"),
                    "参数必须是 JSON 对象,必填字段为 command(字符串)",
                );
            }
        };
        let requested_ms = args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).min(MAX_TIMEOUT_MS);
        let timeout = Duration::from_millis(requested_ms);

        // 挂了常驻 shell 注册表且当前调用归属某个会话 → 走持久路径。
        // 没有会话身份的调用(单测、无宿主的裸跑)退回一次性进程。
        if let (Some(hub), Some(session_id)) = (&self.hub, ctx.session_id.as_deref()) {
            return self
                .execute_persistent(hub, session_id, &args.command, requested_ms, ctx)
                .await;
        }

        let spawn_once = || {
            let mut command = shell::shell_command(&args.command);
            command
                .current_dir(&ctx.cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            command.spawn()
        };
        let mut child = match spawn_once() {
            Ok(child) => child,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound =>
            {
                // os error 2/3:shell 可执行文件路径失效(如 Store 版
                // PowerShell 更新后按版本目录整体换路径)。清缓存重解析
                // 并重试一次;仍失败才是真错误。
                shell::invalidate_windows_shell_cache();
                match spawn_once() {
                    Ok(child) => child,
                    Err(retry_error) => {
                        return tool_error(
                            format!("命令启动失败:{retry_error}"),
                            "确认命令在该宿主 shell 上可用;工作目录是会话工作区",
                        );
                    }
                }
            }
            Err(error) => {
                return tool_error(
                    format!("命令启动失败:{error}"),
                    "确认命令在该宿主 shell 上可用;工作目录是会话工作区",
                );
            }
        };

        let call_cancel = ctx.cancel.child_token();
        let waited = tokio::select! {
            biased;
            _ = call_cancel.cancelled() => {
                let _ = child.kill().await;
                return tool_error(
                    "命令被用户中断",
                    "中断后命令已终止;需要继续时重新发起调用",
                );
            }
            waited = tokio::time::timeout(timeout, child.wait()) => waited,
        };
        match waited {
            // Drop kills the child via kill_on_drop.
            Err(_) => tool_error(
                format!("命令超时({} ms 未结束)", requested_ms),
                format!(
                    "拆小命令分步执行、提高 timeout_ms(上限 {MAX_TIMEOUT_MS}),或用 run_in_background 后台运行"
                ),
            ),
            Ok(Err(error)) => tool_error(
                format!("等待命令结束失败:{error}"),
                "请重试一次;持续失败请报告",
            ),
            Ok(Ok(_)) => match child.wait_with_output().await {
                Err(error) => tool_error(
                    format!("输出捕获失败:{error}"),
                    "请重试一次",
                ),
                Ok(output) => {
                    // A non-zero exit code is data, not a tool failure.
                    let code = output.status.code().unwrap_or(-1);
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let mut content = format!("退出码: {code}\n{stdout}");
                    if !stderr.trim().is_empty() {
                        content.push_str(&format!("\n--- stderr ---\n{stderr}"));
                    }
                    ToolOutput::text(content)
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::session::PermissionMode;
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
            permission_mode: PermissionMode::AutoEdit,
            ask: None,
            call_id: None,
            goal_reader: None,
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
    async fn echoes_and_reports_exit_code() {
        let dir = std::env::temp_dir();
        let tool = BashTool::new();
        let ok = tool.execute(r#"{"command":"echo hi"}"#, &ctx(&dir)).await;
        assert!(!ok.is_error);
        assert!(ok.content.starts_with("退出码: 0"), "{}", ok.content);
        assert!(ok.content.contains("hi"));

        let bad = tool.execute(r#"{"command":"exit 3"}"#, &ctx(&dir)).await;
        assert!(!bad.is_error, "non-zero exit is data");
        assert!(bad.content.starts_with("退出码: 3"), "{}", bad.content);
    }

    #[tokio::test]
    async fn times_out_with_actionable_hint() {
        let dir = std::env::temp_dir();
        let tool = BashTool::new();
        let sleeping = if cfg!(windows) {
            r#"{"command":"ping -n 10 127.0.0.1 >nul","timeout_ms":200}"#
        } else {
            r#"{"command":"sleep 10","timeout_ms":200}"#
        };
        let out = tool.execute(sleeping, &ctx(&dir)).await;
        assert!(out.is_error);
        assert!(out.content.contains("超时"), "{}", out.content);
        assert!(out.content.contains("run_in_background"), "{}", out.content);
    }

    #[tokio::test]
    async fn string_timeout_is_accepted() {
        // 宽容解析:"timeout_ms" 传字符串数字不硬失败。
        let dir = std::env::temp_dir();
        let tool = BashTool::new();
        let command = if cfg!(windows) {
            r#"{"command":"echo ok","timeout_ms":"10000"}"#
        } else {
            r#"{"command":"echo ok","timeout_ms":"10000"}"#
        };
        let out = tool.execute(command, &ctx(&dir)).await;
        assert!(!out.is_error, "{}", out.content);
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
            permission_mode: PermissionMode::AutoEdit,
            ask: None,
            call_id: None,
            goal_reader: None,
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
        assert!(out.content.contains("中断"), "{}", out.content);
    }
}
