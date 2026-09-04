//! Chrome/Edge 启动器:定位可执行文件 → 带调试端口拉起 → 读 DevToolsActivePort。
//!
//! 与 ZCode 的 `resolveChromeExecutablePath`/`runChromeHelper` 同思路:
//! 每个浏览器实例使用独立的持久 user-data-dir(等价 persist 分区),
//! `--remote-debugging-port=0` 让系统挑空闲端口,端口写在
//! `<user-data-dir>/DevToolsActivePort` 文件里。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// 定位可用的浏览器可执行文件(Chrome 优先,Edge 兜底)。
pub fn locate_browser_executable() -> Option<PathBuf> {
    for name in ["chrome.exe", "msedge.exe"] {
        if let Some(path) = locate_windows_browser(name) {
            return Some(path);
        }
    }
    None
}

fn locate_windows_browser(executable: &str) -> Option<PathBuf> {
    // 1) App Paths 注册表(Chrome/Edge 都注册)
    let key = format!(
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\{executable}"
    );
    if let Some(path) = read_registry_default_string(&key) {
        if Path::new(&path).is_file() {
            return Some(PathBuf::from(path));
        }
    }
    // 2) 常见安装位置
    let candidates = [
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    ];
    candidates
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

fn read_registry_default_string(key: &str) -> Option<String> {
    // 免依赖读注册表:reg query 一行解析。失败一律 None(兜底路径还在)。
    let mut command = std::process::Command::new("reg");
    command.args(["query", key, "/ve"]);
    set_no_window_std(&mut command);
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        // "    (默认)    REG_SZ    C:\...\chrome.exe"
        if let Some(index) = line.find("REG_SZ") {
            let value = line[index + "REG_SZ".len()..].trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

// reg query 隐藏窗口用
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 一个运行中的浏览器实例。
pub struct LaunchedBrowser {
    /// 浏览器端点:`ws://127.0.0.1:<port>/devtools/browser/<id>`
    pub websocket_url: String,
    pub process: tokio::process::Child,
}

/// 拉起 headless Chrome/Edge,返回浏览器级 WebSocket 端点。
///
/// `user_data_dir` 必须持久:登录态/Cookie/存储都在里面,等价
/// ZCode 内嵌浏览器的 `persist:zcode-embedded-browser` 分区。
pub async fn launch_headless(
    executable: &Path,
    user_data_dir: &Path,
) -> Result<LaunchedBrowser, String> {
    std::fs::create_dir_all(user_data_dir)
        .map_err(|error| format!("创建浏览器 profile 目录失败: {error}"))?;
    // 孤儿清理:server 进程被硬杀时 kill_on_drop 不触发,旧 Chrome 仍占用
    // profile(新实例会单例转发后立即退出)。启动前按 profile 路径清场。
    kill_orphan_browsers(user_data_dir).await;

    let mut command = Command::new(executable);
    command
        .arg("--remote-debugging-port=0")
        // 有头但移出屏幕:headless 会自动 dismiss JS dialog,
        // 有头才能触发 Page.javascriptDialogOpening(与 ZCode 拦截 dialog 的前提一致)。
        .arg("--window-position=-32000,-32000")
        .arg(format!("--user-data-dir={}", user_data_dir.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-background-networking")
        .arg("--disable-component-update")
        .arg("--disable-sync")
        .arg("--disable-extensions")
        .arg("--disable-breakpad")
        .arg("--metrics-recording-only")
        .arg("--password-store=basic")
        .arg("--window-size=1280,800")
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    set_no_window(&mut command);
    let mut process = command
        .spawn()
        .map_err(|error| format!("启动浏览器失败({}): {error}", executable.display()))?;

    // 读 DevToolsActivePort(Chrome 启动后写入;最多等 15s)。
    // 只认本次启动后新写的 port file:旧实例残留的文件不含新端点。
    let port_file = user_data_dir.join("DevToolsActivePort");
    let previous_mtime = std::fs::metadata(&port_file).ok().and_then(|meta| meta.modified().ok());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if tokio::time::Instant::now() >= deadline {
            let _ = process.kill().await;
            return Err("等待浏览器调试端口超时(DevToolsActivePort 未出现)".to_string());
        }
        let metadata_fresh = std::fs::metadata(&port_file)
            .ok()
            .and_then(|meta| meta.modified().ok())
            .map(|modified| match previous_mtime {
                Some(previous) => modified > previous,
                None => true,
            })
            .unwrap_or(false);
        if metadata_fresh {
            if let Ok(content) = std::fs::read_to_string(&port_file) {
                let mut lines = content.lines();
                if let Some(port) = lines.next().map(str::trim).filter(|p| !p.is_empty()) {
                    let path = lines.next().unwrap_or("").trim();
                    let path = if path.is_empty() {
                        "/devtools/browser".to_string()
                    } else {
                        path.to_string()
                    };
                    return Ok(LaunchedBrowser {
                        websocket_url: format!("ws://127.0.0.1:{port}{path}"),
                        process,
                    });
                }
            }
        }
        // 进程提前退出 = 启动失败
        if let Ok(Some(status)) = process.try_wait() {
            return Err(format!("浏览器启动即退出: {status}"));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 幂等写一个文本文件(覆盖)。
pub async fn write_text(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut file = tokio::fs::File::create(path).await.map_err(|e| e.to_string())?;
    file.write_all(text.as_bytes()).await.map_err(|e| e.to_string())?;
    Ok(())
}

/// CREATE_NO_WINDOW 的统一入口:tokio Command 与 std Command 都走系统 CommandExt。
pub fn set_no_window(command: &mut Command) {
    command.creation_flags(CREATE_NO_WINDOW);
}

pub fn set_no_window_std(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(CREATE_NO_WINDOW);
}

/// 清掉占用同一 profile 的残留 Chrome/Edge 进程(server 硬杀后的孤儿)。
/// 只匹配命令行里带本 profile 目录的进程,不影响用户自己的浏览器。
async fn kill_orphan_browsers(user_data_dir: &Path) {
    let profile = user_data_dir.to_string_lossy().to_string();
    let script = format!(
        "Get-CimInstance Win32_Process -Filter \"Name='chrome.exe' or Name='msedge.exe'\" \
         | Where-Object {{ $_.CommandLine -like '*{profile}*' }} \
         | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}",
        profile = profile.replace('\\', "\\\\")
    );
    let mut command = tokio::process::Command::new("powershell.exe");
    command
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    let _ = command.status().await;
}
