//! Platform shell resolution and the foreground command kernel.
//!
//! Windows: follow the Windows Terminal default profile when it points at a
//! PowerShell executable (`pwsh`/`powershell.exe`) or Git Bash (`bash.exe`);
//! otherwise prefer `pwsh`, then Windows PowerShell. `DENIA_SHELL` overrides the
//! whole chain with an explicit executable path or name.
//! Unix: `bash -c`.
//!
//! 本模块还持有**前台命令执行内核**([`run_command`]):spawn → stdout/stderr
//! 双流捕获 → 超时/取消/退出三路 select → 退出码与结束原因 → 产物与执行报告。
//! `bash` 的一次性进程路径与 `run_checks` 共用它:"命令怎么算跑过、输出怎么留证"
//! 只有一份实现,判据不会在两个工具之间分叉(检查结论绑在退出码上,分叉就是
//! 把"通过"判成两回事)。

use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(windows)]
use std::sync::Mutex;
#[cfg(windows)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use denia_core::session::SessionEvent;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::output::{OutputArtifact, OutputStore, Preview};
use crate::{ExecutionReport, SessionEventSink, end_reason};

#[cfg(windows)]
static WINDOWS_SHELL: Mutex<Option<ResolvedShell>> = Mutex::new(None);
/// 最近一次缓存解析的生成号;spawn 失败时递增,触发下次调用重新解析。
#[cfg(windows)]
static SHELL_EPOCH: AtomicUsize = AtomicUsize::new(0);

/// Windows 上实际参与执行的 shell 方言。它决定命令怎么传参、工具描述让模型
/// 写哪种语法,以及常驻 shell 用哪套包装协议。
#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellDialect {
    PowerShell,
    Bash,
}

#[cfg(windows)]
impl ShellDialect {
    fn as_str(self) -> &'static str {
        match self {
            Self::PowerShell => "powershell",
            Self::Bash => "bash",
        }
    }

    /// 按文件名识别方言。只认明确支持的几种,其余一律 `None`:让未知 shell
    /// 落回降级链,而不是按某个方言猜着传参。
    fn from_executable(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?.to_ascii_lowercase();
        match name.as_str() {
            "pwsh.exe" | "pwsh" | "powershell.exe" | "powershell" => Some(Self::PowerShell),
            "bash.exe" | "bash" => Some(Self::Bash),
            _ => None,
        }
    }
}

/// 一次解析的可执行文件与方言。两者必须一起缓存:只缓存路径会在路径不变、
/// 方言判断变化时让执行参数与工具描述错位。
#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedShell {
    path: PathBuf,
    dialect: ShellDialect,
}

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
    #[cfg(windows)]
    {
        let resolved = resolved_shell();
        ShellRuntime {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            dialect: resolved.dialect.as_str(),
            shell_label: shell_label(&resolved.path, resolved.dialect),
            executable: resolved.path.display().to_string(),
        }
    }
    #[cfg(not(windows))]
    {
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
        resolved_shell().path
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
    } else if runtime.os == "windows" {
        // Git Bash 的 `ps` 只列 MSYS 进程,拿它当“看进程”的示例会误导模型。
        (
            "git status; cargo test; echo $HOME",
            "PowerShell 写法(如 $env:A、Get-Process、cmd /c)",
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
            "bash 工具在 {os} 上通过 {shell} 执行;命令请写 bash 语法,不要用 PowerShell 语法。",
            os = runtime.os,
            shell = runtime.shell_label,
        )
    }
}

#[cfg(windows)]
fn shell_label(path: &Path, dialect: ShellDialect) -> String {
    match dialect {
        ShellDialect::PowerShell => match path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "pwsh.exe" => "PowerShell 7 (pwsh)".to_string(),
            "powershell.exe" => "Windows PowerShell 5.1".to_string(),
            other => other.to_string(),
        },
        ShellDialect::Bash => {
            // 同一个 bash.exe 可能是 Git for Windows,也可能是用户显式指定的
            // 其他 MSYS 发行版;标签只描述事实,不猜版本。
            if path
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains(r"\git\")
            {
                "Git Bash (bash.exe)".to_string()
            } else {
                "bash (bash.exe)".to_string()
            }
        }
    }
}

/// Builds a process that runs one shell command in the session workspace.
pub fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let resolved = resolved_shell();
        let mut child = Command::new(&resolved.path);
        match resolved.dialect {
            ShellDialect::PowerShell => {
                // PowerShell 重定向输出默认走系统 OEM 代码页(中文系统为 GBK),服务端
                // 统一按 UTF-8 解码;先注入输出编码前缀,避免中文输出整体乱码。
                child.args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    &format!("[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; {command}"),
                ]);
            }
            ShellDialect::Bash => {
                // 与 PowerShell 的 `-NoProfile` 对齐:不读用户 profile,避免其中的
                // `cd`/`alias`/提示符让每次调用的结果不可复现。Git Bash 的
                // `/usr/bin` 由运行时自带,不依赖 profile。
                child.args(["--noprofile", "--norc", "-c", command]);
            }
        }
        child
    }
    #[cfg(not(windows))]
    {
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

/// 当前进程要用的 shell(缓存)。缓存的是「路径 + 方言」整体,而不是单独路径:
/// Store 版 PowerShell 更新会整体换版本目录,`bash.exe` 也可能被用户改配置换掉。
#[cfg(windows)]
fn resolved_shell() -> ResolvedShell {
    let epoch = SHELL_EPOCH.load(Ordering::Acquire);
    let mut cached = WINDOWS_SHELL.lock().expect("shell cache poisoned");
    if let Some(resolved) = &*cached {
        // 缓存代数仍一致且文件仍在 → 直接复用;文件被删除时降级重解析。
        let still_valid = SHELL_EPOCH.load(Ordering::Acquire) == epoch && resolved.path.is_file();
        if still_valid {
            return resolved.clone();
        }
        if SHELL_EPOCH.load(Ordering::Acquire) == epoch {
            *cached = None;
        }
    }
    let resolved = resolve_windows_shell();
    *cached = Some(resolved.clone());
    resolved
}

/// 解析顺序:`DENIA_SHELL` 显式覆盖 → Windows Terminal 默认 profile(仅当它是
/// 受支持的 PowerShell 或 Git Bash)→ `pwsh` → Windows PowerShell。
///
/// Git Bash 只在用户**明确**把它设为默认 profile(或写了 `DENIA_SHELL`)时启用;
/// 否则不主动抢在 pwsh 前面,保持既有部署的行为不变。
#[cfg(windows)]
fn resolve_windows_shell() -> ResolvedShell {
    if let Some(resolved) = windows_shell_override() {
        return resolved;
    }
    if let Some(resolved) = windows_terminal_default_shell() {
        return resolved;
    }
    if let Some(path) = first_existing_pwsh_candidate() {
        return ResolvedShell {
            path,
            dialect: ShellDialect::PowerShell,
        };
    }
    ResolvedShell {
        path: windows_powershell_path(),
        dialect: ShellDialect::PowerShell,
    }
}

/// `DENIA_SHELL`:显式指定可执行文件路径或名字(`pwsh`/`powershell`/`bash`)。
///
/// 覆盖 Windows Terminal 设置读不到的场景(便携版、其他终端、CI)。无法识别
/// 方言的名字一律忽略,让自动探测继续——按未知 shell 猜方言比降级更危险。
#[cfg(windows)]
fn windows_shell_override() -> Option<ResolvedShell> {
    let raw = std::env::var_os("DENIA_SHELL")?;
    let text = raw.to_string_lossy();
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let path = PathBuf::from(text);
    if path.is_file() {
        return ShellDialect::from_executable(&path).map(|dialect| ResolvedShell { path, dialect });
    }
    resolve_shell_name(text)
}

#[cfg(windows)]
fn windows_terminal_default_shell() -> Option<ResolvedShell> {
    for path in windows_terminal_settings_paths() {
        // 单个配置文件坏掉(半截 JSON、权限问题)不能阻断后面的候选路径。
        let Some(settings) = read_json_lenient(&path) else {
            continue;
        };
        let Some(commandline) = default_profile_commandline(&settings) else {
            continue;
        };
        if let Some(resolved) = shell_from_commandline(&commandline) {
            return Some(resolved);
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
        local.join(
            "Packages/Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe/LocalState/settings.json",
        ),
        local.join("Microsoft/Windows Terminal/settings.json"),
        local.join("Microsoft/Windows Terminal Preview/settings.json"),
    ]
}

/// 宽松读取 Windows Terminal 设置:先按严格 JSON 解析,失败再按 JSONC
/// (注释 + 尾逗号)清洗一次。清洗只动字符串外的字符,`https://...` 这类值
/// 不会被当成注释截断。
#[cfg(windows)]
fn read_json_lenient(path: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    // 记事本另存为“UTF-8 with BOM”时,`serde_json` 会把 BOM 当非法字符。
    let text = text.trim_start_matches('\u{feff}');
    serde_json::from_str(text).ok().or_else(|| {
        let cleaned = strip_jsonc(text);
        serde_json::from_str(&cleaned).ok()
    })
}

#[cfg(windows)]
fn strip_jsonc(text: &str) -> String {
    let without_comments = strip_json_comments(text.as_bytes());
    let without_trailing = strip_trailing_commas(&without_comments);
    String::from_utf8(without_trailing)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

/// 删除字符串外的 `//` 行注释与 `/* */` 块注释。
#[cfg(windows)]
fn strip_json_comments(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            out.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' => {
                in_string = true;
                out.push(byte);
                index += 1;
            }
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    index += 1;
                }
                index = (index + 2).min(bytes.len());
            }
            _ => {
                out.push(byte);
                index += 1;
            }
        }
    }
    out
}

/// 删除字符串外、只被空白隔开且后面紧跟 `}`/`]` 的逗号。
#[cfg(windows)]
fn strip_trailing_commas(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            out.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' => {
                in_string = true;
                out.push(byte);
                index += 1;
            }
            b',' => {
                let mut next = index + 1;
                while next < bytes.len() && bytes[next].is_ascii_whitespace() {
                    next += 1;
                }
                if !matches!(bytes.get(next), Some(b'}') | Some(b']')) {
                    out.push(byte);
                }
                index += 1;
            }
            _ => {
                out.push(byte);
                index += 1;
            }
        }
    }
    out
}

/// 默认 profile 的 commandline。`source` 指向另一个 profile 时沿链跟随
/// (内置 PowerShell/WSL 等 profile 用 `source` 而不是 `commandline`),
/// 超过 8 层或出现环视为不可解析。
#[cfg(windows)]
fn default_profile_commandline(settings: &serde_json::Value) -> Option<String> {
    let default = settings.get("defaultProfile")?.as_str()?.trim();
    if default.is_empty() {
        return None;
    }
    let list = settings.get("profiles").and_then(|profiles| {
        profiles
            .get("list")
            .and_then(|list| list.as_array())
            .or_else(|| profiles.as_array())
    })?;
    let mut current = normalize_guid(default);
    let mut seen: Vec<String> = Vec::new();
    for _ in 0..8 {
        if seen.contains(&current) {
            return None;
        }
        seen.push(current.clone());
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
            .map(str::trim)
            .filter(|commandline| !commandline.is_empty())
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

/// 从 profile 的 commandline 解析出 shell。可执行文件必须真实存在才采用;
/// 只写了名字(`pwsh.exe`、`bash.exe`)时按已知安装位置解析。
#[cfg(windows)]
fn shell_from_commandline(commandline: &str) -> Option<ResolvedShell> {
    let expanded = expand_percent_env(commandline);
    let token = first_token(&expanded)?;
    let path = PathBuf::from(token);
    if path.is_file() {
        return ShellDialect::from_executable(&path).map(|dialect| ResolvedShell { path, dialect });
    }
    resolve_shell_name(token)
}

/// 只展开 `%NAME%`,查环境变量走 `std::env::var`:Windows 上变量名大小写
/// 不敏感,`%systemroot%` 与 `%SystemRoot%` 等价。
#[cfg(windows)]
fn expand_percent_env(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        if let Some(end) = after.find('%') {
            let name = &after[..end];
            // 变量名只由普通标识符字符组成;含路径分隔符的 `%` 是字面量。
            let plausible = !name.is_empty()
                && name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '(' | ')' | '-'));
            if plausible && let Ok(value) = std::env::var(name) {
                out.push_str(&value);
                rest = &after[end + 1..];
                continue;
            }
        }
        out.push('%');
        rest = after;
    }
    out.push_str(rest);
    out
}

#[cfg(windows)]
fn first_token(text: &str) -> Option<&str> {
    let mut text = text.trim();
    // PowerShell 的调用运算符:`& "C:\...\pwsh.exe" -NoLogo`。
    while let Some(rest) = text.strip_prefix('&') {
        text = rest.trim_start();
    }
    if text.is_empty() {
        return None;
    }
    for quote in ['"', '\''] {
        if let Some(rest) = text.strip_prefix(quote) {
            let end = rest.find(quote)?;
            return Some(&rest[..end]);
        }
    }
    text.split_whitespace().next()
}

/// 按名字解析。`bash` 只认 Git for Windows 的安装位置,不碰 PATH:
/// `C:\Windows\System32\bash.exe` 是 WSL 启动器,自动选中它会得到另一个系统里的
/// shell(甚至因为没有发行版直接失败)。
#[cfg(windows)]
fn resolve_shell_name(name: &str) -> Option<ResolvedShell> {
    match name.to_ascii_lowercase().as_str() {
        "pwsh.exe" | "pwsh" => first_existing_pwsh_candidate().map(|path| ResolvedShell {
            path,
            dialect: ShellDialect::PowerShell,
        }),
        "powershell.exe" | "powershell" => {
            let path = windows_powershell_path();
            path.is_file().then_some(ResolvedShell {
                path,
                dialect: ShellDialect::PowerShell,
            })
        }
        "bash.exe" | "bash" => first_existing_git_bash_candidate().map(|path| ResolvedShell {
            path,
            dialect: ShellDialect::Bash,
        }),
        _ => None,
    }
}

#[cfg(windows)]
fn windows_powershell_path() -> PathBuf {
    let root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    root.join("System32/WindowsPowerShell/v1.0/powershell.exe")
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
    candidates.push(windows_powershell_path());
    candidates
}

#[cfg(windows)]
fn first_existing_pwsh_candidate() -> Option<PathBuf> {
    candidate_pwsh_paths()
        .into_iter()
        .find(|candidate| candidate.is_file())
}

/// Git for Windows 的候选安装位置。Windows 上 `bash.exe` 还有 System32 的 WSL
/// 启动器同名,所以这里只列 Git 的安装根目录,绝不扫 PATH 上的 `bash.exe`。
#[cfg(windows)]
fn candidate_git_bash_paths() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for key in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Ok(base) = std::env::var(key) {
            candidates.push(PathBuf::from(base).join("Git/bin/bash.exe"));
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local).join("Programs/Git/bin/bash.exe"));
    }
    // 从 PATH 上的 `git.exe` 反推安装根:`<root>\cmd\git.exe` 与
    // `<root>\bin\git.exe` 都见过。
    if let Ok(path) = std::env::var("PATH") {
        for entry in path.split(';') {
            let dir = Path::new(entry.trim().trim_matches('"'));
            if dir.join("git.exe").is_file()
                && let Some(root) = dir.parent()
            {
                candidates.push(root.join("bin/bash.exe"));
                candidates.push(root.join("usr/bin/bash.exe"));
            }
        }
    }
    candidates
}

#[cfg(windows)]
fn first_existing_git_bash_candidate() -> Option<PathBuf> {
    candidate_git_bash_paths()
        .into_iter()
        .find(|candidate| candidate.is_file())
}

/// `kill_tree` 的测试脚手架:拉起「外层 shell → 内层 shell」的进程树,并观察
/// 孙进程的生死。前台 bash 与后台任务共用同一条清理路径,脚手架也共用。
#[cfg(all(test, windows))]
pub(crate) mod test_tree {
    use super::{first_existing_pwsh_candidate, windows_powershell_path};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};
    use tokio::process::Command;

    /// 进程探针固定用 PowerShell:它只回答“PID 还在不在”,与工具的 shell
    /// 方言无关,不能让断言随用户默认终端漂移。
    pub(crate) fn powershell_for_tests() -> PathBuf {
        first_existing_pwsh_candidate().unwrap_or_else(windows_powershell_path)
    }

    /// 一条命令拉起这棵树:外层 shell 再拉起一个内层 shell,内层把**自己的**
    /// PID 写进 `marker` 后长睡,外层同样长睡。形状与工具跑 `cargo build` 一致
    /// ——直接子进程是 shell,真正干活的是它的后代。
    pub(crate) fn nested_sleeper_command(marker: &Path) -> String {
        if super::shell_runtime().dialect == "bash" {
            // 当前 shell 是 Git Bash:内层用 PowerShell 写自己的 Windows PID。
            // `$$` 是 MSYS 内部 PID,Windows 的 taskkill/Get-Process 不认,
            // 不能拿它当进程树的锚点。
            let marker = marker.display().to_string().replace('\\', "/");
            return format!(
                "powershell.exe -NoLogo -NoProfile -NonInteractive -Command \"Set-Content -LiteralPath '{marker}' -Value \\$PID; Start-Sleep -Seconds 120\" & wait"
            );
        }
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
        let output = Command::new(powershell_for_tests())
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
    use super::{first_token, resolve_windows_shell};

    #[cfg(windows)]
    #[test]
    fn first_token_handles_quotes_and_call_operator() {
        assert_eq!(
            first_token(r#""C:\Program Files\pwsh.exe" -NoLogo"#),
            Some(r"C:\Program Files\pwsh.exe")
        );
        assert_eq!(
            first_token(r#"& "C:\Program Files\Git\bin\bash.exe" -li"#),
            Some(r"C:\Program Files\Git\bin\bash.exe")
        );
        assert_eq!(
            first_token("'C:\\Program Files\\Git\\bin\\bash.exe' -li"),
            Some(r"C:\Program Files\Git\bin\bash.exe")
        );
        assert_eq!(first_token("pwsh.exe"), Some("pwsh.exe"));
        assert_eq!(first_token("   "), None);
    }

    #[cfg(windows)]
    #[test]
    fn shell_dialect_detection() {
        use super::ShellDialect;
        use std::path::Path;
        assert_eq!(
            ShellDialect::from_executable(Path::new("pwsh.exe")),
            Some(ShellDialect::PowerShell)
        );
        assert_eq!(
            ShellDialect::from_executable(Path::new("PowerShell.exe")),
            Some(ShellDialect::PowerShell)
        );
        assert_eq!(
            ShellDialect::from_executable(Path::new(r"C:\Program Files\Git\bin\bash.exe")),
            Some(ShellDialect::Bash)
        );
        assert_eq!(ShellDialect::from_executable(Path::new("cmd.exe")), None);
        assert_eq!(ShellDialect::from_executable(Path::new("wsl.exe")), None);
    }

    /// 本机解析结果必须是一个真实存在、方言可识别的 shell。不假定一定是
    /// PowerShell:把 Git Bash 设为默认终端的机器上它应当是 bash。
    #[cfg(windows)]
    #[test]
    fn resolves_an_existing_shell_on_this_host() {
        use super::ShellDialect;
        let shell = resolve_windows_shell();
        assert!(shell.path.is_file(), "{}", shell.path.display());
        assert_eq!(
            Some(shell.dialect),
            ShellDialect::from_executable(&shell.path),
            "{}",
            shell.path.display()
        );
    }

    /// 默认 profile 的 commandline 解析:对象/数组两种 profiles 形态、`source`
    /// 链跟随、Git Bash 与 PowerShell 两种具体命令。
    #[cfg(windows)]
    #[test]
    fn default_profile_follows_source_and_accepts_legacy_list() {
        let nested = serde_json::json!({
            "defaultProfile": "{git}",
            "profiles": {
                "list": [
                    {
                        "guid": "{git}",
                        "commandline": "\"C:\\Program Files\\Git\\bin\\bash.exe\" -li"
                    }
                ]
            }
        });
        assert_eq!(
            super::default_profile_commandline(&nested),
            Some(r#""C:\Program Files\Git\bin\bash.exe" -li"#.to_string())
        );

        // 旧版 `profiles` 直接是数组;默认 profile 通过 `source` 指向别名。
        let legacy = serde_json::json!({
            "defaultProfile": "{alias}",
            "profiles": [
                { "guid": "{alias}", "source": "Windows.Terminal.PowershellCore" },
                { "guid": "Windows.Terminal.PowershellCore", "commandline": "pwsh.exe -NoLogo" }
            ]
        });
        assert_eq!(
            super::default_profile_commandline(&legacy),
            Some("pwsh.exe -NoLogo".to_string())
        );

        // 环/缺失 profile 都返回 None,不 panic。
        let cycle = serde_json::json!({
            "defaultProfile": "{a}",
            "profiles": { "list": [
                { "guid": "{a}", "source": "{b}" },
                { "guid": "{b}", "source": "{a}" }
            ] }
        });
        assert_eq!(super::default_profile_commandline(&cycle), None);
        assert_eq!(
            super::default_profile_commandline(&serde_json::json!({})),
            None
        );
    }

    /// settings.json 含 `https://` 与注释时,按字符串边界清洗后仍要能解析。
    #[cfg(windows)]
    #[test]
    fn jsonc_settings_with_urls_survive_lenient_parsing() {
        let dir = std::env::temp_dir().join(format!("denia-shell-jsonc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let text = r#"{
            // 这一行是注释
            "$help": "https://aka.ms/terminal-documentation",
            "$schema": "https://aka.ms/terminal-profiles-schema",
            "defaultProfile": "{git}",
            "profiles": {
                "list": [
                    {
                        "guid": "{git}",
                        "commandline": "\"C:\\Program Files\\Git\\bin\\bash.exe\" -li",
                    },
                ],
            },
        }"#;
        std::fs::write(&path, text).unwrap();
        let settings = super::read_json_lenient(&path).expect("JSONC 设置应能被宽松解析");
        assert_eq!(
            super::default_profile_commandline(&settings),
            Some(r#""C:\Program Files\Git\bin\bash.exe" -li"#.to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `%SystemRoot%` 的大小写不敏感展开:Windows 环境变量名本身不区分大小写,
    /// 按字面替换会把小写写法漏掉。
    #[cfg(windows)]
    #[test]
    fn percent_env_expansion_is_case_insensitive() {
        let root = std::env::var("SystemRoot").expect("Windows 必有 SystemRoot");
        let expanded = super::expand_percent_env("%systemroot%\\System32");
        assert!(
            expanded.eq_ignore_ascii_case(&format!("{root}\\System32")),
            "{expanded}"
        );
    }

    /// 从 commandline 解析出的 shell 必须与文件名方言一致;`cmd.exe`/`wsl.exe`
    /// 不在支持面内,应返回 None 让调用方继续降级。
    #[cfg(windows)]
    #[test]
    fn commandline_resolves_supported_shells_only() {
        use super::ShellDialect;
        if let Some(bash) = super::first_existing_git_bash_candidate() {
            let commandline = format!("\"{}\" -li", bash.display());
            let resolved = super::shell_from_commandline(&commandline).expect("Git Bash 应可解析");
            assert_eq!(resolved.dialect, ShellDialect::Bash);
            assert_eq!(resolved.path, bash);
            assert!(
                !resolved
                    .path
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .ends_with("system32\\bash.exe"),
                "不能退化成 System32 的 WSL 启动器"
            );
        }
        assert!(super::shell_from_commandline("cmd.exe /k").is_none());
        assert!(super::shell_from_commandline("wsl.exe").is_none());
    }

    #[cfg(windows)]
    #[test]
    fn cache_hit_and_invalidation_reparse() {
        // 首次解析并缓存(路径与方言一起缓存)。
        let first = super::resolved_shell();
        assert!(first.path.is_file());
        // 命中缓存:同一结果。
        let second = super::resolved_shell();
        assert_eq!(first, second);
        // 失效后重新解析:结果仍是一个存在的可执行文件(缓存自愈)。
        super::invalidate_windows_shell_cache();
        let third = super::resolved_shell();
        assert!(third.path.is_file());
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
    // 实时增量(sink/deltas_of 脚手架下沉到 support::test_events,与 bash 共用)
    // ---------------------------------------------------------------------

    use std::sync::Arc;
    use std::time::Instant;

    use super::{STREAM_BUDGET_BYTES, SessionEvent, Streamer};
    use crate::support::test_events::{deltas_of, sink};

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
