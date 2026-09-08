//! 有界后台命令注册表。等待与生产者分离，结束状态只提交一次。
use denia_tools::ToolContext;
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    sync::{broadcast, watch},
};
use tokio_util::sync::CancellationToken;

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSnapshot {
    pub id: String,
    pub owner: String,
    pub kind: String,
    pub label: String,
    pub status: String,
    pub started_at: u64,
    pub finished_at: Option<u64>,
    pub exit_code: Option<i32>,
    pub reported: bool,
}
struct Record {
    snapshot: JobSnapshot,
    output: String,
    cursor: usize,
    waiters: usize,
    cancel: CancellationToken,
    changes: watch::Sender<bool>,
}
pub struct Jobs {
    records: Mutex<BTreeMap<String, Record>>,
    pub done: broadcast::Sender<JobSnapshot>,
}
pub struct WaitLease {
    jobs: Arc<Jobs>,
    id: String,
    owner: String,
}
impl Drop for WaitLease {
    fn drop(&mut self) {
        let notification = {
            let mut records = self.jobs.records.lock().unwrap();
            if let Ok(r) = Jobs::owned(&mut records, &self.id, &self.owner) {
                r.waiters = r.waiters.saturating_sub(1);
                (r.waiters == 0 && r.snapshot.finished_at.is_some() && !r.snapshot.reported)
                    .then(|| r.snapshot.clone())
            } else {
                None
            }
        };
        if let Some(snapshot) = notification {
            let _ = self.jobs.done.send(snapshot);
        }
    }
}
impl Jobs {
    pub fn wait_lease(self: &Arc<Self>, id: &str, owner: &str) -> Result<WaitLease, String> {
        let mut records = self.records.lock().unwrap();
        Self::owned(&mut records, id, owner)?.waiters += 1;
        Ok(WaitLease {
            jobs: self.clone(),
            id: id.into(),
            owner: owner.into(),
        })
    }
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            records: Mutex::new(BTreeMap::new()),
            done: broadcast::channel(256).0,
        })
    }
    pub fn list(&self, owner: &str) -> Vec<JobSnapshot> {
        let mut rows: Vec<_> = self
            .records
            .lock()
            .unwrap()
            .values()
            .filter(|r| r.snapshot.owner == owner)
            .map(|r| r.snapshot.clone())
            .collect();
        rows.sort_by_key(|r| r.started_at);
        rows
    }
    fn owned<'a>(
        records: &'a mut BTreeMap<String, Record>,
        id: &str,
        owner: &str,
    ) -> Result<&'a mut Record, String> {
        records
            .get_mut(id)
            .filter(|r| r.snapshot.owner == owner)
            .ok_or_else(|| "任务不存在或不属于当前会话".into())
    }
    pub fn read(&self, id: &str, owner: &str) -> Result<serde_json::Value, String> {
        let mut records = self.records.lock().unwrap();
        let r = Self::owned(&mut records, id, owner)?;
        let terminal = r.snapshot.finished_at.is_some();
        let output = if terminal {
            r.output.clone()
        } else {
            r.output[r.cursor..].to_string()
        };
        r.cursor = r.output.len();
        if terminal {
            r.snapshot.reported = true;
        }
        Ok(serde_json::json!({"job":r.snapshot,"output":output}))
    }

    /// GUI 预览不消耗模型的输出游标或通知领取状态。
    pub fn peek(&self, id: &str, owner: &str) -> Result<serde_json::Value, String> {
        let mut records = self.records.lock().unwrap();
        let r = Self::owned(&mut records, id, owner)?;
        Ok(serde_json::json!({"job":r.snapshot,"output":r.output}))
    }
    pub fn release_notice(&self, id: &str, owner: &str) {
        let mut records = self.records.lock().unwrap();
        if let Ok(r) = Self::owned(&mut records, id, owner) {
            r.snapshot.reported = false;
        }
    }
    pub fn claim_notice(&self, id: &str, owner: &str) -> Option<serde_json::Value> {
        let mut records = self.records.lock().unwrap();
        let r = Self::owned(&mut records, id, owner).ok()?;
        if r.snapshot.reported || r.snapshot.finished_at.is_none() || r.waiters > 0 {
            return None;
        }
        r.snapshot.reported = true;
        Some(serde_json::json!({"job":r.snapshot,"output":r.output}))
    }
    pub fn kill(&self, id: &str, owner: &str) -> Result<(), String> {
        let mut records = self.records.lock().unwrap();
        let r = Self::owned(&mut records, id, owner)?;
        if r.snapshot.finished_at.is_none() {
            r.snapshot.status = "stopping".into();
            r.snapshot.reported = true;
            r.cancel.cancel();
        }
        Ok(())
    }
    pub async fn wait(
        &self,
        id: &str,
        owner: &str,
        ms: u64,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        let mut rx = {
            let mut records = self.records.lock().unwrap();
            let r = Self::owned(&mut records, id, owner)?;
            if r.snapshot.finished_at.is_some() {
                return Ok(());
            }
            r.changes.subscribe()
        };
        tokio::select! { _=cancel.cancelled()=>Err("等待已中断，后台任务继续运行".into()), _=tokio::time::sleep(Duration::from_millis(ms))=>Ok(()), _=rx.changed()=>Ok(()) }
    }
    pub async fn cancel_owner(&self, owner: &str) {
        for job in self.list(owner) {
            let _ = self.kill(&job.id, owner);
            let _ = self
                .wait(&job.id, owner, 10_000, &CancellationToken::new())
                .await;
        }
        self.records
            .lock()
            .unwrap()
            .retain(|_, r| r.snapshot.owner != owner || r.snapshot.finished_at.is_none());
    }
    pub fn start(
        self: &Arc<Self>,
        ctx: &ToolContext,
        command: &str,
        label: &str,
        timeout: u64,
        max_active: usize,
        max_retained: usize,
        cap: usize,
    ) -> Result<JobSnapshot, String> {
        if ctx.cancel.is_cancelled() {
            return Err("启动已取消".into());
        }
        let owner = ctx.session_id.clone().ok_or("缺少会话身份")?;
        // 只读/计划档禁止有写副作用的后台命令(与前台 bash 同口径)。
        let mode = ctx.permission_mode;
        if matches!(
            mode,
            denia_core::session::PermissionMode::ReadOnly
                | denia_core::session::PermissionMode::Plan
        ) && denia_tools::permission::bash_may_write(command)
        {
            return Err(format!(
                "{mode_label}权限不允许该后台命令",
                mode_label = if mode == denia_core::session::PermissionMode::Plan {
                    "计划模式"
                } else {
                    "只读"
                }
            ));
        }
        let mut records = self.records.lock().unwrap();
        if records
            .values()
            .filter(|r| r.snapshot.owner == owner && r.snapshot.finished_at.is_none())
            .count()
            >= max_active
        {
            return Err("当前会话的后台任务并发已达上限".into());
        }
        while records.len() >= max_retained {
            let oldest = records
                .values()
                .filter(|r| r.snapshot.finished_at.is_some() && r.snapshot.reported)
                .min_by_key(|r| r.snapshot.started_at)
                .map(|r| r.snapshot.id.clone());
            match oldest {
                Some(id) => {
                    records.remove(&id);
                }
                None => return Err("后台任务记录已满，请领取已有结果后重试".into()),
            }
        }
        let mut cmd = denia_tools::shell::shell_command(command);
        cmd.current_dir(&ctx.cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| format!("后台命令启动失败：{e}"))?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let cancel = CancellationToken::new();
        let snapshot = JobSnapshot {
            id: format!("bash-{}", uuid::Uuid::new_v4()),
            owner,
            kind: "bash".into(),
            label: label.chars().take(200).collect(),
            status: "running".into(),
            started_at: now(),
            finished_at: None,
            exit_code: None,
            reported: false,
        };
        records.insert(
            snapshot.id.clone(),
            Record {
                snapshot: snapshot.clone(),
                output: String::new(),
                cursor: 0,
                waiters: 0,
                cancel: cancel.clone(),
                changes: watch::channel(false).0,
            },
        );
        drop(records);
        let jobs = self.clone();
        let id = snapshot.id.clone();
        tokio::spawn(async move {
            let out = tokio::spawn(pump(stdout, jobs.clone(), id.clone(), cap));
            let err = tokio::spawn(pump(stderr, jobs.clone(), id.clone(), cap));
            let pid = child.id();
            let (status, exit) = tokio::select! {
                result=child.wait()=>match result {Ok(s)=>(if s.success(){"completed"}else{"failed"},s.code()),Err(_)=>("failed",None)},
                _=cancel.cancelled()=>{kill_tree(pid,&mut child).await;("killed",None)},
                _=tokio::time::sleep(Duration::from_millis(timeout))=>{kill_tree(pid,&mut child).await;jobs.push(&id,"\n后台命令超时",cap);("failed",None)},
            };
            // 后代持有管道时不能无限等待；结果读取不会阻塞注册表锁。
            let out_abort = out.abort_handle();
            let err_abort = err.abort_handle();
            if tokio::time::timeout(Duration::from_secs(2), async {
                let _ = out.await;
                let _ = err.await;
            })
            .await
            .is_err()
            {
                out_abort.abort();
                err_abort.abort();
            }
            let settled = {
                let mut records = jobs.records.lock().unwrap();
                if let Some(r) = records.get_mut(&id) {
                    r.snapshot.status = status.into();
                    r.snapshot.exit_code = exit;
                    r.snapshot.finished_at = Some(now());
                    let _ = r.changes.send(true);
                    Some(r.snapshot.clone())
                } else {
                    None
                }
            };
            if let Some(snapshot) = settled {
                let _ = jobs.done.send(snapshot);
            }
        });
        Ok(snapshot)
    }
    fn push(&self, id: &str, text: &str, cap: usize) {
        if let Some(r) = self.records.lock().unwrap().get_mut(id) {
            r.output.push_str(text);
            if r.output.len() > cap {
                let mut cut = r.output.len() - cap;
                while !r.output.is_char_boundary(cut) {
                    cut += 1;
                }
                r.output.drain(..cut);
                r.cursor = r.cursor.saturating_sub(cut);
            }
        }
    }
}
async fn pump<R: AsyncRead + Unpin>(mut stream: R, jobs: Arc<Jobs>, id: String, cap: usize) {
    let mut bytes = [0u8; 4096];
    let mut pending = Vec::new();
    loop {
        match stream.read(&mut bytes).await {
            Ok(0) => break,
            Ok(n) => {
                pending.extend_from_slice(&bytes[..n]);
                let valid = match std::str::from_utf8(&pending) {
                    Ok(_) => pending.len(),
                    Err(e) if e.error_len().is_none() => e.valid_up_to(),
                    Err(_) => pending.len(),
                };
                jobs.push(&id, &String::from_utf8_lossy(&pending[..valid]), cap);
                pending.drain(..valid);
            }
            Err(e) => {
                jobs.push(&id, &format!("\n读取任务输出失败：{e}"), cap);
                break;
            }
        }
    }
    if !pending.is_empty() {
        jobs.push(&id, &String::from_utf8_lossy(&pending), cap);
    }
}
async fn kill_tree(pid: Option<u32>, child: &mut tokio::process::Child) {
    #[cfg(windows)]
    if let Some(pid) = pid {
        let mut cmd = tokio::process::Command::new("taskkill.exe");
        cmd.args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(0x08000000);
        let _ = cmd.output().await;
    }
    #[cfg(not(windows))]
    let _ = pid;
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn ownership_wait_and_output() {
        let jobs = Jobs::new();
        let ctx = ToolContext {
            session_id: Some("owner".into()),
            selection: None,
            cwd: std::env::temp_dir(),
            cancel: CancellationToken::new(),
            confined: true,
            vision_supported: false,
            emit_event: None,
            file_history: None,
            permission_mode: denia_core::session::PermissionMode::AutoEdit,
            ask: None,
            call_id: None,
        };
        let job = jobs
            .start(&ctx, "echo hello", "测试", 10_000, 2, 8, 4096)
            .unwrap();
        assert!(jobs.read(&job.id, "foreign").is_err());
        assert!(jobs.kill(&job.id, "foreign").is_err());
        let lease = jobs.wait_lease(&job.id, "owner").unwrap();
        jobs.wait(&job.id, "owner", 15_000, &CancellationToken::new())
            .await
            .unwrap();
        assert!(jobs.claim_notice(&job.id, "owner").is_none());
        let first = jobs.read(&job.id, "owner").unwrap();
        drop(lease);
        assert!(first["output"].as_str().unwrap().contains("hello"));
        assert_eq!(first, jobs.read(&job.id, "owner").unwrap());
        assert!(jobs.claim_notice(&job.id, "owner").is_none());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn large_unicode_output_does_not_block_and_wait_does_not_cancel() {
        let jobs = Jobs::new();
        let ctx = ToolContext {
            session_id: Some("bounded".into()),
            selection: None,
            cwd: std::env::temp_dir(),
            cancel: CancellationToken::new(),
            confined: true,
            vision_supported: false,
            emit_event: None,
            file_history: None,
            permission_mode: denia_core::session::PermissionMode::AutoEdit,
            ask: None,
            call_id: None,
        };
        let job = jobs
            .start(
                &ctx,
                "Write-Output ('界' * 50000); Start-Sleep -Milliseconds 500",
                "大输出",
                10_000,
                1,
                8,
                4096,
            )
            .unwrap();
        jobs.wait(&job.id, "bounded", 1, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(jobs.list("bounded")[0].status, "running");
        assert!(
            jobs.start(&ctx, "echo refused", "超限", 10_000, 1, 8, 4096)
                .is_err()
        );
        jobs.wait(&job.id, "bounded", 15_000, &CancellationToken::new())
            .await
            .unwrap();
        let preview = jobs.peek(&job.id, "bounded").unwrap();
        assert_eq!(preview["job"]["status"], "completed");
        assert_eq!(preview["job"]["reported"], false);
        let output = preview["output"].as_str().unwrap();
        assert!(output.len() <= 4096);
        assert!(output.contains('界'));
        assert!(!output.contains('\u{fffd}'));
        assert_eq!(
            jobs.read(&job.id, "bounded").unwrap()["output"],
            preview["output"]
        );
    }
}
