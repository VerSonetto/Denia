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

async fn consume_output(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    preview: Arc<tokio::sync::Mutex<crate::output::Preview>>,
    capture: Option<crate::output::Capture>,
    stream: &str,
) -> std::io::Result<()> {
    use tokio::io::AsyncReadExt;
    let mut buffer = [0u8; 8192];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        preview.lock().await.push(&buffer[..count]);
        if let Some(capture) = &capture {
            capture.append(stream, &buffer[..count]).await;
        }
    }
}

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
                return tool_error(
                    format!("持久 shell 启动任务失败:{join_error}"),
                    "请重试一次",
                );
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
        let requested_ms = args
            .timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS);
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
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
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

        let capture = match &ctx.output_store {
            Some(store) => Some(store.start(&["stdout", "stderr"]).await),
            None => None,
        };
        let stdout_preview = Arc::new(tokio::sync::Mutex::new(crate::output::Preview::default()));
        let stderr_preview = Arc::new(tokio::sync::Mutex::new(crate::output::Preview::default()));
        let mut stdout_task = tokio::spawn(consume_output(
            child.stdout.take().unwrap(),
            stdout_preview.clone(),
            capture.clone(),
            "stdout",
        ));
        let mut stderr_task = tokio::spawn(consume_output(
            child.stderr.take().unwrap(),
            stderr_preview.clone(),
            capture.clone(),
            "stderr",
        ));
        let mut error = None;
        let code = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => { error = Some("命令被用户中断".to_string()); -1 }
            _ = tokio::time::sleep(timeout) => { error = Some(format!("命令超时({requested_ms} ms 未结束)；可拆小命令或使用 run_in_background 后台运行")); -1 }
            result = child.wait() => match result {
                Ok(status) => status.code().unwrap_or(-1),
                Err(failure) => { error = Some(format!("等待命令结束失败:{failure}")); -1 }
            }
        };
        if error.is_some() {
            let _ = child.kill().await;
        }
        let drained = matches!(
            tokio::time::timeout(Duration::from_secs(2), async {
                let stdout = (&mut stdout_task).await;
                let stderr = (&mut stderr_task).await;
                (stdout, stderr)
            })
            .await,
            Ok((Ok(Ok(())), Ok(Ok(()))))
        );
        if !drained {
            stdout_task.abort();
            stderr_task.abort();
        }
        let artifact = match capture {
            Some(capture) => Some(capture.finish(drained).await),
            None => None,
        };
        let mut content = format!(
            "退出码: {code}\n{}",
            stdout_preview.lock().await.render(4_000, 8_000)
        );
        let stderr = stderr_preview.lock().await.render(4_000, 8_000);
        if !stderr.is_empty() {
            content.push_str(&format!("\n--- stderr ---\n{stderr}"));
        }
        if let Some(message) = &error {
            content.insert_str(0, &format!("[工具错误] {message}\n"));
        }
        if !drained {
            content.push_str("\n[输出消费未完整结束，产物可能不完整]");
        }
        content.push_str(&crate::output::artifact_notice(artifact.as_ref()));
        ToolOutput {
            content,
            is_error: error.is_some(),
            artifact,
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
            output_store: None,
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
            read_state: None,
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
    async fn megabyte_streams_do_not_deadlock_and_tail_is_readable() {
        let root = std::env::temp_dir().join(format!("denia-bash-{}", uuid::Uuid::new_v4()));
        let storage = crate::output::OutputStore::open(root.clone())
            .await
            .unwrap();
        let mut context = ctx(&std::env::temp_dir());
        context.output_store = Some(storage.clone());
        for streams in ["stdout", "stderr", "both"] {
            let command = if cfg!(windows) {
                let output = if streams == "stderr" {
                    "[Console]::Error"
                } else {
                    "[Console]::Out"
                };
                let extra = if streams == "both" {
                    "; [Console]::Error.Write(('e' * 1048576)); [Console]::Error.WriteLine('tail-error')"
                } else {
                    ""
                };
                format!("{output}.Write(('x' * 1048576)); {output}.WriteLine('tail-error'){extra}")
            } else {
                let redirect = if streams == "stderr" { " >&2" } else { "" };
                let extra = if streams == "both" {
                    "; head -c 1048576 /dev/zero >&2; echo tail-error >&2"
                } else {
                    ""
                };
                format!("head -c 1048576 /dev/zero{redirect}; echo tail-error{redirect}{extra}")
            };
            let arguments = serde_json::json!({"command":command,"timeout_ms":15000}).to_string();
            let result = tokio::time::timeout(
                Duration::from_secs(20),
                BashTool::new().execute(&arguments, &context),
            )
            .await
            .unwrap();
            assert!(!result.is_error, "{}", result.content);
            assert!(result.content.chars().count() < 32000);
            let artifact = result.artifact.unwrap();
            assert!(artifact.complete);
            let stream = if streams == "stdout" {
                "stdout"
            } else {
                "stderr"
            };
            assert!(
                storage
                    .read(&artifact.output_id, Some(stream), 1, 400)
                    .await
                    .unwrap()
                    .contains("tail-error")
            );
        }
        tokio::fs::remove_dir_all(root).await.unwrap();
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
            output_store: None,
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
            read_state: None,
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
