//! The `bash` tool: one shell command per call.
//!
//! 有 [`ShellHub`] 的部署走**常驻 shell**(同一会话的多次调用共享工作目录、
//! 变量与环境),没有则退回一次性进程。形态差异的取舍见
//! [`crate::shell_session`] 的模块文档。一次性进程的执行内核抽在
//! [`crate::shell::run_command`],与 `run_checks` 共用一份实现。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use denia_core::tool::ToolSchema;
use serde::Deserialize;

use crate::shell::{self, CommandSpec, DEFAULT_TIMEOUT_MS, MAX_TIMEOUT_MS, StreamSpec};
use crate::shell_session::ShellHub;
use crate::support::{parse_tool_args, tool_error};
use crate::{ExecutionReport, Tool, ToolContext, ToolOutput, end_reason};

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
    ///
    /// 这里没有实时输出增量:命令与结果都关在 `PersistentShell::run` 里,
    /// 拿不到读数回调(且它把 stdout/stderr 合并成一条流)。当前部署没有挂
    /// [`ShellHub`],走不到这条路径;真要启用时得先把增量回调补上,否则
    /// 常驻形态的 bash 会静默失去实时输出。
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
                )
                .with_report(ExecutionReport::ended(end_reason::SPAWN_FAILED));
            }
            Err(join_error) => {
                return tool_error(
                    format!("持久 shell 启动任务失败:{join_error}"),
                    "请重试一次",
                )
                .with_report(ExecutionReport::ended(end_reason::SPAWN_FAILED));
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
                .with_report(ExecutionReport::ended(end_reason::CANCELLED))
            }
            result = runner => match result {
                Ok(Ok(captured)) => ToolOutput::text(format!(
                    "退出码: {}\n{}",
                    captured.exit_code.unwrap_or(-1),
                    captured.text
                ))
                .with_report(ExecutionReport::completed(captured.exit_code)),
                Ok(Err(message)) => ToolOutput::error(format!("[工具错误] {message}"))
                    .with_report(ExecutionReport::ended(end_reason::SPAWN_FAILED)),
                Err(join_error) => {
                    tool_error(format!("命令执行任务失败:{join_error}"), "请重试一次")
                        .with_report(ExecutionReport::ended(end_reason::SPAWN_FAILED))
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

        // 挂了常驻 shell 注册表且当前调用归属某个会话 → 走持久路径。
        // 没有会话身份的调用(单测、无宿主的裸跑)退回一次性进程。
        if let (Some(hub), Some(session_id)) = (&self.hub, ctx.session_id.as_deref()) {
            return self
                .execute_persistent(hub, session_id, &args.command, requested_ms, ctx)
                .await;
        }

        // 一次性进程路径:执行内核与 `run_checks` 共用(见 [`shell::run_command`]),
        // 差别只在超时文案与预览预算。
        let run = shell::run_command(
            CommandSpec {
                command: &args.command,
                cwd: &ctx.cwd,
                timeout_ms: requested_ms,
                output_store: ctx.output_store.as_ref(),
                stream: Some(StreamSpec {
                    sink: ctx.emit_event.clone(),
                    call_id: ctx.call_id.clone(),
                }),
                preview_chars: (4_000, 8_000),
                timeout_hint: "可拆小命令或使用 run_in_background 后台运行",
            },
            &ctx.cancel,
        )
        .await;
        // 命令没能启动是工具层的事实(启动失败),与"跑完但非零"严格区分:
        // 后者是命令自己的结果,如实上报而不是升级成工具失败。
        if run.not_started {
            let message = run
                .error
                .clone()
                .unwrap_or_else(|| "命令启动失败".to_string());
            return tool_error(message, "确认命令在该宿主 shell 上可用;工作目录是会话工作区")
                .with_report(run.report());
        }
        ToolOutput {
            content: run.render(),
            is_error: run.error.is_some(),
            artifact: run.artifact.clone(),
            report: Some(run.report()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::session::{PermissionMode, SessionEvent};
    use tokio_util::sync::CancellationToken;

    use crate::SessionEventSink;

    fn sink(events: &Arc<std::sync::Mutex<Vec<SessionEvent>>>) -> SessionEventSink {
        let events = events.clone();
        Arc::new(move |event| events.lock().unwrap().push(event))
    }

    /// 某次调用某条流上收到的增量,按到达顺序。
    fn deltas_of(
        events: &Arc<std::sync::Mutex<Vec<SessionEvent>>>,
        call_id: &str,
        stream: &str,
    ) -> Vec<String> {
        events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                SessionEvent::ToolOutputChunk {
                    call_id: id,
                    stream: name,
                    text,
                } if id == call_id && name == stream => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

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
            task_ledger: None,
            read_state: None,
        }
    }

    /// 端到端:一次真实命令会把输出按增量推出去,且推的是**命令的输出** ——
    /// 工具自己的框架行(退出码)不在其中。
    #[tokio::test]
    async fn streams_a_real_command_without_framing_lines() {
        let dir = std::env::temp_dir();
        let events: Arc<std::sync::Mutex<Vec<SessionEvent>>> = Arc::default();
        let mut context = ctx(&dir);
        context.emit_event = Some(sink(&events));
        context.call_id = Some("call-1".to_string());
        let command = if cfg!(windows) {
            "Write-Output 'line1'; Write-Output 'line2'; Write-Output 'line3'"
        } else {
            "echo line1; echo line2; echo line3"
        };
        let arguments = serde_json::json!({"command": command, "timeout_ms": 30000}).to_string();
        let out = BashTool::new().execute(&arguments, &context).await;
        assert!(!out.is_error, "{}", out.content);
        let streamed = deltas_of(&events, "call-1", "stdout").concat();
        for line in ["line1", "line2", "line3"] {
            assert!(
                out.content.contains(line),
                "结果里缺 {line}:{}",
                out.content
            );
            assert!(streamed.contains(line), "实时增量里缺 {line}:{streamed}");
        }
        assert!(
            !streamed.contains("退出码"),
            "框架行不该进实时增量:{streamed}"
        );
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
        // 结构化事实与文案同源:界面读 `report.exit_code`,不必再解析首行。
        let report = ok.report.as_ref().expect("正常返回一定带执行报告");
        assert_eq!(report.exit_code, Some(0));
        assert_eq!(report.end_reason.as_deref(), Some(end_reason::COMPLETED));

        let bad = tool.execute(r#"{"command":"exit 3"}"#, &ctx(&dir)).await;
        assert!(!bad.is_error, "non-zero exit is data");
        assert!(bad.content.starts_with("退出码: 3"), "{}", bad.content);
        // 非零退出码如实上报,但不把 is_error 翻成 true —— 这是命令的结果,
        // 不是工具失败。
        assert_eq!(
            bad.report.as_ref().and_then(|report| report.exit_code),
            Some(3)
        );
        assert!(!bad.is_error, "非零退出不得升级成工具失败");
    }

    #[tokio::test]
    async fn timeout_reports_reason_without_exit_code() {
        let dir = std::env::temp_dir();
        let tool = BashTool::new();
        let sleeping = if cfg!(windows) {
            r#"{"command":"ping -n 10 127.0.0.1 >nul","timeout_ms":200}"#
        } else {
            r#"{"command":"sleep 10","timeout_ms":200}"#
        };
        let out = tool.execute(sleeping, &ctx(&dir)).await;
        let report = out.report.as_ref().expect("超时也要留下结束原因");
        assert_eq!(report.end_reason.as_deref(), Some(end_reason::TIMEOUT));
        // 文案里的 `-1` 只是哨兵;结构化字段必须能区分"没拿到退出码"与
        // "退出码是 -1"。
        assert_eq!(report.exit_code, None);
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
            task_ledger: None,
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

    /// 取消前台命令时,shell 拉起的孙进程也必须一起消失。
    ///
    /// 回归用例:此前取消分支只 `child.kill()`,而 tokio 在 Windows 上走
    /// TerminateProcess —— shell 死了,它拉起的 cargo/node 等后代继续跑,
    /// 还握着输出管道(于是 drain 只能等满 2 秒再 abort)。
    #[cfg(windows)]
    #[tokio::test]
    async fn cancel_kills_grandchild_processes() {
        let dir = std::env::temp_dir().join(format!("denia-bash-cancel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("grandchild.pid");
        let cancel = CancellationToken::new();
        let mut context = ctx(&dir);
        context.cancel = cancel.clone();
        let arguments = serde_json::json!({
            "command": crate::shell::test_tree::nested_sleeper_command(&marker),
            "timeout_ms": 60_000,
        })
        .to_string();
        let tool = BashTool::new();
        let handle = tokio::spawn(async move { tool.execute(&arguments, &context).await });
        // 等进程树真的拉起来(孙进程写出自己的 PID)再取消,免得测的是竞态。
        let grandchild = crate::shell::test_tree::wait_for_pid(&marker).await;
        cancel.cancel();
        let out = handle.await.unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("中断"), "{}", out.content);
        assert!(
            crate::shell::test_tree::wait_until_gone(grandchild).await,
            "取消后孙进程 {grandchild} 还在跑:前台清理没有连根拔"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// 超时同样要连根拔。
    ///
    /// 这里不看 PID 也能判定:泄漏的孙进程握着输出管道不放,read 任务永远到不了
    /// EOF,工具只能等满 2 秒再 abort 并退回“输出消费未完整结束”。连根拔之后
    /// 管道正常关闭,这句话不应该出现。
    #[cfg(windows)]
    #[tokio::test]
    async fn timeout_kills_grandchild_processes() {
        let dir = std::env::temp_dir().join(format!("denia-bash-timeout-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("grandchild.pid");
        let arguments = serde_json::json!({
            "command": crate::shell::test_tree::nested_sleeper_command(&marker),
            "timeout_ms": 2_000,
        })
        .to_string();
        let out = BashTool::new().execute(&arguments, &ctx(&dir)).await;
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("超时"), "{}", out.content);
        assert!(
            !out.content.contains("输出消费未完整结束"),
            "超时后还有后代握着输出管道:{}",
            out.content
        );
        // 机器慢时内层 shell 可能还没起来(那就只剩管道这条信号),起来了就必须死。
        if let Some(grandchild) = crate::shell::test_tree::read_pid(&marker) {
            assert!(
                crate::shell::test_tree::wait_until_gone(grandchild).await,
                "超时后孙进程 {grandchild} 还在跑:前台清理没有连根拔"
            );
        }
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
