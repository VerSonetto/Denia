//! Native OS folder chooser.
//!
//! On Windows, a long-running denia process cannot steal the foreground from
//! the browser (Win32 foreground lock). Showing `IFileOpenDialog` in-process
//! therefore lands the picker on the taskbar instead of in front of the user.
//!
//! The fix matches dsh: spawn a short-lived copy of this binary (`--pick-folder`)
//! so the dialog is that process's first window, then force it above other
//! windows. Other platforms keep the in-process rfd call.

use std::process::Stdio;

use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::json;

use crate::error::ApiError;

/// User-visible dialog title.
pub const DIALOG_TITLE: &str = "denia — 选择工作区目录";

/// 文件引用对话框标题（与目录选择器区分，避免用户以为在换工作区）。
pub const FILES_DIALOG_TITLE: &str = "denia — 选择要引用的文件";

/// Exclusive CLI mode: show the native folder picker, print one JSON line, exit.
/// Must run before tracing is installed so stdout stays a single JSON value.
pub fn run_as_cli() -> ! {
    run_picker_as_cli(PickerKind::Folder)
}

pub fn run_files_as_cli() -> ! {
    run_picker_as_cli(PickerKind::Files)
}

fn run_picker_as_cli(kind: PickerKind) -> ! {
    #[cfg(windows)]
    win_foreground::start_booster();

    let paths = kind.pick();
    let output = match kind {
        PickerKind::Folder => json!({ "path": paths.first() }),
        PickerKind::Files => json!({ "paths": paths }),
    };
    match serde_json::to_string(&output) {
        Ok(line) => {
            println!("{line}");
            std::process::exit(0);
        }
        Err(_) => std::process::exit(1),
    }
}

/// Open the host folder picker. `None` means the user cancelled.
pub async fn pick_folder() -> Result<Option<String>, ApiError> {
    Ok(pick_paths(PickerKind::Folder).await?.into_iter().next())
}

/// 返回宿主机的绝对文件路径；取消时是空列表。
///
/// 只回路径不回内容：文件留在原处，模型用 read_file 按需读，
/// 不再复制一份到 uploads 目录。
pub async fn pick_files() -> Result<Vec<String>, ApiError> {
    pick_paths(PickerKind::Files).await
}

async fn pick_paths(kind: PickerKind) -> Result<Vec<String>, ApiError> {
    #[cfg(windows)]
    {
        pick_via_child(kind).await
    }
    #[cfg(not(windows))]
    {
        tokio::task::spawn_blocking(move || kind.pick())
            .await
            .map_err(|error| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "fs/pick-failed",
                    error.to_string(),
                )
            })
    }
}

#[derive(Clone, Copy)]
enum PickerKind {
    Folder,
    Files,
}

impl PickerKind {
    fn flag(self) -> &'static str {
        match self {
            Self::Folder => "--pick-folder",
            Self::Files => "--pick-files",
        }
    }

    fn pick(self) -> Vec<String> {
        let paths = match self {
            Self::Folder => rfd::FileDialog::new()
                .set_title(DIALOG_TITLE)
                .pick_folder()
                .into_iter()
                .collect(),
            Self::Files => rfd::FileDialog::new()
                .set_title(FILES_DIALOG_TITLE)
                .pick_files()
                .unwrap_or_default(),
        };
        paths
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect()
    }
}

#[cfg(windows)]
async fn pick_via_child(kind: PickerKind) -> Result<Vec<String>, ApiError> {
    let exe = std::env::current_exe().map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "fs/pick-failed",
            format!("cannot locate denia executable: {error}"),
        )
    })?;
    let mut command = picker_command(&exe, kind);
    let child = command.spawn().map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "fs/pick-failed",
            format!("failed to start native picker: {error}"),
        )
    })?;
    if let Some(pid) = child.id() {
        win_foreground::allow_set_foreground(pid);
    }
    let output = child.wait_with_output().await.map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "fs/pick-failed",
            format!("native picker exited unexpectedly: {error}"),
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "fs/pick-failed",
            format!(
                "native picker failed (status {}): {}",
                output.status,
                stderr.trim()
            ),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed = match kind {
        PickerKind::Folder => parse_pick_folder_output(&stdout).map(|p| p.into_iter().collect()),
        PickerKind::Files => parse_pick_files_output(&stdout),
    };
    parsed.map_err(|message| {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "fs/pick-failed", message)
    })
}

fn picker_command(exe: &std::path::Path, kind: PickerKind) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(exe);
    command
        .arg(kind.flag())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        // Hide the child's console so the folder dialog is its first window.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

#[derive(Debug, Deserialize)]
struct PickFolderOutput {
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PickFilesOutput {
    paths: Vec<String>,
}

/// Child protocol: one JSON object `{ "path": string | null }` on stdout.
fn parse_pick_folder_output(stdout: &str) -> Result<Option<String>, String> {
    let parsed: PickFolderOutput = parse_picker_output(stdout)?;
    Ok(parsed.path)
}

/// 文件选择器的子进程协议：`{ "paths": [绝对路径…] }`，取消时为空数组。
fn parse_pick_files_output(stdout: &str) -> Result<Vec<String>, String> {
    let parsed: PickFilesOutput = parse_picker_output(stdout)?;
    Ok(parsed.paths)
}

/// 两种选择器共用的解析：取最后一行非空 JSON。
/// 选最后一行是因为 rfd/系统库偶尔先往 stdout 写警告。
fn parse_picker_output<T: serde::de::DeserializeOwned>(stdout: &str) -> Result<T, String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err("native picker returned empty output".to_string());
    }
    let line = trimmed
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap();
    serde_json::from_str(line.trim())
        .map_err(|error| format!("native picker returned unreadable output: {error}"))
}

/// Shared by `main` so the exclusive CLI mode is decided before tracing.
pub fn is_pick_folder_mode(mut args: impl Iterator<Item = String>) -> bool {
    args.any(|arg| arg == PickerKind::Folder.flag())
}

pub fn is_pick_files_mode(mut args: impl Iterator<Item = String>) -> bool {
    args.any(|arg| arg == PickerKind::Files.flag())
}

#[cfg(windows)]
mod win_foreground {
    use std::thread;
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{BOOL, FALSE, HWND, LPARAM, TRUE};
    use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentProcessId};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AllowSetForegroundWindow, BringWindowToTop, EnumWindows, GetForegroundWindow,
        GetWindowThreadProcessId, HWND_TOPMOST, IsWindowVisible, SW_RESTORE, SWP_NOMOVE,
        SWP_NOSIZE, SWP_SHOWWINDOW, SetForegroundWindow, SetWindowPos, ShowWindow,
    };

    pub fn allow_set_foreground(pid: u32) {
        unsafe {
            let _ = AllowSetForegroundWindow(pid);
        }
    }

    /// Poll for this process's first top-level window and pin it above others.
    /// `IFileOpenDialog::Show` is modal on the main thread, so this must run
    /// on a helper thread started before `Show`.
    pub fn start_booster() {
        thread::spawn(|| {
            for _ in 0..40 {
                thread::sleep(Duration::from_millis(50));
                if let Some(hwnd) = find_our_toplevel() {
                    raise(hwnd);
                }
            }
        });
    }

    fn find_our_toplevel() -> Option<HWND> {
        let mut found: Option<HWND> = None;
        unsafe {
            let _ = EnumWindows(Some(enum_proc), std::ptr::from_mut(&mut found) as LPARAM);
        }
        found
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let found = unsafe { &mut *(lparam as *mut Option<HWND>) };
        let mut pid = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid != GetCurrentProcessId() || IsWindowVisible(hwnd) == 0 {
                return TRUE;
            }
        }
        *found = Some(hwnd);
        FALSE
    }

    fn raise(hwnd: HWND) {
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            // Stay topmost for the life of the modal dialog so a failed
            // SetForegroundWindow still leaves the picker visible.
            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
            );
            let _ = BringWindowToTop(hwnd);

            let dialog_tid = GetWindowThreadProcessId(hwnd, std::ptr::null_mut());
            let foreground = GetForegroundWindow();
            let foreground_tid = GetWindowThreadProcessId(foreground, std::ptr::null_mut());
            if foreground_tid != 0 && foreground_tid != dialog_tid {
                let _ = AttachThreadInput(dialog_tid, foreground_tid, TRUE);
                let _ = SetForegroundWindow(hwnd);
                let _ = AttachThreadInput(dialog_tid, foreground_tid, FALSE);
            } else {
                let _ = SetForegroundWindow(hwnd);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cancel() {
        assert_eq!(parse_pick_folder_output(r#"{"path":null}"#).unwrap(), None);
    }

    #[test]
    fn parse_path() {
        assert_eq!(
            parse_pick_folder_output(r#"{"path":"D:\\code"}"#)
                .unwrap()
                .as_deref(),
            Some(r"D:\code")
        );
    }

    #[test]
    fn parse_uses_last_json_line() {
        let stdout = "warn: something\n{\"path\":\"C:\\\\ws\"}\n";
        assert_eq!(
            parse_pick_folder_output(stdout).unwrap().as_deref(),
            Some(r"C:\ws")
        );
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(parse_pick_folder_output("  \n").is_err());
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_pick_folder_output("not json").is_err());
    }

    #[test]
    fn picker_command_uses_exclusive_flag() {
        let command = picker_command(std::path::Path::new("denia"), PickerKind::Folder);
        let std_cmd = command.as_std();
        let args: Vec<&std::ffi::OsStr> = std_cmd.get_args().collect();
        assert_eq!(args, ["--pick-folder"]);
    }

    #[test]
    fn files_picker_uses_its_own_flag() {
        let command = picker_command(std::path::Path::new("denia"), PickerKind::Files);
        let args: Vec<&std::ffi::OsStr> = command.as_std().get_args().collect();
        assert_eq!(args, ["--pick-files"]);
        assert!(is_pick_files_mode(
            ["--port", "3601", "--pick-files"]
                .into_iter()
                .map(str::to_string)
        ));
        // 两个模式不能互相误判：主进程靠它决定走哪个对话框。
        assert!(!is_pick_folder_mode(
            ["--pick-files"].into_iter().map(str::to_string)
        ));
        assert!(!is_pick_files_mode(
            ["--pick-folder"].into_iter().map(str::to_string)
        ));
    }

    #[test]
    fn parse_files_keeps_every_path_in_order() {
        let stdout = "warn\n{\"paths\":[\"D:\\\\a b\\\\x.pdf\",\"C:\\\\ws\\\\y.txt\"]}\n";
        assert_eq!(
            parse_pick_files_output(stdout).unwrap(),
            [r"D:\a b\x.pdf", r"C:\ws\y.txt"]
        );
    }

    #[test]
    fn parse_files_cancel_is_empty_not_error() {
        assert!(parse_pick_files_output(r#"{"paths":[]}"#).unwrap().is_empty());
    }

    #[test]
    fn parse_files_rejects_other_shape() {
        // 目录选择器的输出不能被当成文件列表（字段名不同）。
        assert!(parse_pick_files_output(r#"{"path":null}"#).is_err());
        assert!(parse_pick_files_output("not json").is_err());
    }

    #[test]
    fn is_pick_folder_mode_detects_flag() {
        assert!(is_pick_folder_mode(
            ["--port", "3601", "--pick-folder"]
                .into_iter()
                .map(str::to_string)
        ));
        assert!(!is_pick_folder_mode(
            ["--port", "3601"].into_iter().map(str::to_string)
        ));
    }
}
