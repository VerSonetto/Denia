//! Platform shell resolution for the `bash` tool.
//!
//! Windows: PowerShell (`pwsh` or `powershell.exe`), preferring the Windows
//! Terminal default profile when it points at a PowerShell executable.
//! Unix: `bash -c`.

use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(windows)]
use std::sync::Mutex;

use tokio::process::Command;

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
            &format!(
                "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; {command}"
            ),
        ]);
        child
    } else {
        let mut child = Command::new("bash");
        child.args(["-c", command]);
        child
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
    match &*cached {
        Some(path) => {
            // 缓存代数仍一致且文件仍在 → 直接复用;文件被删除时降级重解析。
            let still_valid =
                SHELL_EPOCH.load(Ordering::Acquire) == epoch && path.is_file();
            if still_valid {
                return path.clone();
            }
            if SHELL_EPOCH.load(Ordering::Acquire) == epoch {
                *cached = None;
            }
        }
        None => {}
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
        if let Some(commandline) = default_profile_commandline(&settings) {
            if let Some(shell) = powershell_from_commandline(&commandline) {
                return Some(shell);
            }
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
}
