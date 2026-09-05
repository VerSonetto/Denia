//! Platform shell resolution for the `bash` tool.
//!
//! Windows: PowerShell (`pwsh` or `powershell.exe`), preferring the Windows
//! Terminal default profile when it points at a PowerShell executable.
//! Unix: `bash -c`.

use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::sync::OnceLock;

use tokio::process::Command;

#[cfg(windows)]
static WINDOWS_SHELL: OnceLock<PathBuf> = OnceLock::new();

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

/// Model-facing `bash` tool description with host and shell variables filled in.
pub fn bash_tool_description(runtime: &ShellRuntime) -> String {
    let (examples, avoid) = if runtime.dialect == "powershell" {
        (
            "Get-ChildItem; Get-Content .\\src\\lib.rs; $env:USERPROFILE",
            "bash/sh/cmd syntax (ls, cat, export, cmd /c)",
        )
    } else {
        (
            "ls; cat src/lib.rs; echo $HOME",
            "PowerShell syntax (Get-ChildItem, $env:VAR, cmd /c)",
        )
    };
    format!(
        "Run one shell command in the session workspace; returns exit code, stdout, and stderr.\n\
         Host: {os} ({arch}). Shell: {shell} ({executable}).\n\
         Write `command` in {dialect} syntax only — not {avoid}.\n\
         Examples on this host: {examples}.",
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
pub fn bash_command_param_description(runtime: &ShellRuntime) -> String {
    let example = if runtime.dialect == "powershell" {
        "Get-ChildItem"
    } else {
        "ls -la"
    };
    format!(
        "One {dialect} command line for {shell} on {os} ({arch}). Example: {example}.",
        dialect = runtime.dialect,
        shell = runtime.shell_label,
        os = runtime.os,
        arch = runtime.arch,
        example = example,
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

#[cfg(windows)]
fn windows_shell_executable() -> &'static Path {
    WINDOWS_SHELL.get_or_init(resolve_windows_shell)
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

    #[test]
    fn bash_tool_description_includes_host_and_shell() {
        let runtime = super::shell_runtime();
        let description = super::bash_tool_description(&runtime);
        assert!(description.contains(runtime.os));
        assert!(description.contains(runtime.arch));
        assert!(description.contains(&runtime.shell_label));
        assert!(description.contains(runtime.dialect));
    }
}
