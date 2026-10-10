//! Platform shell resolution and the foreground command kernel.
//!
//! Windows: PowerShell (`pwsh` or `powershell.exe`), preferring the Windows
//! Terminal default profile when it points at a PowerShell executable.
//! Unix: `bash -c`.
//!
//! 本模块还持有**前台命令执行内核**([`run_command`]):spawn → stdout/stderr
//! 双流捕获 → 超时/取消/退出三路 select → 退出码与结束原因 → 产物与执行报告。
//! `bash` 的一次性进程路径与 `run_checks` 共用它:"命令怎么算跑过、输出怎么留证"
//! 只有一份实现,判据不会在两个工具之间分叉(检查结论绑在退出码上,分叉就是
//! 把"通过"判成两回事)。

use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::sync::Mutex;
use std::sync::Arc;
#[cfg(windows)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use denia_core::session::SessionEvent;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::output::{OutputArtifact, OutputStore, Preview};
use crate::{ExecutionReport, SessionEventSink, end_reason};

#[cfg(windows)]
static WINDOWS_SHELL: Mutex<Option<PathBuf>> = Mutex::new(None);
/// 最近一次缓存解析的生成号;spawn 失败时递增,触发下次调用重新解析。
#[cfg(windows)]
static SHELL_EPOCH: AtomicUsize = AtomicUsize::new(0);

/// Resolved shell facts for model-facing tool descriptions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellRuntime {
    /// `std::env::consts::OS` (for example `windows`, `linux`, `macos`).
    pub os: &'static str,
    /// `std::env::consts::ARCH` (for example `x86_64`, `aarch64`).
    pub arch: &'static str,
    /// Command dialect the model must write: `powershell` or `bash`.
    pub dialect: &'static str,
    /// Human label for the resolved executable.
    pub shell_label: String,
    /// Resolved executable path shown to the model.
    pub executable: String,
}

/// Facts about the shell this process will spawn for `bash` tool calls.
pub fn shell_runtime() -> ShellRuntime {
    if cfg!(windows) {
        let path = resolve_windows_shell();
        let shell_label = powershell_label(&path);
        ShellRuntime {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            dialect: "powershell",
            shell_label,
            executable: path.display().to_string(),
        }
    } else {
        ShellRuntime {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            dialect: "bash",
            shell_label: "bash".to_string(),
            executable: "bash".to_string(),
        }
    }
}

/// 一次性 shell 与长驻 shell 共用的可执行文件解析。
///
/// 长驻 shell 也必须走这份缓存(而不是自己写 `pwsh`):Store 版 PowerShell
/// 更新会整体换版本目录,硬编码名字会指到不存在的路径。
pub fn shell_executable_path() -> std::path::PathBuf {
    #[cfg(windows)]
    {
        windows_shell_executable()
    }
    #[cfg(not(windows))]
    {
        std::path::PathBuf::from("bash")
    }
}

/// Model-facing `bash` tool description(一次性进程版)with host and shell
/// variables filled in.
///
/// 示例只挑**真该跑 shell**的动作(构建/测试/进程/环境变量)。这里曾把
/// `Get-ChildItem` / `Get-Content` 当示例——本意是演示 PowerShell 方言,
/// 实际成了"用 bash 列目录、读文件"的正面示范,把模型推进了最慢的那条路。
/// 方言示例必须与专用工具的职责边界无交集。
///
/// 末段是**形态**引导。这里曾写"每次调用都新起一个 shell 进程……一条命令做完
/// 一件事"——前半句是事实,后半句是在教模型别分步,直接催生了平均 564 字符的
/// 巨型内联脚本。一次性进程下正确的对策是**落成脚本文件**,不是压成一行。
pub fn bash_tool_description(runtime: &ShellRuntime) -> String {
    format!(
        "{}{}",
        bash_description_head(runtime),
        "每次调用都是一个新进程:工作目录、变量、函数都不保留,状态要靠命令自己带(写绝对路径,或把 cd 与后续动作放进同一条命令)。逻辑成段时,**用 write_file 把它落成一个脚本文件再执行**——改一行只改文件,比反复重发整段命令既稳又省;不要为了\"一次到位\"把多步逻辑压成一条长命令。"
    )
}

/// 常驻 shell 版的描述:状态跨调用保留,所以不必也不该"一条命令做完"。
pub fn persistent_bash_tool_description(runtime: &ShellRuntime) -> String {
    format!(
        "{}{}",
        bash_description_head(runtime),
        "这个 shell 是**常驻**的:工作目录、变量、函数、环境变量都跨调用保留,本会话后续的 bash 调用接着上一次的状态继续。所以按步骤下命令:先 cd 一次,后面就不用再 cd;想先看结果再决定下一步,就分两次调用;逻辑确实成段时,同样可以 write_file 落成脚本文件再执行它。不要为了\"一次到位\"把多步逻辑压成一条长命令。"
    )
}

/// 两版描述的共同前缀:宿主事实 + 方言约束 + 不诱导的示例。
fn bash_description_head(runtime: &ShellRuntime) -> String {
    let (examples, avoid) = if runtime.dialect == "powershell" {
        (
            "git status; cargo test; Get-Process; $env:USERPROFILE",
            "bash/cmd 写法(如 export A=1、echo $A、cmd /c)",
        )
    } else {
        (
            "git status; cargo test; ps aux; echo $HOME",
            "PowerShell 写法(如 $env:A、Get-Process、cmd /c)",
        )
    };
    format!(
        "在会话工作区执行一条 shell 命令,返回退出码与输出(stdout/stderr 合并)。\n\
         宿主:{os}({arch});shell:{shell}({executable})。\n\
         command 只能用 {dialect} 语法,不要写{avoid}。\n\
         本机示例:{examples}。\n",
        os = runtime.os,
        arch = runtime.arch,
        shell = runtime.shell_label,
        executable = runtime.executable,
        dialect = runtime.dialect,
        avoid = avoid,
        examples = examples,
    )
}

/// JSON Schema `command` property description for the `bash` tool.
///
/// 不给"列目录/读文件"类示例:模型会把参数示例当成首选写法照抄。
pub fn bash_command_param_description(runtime: &ShellRuntime) -> String {
    format!(
        "{os}({arch})上 {shell} 的单条 {dialect} 命令行,在会话工作区执行。示例:git status",
        dialect = runtime.dialect,
        shell = runtime.shell_label,
        os = runtime.os,
        arch = runtime.arch,
    )
}

/// One-line shell note for the system prompt (Chinese).
pub fn shell_system_prompt_note(runtime: &ShellRuntime) -> String {
    if runtime.dialect == "powershell" {
        format!(
            "bash 工具在 {os} 上通过 {shell} 执行(工具名沿用 bash);命令请写 PowerShell 语法,不要用 bash/cmd 语法。",
            os = runtime.os,
            shell = runtime.shell_label,
        )
    } else {
        format!(
            "bash 工具在 {os} 上通过 bash 执行;命令请写 bash 语法,不要用 PowerShell 语法。",
            os = runtime.os,
        )
    }
}

#[cfg(windows)]
fn powershell_label(path: &Path) -> String {
    match path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "pwsh.exe" => "PowerShell 7 (pwsh)".to_string(),
        "powershell.exe" => "Windows PowerShell 5.1".to_string(),
        other => other.to_string(),
    }
}

/// Builds a process that runs one shell command in the session workspace.
pub fn shell_command(command: &str) -> Command {
    if cfg!(windows) {
        let program = windows_shell_executable();
        let mut child = Command::new(program);
        // PowerShell 重定向输出默认走系统 OEM 代码页(中文系统为 GBK),服务端
        // 统一按 UTF-8 解码;先注入输出编码前缀,避免中文输出整体乱码。
        child.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; {command}"),
        ]);
        child
    } else {
        let mut child = Command::new("bash");
        child.args(["-c", command]);
        child
    }
}

/// 终止进程及其全部子孙进程,然后回收直接子进程。
///
/// `bash` 工具的前台一次性进程与后台任务共用这一份实现:两者都在 select 的
/// 取消/超时分支里清理,各自的判定条件不变。
///
/// Windows 上 [`tokio::process::Child::kill`] 走 `TerminateProcess`,只管直接
/// 子进程:shell 拉起的 cargo/node 等后代不受影响,取消后继续占端口、文件锁与
/// CPU。所以先按 PID 用 `taskkill /T /F` 连根拔,再 kill 直接子进程兜底(它已
/// 经退出时返回错误,忽略即可)。
///
/// 非 Windows 保持原有语义:只终止直接子进程。Unix 侧要连根拔得先在 spawn 时
/// 把子进程设成新进程组组长(否则 `killpg(pid)` 可能命中同号的无关进程组),
/// 那是 spawn 行为的改动,不在本次范围。
pub async fn kill_tree(pid: Option<u32>, child: &mut tokio::process::Child) {
    #[cfg(windows)]
    {
        // taskkill 是控制台程序:不加 CREATE_NO_WINDOW 会闪一个黑窗。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        if let Some(pid) = pid {
            let mut command = Command::new("taskkill.exe");
            command
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .creation_flags(CREATE_NO_WINDOW);
            let _ = command.output().await;
        }
    }
    #[cfg(not(windows))]
    let _ = pid;
    let _ = child.kill().await;
    let _ = child.wait().await;
}

// ---------------------------------------------------------------------------
// 前台命令执行内核
// ---------------------------------------------------------------------------

/// 前台命令的默认超时(毫秒)。
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// 前台命令的超时上限(毫秒)。更长的等待交给后台任务,不要靠加大超时硬等。
pub const MAX_TIMEOUT_MS: u64 = 600_000;

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
pub(crate) const STREAM_BUDGET_BYTES: usize = 128 * 1024;

/// 把命令输出按增量推给会话日志的节流器。
///
/// 三条纪律:
/// 1. **增量**:每次只发新增正文,前端按序拼接(不是整段重发);
/// 2. **UTF-8 安全**:尾部不完整的多字节字符留到下一块 —— 切出半个字符会让
///    替换字符永久留在前端已拼接的历史里;
/// 3. **有界**:单条不超过一个读块,单次调用不超过 [`STREAM_BUDGET_BYTES`]。
///
/// 没有会话身份(`sink` 为 None)或没有工具调用 id 时退化为空实现:
/// 单测与裸跑路径不必为此分叉,也拿不到可挂载增量的事件。
pub(crate) struct Streamer {
    sink: Option<SessionEventSink>,
    call_id: String,
    stream: &'static str,
    /// 尚未凑成合法 UTF-8 的尾部字节。
    pending: Vec<u8>,
    last_emit: Option<Instant>,
    emitted: usize,
}

impl Streamer {
    pub(crate) fn new(
        sink: Option<SessionEventSink>,
        call_id: Option<&str>,
        stream: &'static str,
    ) -> Self {
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
    pub(crate) fn push(&mut self, bytes: &[u8]) {
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
    preview: Arc<tokio::sync::Mutex<Preview>>,
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

/// 实时增量通道:把命令输出按增量推给会话日志的两端。
#[derive(Clone, Default)]
pub struct StreamSpec {
    /// 会话事件 sink;`None` = 不推增量。
    pub sink: Option<SessionEventSink>,
    /// 当前工具调用 id(增量挂到这条工具行上);缺了也不推。
    pub call_id: Option<String>,
}

/// 一次前台命令的执行规格。
pub struct CommandSpec<'a> {
    /// 命令行(交给宿主 shell,方言见 [`shell_command`])。
    pub command: &'a str,
    /// 工作目录。
    pub cwd: &'a Path,
    /// 超时(毫秒;调用方负责归一到 [`MAX_TIMEOUT_MS`] 之内)。
    pub timeout_ms: u64,
    /// 输出产物存储;`None` = 本次调用不留产物。
    pub output_store: Option<&'a Arc<OutputStore>>,
    /// 实时增量通道;`None` = 不推增量(单测与无会话身份的调用)。
    pub stream: Option<StreamSpec>,
    /// 结果预览的首尾字符预算 `(head, tail)`:完整输出留在产物里,预览只
    /// 服务"模型看得见关键结论"与日志成本。多命令调用(如检查)要调小。
    pub preview_chars: (usize, usize),
    /// 超时文案里跟在"；"之后的下一步建议(不同工具该做的事不同)。
    pub timeout_hint: &'a str,
}

/// 一次前台命令的运行事实。
///
/// 与文案([`CommandRun::render`])同源:界面读这里,不再去解析首行。
pub struct CommandRun {
    /// 进程退出码;`None` = 没拿到(超时 / 被中断 / 启动或等待失败)。
    pub exit_code: Option<i32>,
    /// 结束原因,取值见 [`crate::end_reason`]。
    pub ended: &'static str,
    /// 工具侧错误文案(启动失败 / 等待失败 / 超时 / 被中断);`None` = 跑完了。
    pub error: Option<String>,
    /// 命令**没能启动**(可执行文件不可用、启动被拒……)。与"跑完但非零"和
    /// "等它结束失败"是三种不同的事实:只有这一种意味着命令根本没跑。
    pub not_started: bool,
    /// stdout 预览。
    pub stdout: String,
    /// stderr 预览。
    pub stderr: String,
    /// 输出产物(有存储时)。
    pub artifact: Option<OutputArtifact>,
    /// 输出消费是否完整结束。
    pub drained: bool,
}

impl CommandRun {
    /// 命令没跑起来:没有退出码,也没有输出可谈。
    fn not_started(message: String) -> Self {
        Self {
            exit_code: None,
            ended: end_reason::SPAWN_FAILED,
            error: Some(message),
            not_started: true,
            stdout: String::new(),
            stderr: String::new(),
            artifact: None,
            drained: true,
        }
    }

    /// 结构化执行报告(界面与诊断用,与 [`Self::render`] 同源)。
    pub fn report(&self) -> ExecutionReport {
        ExecutionReport {
            exit_code: self.exit_code,
            end_reason: Some(self.ended.to_string()),
            // 命令间接改动的文件推测不出来(命令本身就是任意程序),留空。
            files: Vec::new(),
            evidence_complete: self.artifact.as_ref().map(|item| item.complete),
        }
    }

    /// 模型可读的结果文本(退出码首行 + stdout + stderr + 各种提示)。
    pub fn render(&self) -> String {
        // 没有退出码时首行给 `-1` 哨兵:它只是文案里的展示约定,结构化字段
        // (`exit_code: None`)才区分"没拿到"与"取值为 0"。
        let mut content = format!("退出码: {}\n{}", self.exit_code.unwrap_or(-1), self.stdout);
        if !self.stderr.is_empty() {
            content.push_str(&format!("\n--- stderr ---\n{}", self.stderr));
        }
        if let Some(message) = &self.error {
            content.insert_str(0, &format!("[工具错误] {message}\n"));
        }
        if !self.drained {
            content.push_str("\n[输出消费未完整结束，产物可能不完整]");
        }
        content.push_str(&crate::output::artifact_notice(self.artifact.as_ref()));
        content
    }
}

/// 跑一条前台命令并捕获输出。
///
/// 语义是"等它跑完":超时、用户中断与正常退出三路都收成一个 [`CommandRun`],
/// 调用方据此判定(退出码、结束原因),不必自己管进程清理 —— 取消与超时都会
/// **连根拔**子孙进程([`kill_tree`]),否则 shell 拉起的 cargo/node 会继续
/// 占着端口、文件锁与输出管道。
pub async fn run_command(spec: CommandSpec<'_>, cancel: &CancellationToken) -> CommandRun {
    let spawn_once = || {
        let mut command = shell_command(spec.command);
        command
            .current_dir(spec.cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        command.spawn()
    };
    let mut child = match spawn_once() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // os error 2/3:shell 可执行文件路径失效(如 Store 版 PowerShell
            // 更新后按版本目录整体换路径)。清缓存重解析并重试一次;仍失败
            // 才是真错误。
            invalidate_windows_shell_cache();
            match spawn_once() {
                Ok(child) => child,
                Err(retry_error) => {
                    return CommandRun::not_started(format!("命令启动失败:{retry_error}"));
                }
            }
        }
        Err(error) => return CommandRun::not_started(format!("命令启动失败:{error}")),
    };

    let capture = match spec.output_store {
        Some(store) => Some(store.start(&["stdout", "stderr"]).await),
        None => None,
    };
    let stdout_preview = Arc::new(tokio::sync::Mutex::new(Preview::default()));
    let stderr_preview = Arc::new(tokio::sync::Mutex::new(Preview::default()));
    let mk_streamer = |name: &'static str| match &spec.stream {
        Some(stream) => Streamer::new(stream.sink.clone(), stream.call_id.as_deref(), name),
        None => Streamer::new(None, None, name),
    };
    let mut stdout_task = tokio::spawn(consume_output(
        child.stdout.take().expect("stdout is piped"),
        stdout_preview.clone(),
        capture.clone(),
        "stdout",
        mk_streamer("stdout"),
    ));
    let mut stderr_task = tokio::spawn(consume_output(
        child.stderr.take().expect("stderr is piped"),
        stderr_preview.clone(),
        capture.clone(),
        "stderr",
        mk_streamer("stderr"),
    ));
    // 清理子孙进程要按 PID 连根拔,而 select 的分支里借不到 child(等待分支
    // 已经拿走了可变借用),所以 pid 得提前取。
    let pid = child.id();
    let mut error = None;
    // 结束原因是给界面/日志的事实:退出码 -1 只是文案里的哨兵,用它表达
    // "超时/被中断"会把两类结果混成一种(见 `ExecutionReport` 的注释)。
    let mut ended = end_reason::COMPLETED;
    let mut exit_code = None;
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            error = Some("命令被用户中断".to_string());
            ended = end_reason::CANCELLED;
        }
        _ = tokio::time::sleep(Duration::from_millis(spec.timeout_ms)) => {
            error = Some(format!("命令超时({} ms 未结束)；{}", spec.timeout_ms, spec.timeout_hint));
            ended = end_reason::TIMEOUT;
        }
        result = child.wait() => match result {
            Ok(status) => exit_code = status.code(),
            Err(failure) => {
                error = Some(format!("等待命令结束失败:{failure}"));
                ended = end_reason::SPAWN_FAILED;
            }
        }
    }
    if error.is_some() {
        kill_tree(pid, &mut child).await;
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
    let (head, tail) = spec.preview_chars;
    let stdout = stdout_preview.lock().await.render(head, tail);
    let stderr = stderr_preview.lock().await.render(head, tail);
    CommandRun {
        exit_code,
        ended,
        error,
        not_started: false,
        stdout,
        stderr,
        artifact,
        drained,
    }
}

/// Marks the cached shell path as stale; the next resolution re-scans.
///
/// Store 版 PowerShell(WindowsApps)更新会按版本目录整体换路径,长驻进程
/// 缓存的可执行文件路径会失效(os error 3),必须允许后续调用重新解析。
#[cfg(windows)]
pub fn invalidate_windows_shell_cache() {
    SHELL_EPOCH.fetch_add(1, Ordering::Release);
}

#[cfg(windows)]
fn windows_shell_executable() -> PathBuf {
    let epoch = SHELL_EPOCH.load(Ordering::Acquire);
    let mut cached = WINDOWS_SHELL.lock().expect("shell cache poisoned");
    if let Some(path) = &*cached {
        // 缓存代数仍一致且文件仍在 → 直接复用;文件被删除时降级重解析。
        let still_valid = SHELL_EPOCH.load(Ordering::Acquire) == epoch && path.is_file();
        if still_valid {
            return path.clone();
        }
        if SHELL_EPOCH.load(Ordering::Acquire) == epoch {
            *cached = None;
        }
    }
    let path = resolve_windows_shell();
    *cached = Some(path.clone());
    path
}

#[cfg(windows)]
fn resolve_windows_shell() -> PathBuf {
    if let Some(path) = windows_terminal_default_powershell() {
        return path;
    }
    if let Some(path) = first_existing_pwsh_candidate() {
        return path;
    }
    PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe")
}

#[cfg(windows)]
fn windows_terminal_default_powershell() -> Option<PathBuf> {
    for path in windows_terminal_settings_paths() {
        let settings = read_json_lenient(&path)?;
        if let Some(commandline) = default_profile_commandline(&settings)
            && let Some(shell) = powershell_from_commandline(&commandline)
        {
            return Some(shell);
        }
    }
    None
}

#[cfg(windows)]
fn windows_terminal_settings_paths() -> Vec<PathBuf> {
    let Some(local) = std::env::var_os("LOCALAPPDATA") else {
        return Vec::new();
    };
    let local = PathBuf::from(local);
    vec![
        local.join("Packages/Microsoft.WindowsTerminal_8wekyb3d8bbwe/LocalState/settings.json"),
        local.join("Microsoft/Windows Terminal/settings.json"),
    ]
}

#[cfg(windows)]
fn read_json_lenient(path: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text)
        .ok()
        .or_else(|| serde_json::from_str(&strip_json_line_comments(&text)).ok())
}

#[cfg(windows)]
fn strip_json_line_comments(text: &str) -> String {
    text.lines()
        .map(|line| line.split("//").next().unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(windows)]
fn default_profile_commandline(settings: &serde_json::Value) -> Option<String> {
    let default = settings.get("defaultProfile")?.as_str()?;
    let list = settings.get("profiles")?.get("list")?.as_array()?;
    let mut current = normalize_guid(default);
    for _ in 0..8 {
        let profile = list.iter().find(|entry| {
            entry
                .get("guid")
                .and_then(|guid| guid.as_str())
                .is_some_and(|guid| normalize_guid(guid) == current)
        })?;
        if let Some(source) = profile.get("source").and_then(|value| value.as_str()) {
            current = normalize_guid(source);
            continue;
        }
        return profile
            .get("commandline")
            .and_then(|value| value.as_str())
            .map(str::to_string);
    }
    None
}

#[cfg(windows)]
fn normalize_guid(guid: &str) -> String {
    guid.trim_matches('{')
        .trim_matches('}')
        .to_ascii_lowercase()
}

#[cfg(windows)]
fn powershell_from_commandline(commandline: &str) -> Option<PathBuf> {
    let expanded = expand_percent_env(commandline);
    let token = first_token(&expanded)?;
    let path = PathBuf::from(token);
    if path.is_file() && is_powershell_name(path.file_name()?.to_str()?) {
        return Some(path);
    }
    if is_powershell_name(token) {
        return resolve_powershell_name(token);
    }
    None
}

#[cfg(windows)]
fn expand_percent_env(text: &str) -> String {
    let mut out = text.to_string();
    for (key, value) in std::env::vars() {
        out = out.replace(&format!("%{key}%"), &value);
    }
    out
}

#[cfg(windows)]
fn first_token(text: &str) -> Option<&str> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(rest) = text.strip_prefix('"') {
        let end = rest.find('"')?;
        return Some(&rest[..end]);
    }
    text.split_whitespace().next()
}

#[cfg(windows)]
fn is_powershell_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "pwsh.exe" | "pwsh" | "powershell.exe" | "powershell"
    )
}

#[cfg(windows)]
fn resolve_powershell_name(name: &str) -> Option<PathBuf> {
    if name.eq_ignore_ascii_case("pwsh") || name.eq_ignore_ascii_case("pwsh.exe") {
        return first_existing_pwsh_candidate();
    }
    let system_root = std::env::var_os("SystemRoot").map(PathBuf::from)?;
    let path = system_root.join("System32/WindowsPowerShell/v1.0/powershell.exe");
    path.is_file().then_some(path)
}

#[cfg(windows)]
fn candidate_pwsh_paths() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(program_files) = std::env::var("ProgramFiles") {
        candidates.push(PathBuf::from(program_files).join("PowerShell/7/pwsh.exe"));
    }
    // Store 版 PowerShell 的执行别名:跨版本更新保持不变,优先于
    // PATH 里按版本号命名的 WindowsApps 包目录(更新后旧目录会被删除)。
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(local)
                .join("Microsoft")
                .join("WindowsApps")
                .join("pwsh.exe"),
        );
    }
    if let Ok(path) = std::env::var("PATH") {
        for entry in path.split(';') {
            let trimmed = entry.trim().trim_matches('"');
            if !trimmed.is_empty() {
                candidates.push(PathBuf::from(trimmed).join("pwsh.exe"));
            }
        }
    }
    candidates.push(PathBuf::from(
        r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
    ));
    candidates
}

#[cfg(windows)]
fn first_existing_pwsh_candidate() -> Option<PathBuf> {
    candidate_pwsh_paths()
        .into_iter()
        .find(|candidate| candidate.is_file())
}

/// `kill_tree` 的测试脚手架:拉起「外层 shell → 内层 shell」的进程树,并观察
/// 孙进程的生死。前台 bash 与后台任务共用同一条清理路径,脚手架也共用。
#[cfg(all(test, windows))]
pub(crate) mod test_tree {
    use super::windows_shell_executable;
    use std::path::Path;
    use std::time::{Duration, Instant};
    use tokio::process::Command;

    /// 一条命令拉起这棵树:外层 shell 再拉起一个内层 shell,内层把**自己的**
    /// PID 写进 `marker` 后长睡,外层同样长睡。形状与工具跑 `cargo build` 一致
    /// ——直接子进程是 shell,真正干活的是它的后代。
    pub(crate) fn nested_sleeper_command(marker: &Path) -> String {
        let marker = marker.display();
        format!(
            "$exe = (Get-Process -Id $PID).Path; \
             $inner = 'Set-Content -LiteralPath \"{marker}\" -Value $PID; Start-Sleep -Seconds 120'; \
             & $exe -NoLogo -NoProfile -NonInteractive -Command $inner; \
             Start-Sleep -Seconds 120"
        )
    }

    /// 读孙进程报出的 PID(还没写出来就返回 None)。
    pub(crate) fn read_pid(marker: &Path) -> Option<u32> {
        std::fs::read_to_string(marker).ok()?.trim().parse().ok()
    }

    /// 等 `nested_sleeper_command` 的孙进程报出自己的 PID。
    pub(crate) async fn wait_for_pid(marker: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(pid) = read_pid(marker) {
                return pid;
            }
            assert!(
                Instant::now() < deadline,
                "进程树没有拉起孙进程(缺 {} 里的 PID)",
                marker.display()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// 进程是否还活着。查不出来时当“活着”:失败不能伪装成通过。
    pub(crate) async fn process_alive(pid: u32) -> bool {
        let script = format!(
            "if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ 'alive' }} else {{ 'gone' }}"
        );
        let output = Command::new(windows_shell_executable())
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &script,
            ])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .output()
            .await;
        match output {
            Ok(output) => String::from_utf8_lossy(&output.stdout).contains("alive"),
            Err(_) => true,
        }
    }

    /// 等进程消失(给终止一个落地的宽限,而不是假定它瞬间生效)。
    pub(crate) async fn wait_until_gone(pid: u32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if !process_alive(pid).await {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::{first_token, is_powershell_name, resolve_windows_shell};

    #[cfg(windows)]
    #[test]
    fn first_token_handles_quotes() {
        assert_eq!(
            first_token(r#""C:\Program Files\pwsh.exe" -NoLogo"#),
            Some(r"C:\Program Files\pwsh.exe")
        );
        assert_eq!(first_token("pwsh.exe"), Some("pwsh.exe"));
    }

    #[cfg(windows)]
    #[test]
    fn powershell_name_detection() {
        assert!(is_powershell_name("pwsh.exe"));
        assert!(is_powershell_name("PowerShell.exe"));
        assert!(!is_powershell_name("cmd.exe"));
    }

    #[cfg(windows)]
    #[test]
    fn resolves_some_powershell_on_this_host() {
        let shell = resolve_windows_shell();
        let name = shell
            .file_name()
            .and_then(|part| part.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        assert!(
            name == "pwsh.exe" || name == "powershell.exe",
            "expected PowerShell, got {}",
            shell.display()
        );
        assert!(shell.is_file(), "{}", shell.display());
    }

    #[cfg(windows)]
    #[test]
    fn cache_hit_and_invalidation_reparse() {
        // 首次解析并缓存。
        let first = super::windows_shell_executable();
        assert!(first.is_file());
        // 命中缓存:同一路径。
        let second = super::windows_shell_executable();
        assert_eq!(first, second);
        // 失效后重新解析:结果仍是一个存在的可执行文件(缓存自愈)。
        super::invalidate_windows_shell_cache();
        let third = super::windows_shell_executable();
        assert!(third.is_file());
    }

    #[cfg(windows)]
    #[test]
    fn store_alias_candidate_preferred_over_versioned_path() {
        // 候选顺序:Store 别名目录必须在 PATH 版本目录之前——
        // Store 更新按版本目录换路径,别名不受影响。
        let candidates = super::candidate_pwsh_paths();
        let alias = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap())
            .join("Microsoft")
            .join("WindowsApps")
            .join("pwsh.exe");
        let alias_index = candidates
            .iter()
            .position(|c| c.as_os_str() == alias.as_os_str())
            .expect("WindowsApps 别名应始终在候选列表中");
        let versioned = candidates.iter().position(|c| {
            c.to_string_lossy()
                .to_ascii_lowercase()
                .contains("windowsapps\\microsoft.powershell_")
        });
        if let Some(versioned) = versioned {
            assert!(
                alias_index < versioned,
                "WindowsApps 别名({alias_index})应排在版本目录({versioned})之前"
            );
        }
    }

    /// 取消/超时要连根拔:只终止直接子进程时,shell 拉起的后代会被留下继续
    /// 占端口、文件锁与 CPU(Windows 上 tokio 只 TerminateProcess 子进程本身)。
    #[cfg(windows)]
    #[tokio::test]
    async fn kill_tree_reaps_descendant_processes() {
        let dir = std::env::temp_dir().join(format!("denia-kill-tree-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("grandchild.pid");
        let mut child = super::shell_command(&super::test_tree::nested_sleeper_command(&marker))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id();
        let grandchild = super::test_tree::wait_for_pid(&marker).await;
        assert!(
            super::test_tree::process_alive(grandchild).await,
            "孙进程 {grandchild} 应该在树里活着"
        );
        super::kill_tree(pid, &mut child).await;
        assert!(
            super::test_tree::wait_until_gone(grandchild).await,
            "kill_tree 之后孙进程 {grandchild} 仍在运行,进程树没被连根拔"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bash_tool_description_includes_host_and_shell() {
        let runtime = super::shell_runtime();
        let description = super::bash_tool_description(&runtime);
        assert!(description.contains(runtime.os));
        assert!(description.contains(runtime.arch));
        assert!(description.contains(&runtime.shell_label));
        assert!(description.contains(runtime.dialect));
    }

    /// 回归保护:bash 描述不能再拿"列目录/读文件"当示例。
    ///
    /// 模型会把参数示例当成首选写法照抄。这里曾写
    /// `本机示例:Get-ChildItem; Get-Content .\src\lib.rs`——本意是演示
    /// PowerShell 方言,实际成了用 bash 列目录、读文件的正面示范,而这两件事
    /// 都有专用工具(ls / read_file),shell 版本还会扫进 .gitignore 忽略的
    /// 目录树,慢几个数量级。
    #[test]
    fn bash_description_never_advertises_listing_or_reading_examples() {
        for dialect in ["powershell", "bash"] {
            let runtime = super::ShellRuntime {
                os: std::env::consts::OS,
                arch: std::env::consts::ARCH,
                dialect,
                shell_label: dialect.to_string(),
                executable: dialect.to_string(),
            };
            for text in [
                super::bash_tool_description(&runtime),
                super::bash_command_param_description(&runtime),
            ] {
                for banned in [
                    "Get-ChildItem",
                    "Get-Content",
                    "Select-String",
                    "ls -la",
                    "cat ",
                    "find ",
                    "grep ",
                    "dir /s",
                ] {
                    assert!(
                        !text.contains(banned),
                        "bash 描述在 {dialect} 方言下出现了诱导性示例 {banned:?}:{text}"
                    );
                }
            }
        }
    }

    // ---------------------------------------------------------------------
    // 实时增量(+ 供 bash 的端到端用例复用的两个脚手架)
    // ---------------------------------------------------------------------

    use std::sync::Arc;
    use std::time::Instant;

    use super::{STREAM_BUDGET_BYTES, SessionEvent, SessionEventSink, Streamer};

    /// 收事件的测试 sink。
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
}
