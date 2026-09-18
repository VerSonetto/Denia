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

/// Exclusive CLI mode: show the native folder picker, print one JSON line, exit.
/// Must run before tracing is installed so stdout stays a single JSON value.
pub fn run_as_cli() -> ! {
    #[cfg(windows)]
    win_foreground::start_booster();

    let picked = rfd::FileDialog::new().set_title(DIALOG_TITLE).pick_folder();
    let path = picked.map(|p| p.to_string_lossy().to_string());
    match serde_json::to_string(&json!({ "path": path })) {
        Ok(line) => {
            println!("{line}");
            std::process::exit(0);
        }
        Err(_) => std::process::exit(1),
    }
}

/// Open the host folder picker. `None` means the user cancelled.
pub async fn pick_folder() -> Result<Option<String>, ApiError> {
    #[cfg(windows)]
    {
        pick_folder_via_child().await
    }
    #[cfg(not(windows))]
    {
        pick_folder_in_process().await
    }
}

#[cfg(not(windows))]
async fn pick_folder_in_process() -> Result<Option<String>, ApiError> {
    let picked = tokio::task::spawn_blocking(|| {
        rfd::FileDialog::new().set_title(DIALOG_TITLE).pick_folder()
    })
    .await
    .map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "fs/pick-failed",
            error.to_string(),
        )
    })?;
    Ok(picked.map(|path| path.to_string_lossy().to_string()))
}

#[cfg(windows)]
async fn pick_folder_via_child() -> Result<Option<String>, ApiError> {
    let exe = std::env::current_exe().map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "fs/pick-failed",
            format!("cannot locate denia executable: {error}"),
        )
    })?;
    let mut command = picker_command(&exe);
    let child = command.spawn().map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "fs/pick-failed",
            format!("failed to start folder picker: {error}"),
        )
    })?;
    if let Some(pid) = child.id() {
        win_foreground::allow_set_foreground(pid);
    }
    let output = child.wait_with_output().await.map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "fs/pick-failed",
            format!("folder picker exited unexpectedly: {error}"),
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "fs/pick-failed",
            format!(
                "folder picker failed (status {}): {}",
                output.status,
                stderr.trim()
            ),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_pick_folder_output(&stdout).map_err(|message| {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "fs/pick-failed", message)
    })
}

fn picker_command(exe: &std::path::Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(exe);
    command
        .arg("--pick-folder")
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

/// Child protocol: one JSON object `{ "path": string | null }` on stdout.
fn parse_pick_folder_output(stdout: &str) -> Result<Option<String>, String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err("folder picker returned empty output".to_string());
    }
    // Take the last non-empty line in case a library wrote a warning first.
    let line = trimmed
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap();
    let parsed: PickFolderOutput = serde_json::from_str(line.trim())
        .map_err(|error| format!("folder picker returned unreadable output: {error}"))?;
    Ok(parsed.path)
}

/// Shared by `main` so the exclusive CLI mode is decided before tracing.
pub fn is_pick_folder_mode(mut args: impl Iterator<Item = String>) -> bool {
    args.any(|arg| arg == "--pick-folder")
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
        let command = picker_command(std::path::Path::new("denia"));
        let std_cmd = command.as_std();
        let args: Vec<&std::ffi::OsStr> = std_cmd.get_args().collect();
        assert_eq!(args, ["--pick-folder"]);
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
