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
    artifact: Option<denia_tools::output::OutputArtifact>,
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
        Ok(serde_json::json!({"job":r.snapshot,"output":output,"outputArtifact":r.artifact}))
    }

    /// GUI 预览不消耗模型的输出游标或通知领取状态。
    pub fn peek(&self, id: &str, owner: &str) -> Result<serde_json::Value, String> {
        let mut records = self.records.lock().unwrap();
        let r = Self::owned(&mut records, id, owner)?;
        Ok(serde_json::json!({"job":r.snapshot,"output":r.output,"outputArtifact":r.artifact}))
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
        Some(serde_json::json!({"job":r.snapshot,"output":r.output,"outputArtifact":r.artifact}))
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
                artifact: None,
            },
        );
        drop(records);
        let jobs = self.clone();
        let id = snapshot.id.clone();
        let output_store = ctx.output_store.clone();
        tokio::spawn(async move {
            let capture = match output_store {
                Some(store) => Some(store.start(&["stdout", "stderr"]).await),
                None => None,
            };
            let stdout_preview = Arc::new(tokio::sync::Mutex::new(
                denia_tools::output::Preview::default(),
            ));
            let stderr_preview = Arc::new(tokio::sync::Mutex::new(
                denia_tools::output::Preview::default(),
            ));
            let out = tokio::spawn(pump(
                stdout,
                jobs.clone(),
                id.clone(),
                cap,
                capture.clone(),
                "stdout",
                stdout_preview.clone(),
            ));
            let err = tokio::spawn(pump(
                stderr,
                jobs.clone(),
                id.clone(),
                cap,
                capture.clone(),
                "stderr",
                stderr_preview.clone(),
            ));
            let pid = child.id();
            let (status, exit) = tokio::select! {
                result=child.wait()=>match result {Ok(s)=>(if s.success(){"completed"}else{"failed"},s.code()),Err(_)=>("failed",None)},
                _=cancel.cancelled()=>{denia_tools::shell::kill_tree(pid,&mut child).await;("killed",None)},
                _=tokio::time::sleep(Duration::from_millis(timeout))=>{denia_tools::shell::kill_tree(pid,&mut child).await;jobs.push(&id,"\n后台命令超时",cap);("failed",None)},
            };
            // 后代持有管道时不能无限等待；结果读取不会阻塞注册表锁。
            let out_abort = out.abort_handle();
            let err_abort = err.abort_handle();
            let drained = matches!(
                tokio::time::timeout(Duration::from_secs(2), async { (out.await, err.await) })
                    .await,
                Ok((Ok(true), Ok(true)))
            );
            if !drained {
                out_abort.abort();
                err_abort.abort();
            }
            let artifact = match capture {
                Some(capture) => Some(capture.finish(drained).await),
                None => None,
            };
            let notice = denia_tools::output::artifact_notice(artifact.as_ref());
            // cap is bytes; allow four bytes per Unicode character plus framing.
            let per_stream = (cap.saturating_sub(notice.len() + 32) / 8).min(12_000);
            let preview = format!(
                "{}\n--- stderr ---\n{}{}",
                stdout_preview
                    .lock()
                    .await
                    .render(per_stream / 3, per_stream * 2 / 3),
                stderr_preview
                    .lock()
                    .await
                    .render(per_stream / 3, per_stream * 2 / 3),
                notice
            );
            let settled = {
                let mut records = jobs.records.lock().unwrap();
                if let Some(r) = records.get_mut(&id) {
                    r.output = preview;
                    r.cursor = 0;
                    r.artifact = artifact;
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
async fn pump<R: AsyncRead + Unpin>(
    mut stream: R,
    jobs: Arc<Jobs>,
    id: String,
    cap: usize,
    capture: Option<denia_tools::output::Capture>,
    name: &str,
    preview: Arc<tokio::sync::Mutex<denia_tools::output::Preview>>,
) -> bool {
    let mut bytes = [0u8; 4096];
    let mut pending = Vec::new();
    loop {
        match stream.read(&mut bytes).await {
            Ok(0) => break,
            Ok(n) => {
                preview.lock().await.push(&bytes[..n]);
                if let Some(capture) = &capture {
                    capture.append(name, &bytes[..n]).await;
                }
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
                return false;
            }
        }
    }
    if !pending.is_empty() {
        jobs.push(&id, &String::from_utf8_lossy(&pending), cap);
    }
    true
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn ownership_wait_and_output() {
        let jobs = Jobs::new();
        let ctx = ToolContext {
            output_store: None,
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
            goal_reader: None,
            task_ledger: None,
            read_state: None,
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
        let root = std::env::temp_dir().join(format!("denia-job-output-{}", uuid::Uuid::new_v4()));
        let store = denia_tools::output::OutputStore::open(root.clone())
            .await
            .unwrap();
        let ctx = ToolContext {
            output_store: Some(store.clone()),
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
            goal_reader: None,
            task_ledger: None,
            read_state: None,
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
        let artifact = &preview["outputArtifact"];
        assert_eq!(artifact["complete"], true);
        assert!(
            store
                .read(
                    artifact["output_id"].as_str().unwrap(),
                    Some("stdout"),
                    1,
                    400
                )
                .await
                .unwrap()
                .contains('界')
        );
        assert_eq!(
            jobs.read(&job.id, "bounded").unwrap()["output"],
            preview["output"]
        );
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    /// 后台用例的 `ToolContext`(与上面两条用例同形状,提取出来给进程树用例复用)。
    #[cfg(windows)]
    fn ctx(dir: &std::path::Path, session: &str) -> ToolContext {
        ToolContext {
            output_store: None,
            session_id: Some(session.into()),
            selection: None,
            cwd: dir.to_path_buf(),
            cancel: CancellationToken::new(),
            confined: true,
            vision_supported: false,
            emit_event: None,
            file_history: None,
            permission_mode: denia_core::session::PermissionMode::AutoEdit,
            ask: None,
            call_id: None,
            goal_reader: None,
            task_ledger: None,
            read_state: None,
        }
    }

    /// 拉起「外层 shell → 内层 shell」进程树的那条命令:内层 shell 把自己的
    /// PID 写进 `marker` 后长睡,外层同样长睡——形状与后台跑 `cargo build`
    /// 一致(直接子进程是 shell,真正干活的是它的后代)。
    ///
    /// `denia_tools::shell::test_tree` 是 `pub(crate)` 且只在 tools 自己的测试
    /// 构建里编译,跨 crate 拿不到,这里按同一形状写一份最小等价脚手架。
    #[cfg(windows)]
    fn nested_sleeper_command(marker: &std::path::Path) -> String {
        let marker = marker.display();
        format!(
            "$exe = (Get-Process -Id $PID).Path; \
             $inner = 'Set-Content -LiteralPath \"{marker}\" -Value $PID; Start-Sleep -Seconds 120'; \
             & $exe -NoLogo -NoProfile -NonInteractive -Command $inner; \
             Start-Sleep -Seconds 120"
        )
    }

    /// 读孙进程报出的 PID(还没写出来就返回 None)。
    #[cfg(windows)]
    fn read_pid(marker: &std::path::Path) -> Option<u32> {
        std::fs::read_to_string(marker).ok()?.trim().parse().ok()
    }

    /// 等孙进程报出自己的 PID,宽限 `budget` 后放弃。
    #[cfg(windows)]
    async fn wait_for_pid(marker: &std::path::Path, budget: Duration) -> Option<u32> {
        let deadline = std::time::Instant::now() + budget;
        loop {
            if let Some(pid) = read_pid(marker) {
                return Some(pid);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// 进程是否还活着。查不出来时当「活着」:失败不能伪装成通过。
    #[cfg(windows)]
    async fn process_alive(pid: u32) -> bool {
        let script = format!(
            "if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ 'alive' }} else {{ 'gone' }}"
        );
        let output = tokio::process::Command::new(denia_tools::shell::shell_executable_path())
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &script,
            ])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .output()
            .await;
        match output {
            Ok(output) => String::from_utf8_lossy(&output.stdout).contains("alive"),
            Err(_) => true,
        }
    }

    /// 等进程消失(给终止一个落地的宽限,而不是假定它瞬间生效)。
    #[cfg(windows)]
    async fn wait_until_gone(pid: u32) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if !process_alive(pid).await {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// 回归:后台任务被**取消**时,它拉起的孙进程也必须一起消失。
    ///
    /// 此前只有前台 3 条 Windows 用例覆盖 `kill_tree`,「前后台复用同一实现」
    /// 只是读代码得出的结论。这里走真实的后台取消链路:`Jobs::kill` →
    /// `cancel.cancel()` → 任务 select 的 `cancel.cancelled()` 分支
    /// (`denia_tools::shell::kill_tree`),再断言孙进程已被清理。
    ///
    /// 同步不靠 sleep 猜时序:孙进程先把自己的 PID 写进临时文件,轮询到 PID
    /// 再取消;断言消失时同样轮询,并给 15 秒宽限。
    #[cfg(windows)]
    #[tokio::test]
    async fn cancel_kills_grandchild_processes() {
        let dir = std::env::temp_dir().join(format!("denia-job-cancel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("grandchild.pid");
        let jobs = Jobs::new();
        let job = jobs
            .start(
                &ctx(&dir, "cancel"),
                &nested_sleeper_command(&marker),
                "取消进程树",
                60_000,
                1,
                8,
                4096,
            )
            .unwrap();
        // 等进程树真的拉起来(孙进程写出自己的 PID)再取消,免得测的是竞态。
        let grandchild = wait_for_pid(&marker, Duration::from_secs(30))
            .await
            .expect("进程树没有拉起孙进程(缺 grandchild.pid)");
        assert!(
            process_alive(grandchild).await,
            "孙进程 {grandchild} 应该在树里活着"
        );
        jobs.kill(&job.id, "cancel").unwrap();
        jobs.wait(&job.id, "cancel", 15_000, &CancellationToken::new())
            .await
            .unwrap();
        let settled = jobs.peek(&job.id, "cancel").unwrap();
        assert_eq!(
            settled["job"]["status"], "killed",
            "取消后任务状态应为 killed:{}",
            settled["job"]
        );
        assert!(
            wait_until_gone(grandchild).await,
            "取消后孙进程 {grandchild} 还在跑:后台清理没有连根拔"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// 回归:后台任务**超时**时同样连根拔(select 里另一条 `kill_tree` 调用点)。
    ///
    /// 超时设 12 秒、等 PID 给 10 秒宽限:正常机器上内层 shell 起完约 1-2 秒,
    /// 10 秒足够宽。若这里真的拿不到 PID,说明机器异常慢,直接报失败而不是
    /// 悄悄放行。
    #[cfg(windows)]
    #[tokio::test]
    async fn timeout_kills_grandchild_processes() {
        let dir = std::env::temp_dir().join(format!("denia-job-timeout-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("grandchild.pid");
        let jobs = Jobs::new();
        let job = jobs
            .start(
                &ctx(&dir, "timeout"),
                &nested_sleeper_command(&marker),
                "超时进程树",
                12_000,
                1,
                8,
                4096,
            )
            .unwrap();
        let grandchild = wait_for_pid(&marker, Duration::from_secs(10))
            .await
            .expect("超时前没等到孙进程 PID:内层 shell 10 秒还没起来,机器过慢");
        jobs.wait(&job.id, "timeout", 20_000, &CancellationToken::new())
            .await
            .unwrap();
        let settled = jobs.peek(&job.id, "timeout").unwrap();
        assert_eq!(
            settled["job"]["status"], "failed",
            "超时后任务状态应为 failed(超时分支):{}",
            settled["job"]
        );
        assert!(
            wait_until_gone(grandchild).await,
            "超时后孙进程 {grandchild} 还在跑:后台清理没有连根拔"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
