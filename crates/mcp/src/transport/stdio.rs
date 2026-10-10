//! stdio 传输:一个子进程 = 一条连接,stdin/stdout 承载协议帧。

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use crate::client::McpClientError;
use crate::protocol::JsonRpcRequest;
use crate::transport::McpTransport;

/// 一个 stdio MCP 连接。
pub struct StdioTransport {
    name: String,
    child: Mutex<Option<Child>>,
    stdin: Mutex<ChildStdin>,
    stdout: Mutex<BufReader<ChildStdout>>,
    next_id: Mutex<u64>,
}

impl StdioTransport {
    pub async fn connect(
        name: impl Into<String>,
        command: &str,
        args: &[String],
        envs: &HashMap<String, String>,
        cwd: Option<&std::path::Path>,
    ) -> Result<Self, McpClientError> {
        let name = name.into();
        let (program, console_shim) = resolve_program(command);
        let mut command_builder = Command::new(&program);
        if console_shim {
            // 由 cmd.exe 承载的 .cmd/.bat shim:GUI 宿主下直接起会弹控制台
            // 窗口,与协议通道无关,压掉。
            #[cfg(windows)]
            {
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                command_builder.creation_flags(CREATE_NO_WINDOW);
            }
        }
        command_builder
            .args(args)
            .envs(envs)
            // stdio 是协议通道;stderr 只承载服务器日志,丢弃以免污染帧。
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // 子进程自己一个进程组:kill 时不会波及 denia 本身。
            .kill_on_drop(true);
        if let Some(cwd) = cwd {
            command_builder.current_dir(cwd);
        }
        let mut child = command_builder
            .spawn()
            .map_err(|error| McpClientError::Spawn(format!("{command}: {error}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpClientError::Spawn("stdin 管道不可用".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpClientError::Spawn("stdout 管道不可用".to_string()))?;
        Ok(Self {
            name,
            child: Mutex::new(Some(child)),
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(BufReader::new(stdout)),
            next_id: Mutex::new(1),
        })
    }
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, McpClientError> {
        let id = {
            let mut next = self.next_id.lock().await;
            let id = *next;
            *next += 1;
            id
        };
        let frame = serde_json::to_string(&JsonRpcRequest::new(id, method, params))
            .map_err(|error| McpClientError::Disconnected(error.to_string()))?;
        {
            let mut stdin = self.stdin.lock().await;
            write_frame(&mut stdin, &frame).await?;
        }
        let raw = tokio::time::timeout(timeout, self.read_response(id))
            .await
            .map_err(|_| McpClientError::Timeout {
                method: method.to_string(),
            })??;
        crate::client::decode_response(&raw)
    }

    async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpClientError> {
        let frame = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .map_err(|error| McpClientError::Disconnected(error.to_string()))?;
        let mut stdin = self.stdin.lock().await;
        write_frame(&mut stdin, &frame).await
    }

    async fn shutdown(&self) {
        let mut child = self.child.lock().await;
        if let Some(child) = child.as_mut() {
            let _ = child.start_kill();
        }
        if let Some(child) = child.as_mut() {
            let _ = child.wait().await;
        }
        *child = None;
    }
}

impl StdioTransport {
    /// 读到 id 匹配的响应为止;通知(无 id)与其它 id 的报文跳过。
    async fn read_response(&self, id: u64) -> Result<String, McpClientError> {
        let mut stdout = self.stdout.lock().await;
        let mut line = String::new();
        loop {
            line.clear();
            let read = stdout
                .read_line(&mut line)
                .await
                .map_err(|error| McpClientError::Disconnected(error.to_string()))?;
            if read == 0 {
                return Err(McpClientError::Disconnected(
                    "服务器进程已退出(stdout 关闭)".to_string(),
                ));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
                // 非 JSON 行(服务器日志误写 stdout):跳过,不污染协议。
                tracing::debug!(server = %self.name, "ignoring non-JSON line from MCP server");
                continue;
            };
            let matches = value
                .get("id")
                .and_then(Value::as_u64)
                .is_some_and(|response_id| response_id == id);
            if matches {
                return Ok(trimmed.to_string());
            }
        }
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        // 同步上下文无法 await:先发 kill,回收交给 tokio/OS。
        // `kill_on_drop(true)` 是最后一道保险。
        if let Ok(mut child) = self.child.try_lock()
            && let Some(child) = child.as_mut()
        {
            let _ = child.start_kill();
        }
    }
}

#[cfg(windows)]
fn is_console_shim(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with(".cmd") || lower.ends_with(".bat")
}

/// Windows 上 CreateProcess 不解析 PATHEXT:配置里的 `npx` 只有 `npx.cmd`,
/// 直接交给系统就是 `program not found`。这里按 cmd 的顺序(PATH 目录 ×
/// PATHEXT 扩展名)在进程内 stat 出真实路径,再交给 CreateProcess。
/// 返回值第二项表示该程序由 cmd.exe 承载,需要压制控制台窗口。
/// 解析不出来时原样返回,让错误信息保持可读的"程序名: not found"形态。
#[cfg(windows)]
fn resolve_program(command: &str) -> (String, bool) {
    // 带路径分隔符的写法交给系统原样解析。
    if command.contains('/') || command.contains('\\') {
        return (command.to_string(), is_console_shim(command));
    }
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    let lower = command.to_lowercase();
    if exts
        .split(';')
        .filter(|e| !e.is_empty())
        .any(|e| lower.ends_with(&e.to_lowercase()))
    {
        // 名字已含可执行扩展名,CreateProcess 自己会按 PATH 找。
        return (command.to_string(), is_console_shim(command));
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for ext in exts.split(';').filter(|e| !e.is_empty()) {
                // PATHEXT 惯例大写(.CMD),但磁盘上的 npm shim 是小写;
                // 拼成小写让返回路径与真实文件名一致,大小写敏感场景也不炸。
                let full = dir.join(format!("{}{}", command, ext.to_lowercase()));
                if full.is_file() {
                    let resolved = full.to_string_lossy().into_owned();
                    let console_shim = is_console_shim(&resolved);
                    return (resolved, console_shim);
                }
            }
        }
    }
    (command.to_string(), false)
}

#[cfg(not(windows))]
fn resolve_program(command: &str) -> (String, bool) {
    (command.to_string(), false)
}

/// 写一行 JSON 帧(newline-delimited JSON)。
async fn write_frame(stdin: &mut ChildStdin, frame: &str) -> Result<(), McpClientError> {
    stdin
        .write_all(frame.as_bytes())
        .await
        .map_err(|error| McpClientError::Disconnected(error.to_string()))?;
    stdin
        .write_all(b"\n")
        .await
        .map_err(|error| McpClientError::Disconnected(error.to_string()))?;
    stdin
        .flush()
        .await
        .map_err(|error| McpClientError::Disconnected(error.to_string()))?;
    Ok(())
}

#[cfg(windows)]
#[cfg(test)]
mod tests {
    use super::{StdioTransport, is_console_shim, resolve_program};
    use crate::client::McpClientError;
    use crate::transport::McpTransport;
    use std::collections::HashMap;

    /// PATH 是进程级共享状态,而测试默认并行跑:改动它的用例必须串行。
    static PATH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 造一个临时 bin 目录,里面放 `denia-mcp-test-shim.cmd`,返回该目录。
    fn shim_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "denia-mcp-pathext-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("临时目录可建");
        std::fs::write(dir.join("denia-mcp-test-shim.cmd"), "@echo off\r\n").expect("shim 可写");
        dir
    }

    /// 把 PATH 换成单一目录;返回原 PATH 供调用方恢复。
    fn isolate_path(dir: &std::path::Path) -> Option<std::ffi::OsString> {
        let original = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", dir) };
        original
    }

    fn restore_path(original: Option<std::ffi::OsString>) {
        match original {
            Some(value) => unsafe { std::env::set_var("PATH", value) },
            None => unsafe { std::env::remove_var("PATH") },
        }
    }

    #[test]
    fn bare_name_resolves_through_pathext() {
        // 这就是线上故障的形态:配置写 `npx`,磁盘上只有 `npx.cmd`。
        let _serial = PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = shim_dir();
        let original = isolate_path(&dir);

        let (program, console_shim) = resolve_program("denia-mcp-test-shim");
        restore_path(original);

        assert_eq!(
            program,
            dir.join("denia-mcp-test-shim.cmd").to_string_lossy(),
            "裸名应解析为 PATH 目录里的 .cmd 全路径"
        );
        assert!(console_shim, ".cmd 由 cmd.exe 承载,需要压制控制台窗口");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_extension_is_left_to_create_process() {
        let _serial = PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = shim_dir();
        let original = isolate_path(&dir);

        // 已带可执行扩展名:不改写,只判定窗口行为。
        let (program, console_shim) = resolve_program("denia-mcp-test-shim.cmd");
        let (node_program, node_shim) = resolve_program("node.exe");
        restore_path(original);

        assert_eq!(program, "denia-mcp-test-shim.cmd");
        assert!(console_shim);
        assert_eq!(node_program, "node.exe");
        assert!(!node_shim);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_bare_name_is_returned_unchanged() {
        // 解析不出来时保持原样,让报错仍是用户写的那个名字。
        let (program, console_shim) = resolve_program("denia-mcp-test-definitely-not-here");
        assert_eq!(program, "denia-mcp-test-definitely-not-here");
        assert!(!console_shim);
    }

    #[test]
    fn paths_with_separators_are_not_rewritten() {
        let (program, console_shim) = resolve_program(r"C:\Program Files\nodejs\npx");
        assert_eq!(program, r"C:\Program Files\nodejs\npx");
        assert!(!console_shim, "无扩展名的路径不猜它是 shim");

        let (program, console_shim) = resolve_program(r"C:\Program Files\nodejs\npx.cmd");
        assert_eq!(program, r"C:\Program Files\nodejs\npx.cmd");
        assert!(console_shim);
    }

    // 跨 await 持有是故意的:connect() 内部要读 PATH,持锁期间没有别的用例能改它。
    // 这是测试专用锁,不参与任何生产路径,不存在死锁面。
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn spawn_path_accepts_bare_cmd_shim_name() {
        // 端到端:connect 收到裸名,解析出 .cmd 全路径,真的把进程拉起来,
        // stdin/stdout 管道可用。这是线上 `npx: program not found` 的正面回归。
        let _serial = PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = shim_dir();
        let original = isolate_path(&dir);

        let connect = StdioTransport::connect(
            "pathext-e2e",
            "denia-mcp-test-shim",
            &[],
            &HashMap::new(),
            None,
        )
        .await;

        restore_path(original);
        let _ = std::fs::remove_dir_all(&dir);

        // shim 立刻 `@echo off` 退出:stdout 关闭会走 Disconnected,
        // 关键是绝不能是 Spawn 失败——那才是本 bug 的原始症状。
        match connect {
            Err(McpClientError::Spawn(message)) => {
                panic!("裸 .cmd 名仍然 spawn 失败: {message}")
            }
            Ok(transport) => {
                transport.shutdown().await;
            }
            Err(_) => {}
        }
    }

    #[test]
    fn console_shim_detects_case_insensitively() {
        assert!(is_console_shim("NPX.CMD"));
        assert!(is_console_shim("build.bat"));
        assert!(!is_console_shim("node.exe"));
        assert!(!is_console_shim("npx"));
    }

    /// 真实环境冒烟:需要装 Node 的机器上手动跑
    /// `cargo test -p denia-mcp --lib -- --ignored`。
    /// CI 机器不一定有 node,所以默认跳过。
    #[tokio::test]
    #[ignore = "requires Node.js on PATH"]
    async fn real_npx_shim_resolves() {
        let (program, console_shim) = resolve_program("npx");
        assert!(
            program.to_lowercase().ends_with(r"\npx.cmd"),
            "npx 应解析到 npx.cmd,实际得到 {program}"
        );
        assert!(
            program.to_lowercase().contains("nodejs"),
            "路径异常: {program}"
        );
        assert!(console_shim);

        // 线上故障的原始形态:配置只写 `npx`。spawn 必须成功。
        let args: Vec<String> = vec!["--version".to_string()];
        match StdioTransport::connect("real-npx", "npx", &args, &HashMap::new(), None).await {
            Err(McpClientError::Spawn(message)) => {
                panic!("npx 裸名仍然 spawn 失败: {message}")
            }
            Ok(transport) => transport.shutdown().await,
            Err(other) => panic!("spawn 成功但后续异常(可接受): {other:?}"),
        }
    }
}
