use super::*;

impl Runtime {
    pub(super) fn sync_inbox(inbox: &mut Inbox, session: &denia_session::Session) {
        for event in session.events_after(inbox.seq) {
            inbox.seq = event.seq;
            match event.event {
                SessionEvent::AgentInbox { id, text, source } => {
                    inbox.seen.insert(id.clone());
                    inbox.pending.insert(event.seq, (id, text, source));
                }
                SessionEvent::AgentDelivery { id, .. } => {
                    inbox.seen.insert(id.clone());
                    inbox.pending.retain(|_, (key, _, _)| *key != id);
                }
                _ => {}
            }
        }
    }
    pub async fn enqueue(
        &self,
        target: &str,
        id: String,
        text: String,
        source: String,
    ) -> Result<String, String> {
        if text.len() > self.config().output_bytes * 2 {
            return Err("代理消息超过配置的体积上限".into());
        }
        if self.test_fault("completion-enqueue") && source == "subagent-settled" {
            return Err("注入故障：完成通知入队失败".into());
        }
        if source.starts_with("agent:") {
            self.inner.paused.lock().unwrap().remove(target);
            self.inner.wakes.lock().unwrap().insert(target.into(), 0);
        }
        let live = self.live(target).await?;
        let inner = self.inner.clone();
        let target = target.to_string();
        let result_id = id.clone();
        let wake_target = target.clone();
        let limit = self.config().max_pending_messages;
        tokio::task::spawn_blocking(move || {
            let mut inboxes = inner.inboxes.lock().unwrap();
            let inbox = inboxes.entry(target).or_default();
            Self::sync_inbox(inbox, &live.session);
            if inbox.seen.contains(&id) {
                return Ok(());
            }
            if inbox.pending.len() >= limit {
                return Err("目标代理待收消息已达上限".to_string());
            }
            let event = live
                .session
                .append(SessionEvent::AgentInbox { id, text, source })
                .map_err(|e| e.to_string())?;
            live.session.flush().map_err(|e| e.to_string())?;
            let _ = live.followers.send(event);
            Self::sync_inbox(inbox, &live.session);
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())??;
        self.wake(&wake_target);
        Ok(result_id)
    }
    pub(super) fn wake(&self, id: &str) {
        self.defer_wake(id);
        if !self.inner.delivery.start_wake(id) {
            return;
        }
        let runtime = self.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            use futures::FutureExt;
            loop {
                let result = std::panic::AssertUnwindSafe(runtime.resume_pending(&id))
                    .catch_unwind()
                    .await;
                match result {
                    Ok(Err(error)) => tracing::error!(session=%id,%error,"恢复代理消息失败"),
                    Err(_) => tracing::error!(session=%id,"代理运行任务异常退出"),
                    _ => {}
                }
                // on_idle 或新投递可能发生在本 worker 尚未释放所有权时。
                // 保留这次请求并串行处理，不依赖心跳或下一条用户消息。
                if !runtime.inner.delivery.finish_wake(&id) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        });
    }

    /// 登记一次"现在跑不了、但必须跑"的唤醒。
    ///
    /// 消费点有两处,都不依赖消息到达那一刻的时序:
    /// ① [`Runtime::on_idle`] —— 任何会话（含子代理自己）每轮结束时扫全量
    ///    inbox 并逐个唤醒,这正是"主代理 wait_agent 轮次结束"的那个边沿;
    /// ② 准入槽位释放 —— 见 `Admission` 释放路径的连带扫描。
    pub(super) fn defer_wake(&self, id: &str) {
        self.inner.deferred_wakes.lock().unwrap().insert(id.into());
        tracing::debug!(session = id, "唤醒已挂起,等待目标空闲后重试");
    }
}

/// 未结算的审批/提问数量与最后一次终态摘要。///
/// 扫描窗口限定在最近 [`INTERACTION_SCAN_TAIL`] 条事件：等待中的交互必然是
/// 最近发生的（进程正阻塞在它上面），而每次列表刷新都全量扫长会话代价太大。
/// 比窗口更早的未结算请求已由加载期收敛（见 `session::recovery`）补上终态。
pub(super) const INTERACTION_SCAN_TAIL: usize = 2000;

pub(super) fn pending_interactions(
    session: &denia_session::Session,
) -> (usize, usize, Option<String>) {
    let events = session.events();
    let start = events.len().saturating_sub(INTERACTION_SCAN_TAIL);
    let mut approvals: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut asks: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut result: Option<String> = None;
    for envelope in &events[start..] {
        match &envelope.event {
            SessionEvent::ApprovalAsked { request_id, .. } => {
                approvals.insert(request_id.as_str());
            }
            SessionEvent::ApprovalDecided { request_id, .. } => {
                approvals.remove(request_id.as_str());
            }
            SessionEvent::AskRequested { request_id, .. } => {
                asks.insert(request_id.as_str());
            }
            SessionEvent::AskResolved { request_id, .. } => {
                asks.remove(request_id.as_str());
            }
            SessionEvent::TurnEnd { reason, .. } => result = Some(turn_end_label(reason)),
            _ => {}
        }
    }
    (approvals.len(), asks.len(), result)
}
