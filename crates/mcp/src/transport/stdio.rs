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
        let mut command_builder = Command::new(command);
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
