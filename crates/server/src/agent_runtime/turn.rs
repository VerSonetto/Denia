use super::*;

impl Runtime {
    pub fn human_turn(&self, id: &str, selection: &ModelSelection) {
        self.inner.paused.lock().unwrap().remove(id);
        self.inner
            .selections
            .lock()
            .unwrap()
            .insert(id.into(), selection.clone());
        self.inner.wakes.lock().unwrap().insert(id.into(), 0);
        // 用户的真实轮次 = 模型重新获得成功执行:goal 失败计数清零。
        self.inner.goal_failures.lock().unwrap().remove(id);
    }

    pub(super) async fn resume_pending(&self, id: &str) -> Result<(), String> {
        if self.inner.paused.lock().unwrap().contains(id) {
            return Ok(());
        }
        let live = self.live(id).await?;
        // 旧 child（本计划之前的描述符，快照版本 0）没有可复现的指令投影：
        // 它的历史里是否已混入全局 AGENTS.md 的自动注入无法按正文可靠区分
        // （计划 9.4 禁止按正文猜）。首次继续前建立**模型历史投影**并落盘：
        // 明确按事件类别排除旧父运行态与全部自动注入通道，审计 UI 仍看完整
        // 日志；投影文件使在线加载与冷恢复得到同一结果。建立失败才退回
        // "拒绝续跑 + 重新派遣"。
        if let Some(child) = live.session.header().subagent.as_ref()
            && child.snapshot_version < denia_core::subagent::SUBAGENT_SNAPSHOT_VERSION
            && live.session.history_projection().is_none()
        {
            match live.session.apply_history_projection() {
                Ok(true) => {
                    tracing::info!(session = id, "已为旧子代理建立模型历史投影");
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::error!(%error, session = id, "旧子代理历史投影建立失败");
                    self.inner.paused.lock().unwrap().insert(id.into());
                    // 单独取父 id：锁守卫不能跨 await（if-let 临时值活到块尾）。
                    let parent = self
                        .inner
                        .children
                        .lock()
                        .unwrap()
                        .get(id)
                        .map(|child| child.parent_id.clone());
                    if let Some(parent) = parent {
                        let text = format!(
                            "[子代理 {id} 无法直接继续]\n为旧子代理建立模型历史投影失败：{error}。\
                                 为不把可能含有全局规则注入的旧上下文继续送模型，已拒绝续跑。\n\
                                 处理方式：查看它的日志确认结论，然后用 spawn_agent 以自包含 prompt 重新派遣。（{}）",
                            denia_core::subagent::codes::LEGACY_RESUME_UNSUPPORTED
                        );
                        if let Err(error) = self
                            .enqueue(
                                &parent,
                                format!("legacy-resume:{id}"),
                                text,
                                "subagent-legacy".into(),
                            )
                            .await
                        {
                            tracing::error!(%error, "旧子代理迁移诊断投递失败");
                        }
                    }
                    return Ok(());
                }
            }
        }
        let (pending, completion, last_pending_seq) = {
            let mut inboxes = self.inner.inboxes.lock().unwrap();
            let inbox = inboxes.entry(id.into()).or_default();
            Self::sync_inbox(inbox, &live.session);
            (
                !inbox.pending.is_empty(),
                inbox
                    .pending
                    .values()
                    .any(|(_, _, source)| crate::lifecycle::is_completion_source(source)),
                inbox.pending.keys().next_back().copied(),
            )
        };
        if !pending {
            self.inner.deferred_wakes.lock().unwrap().remove(id);
            self.inner.completion_marks.lock().unwrap().remove(id);
            return Ok(());
        }
        if self.inner.delivery.is_cleaning(id) {
            return Ok(());
        }
        let selection = self
            .inner
            .selections
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .or_else(|| {
                live.session
                    .header()
                    .subagent
                    .as_ref()
                    .map(|s| s.selection.clone())
            })
            .or_else(|| {
                live.session.events().iter().rev().find_map(|e| {
                    if let SessionEvent::RequestHeader {
                        header: snapshot, ..
                    } = &e.event
                    {
                        Some(ModelSelection {
                            provider: snapshot.config.provider.clone(),
                            model: snapshot.config.model.clone(),
                            reasoning_effort: snapshot.config.reasoning_effort.clone(),
                        })
                    } else {
                        None
                    }
                })
            });
        let admission = self.inner.admission.enter().await;
        if self.inner.paused.lock().unwrap().contains(id) {
            return Ok(());
        }
        let Some(selection) = selection else {
            return Ok(());
        };
        // 这一批待收消息是否还没被“真正开跑”处理过：是则完成通知可以越过
        // 普通唤醒上限；否则按普通上限计，避免失败重试无界自旋。
        let completion_fresh = completion && {
            let marks = self.inner.completion_marks.lock().unwrap();
            match last_pending_seq {
                Some(seq) => marks.get(id).copied() != Some(seq),
                None => false,
            }
        };
        {
            let mut wakes = self.inner.wakes.lock().unwrap();
            let n = wakes.entry(id.into()).or_default();
            if !completion_fresh && *n >= self.config().max_consecutive_wakes {
                return Ok(());
            }
            if live
                .running
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                // 目标正在跑（典型：主代理卡在 wait_agent 等子代理结果）。这次
                // 唤醒没有别的边沿会再来,必须登记挂起,由它自己那轮结束时消费,
                // 否则消息永久躺在 inbox 里直到用户手动发消息。
                drop(wakes);
                self.defer_wake(id);
                return Ok(());
            }
            if live.session.header().subagent.is_some() {
                // 终态 child 的消息唤醒同样走统一准入（与创建共用一份槽位账）。
                let limit = self.policy().max_concurrent_runs;
                if !self.inner.admission.reserve(id, limit) {
                    live.running.store(false, Ordering::SeqCst);
                    // 槽位已满同样是"现在跑不了"而非"不需要跑"：别的子代理结束
                    // 时会走 on_idle 的全量扫描重新捞起这里的挂起项,漏登记就是
                    // 子代理补一条指令后永远收不到回复。
                    drop(wakes);
                    self.defer_wake(id);
                    return Ok(());
                }
            }
            if !completion_fresh {
                *n += 1;
            } else if let Some(seq) = last_pending_seq {
                self.inner
                    .completion_marks
                    .lock()
                    .unwrap()
                    .insert(id.into(), seq);
            }
        }
        self.inner.deferred_wakes.lock().unwrap().remove(id);
        let runtime = self.clone();
        let guard = RunningGuard::with_on_idle(
            live.clone(),
            self.inner.events.clone(),
            Arc::new(move |id| runtime.on_idle(id)),
        );
        let token = CancellationToken::new();
        *live.cancel.lock().unwrap() = Some(token.clone());
        drop(admission);
        let _ = self.inner.events.send(ServerEvent::RunningChanged {
            id: id.into(),
            running: true,
        });
        let driver = self
            .inner
            .driver
            .get()
            .and_then(Weak::upgrade)
            .ok_or("会话驱动器尚未就绪")?;
        let followers = live.followers.clone();
        let runtime = self.clone();
        let session_id = id.to_string();
        let emit: Arc<dyn Fn(&SessionEnvelope) + Send + Sync> = Arc::new(move |e| {
            runtime.observe_activity(&session_id, &e.event);
            if matches!(e.event, SessionEvent::TurnEnd { .. }) {
                runtime.capture_completion(&session_id);
            }
            let _ = followers.send(e.clone());
        });
        driver
            .run_turn(
                &live.session,
                &selection,
                "",
                Vec::new(),
                Vec::new(),
                Vec::new(),
                self.inner
                    .registry
                    .resolve_call(
                        &selection.provider,
                        &selection.model,
                        selection.reasoning_effort.as_deref(),
                    )
                    .await
                    .map(|r| r.info.input_modalities.iter().any(|m| m == "image"))
                    .unwrap_or(false),
                token,
                emit,
            )
            .await;
        drop(guard);
        Ok(())
    }
    /// 写一个 goal 操作事件(阻塞 fs → spawn_blocking)并推给 follow 订阅者。
    pub(super) async fn publish_goal(
        live: &Arc<crate::state::LiveSession>,
        op: GoalOp,
    ) -> Result<SessionEnvelope, String> {
        let owned = live.clone();
        let envelope = tokio::task::spawn_blocking(move || -> Result<SessionEnvelope, String> {
            let envelope = owned.session.apply_goal(op).map_err(|e| e.to_string())?;
            owned.session.flush().map_err(|e| e.to_string())?;
            Ok(envelope)
        })
        .await
        .map_err(|e| e.to_string())??;
        let _ = live.followers.send(envelope.clone());
        Ok(envelope)
    }

    pub(super) fn turn_failure_summary(reason: &denia_core::session::TurnEndReason) -> String {
        match reason {
            denia_core::session::TurnEndReason::Error { failure } => {
                format!("{}({})", failure.message, failure.code)
            }
            denia_core::session::TurnEndReason::LoopDetected { repeats } => {
                format!("死循环保护触发(连续重复 {repeats} 次)")
            }
            _ => "执行失败".into(),
        }
    }

    /// 最近一次 goal 状态变更之后落下的最后一条 turn 结局。
    ///
    /// 水位 = 日志里最后一条 `SessionEvent::Goal` 的 `seq`(每条 goal 操作
    /// 都落事件:set/edit/pause/resume/round…)。只有水位之后的 `TurnEnd`
    /// 才算"上一轮的结果":水位之前的结局已经被该操作消化掉了。
    ///
    /// 没有水位会读错结局——resume 之后还没有新 `TurnEnd` 时,日志里最后一条
    /// `TurnEnd` 仍是暂停前那条 `Aborted`,于是刚恢复就又落一次 Pause,恢复
    /// 永远无效(判据不变,必然复现)。冷日志(无 goal 事件)水位取 0,与
    /// 旧行为一致。
    pub(super) fn outcome_since_goal_change(events: &[SessionEnvelope]) -> Option<TurnEndReason> {
        let watermark = events
            .iter()
            .rev()
            .find_map(|e| matches!(e.event, SessionEvent::Goal { .. }).then_some(e.seq))
            .unwrap_or(0);
        events
            .iter()
            .rev()
            .take_while(|e| e.seq > watermark)
            .find_map(|e| match &e.event {
                SessionEvent::TurnEnd { reason, .. } => Some(reason.clone()),
                _ => None,
            })
    }

    /// goal 续跑入口:turn 结束(idle)或状态恢复后调用。目标 active 且
    /// 预算/轮次/失败护栏都放行时,自动发起新的 goal 轮;与 inbox 唤醒通过 running
    /// CAS 自然互斥,后到者放弃,等下一轮 on_idle 收敛。
    pub fn continue_goal(&self, id: &str) {
        let runtime = self.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            if let Err(error) = runtime.continue_goal_inner(&id).await {
                tracing::warn!(session = %id, %error, "goal 续跑检查失败");
            }
        });
    }

    pub(super) async fn continue_goal_inner(&self, id: &str) -> Result<(), String> {
        let live = self.live(id).await?;
        if live.session.header().subagent.is_some() {
            return Ok(());
        }
        // 预设关闭 goal 功能时续跑一并关闭:装配层已摘 goal 工具与纪律段,
        // 注入管线已关状态通道,这里关掉续跑状态机,三个层面读同一份声明。
        if let Some(driver) = self.inner.driver.get().and_then(|weak| weak.upgrade())
            && !driver.features_for(&live.session).goal
        {
            return Ok(());
        }
        let Some(goal) = live.session.goal() else {
            return Ok(());
        };
        if goal.status != denia_core::session::GoalStatus::Active {
            return Ok(());
        }
        let config = self.goals_config();

        // 上一轮结局分类(最近一次 goal 状态变更之后没有 turn 视为正常,
        // 直接开跑——resume 后即属此列)。
        let last_reason = live.session.with_events(Self::outcome_since_goal_change);
        match &last_reason {
            // 用户手动停止:目标自动暂停,等用户恢复——避免"停不下来"。
            Some(denia_core::session::TurnEndReason::Aborted { .. }) => {
                Self::publish_goal(&live, GoalOp::Pause).await?;
                return Ok(());
            }
            // 执行预算耗尽不是“执行失败”:它是护栏生效。用 goal 预算语义
            // 停下自动续跑(不累计连续失败、不标 blocked)——继续跑只会把
            // 同一个失效模式再烧一轮。
            Some(denia_core::session::TurnEndReason::Error { failure })
                if failure.code == denia_core::error::codes::BUDGET_EXHAUSTED =>
            {
                Self::publish_goal(&live, GoalOp::BudgetLimit).await?;
                return Ok(());
            }
            // 执行失败:计数 +1;达到上限标记 blocked,未达限等用户处理
            // (自动重试大概率再失败,烧预算)。
            Some(
                reason @ (denia_core::session::TurnEndReason::Error { .. }
                | denia_core::session::TurnEndReason::LoopDetected { .. }),
            ) => {
                let failures = {
                    let mut map = self.inner.goal_failures.lock().unwrap();
                    let count = map.entry(id.to_string()).or_default();
                    *count += 1;
                    *count
                };
                if failures >= config.max_consecutive_failures {
                    self.inner.goal_failures.lock().unwrap().remove(id);
                    let summary = Self::turn_failure_summary(reason);
                    Self::publish_goal(
                        &live,
                        GoalOp::Block {
                            reason: format!("连续 {failures} 轮执行失败,最近一次:{summary}"),
                        },
                    )
                    .await?;
                }
                return Ok(());
            }
            Some(_) => {
                self.inner.goal_failures.lock().unwrap().remove(id);
            }
            None => {}
        }

        // 预算检查:耗尽 → budget_limited + 最后一轮收尾(收尾轮结束后
        // 状态非 active,自然停止;用户提高预算会回到 active)。
        let used = live.session.goal_tokens_used().unwrap_or(0);
        let wrapup = goal.token_budget.is_some_and(|budget| used >= budget);
        if wrapup {
            Self::publish_goal(&live, GoalOp::BudgetLimit).await?;
        } else if goal.rounds_started >= config.max_rounds {
            // 轮次耗尽:状态保持 active,不再续跑(resume 被拒)。
            return Ok(());
        }

        // selection 回推(仿 resume_pending:内存优先,日志 RequestHeader 兜底)。
        let selection = self
            .inner
            .selections
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .or_else(|| {
                live.session.events().iter().rev().find_map(|e| {
                    if let SessionEvent::RequestHeader {
                        header: snapshot, ..
                    } = &e.event
                    {
                        Some(ModelSelection {
                            provider: snapshot.config.provider.clone(),
                            model: snapshot.config.model.clone(),
                            reasoning_effort: snapshot.config.reasoning_effort.clone(),
                        })
                    } else {
                        None
                    }
                })
            });
        let Some(selection) = selection else {
            return Ok(());
        };
        if live
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Ok(());
        }
        let runtime = self.clone();
        let guard = RunningGuard::with_on_idle(
            live.clone(),
            self.inner.events.clone(),
            Arc::new(move |id| runtime.on_idle(id)),
        );
        let token = CancellationToken::new();
        *live.cancel.lock().unwrap() = Some(token.clone());
        let _ = self.inner.events.send(ServerEvent::RunningChanged {
            id: id.into(),
            running: true,
        });

        // Round 事件:轮次推进落盘。续跑提示不再单独落 goal-round 消息——
        // 目标状态块(channel "goal")携带轮次与用量,每轮用量变化使注入
        // 内容自动更新一次,避免同一轮出现两条重复的"[denia 目标]"消息。
        let round = goal.rounds_started + 1;
        let _round_envelope = Self::publish_goal(&live, GoalOp::Round).await?;
        tracing::debug!(session = id, round = round, "goal 轮次推进");

        let vision_supported = self
            .inner
            .registry
            .resolve_call(
                &selection.provider,
                &selection.model,
                selection.reasoning_effort.as_deref(),
            )
            .await
            .map(|r| r.info.input_modalities.iter().any(|m| m == "image"))
            .unwrap_or(false);
        let driver = self
            .inner
            .driver
            .get()
            .and_then(Weak::upgrade)
            .ok_or("会话驱动器尚未就绪")?;
        let followers = live.followers.clone();
        let runtime = self.clone();
        let session_id = id.to_string();
        let emit: Arc<dyn Fn(&SessionEnvelope) + Send + Sync> = Arc::new(move |e| {
            runtime.observe_activity(&session_id, &e.event);
            if matches!(e.event, SessionEvent::TurnEnd { .. }) {
                runtime.capture_completion(&session_id);
            }
            let _ = followers.send(e.clone());
        });
        driver
            .run_turn(
                &live.session,
                &selection,
                "",
                Vec::new(),
                Vec::new(),
                Vec::new(),
                vision_supported,
                token,
                emit,
            )
            .await;
        drop(guard);
        Ok(())
    }

    pub fn on_idle(&self, id: &str) {
        // 轮次收尾是最后一道动作边界:即使某个异常路径漏了 StepEnd，
        // 这里也必须把模型判回空闲，否则完成结果会被永久留在等待队列。
        if let Some(live) = self.inner.live.get(id) {
            live.set_activity(ModelActivity::Idle);
            live.running.store(false, Ordering::SeqCst);
        }
        self.capture_completion(id);
        self.inner.admission.release(id);
        let aborted = self.inner.live.get(id).is_some_and(|l| {
            l.session
                .with_events(Self::outcome_since_goal_change)
                .is_some_and(|reason| matches!(reason, TurnEndReason::Aborted { .. }))
        });
        if aborted {
            self.inner.paused.lock().unwrap().insert(id.into());
        } else {
            // 这个会话刚走完一轮:自己的挂起唤醒先撤下再重推,语义是"我空闲了,
            // 现在能不能跑由 resume_pending 判定";判定不过(忙/暂停/额度)会再登记回去。
            self.inner.deferred_wakes.lock().unwrap().remove(id);
            self.wake(id);
        }
        self.inner.delivery.expedite(id);
        let queued: Vec<_> = self
            .inner
            .inboxes
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, inbox)| !inbox.pending.is_empty())
            .map(|(id, _)| id.clone())
            .collect();
        for queued_id in queued {
            if queued_id != id {
                self.wake(&queued_id);
            }
        }
        self.continue_goal(id);
    }
}
