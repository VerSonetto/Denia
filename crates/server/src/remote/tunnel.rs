//! cloudflared 快速隧道子进程管理。
//!
//! 生命周期与主服务绑定:进程随 `start` 起、随 `stop`/进程退出死。这里不做
//! "后台守护"式的自愈重启 —— 隧道意外断开时正确的反应是让用户知道并重新
//! 发起,而不是在用户不知情的情况下重新开一个公网入口。
//!
//! 快速隧道不落盘任何凭证(不写 `~/.cloudflared`),所以"清理临时凭证"在
//! 这里只剩一件事:确认子进程真的死了,并清掉 pid 文件。

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

/// 从 cloudflared 输出里等 URL 的上限。实测本机约 5 秒;给到 30 秒是留
/// 网络慢的余量,再长就该让用户看到失败而不是一直转圈。
const URL_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// 隧道对外信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelInfo {
    /// `https://xxx.trycloudflare.com`(不带结尾斜杠)。
    pub url: String,
    /// 隧道主机名,Host 校验白名单要用。
    pub host: String,
    pub pid: u32,
    pub started_at: u64,
}

struct Running {
    child: Child,
    info: TunnelInfo,
}

/// 隧道进程管理器。
#[derive(Default)]
pub struct TunnelManager {
    running: Mutex<Option<Running>>,
}

impl TunnelManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前隧道信息(未运行时 None)。
    pub async fn info(&self) -> Option<TunnelInfo> {
        self.running
            .lock()
            .await
            .as_ref()
            .map(|running| running.info.clone())
    }

    /// 起一条快速隧道,回源到 `origin`(形如 `http://127.0.0.1:3602`)。
    ///
    /// 失败路径必须干净:起不来、等不到 URL、超时,都要把已 spawn 的子进程
    /// 杀掉再返回错误,不留孤儿。
    pub async fn start(
        &self,
        cloudflared: &str,
        origin: &str,
        pid_file: &Path,
    ) -> Result<TunnelInfo, String> {
        let mut guard = self.running.lock().await;
        if guard.is_some() {
            return Err("隧道已在运行,请先关闭再重开".to_string());
        }

        let mut command = Command::new(cloudflared);
        command
            .arg("tunnel")
            .arg("--url")
            .arg(origin)
            // 关掉自动更新:它会在运行中途重启进程,隧道 URL 随之失效。
            .arg("--no-autoupdate")
            .arg("--loglevel")
            .arg("info")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        scrub_environment(&mut command);

        let mut child = command.spawn().map_err(|error| {
            format!("无法启动 cloudflared({cloudflared}):{error};请确认已安装并位于 PATH 中")
        })?;
        let pid = child.id().unwrap_or(0);
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "cloudflared 未提供 stderr,无法解析隧道地址".to_string())?;

        let url = match tokio::time::timeout(URL_WAIT_TIMEOUT, read_tunnel_url(stderr)).await {
            Ok(Ok(url)) => url,
            Ok(Err(message)) => {
                kill_child(&mut child).await;
                return Err(message);
            }
            Err(_) => {
                kill_child(&mut child).await;
                return Err(format!(
                    "等待 cloudflared 分配隧道地址超时({}秒)",
                    URL_WAIT_TIMEOUT.as_secs()
                ));
            }
        };

        let host = url
            .strip_prefix("https://")
            .unwrap_or(&url)
            .trim_end_matches('/')
            .to_string();
        let info = TunnelInfo {
            url: url.trim_end_matches('/').to_string(),
            host,
            pid,
            started_at: super::audit::now_millis(),
        };

        if let Some(parent) = pid_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(error) = std::fs::write(pid_file, pid.to_string()) {
            tracing::warn!(%error, path = %pid_file.display(), "could not write tunnel pid file");
        }

        *guard = Some(Running {
            child,
            info: info.clone(),
        });
        Ok(info)
    }

    /// 关闭隧道:杀进程并等它真的退出。返回被杀掉的隧道信息。
    pub async fn stop(&self, pid_file: &Path) -> Option<TunnelInfo> {
        let mut guard = self.running.lock().await;
        let mut running = guard.take()?;
        kill_child(&mut running.child).await;
        let _ = std::fs::remove_file(pid_file);
        Some(running.info)
    }

    /// 服务退出时的兜底:无论状态如何都尝试收干净。
    pub async fn shutdown(&self, pid_file: &Path) {
        let _ = self.stop(pid_file).await;
        let _ = std::fs::remove_file(pid_file);
    }
}

/// 逐行读 stderr,抓出快速隧道地址。
///
/// 实测输出形如(带框):
/// ```text
/// INF |  Your quick Tunnel has been created! Visit it at (it may take some time to be reachable):  |
/// INF |  https://plot-knit-normally-strict.trycloudflare.com                                       |
/// ```
/// 所以按"行内出现 https://<label>.trycloudflare.com"匹配,不依赖框线。
async fn read_tunnel_url(stderr: tokio::process::ChildStderr) -> Result<String, String> {
    let mut lines = BufReader::new(stderr).lines();
    let mut tail: Vec<String> = Vec::new();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if let Some(url) = extract_tunnel_url(&line) {
                    return Ok(url);
                }
                // 只留最近几行用于报错:cloudflared 启动日志很长,全留没意义。
                if tail.len() == 8 {
                    tail.remove(0);
                }
                tail.push(line.trim().to_string());
            }
            Ok(None) => {
                return Err(format!(
                    "cloudflared 在给出隧道地址前就退出了:{}",
                    tail.join(" | ")
                ));
            }
            Err(error) => return Err(format!("读取 cloudflared 输出失败:{error}")),
        }
    }
}

/// 从一行日志里提取快速隧道 URL。只认 trycloudflare.com,避免把日志里
/// 其它 URL(文档链接等)误当隧道地址。
fn extract_tunnel_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let rest = &line[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '|')
        .unwrap_or(rest.len());
    let candidate = &rest[..end];
    let host = candidate.strip_prefix("https://")?;
    let label = host.split('.').next().unwrap_or_default();
    if label.is_empty() || !host.ends_with(".trycloudflare.com") {
        return None;
    }
    if !label
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return None;
    }
    Some(candidate.to_string())
}

/// 杀掉子进程并回收。Windows 上 `kill` 只终止直接子进程,cloudflared 没有
/// 子进程,够用。
async fn kill_child(child: &mut Child) {
    if let Err(error) = child.kill().await {
        tracing::warn!(%error, "could not kill cloudflared");
    }
    // 必须 wait:否则退出后进程表里会留一个僵尸条目。
    let _ = child.wait().await;
}

/// 清掉上次运行残留的隧道进程。
///
/// 只有"确实是我们留下的"才动手:pid 文件里记的进程必须先被确认是
/// cloudflared,否则宁可留着那个 pid 不碰(杀错进程的代价远高于一个
/// 无人连接的隧道进程)。
pub fn cleanup_stale(pid_file: &Path) {
    let Ok(raw) = std::fs::read_to_string(pid_file) else {
        return;
    };
    let Ok(pid) = raw.trim().parse::<u32>() else {
        let _ = std::fs::remove_file(pid_file);
        return;
    };
    match process_is_cloudflared(pid) {
        Some(true) => {
            tracing::warn!(pid, "cleaning up cloudflared left over from a previous run");
            kill_pid(pid);
        }
        Some(false) => {
            tracing::warn!(pid, "stale tunnel pid no longer belongs to cloudflared; leaving it alone");
        }
        None => {}
    }
    let _ = std::fs::remove_file(pid_file);
}

#[cfg(windows)]
fn process_is_cloudflared(pid: u32) -> Option<bool> {
    let output = std::process::Command::new("tasklist.exe")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .creation_flags_no_window()
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    if !text.contains("cloudflared") {
        return Some(false);
    }
    Some(true)
}

#[cfg(not(windows))]
fn process_is_cloudflared(pid: u32) -> Option<bool> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(comm.to_ascii_lowercase().contains("cloudflared"))
}

#[cfg(windows)]
fn kill_pid(pid: u32) {
    let _ = std::process::Command::new("taskkill.exe")
        .args(["/PID", &pid.to_string(), "/F"])
        .creation_flags_no_window()
        .output();
}

#[cfg(not(windows))]
fn kill_pid(pid: u32) {
    let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).output();
}

/// 子进程不该继承 denia 的进程环境:凭据、代理设置都不需要传给隧道进程。
fn scrub_environment(command: &mut Command) {
    for key in [
        "DENIA_HOME",
        "DSH_RS_HOME",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
    ] {
        command.env_remove(key);
    }
}

/// `std::os::windows::process::CommandExt` 的小包装:隐藏控制台窗口。
#[cfg(windows)]
trait NoWindow {
    fn creation_flags_no_window(&mut self) -> &mut Self;
}

#[cfg(windows)]
impl NoWindow for std::process::Command {
    fn creation_flags_no_window(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        self.creation_flags(0x0800_0000) // CREATE_NO_WINDOW
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_url_from_real_cloudflared_output() {
        let line = "2026-09-15T03:10:57Z INF |  https://plot-knit-normally-strict.trycloudflare.com                                       |";
        assert_eq!(
            extract_tunnel_url(line).as_deref(),
            Some("https://plot-knit-normally-strict.trycloudflare.com")
        );
    }

    #[test]
    fn ignores_unrelated_urls() {
        assert_eq!(extract_tunnel_url("see https://developers.cloudflare.com/tunnel"), None);
        assert_eq!(extract_tunnel_url("INF no url here"), None);
        assert_eq!(extract_tunnel_url("https://example.com"), None);
        // 域名标签里出现非法字符不认。
        assert_eq!(extract_tunnel_url("https://bad_label.trycloudflare.com"), None);
    }

    #[test]
    fn strips_trailing_punctuation() {
        let line = "INF |  https://abc-def.trycloudflare.com  |";
        assert_eq!(
            extract_tunnel_url(line).as_deref(),
            Some("https://abc-def.trycloudflare.com")
        );
    }

    #[test]
    fn cleanup_removes_pid_file_even_when_process_is_gone() {
        let dir = std::env::temp_dir().join(format!("denia-tunnel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("tunnel.pid");
        // 用一个几乎不可能存在的 pid:清理必须只是删文件,不误杀。
        std::fs::write(&pid_file, "4294967290").unwrap();
        cleanup_stale(&pid_file);
        assert!(!pid_file.exists(), "残留 pid 文件必须被清掉");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_tolerates_garbage_pid_file() {
        let dir = std::env::temp_dir().join(format!("denia-tunnel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("tunnel.pid");
        std::fs::write(&pid_file, "not-a-pid").unwrap();
        cleanup_stale(&pid_file);
        assert!(!pid_file.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
