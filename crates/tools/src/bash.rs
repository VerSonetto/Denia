//! The `bash` tool: one shell command per call.
//!
//! 有 [`ShellHub`] 的部署走**常驻 shell**(同一会话的多次调用共享工作目录、
//! 变量与环境),没有则退回一次性进程。形态差异的取舍见
//! [`crate::shell_session`] 的模块文档。

use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use denia_core::session::SessionEvent;
use denia_core::tool::ToolSchema;
use serde::Deserialize;

use crate::SessionEventSink;
use crate::shell_session::ShellHub;
use crate::support::{parse_tool_args, tool_error};
use crate::{Tool, ToolContext, ToolOutput, shell};

/// 实时输出的节流间隔。
///
/// 节奏取"人眼够用、日志不炸"的折中:输出密集时每 [`STREAM_INTERVAL`] 最多
/// 推一条,首块立即推(短命令也要能立刻看到东西)。停顿时由定时分支补推,
/// 见 [`consume_output`]。
const STREAM_INTERVAL: Duration = Duration::from_millis(120);

/// 单次调用的实时输出总量上限。
///
/// 命令结束后 `ToolResult` 的首尾预览与产物已经覆盖完整内容,实时通道再往上
/// 堆只有日志成本;到顶即停推,界面随即由最终结果接替。
const STREAM_BUDGET_BYTES: usize = 128 * 1024;

/// 把命令输出按增量推给会话日志的节流器。
///
/// 三条纪律:
/// 1. **增量**:每次只发新增正文,前端按序拼接(不是整段重发);
/// 2. **UTF-8 安全**:尾部不完整的多字节字符留到下一块 —— 切出半个字符会让
///    替换字符永久留在前端已拼接的历史里;
/// 3. **有界**:单条不超过一个读块,单次调用不超过 [`STREAM_BUDGET_BYTES`]。
///
/// 没有会话身份(`emit_event` 为 None)或没有工具调用 id 时退化为空实现:
/// 单测与裸跑路径不必为此分叉,也拿不到可挂载增量的事件。
struct Streamer {
    sink: Option<SessionEventSink>,
    call_id: String,
    stream: &'static str,
    /// 尚未凑成合法 UTF-8 的尾部字节。
    pending: Vec<u8>,
    last_emit: Option<Instant>,
    emitted: usize,
}

impl Streamer {
    fn new(sink: Option<SessionEventSink>, call_id: Option<&str>, stream: &'static str) -> Self {
        Self {
            sink,
            call_id: call_id.unwrap_or_default().to_string(),
            stream,
            pending: Vec::new(),
            last_emit: None,
            emitted: 0,
        }
    }

    fn enabled(&self) -> bool {
        self.sink.is_some() && !self.call_id.is_empty()
    }

    /// 收下一块输出;到点就推。
    fn push(&mut self, bytes: &[u8]) {
        if !self.enabled() {
            return;
        }
        self.pending.extend_from_slice(bytes);
        self.flush_if_due();
    }

    /// 到点(或首块)才推。调用点有两处:每次读返回、每个节流周期。
    fn flush_if_due(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        if let Some(at) = self.last_emit
            && at.elapsed() < STREAM_INTERVAL
        {
            return;
        }
        let boundary = utf8_boundary(&self.pending);
        if boundary == 0 {
            return;
        }
        let bytes: Vec<u8> = self.pending.drain(..boundary).collect();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        self.emit(&text);
    }

    /// 流结束:补推尾部残留(含被截断的半个字符,由 lossy 收尾)。
    fn finish(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let bytes = std::mem::take(&mut self.pending);
        let text = String::from_utf8_lossy(&bytes).into_owned();
        self.emit(&text);
    }

    fn emit(&mut self, text: &str) {
        let Some(sink) = self.sink.clone() else {
            return;
        };
        if text.is_empty() || self.emitted >= STREAM_BUDGET_BYTES {
            return;
        }
        let room = STREAM_BUDGET_BYTES - self.emitted;
        let text = if text.len() > room {
            &text[..floor_char_boundary(text, room)]
        } else {
            text
        };
        if text.is_empty() {
            return;
        }
        self.emitted += text.len();
        self.last_emit = Some(Instant::now());
        sink(SessionEvent::ToolOutputChunk {
            call_id: self.call_id.clone(),
            stream: self.stream.to_string(),
            text: text.to_string(),
        });
    }
}

/// `pending` 里最后一个完整字符的结尾(可安全切分的长度)。
///
/// 尾部被截断的多字节字符留到下一块;而**真坏字节**要连同它一起放行 ——
/// 否则它会永远卡在缓冲最前面,后面的输出再也发不出去。
fn utf8_boundary(pending: &[u8]) -> usize {
    match std::str::from_utf8(pending) {
        Ok(_) => pending.len(),
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        Err(error) => error.valid_up_to() + error.error_len().unwrap_or(1),
    }
}

/// `str::floor_char_boundary` 的稳定版替身(后者仍是 unstable)。
fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut cut = index.min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    cut
}

async fn consume_output(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    preview: Arc<tokio::sync::Mutex<crate::output::Preview>>,
    capture: Option<crate::output::Capture>,
    stream: &'static str,
    mut streamer: Streamer,
) -> std::io::Result<()> {
    use tokio::io::AsyncReadExt;
    let mut buffer = [0u8; 8192];
    loop {
        tokio::select! {
            read = reader.read(&mut buffer) => {
                let count = read?;
                if count == 0 {
                    break;
                }
                preview.lock().await.push(&buffer[..count]);
                if let Some(capture) = &capture {
                    capture.append(stream, &buffer[..count]).await;
                }
                streamer.push(&buffer[..count]);
            }
            // 输出停顿时也要把已积累的增量推出去:否则"打印一行然后长时间
            // 静默"的命令会把那一行压到命令结束才显示,实时性归零。
            _ = tokio::time::sleep(STREAM_INTERVAL) => streamer.flush_if_due(),
        }
    }
    streamer.finish();
    Ok(())
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
            Streamer::new(ctx.emit_event.clone(), ctx.call_id.as_deref(), "stdout"),
        ));
        let mut stderr_task = tokio::spawn(consume_output(
            child.stderr.take().unwrap(),
            stderr_preview.clone(),
            capture.clone(),
            "stderr",
            Streamer::new(ctx.emit_event.clone(), ctx.call_id.as_deref(), "stderr"),
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
            read_state: None,
        }
    }

    /// 首块立即推(短命令也要能看到实时性),窗口内的后续块先攒着,结束必收尾。
    ///
    /// 白盒地钉住 `last_emit` 而不是真的等 120ms:节流窗口是时钟行为,靠
    /// sleep 去测只会换来一个在慢机器上随机失败的用例。
    #[test]
    fn streamer_is_immediate_then_throttled() {
        let events: Arc<std::sync::Mutex<Vec<SessionEvent>>> = Arc::default();
        let mut streamer = Streamer::new(Some(sink(&events)), Some("call-1"), "stdout");
        streamer.push(b"first");
        assert_eq!(
            deltas_of(&events, "call-1", "stdout"),
            vec!["first".to_string()]
        );

        streamer.last_emit = Some(Instant::now());
        streamer.push(b"second");
        assert_eq!(
            deltas_of(&events, "call-1", "stdout").len(),
            1,
            "节流窗口内不该再推"
        );

        streamer.finish();
        assert_eq!(
            deltas_of(&events, "call-1", "stdout"),
            vec!["first".to_string(), "second".to_string()],
            "结束必须把攒下的补出去"
        );
    }

    /// 增量必须能原样拼回输出:逐字节喂(把每个多字节字符都切在最坏的位置),
    /// 拼回来仍要一字不差、且不出现替换字符 —— 半个字符一旦拼进前端的历史
    /// 就永久留在那里,重连也不会修好。
    #[test]
    fn streamer_deltas_reconstruct_text_without_replacement_chars() {
        let events: Arc<std::sync::Mutex<Vec<SessionEvent>>> = Arc::default();
        let mut streamer = Streamer::new(Some(sink(&events)), Some("call-1"), "stdout");
        let text = "测试🦀中文 stdout\n".repeat(40);
        for byte in text.as_bytes() {
            streamer.push(std::slice::from_ref(byte));
        }
        streamer.finish();
        let deltas = deltas_of(&events, "call-1", "stdout");
        assert!(!deltas.is_empty(), "整段输出不该一个增量都没有");
        assert_eq!(deltas.concat(), text, "增量拼不回原文");
        assert!(
            !deltas.iter().any(|delta| delta.contains('\u{FFFD}')),
            "多字节字符被切开,产生了替换字符"
        );
    }

    /// 实时通道有总量上限:喂满即停,超出部分不再推(命令结束后由结果与
    /// 产物覆盖完整内容,继续往日志里灌只有成本)。
    #[test]
    fn streamer_stops_at_budget() {
        let events: Arc<std::sync::Mutex<Vec<SessionEvent>>> = Arc::default();
        let mut streamer = Streamer::new(Some(sink(&events)), Some("call-1"), "stdout");
        let block = "x".repeat(8192);
        for _ in 0..(STREAM_BUDGET_BYTES / 8192 + 20) {
            streamer.push(block.as_bytes());
        }
        streamer.finish();
        let total: usize = deltas_of(&events, "call-1", "stdout")
            .iter()
            .map(String::len)
            .sum();
        assert_eq!(total, STREAM_BUDGET_BYTES, "推满即止,不多不少");
    }

    /// 没有调用 id(单测、裸跑)时不推任何东西:增量没有可挂载的工具行。
    #[test]
    fn streamer_is_inert_without_a_call_id() {
        let events: Arc<std::sync::Mutex<Vec<SessionEvent>>> = Arc::default();
        let mut streamer = Streamer::new(Some(sink(&events)), None, "stdout");
        streamer.push(b"hello");
        streamer.finish();
        assert!(events.lock().unwrap().is_empty());
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
