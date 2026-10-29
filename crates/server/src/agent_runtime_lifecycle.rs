// 完成结果的投递调度：后台任务与子代理共用一条队列。
//
// 生产者只登记结果；本文件决定“什么时候交回模型”。唯一的硬约束是
// [`LiveSession::mid_action`]：模型正在输出或执行工具时结果留在队列里，
// 等它完成当前动作；模型空闲(或已经不在运行)时立刻落进收件箱：
//
// - 空闲(未运行)：入队 + 唤醒，必要时开新一轮；
// - 运行中但动作间空闲：入队，由当前轮次的下一个 step 边界领取；
// - 运行中且正在输出/执行工具：留在队列；动作结束由
//   [`Runtime::observe_activity`] 催办，或轮次收尾时由 `on_idle` 催办。
//
// 投递事实仍以父会话日志为准(`AgentInbox` / `AgentDelivery`)：队列只重试
// 尚未落日志的结果，因此等待、补投、重启恢复都不会重复交付。

impl Runtime {
    /// 把一条模型事件折算成“是否正处在不可打断的动作里”。
    ///
    /// 输出段从首个 `AssistantChunk` 到 `AssistantMessage`；如果这条消息带
    /// 工具调用，动作继续到本 step 的 `StepEnd`(工具执行完)。等待用户审批
    /// 或回答不算输出/工具，此时投递不会打断模型；能否自动开新一轮仍由
    /// `running` 与唤醒预算裁决。
    pub(crate) fn observe_activity(&self, id: &str, event: &SessionEvent) {
        let Some(live) = self.inner.live.get(id) else {
            return;
        };
        let next = match event {
            SessionEvent::AssistantChunk { .. } => ModelActivity::Busy,
            SessionEvent::AssistantMessage { blocks, .. } => {
                if blocks
                    .iter()
                    .any(|block| matches!(block, denia_core::stream::ContentBlock::ToolCall { .. }))
                {
                    ModelActivity::Busy
                } else {
                    ModelActivity::Idle
                }
            }
            SessionEvent::ToolCall { .. } => ModelActivity::Busy,
            SessionEvent::StepEnd { .. } | SessionEvent::TurnEnd { .. } => ModelActivity::Idle,
            _ => return,
        };
        if live.set_activity(next) && next == ModelActivity::Idle {
            // 动作边界：不等退避计时，立刻重试这个目标的待投递结果。
            self.inner.delivery.expedite(id);
        }
    }

    /// 目标模型此刻是否正处在一个不能打断的动作里。
    fn model_mid_action(&self, id: &str) -> bool {
        self.inner
            .live
            .get(id)
            .is_some_and(|live| live.mid_action())
    }

    fn capture_completion(&self, id: &str) {
        let child = self.inner.children.lock().unwrap().get(id).cloned();
        let Some(child) = child else {
            return;
        };
        if child.descriptor.mode == "memory" {
            return;
        }
        let Some(live) = self.inner.live.get(id) else {
            return;
        };
        let mut events = live.session.events_after(0);
        if !live.running.load(Ordering::SeqCst) {
            let started = events.iter().rev().find_map(|event| match event.event {
                SessionEvent::TurnStart { turn } => Some((event.seq, turn)),
                _ => None,
            });
            if let Some((seq, turn)) = started
                && !events.iter().any(|event| event.seq > seq && matches!(event.event, SessionEvent::TurnEnd { turn: ended, .. } if ended == turn))
            {
                match live.session.append(SessionEvent::TurnEnd { turn, reason: TurnEndReason::Interrupted }) {
                    Ok(event) => { let _ = live.followers.send(event.clone()); events.push(event); }
                    Err(error) => { tracing::error!(session=id,%error,"异常子代理终态记录失败"); return; }
                }
            }
        }
        if let Some(index) = events
            .iter()
            .rposition(|event| matches!(event.event, SessionEvent::TurnEnd { .. }))
        {
            self.queue_completion(&child, &live.session, &events[..=index], true);
        }
    }

    fn queue_completion(
        &self,
        child: &Child,
        session: &denia_session::Session,
        events: &[SessionEnvelope],
        cleanup: bool,
    ) {
        let Some(end) = events.last() else {
            return;
        };
        let SessionEvent::TurnEnd { turn, .. } = end.event else {
            return;
        };
        // fork 种子里的父轮次不是这个子代理执行过的轮次。
        let input = events.iter().find(|event| {
            matches!(&event.event,
            SessionEvent::AgentInbox { source, .. } | SessionEvent::AgentDelivery { source, .. }
                if source.starts_with("agent:"))
        });
        if child.descriptor.fork.is_some() && input.is_none_or(|input| input.seq > end.seq) {
            return;
        }
        let text = events
            .iter()
            .rev()
            .find_map(|event| match &event.event {
                SessionEvent::AssistantMessage {
                    turn: message_turn,
                    blocks,
                    ..
                } if *message_turn == turn => Some(
                    blocks
                        .iter()
                        .filter_map(|block| match block {
                            denia_core::stream::ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                _ => None,
            })
            .unwrap_or_default();
        let limit = (self.config().output_bytes / 4).max(512);
        let body: String = text.chars().take(limit).collect();
        let truncated = if text.chars().count() > limit {
            format!(
                "\n（摘要已截断：子代理输出共 {} 字符，全文见子会话日志与产物。）",
                text.chars().count()
            )
        } else {
            String::new()
        };
        let leftover: Vec<_> = self
            .inner
            .jobs
            .list(&child.id)
            .into_iter()
            .filter(|job| job.finished_at.is_none())
            .collect();
        let cancelled_note = if leftover.is_empty() {
            String::new()
        } else {
            format!(
                "\n[已取消的子代理后台工作] 共 {} 项未完成，宿主已要求终止并清理（长时间服务应交回主代理启动）：{}",
                leftover.len(),
                leftover
                    .iter()
                    .map(|job| job.label.as_str())
                    .collect::<Vec<_>>()
                    .join("、")
            )
        };
        let handoff = subagent_handoff_at(Some(child), session, false, events);
        let mut summary = handoff.notice_text(&body, &truncated, &cancelled_note);
        // 交接信息本身也可能很大；不能让体积限制变成永久无法重试的通知。
        let max = self.config().output_bytes * 2;
        if summary.len() > max {
            let suffix = "\n（交接摘要已截断，完整结果见子会话日志与 wait_agent。）";
            summary = format!(
                "{}{}",
                truncate_utf8(&summary, max.saturating_sub(suffix.len())),
                suffix
            );
        }
        self.inner.delivery.push(
            Completion {
                child: child.id.clone(),
                target: child.parent_id.clone(),
                id: format!("settled:{}:turn:{turn}:seq:{}", child.id, end.seq),
                legacy_id: format!("settled:{}:{}", child.id, turn + 1),
                text: summary,
                source: "subagent-settled".into(),
            },
            cleanup,
        );
    }

    fn start_delivery_worker(&self) {
        let weak = Arc::downgrade(&self.inner);
        let delivery = self.inner.delivery.clone();
        tokio::spawn(async move {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let runtime = Runtime { inner };
            if let Err(error) = runtime.recover_deliveries().await {
                tracing::error!(%error,"代理完成通知恢复失败");
            }
            drop(runtime);
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
            loop {
                tokio::select! { _ = delivery.changed.notified() => {}, _ = tick.tick() => {} }
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let runtime = Runtime { inner };
                runtime.retry_deliveries().await;
            }
        });
    }

    async fn retry_deliveries(&self) {
        for completion in self.inner.delivery.due() {
            if !self.inner.sessions.exists(&completion.target)
                || !self.inner.sessions.exists(&completion.child)
            {
                self.inner.delivery.acknowledge(&completion.id);
                tracing::warn!(target=%completion.target,child=%completion.child,"完成通知目标已删除，停止补投");
                continue;
            }
            // 模型正在输出或执行工具：等它完成当前动作再投，绝不打断。
            // 队列保留这条结果，动作边界上的 `expedite` 会立刻重新催办。
            if self.model_mid_action(&completion.target) {
                tracing::debug!(target=%completion.target,id=%completion.id,"模型动作进行中，完成结果等待动作边界");
                continue;
            }
            let result = async {
                let child = self.live(&completion.child).await?;
                child.session.flush().map_err(|error| error.to_string())?;
                let parent = self.live(&completion.target).await?;
                let delivered = {
                    let mut inboxes = self.inner.inboxes.lock().unwrap();
                    let inbox = inboxes.entry(completion.target.clone()).or_default();
                    Self::sync_inbox(inbox, &parent.session);
                    inbox.seen.contains(&completion.id)
                        || inbox.seen.contains(&completion.legacy_id)
                };
                if delivered {
                    // 已经落过日志：目标空闲时补一次唤醒；正在跑就交给
                    // 当前轮次下一个 step 边界的 drain。
                    if !parent.running.load(Ordering::SeqCst) {
                        self.wake(&completion.target);
                    }
                } else {
                    self.enqueue(
                        &completion.target,
                        completion.id.clone(),
                        completion.text.clone(),
                        completion.source.clone(),
                    )
                    .await?;
                }
                Ok::<_, String>(())
            }
            .await;
            match result {
                Ok(()) => self.inner.delivery.acknowledge(&completion.id),
                Err(error) => tracing::warn!(child=%completion.child,%error,"完成结果待补投"),
            }
        }
        for (notice, child) in self.inner.delivery.claim_cleanups() {
            let runtime = self.clone();
            tokio::spawn(async move {
                use futures::FutureExt;
                let result = std::panic::AssertUnwindSafe(async {
                    runtime.inner.jobs.cancel_owner(&child).await;
                    runtime.close_owned_tabs(&child).await;
                })
                .catch_unwind()
                .await;
                if result.is_err() {
                    tracing::error!(session=%child,"子代理资源清理异常退出");
                }
                runtime.inner.delivery.finish_cleanup(&notice);
                runtime.wake(&child);
            });
        }
        let deferred: Vec<_> = self
            .inner
            .deferred_wakes
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect();
        for id in deferred {
            if self.inner.paused.lock().unwrap().contains(&id)
                || self.inner.delivery.is_cleaning(&id)
                || self
                    .inner
                    .live
                    .get(&id)
                    .is_some_and(|live| live.running.load(Ordering::SeqCst))
            {
                continue;
            }
            let pending = self.inner.inboxes.lock().unwrap().get(&id).map(|inbox| {
                (
                    inbox
                        .pending
                        .values()
                        .any(|(_, _, source)| crate::lifecycle::is_completion_source(source)),
                    inbox.pending.keys().next_back().copied(),
                )
            });
            let runnable = match pending {
                Some((completion, seq)) if completion => {
                    // 与 `resume_pending` 同一口径:同一批待收消息已经尝试过
                    // 就回到普通上限,不做无界空转。
                    seq.is_some_and(|seq| {
                        self.inner
                            .completion_marks
                            .lock()
                            .unwrap()
                            .get(&id)
                            .copied()
                            != Some(seq)
                    })
                }
                Some(_) => {
                    self.inner
                        .wakes
                        .lock()
                        .unwrap()
                        .get(&id)
                        .copied()
                        .unwrap_or(0)
                        < self.config().max_consecutive_wakes
                }
                None => false,
            };
            if runnable {
                self.wake(&id);
            }
        }
    }

    async fn recover_deliveries(&self) -> Result<(), String> {
        let sessions = self.inner.sessions.clone();
        let entries = tokio::task::spawn_blocking(move || sessions.list())
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        for entry in entries {
            let sessions = self.inner.sessions.clone();
            let id = entry.id.clone();
            let (pending, last_reason) = tokio::task::spawn_blocking(move || {
                let mut pending = HashSet::new();
                let mut last_reason = None;
                sessions
                    .for_each_event(&id, |event| {
                        match event.event {
                            SessionEvent::AgentInbox { id, .. } => {
                                pending.insert(id);
                            }
                            SessionEvent::AgentDelivery { id, .. } => {
                                pending.remove(&id);
                            }
                            SessionEvent::TurnEnd { reason, .. } => last_reason = Some(reason),
                            _ => {}
                        }
                        Ok(())
                    })
                    .map(|_| (!pending.is_empty(), last_reason))
            })
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
            if matches!(last_reason, Some(TurnEndReason::Aborted { .. })) {
                self.inner.paused.lock().unwrap().insert(entry.id.clone());
            }
            if pending {
                let live = self.live(&entry.id).await?;
                let mut inboxes = self.inner.inboxes.lock().unwrap();
                Self::sync_inbox(inboxes.entry(entry.id.clone()).or_default(), &live.session);
                drop(inboxes);
                self.wake(&entry.id);
            }
            let child = self.inner.children.lock().unwrap().get(&entry.id).cloned();
            if let Some(child) = child
                && child.descriptor.mode != "memory"
            {
                let live = self.live(&child.id).await?;
                let events = live.session.events_after(0);
                let input_seq = events.iter().find_map(|event| matches!(&event.event,
                    SessionEvent::AgentInbox { source, .. } | SessionEvent::AgentDelivery { source, .. }
                        if source.starts_with("agent:")).then_some(event.seq)).unwrap_or(0);
                for (index, event) in events.iter().enumerate() {
                    if event.seq > input_seq && matches!(event.event, SessionEvent::TurnEnd { .. })
                    {
                        self.queue_completion(&child, &live.session, &events[..=index], false);
                    }
                }
            }
            tokio::task::yield_now().await;
        }
        self.retry_deliveries().await;
        Ok(())
    }
}
