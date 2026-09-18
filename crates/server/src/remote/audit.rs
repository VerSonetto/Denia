//! 审计日志:连接、兑换、失败与断开都留痕,但**绝不落明文凭据**。
//!
//! 落盘格式是 JSONL(每行一条 JSON),与项目里 session 日志同风格:追加写、
//! 单行独立、损坏一行不影响其余。写入失败只告警不阻断 —— 审计不该成为
//! 可用性单点,但也不能静默失败,所以走 tracing::warn。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// 一条审计记录。字段名固定,便于外部工具按 jq 查询。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditRecord {
    /// epoch 毫秒。
    pub at: u64,
    /// 事件名:`ticket-issued` / `exchange` / `pin-verify` / `session-open` /
    /// `session-revoked` / `disconnect` / `rate-limited` / `tunnel-start` …
    pub event: String,
    /// 来源 IP。局域网与隧道都记;取不到时是 `unknown`。
    pub peer: String,
    /// 通道:`lan` / `tunnel` / `local`。
    pub via: String,
    /// 结果:`ok` / `denied` / `expired` / `limited`。
    pub outcome: String,
    /// 人类可读补充。**此处不得出现 ticket / 令牌 / PIN 的明文**。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl AuditRecord {
    pub fn new(event: &str, peer: &str, via: &str, outcome: &str) -> Self {
        Self {
            at: now_millis(),
            event: event.to_string(),
            peer: peer.to_string(),
            via: via.to_string(),
            outcome: outcome.to_string(),
            detail: None,
        }
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

/// 追加式审计写入器。`path` 为 None 表示关闭(配置层已禁止关闭,留 None
/// 只是为了让测试能构造无副作用实例)。
pub struct AuditLog {
    path: Option<PathBuf>,
    lock: Mutex<()>,
}

impl AuditLog {
    /// `home` 下的相对路径;绝对路径原样使用。
    pub fn new(home: &Path, path: &str) -> Self {
        let trimmed = path.trim();
        let resolved = if Path::new(trimmed).is_absolute() {
            PathBuf::from(trimmed)
        } else {
            home.join(trimmed)
        };
        Self {
            path: Some(resolved),
            lock: Mutex::new(()),
        }
    }

    /// 关闭状态:只记录到 tracing。
    pub fn disabled() -> Self {
        Self {
            path: None,
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// 追加一条记录。串行化在锁内完成:并发请求下不会写出半行交织的 JSON。
    pub fn write(&self, record: &AuditRecord) {
        let Some(path) = &self.path else {
            return;
        };
        let line = match serde_json::to_string(record) {
            Ok(line) => line,
            Err(error) => {
                tracing::warn!(%error, "audit record serialization failed");
                return;
            }
        };
        let _guard = self.lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            tracing::warn!(%error, path = %parent.display(), "could not create audit directory");
            return;
        }
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            Ok(mut file) => {
                if let Err(error) = writeln!(file, "{line}") {
                    tracing::warn!(%error, path = %path.display(), "could not append audit record");
                }
            }
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "could not open audit log");
            }
        }
    }
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_jsonl_lines() {
        let dir = std::env::temp_dir().join(format!("denia-audit-{}", uuid::Uuid::new_v4()));
        let log = AuditLog::new(&dir, "remote/audit.jsonl");
        log.write(&AuditRecord::new("exchange", "192.168.1.5", "lan", "ok"));
        log.write(
            &AuditRecord::new("pin-verify", "192.168.1.5", "tunnel", "denied")
                .detail("attempt 2/3"),
        );
        let body = std::fs::read_to_string(log.path().unwrap()).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["event"], "exchange");
        assert_eq!(first["via"], "lan");
        assert!(first["detail"].is_null(), "无 detail 时不该输出该字段");
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["outcome"], "denied");
        assert_eq!(second["detail"], "attempt 2/3");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disabled_log_writes_nothing() {
        let log = AuditLog::disabled();
        assert!(log.path().is_none());
        log.write(&AuditRecord::new("exchange", "peer", "lan", "ok"));
    }
}
