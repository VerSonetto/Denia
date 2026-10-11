impl Runtime {
    fn capture_completion(&self, id: &str) {
        let child = self.inner.children.lock().unwrap().get(id).cloned();
        let Some(child) = child else { return; };
        if child.descriptor.mode == "memory" { return; }
        let Some(live) = self.inner.live.get(id) else { return; };
        let mut events = live.session.events_after(0);
        if !live.running.load(Ordering::SeqCst) {
            let started = events.iter().rev().find_map(|event| match event.event {
                SessionEvent::TurnStart { turn } => Some((event.seq, turn)), _ => None,
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
        if let Some(index) = events.iter().rposition(|event| matches!(event.event, SessionEvent::TurnEnd { .. })) {
            self.queue_completion(&child, &live.session, &events[..=index], true);
        }
    }

    fn queue_completion(&self, child: &Child, session: &denia_session::Session, events: &[SessionEnvelope], cleanup: bool) {
        let Some(end) = events.last() else { return; };
        let SessionEvent::TurnEnd { turn, .. } = end.event else { return; };
        // fork 种子里的父轮次不是这个子代理执行过的轮次。
        let input = events.iter().find(|event| matches!(&event.event,
            SessionEvent::AgentInbox { source, .. } | SessionEvent::AgentDelivery { source, .. }
                if source.starts_with("agent:")));
        if child.descriptor.fork.is_some() && input.is_none_or(|input| input.seq > end.seq) { return; }
        let text = events.iter().rev().find_map(|event| match &event.event {
            SessionEvent::AssistantMessage { turn: message_turn, blocks, .. } if *message_turn == turn => Some(blocks.iter().filter_map(|block| match block {
                denia_core::stream::ContentBlock::Text { text } => Some(text.as_str()), _ => None,
            }).collect::<Vec<_>>().join("\n")), _ => None,
        }).unwrap_or_default();
        let limit = (self.config().output_bytes / 4).max(512);
        let body: String = text.chars().take(limit).collect();
        let truncated = if text.chars().count() > limit {
            format!("\n（摘要已截断：子代理输出共 {} 字符，全文见子会话日志与产物。）", text.chars().count())
        } else { String::new() };
        let leftover: Vec<_> = self.inner.jobs.list(&child.id).into_iter().filter(|job| job.finished_at.is_none()).collect();
        let cancelled_note = if leftover.is_empty() { String::new() } else {
            format!("\n[已取消的子代理后台工作] 共 {} 项未完成，宿主已要求终止并清理（长时间服务应交回主代理启动）：{}",
                leftover.len(), leftover.iter().map(|job| job.label.as_str()).collect::<Vec<_>>().join("、"))
        };
        let handoff = subagent_handoff_at(Some(child), session, false, events);
        let mut summary = handoff.notice_text(&body, &truncated, &cancelled_note);
        // 交接信息本身也可能很大；不能让体积限制变成永久无法重试的通知。
        let max = self.config().output_bytes * 2;
        if summary.len() > max {
            let suffix = "\n（交接摘要已截断，完整结果见子会话日志与 wait_agent。）";
            summary = format!("{}{}", truncate_utf8(&summary, max.saturating_sub(suffix.len())), suffix);
        }
        self.inner.delivery.push(Notice {
            child: child.id.clone(), parent: child.parent_id.clone(),
            id: format!("settled:{}:turn:{turn}:seq:{}", child.id, end.seq),
            legacy_id: format!("settled:{}:{}", child.id, turn + 1), text: summary,
        }, cleanup);
    }

    fn start_delivery_worker(&self) {
        let weak = Arc::downgrade(&self.inner);
        let delivery = self.inner.delivery.clone();
        tokio::spawn(async move {
            let Some(inner) = weak.upgrade() else { return; };
            let runtime = Runtime { inner };
            if let Err(error) = runtime.recover_deliveries().await {
                tracing::error!(%error,"代理完成通知恢复失败");
            }
            drop(runtime);
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
            loop {
                tokio::select! { _ = delivery.changed.notified() => {}, _ = tick.tick() => {} }
                let Some(inner) = weak.upgrade() else { return; };
                let runtime = Runtime { inner };
                runtime.retry_deliveries().await;
            }
        });
    }

    async fn retry_deliveries(&self) {
        for notice in self.inner.delivery.due() {
            if !self.inner.sessions.exists(&notice.parent) || !self.inner.sessions.exists(&notice.child) {
                self.inner.delivery.acknowledge(&notice.id);
                tracing::warn!(parent=%notice.parent,child=%notice.child,"完成通知目标已删除，停止补投");
                continue;
            }
            let result = async {
                let child = self.live(&notice.child).await?;
                child.session.flush().map_err(|error| error.to_string())?;
                let parent = self.live(&notice.parent).await?;
                let delivered = {
                    let mut inboxes = self.inner.inboxes.lock().unwrap();
                    let inbox = inboxes.entry(notice.parent.clone()).or_default();
                    Self::sync_inbox(inbox, &parent.session);
                    inbox.seen.contains(&notice.id) || inbox.seen.contains(&notice.legacy_id)
                };
                if delivered {
                    self.wake(&notice.parent);
                } else {
                    self.enqueue(&notice.parent, notice.id.clone(), notice.text.clone(), "subagent-settled".into()).await?;
                }
                Ok::<_, String>(())
            }.await;
            match result {
                Ok(()) => self.inner.delivery.acknowledge(&notice.id),
                Err(error) => tracing::warn!(child=%notice.child,%error,"子代理结果待补投"),
            }
        }
        for (notice, child) in self.inner.delivery.claim_cleanups() {
            let runtime = self.clone();
            tokio::spawn(async move {
                use futures::FutureExt;
                let result = std::panic::AssertUnwindSafe(async {
                    runtime.inner.jobs.cancel_owner(&child).await;
                    runtime.close_owned_tabs(&child).await;
                }).catch_unwind().await;
                if result.is_err() {
                    tracing::error!(session=%child,"子代理资源清理异常退出");
                }
                runtime.inner.delivery.finish_cleanup(&notice);
                runtime.wake(&child);
            });
        }
        let deferred: Vec<_> = self.inner.deferred_wakes.lock().unwrap().iter().cloned().collect();
        for id in deferred {
            if self.inner.paused.lock().unwrap().contains(&id) || self.inner.delivery.is_cleaning(&id)
                || self.inner.live.get(&id).is_some_and(|live| live.running.load(Ordering::SeqCst)) { continue; }
            let pending = self.inner.inboxes.lock().unwrap().get(&id).map(|inbox| {
                (
                    inbox.pending.values().any(|(_, _, source)| source == "subagent-settled"),
                    inbox.pending.keys().next_back().copied(),
                )
            });
            let runnable = match pending {
                Some((completion, seq)) if completion => {
                    // 与 `resume_pending` 同一口径:同一批待收消息已经尝试过
                    // 就回到普通上限,不做无界空转。
                    seq.is_some_and(|seq| self.inner.completion_marks.lock().unwrap().get(&id).copied() != Some(seq))
                }
                Some(_) => self.inner.wakes.lock().unwrap().get(&id).copied().unwrap_or(0)
                    < self.config().max_consecutive_wakes,
                None => false,
            };
            if runnable { self.wake(&id); }
        }
    }

    async fn recover_deliveries(&self) -> Result<(), String> {
        let sessions = self.inner.sessions.clone();
        let entries = tokio::task::spawn_blocking(move || sessions.list()).await.map_err(|error| error.to_string())?.map_err(|error| error.to_string())?;
        for entry in entries {
            let sessions = self.inner.sessions.clone();
            let id = entry.id.clone();
            let (pending, last_reason) = tokio::task::spawn_blocking(move || {
                let mut pending = HashSet::new();
                let mut last_reason = None;
                sessions.for_each_event(&id, |event| {
                    match event.event {
                        SessionEvent::AgentInbox { id, .. } => { pending.insert(id); }
                        SessionEvent::AgentDelivery { id, .. } => { pending.remove(&id); }
                        SessionEvent::TurnEnd { reason, .. } => last_reason = Some(reason),
                        _ => {}
                    }
                    Ok(())
                }).map(|_| (!pending.is_empty(), last_reason))
            }).await.map_err(|error| error.to_string())?.map_err(|error| error.to_string())?;
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
            if let Some(child) = child && child.descriptor.mode != "memory" {
                let live = self.live(&child.id).await?;
                let events = live.session.events_after(0);
                let input_seq = events.iter().find_map(|event| matches!(&event.event,
                    SessionEvent::AgentInbox { source, .. } | SessionEvent::AgentDelivery { source, .. }
                        if source.starts_with("agent:")).then_some(event.seq)).unwrap_or(0);
                for (index, event) in events.iter().enumerate() {
                    if event.seq > input_seq && matches!(event.event, SessionEvent::TurnEnd { .. }) {
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
