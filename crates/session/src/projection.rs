//! Session projection responsibilities.
use super::*;

impl Session {
    /// 使用与在线累计相同的计费口径，投影某个终态之前的用量。
    pub fn project_turn_token_usage(events: &[SessionEnvelope]) -> TurnTokenUsage {
        let mut meter = ContextMeter::new();
        let mut start = 0;
        for (index, envelope) in events.iter().enumerate() {
            if matches!(envelope.event, SessionEvent::TurnStart { .. }) {
                start = index;
            }
            if matches!(envelope.event, SessionEvent::TurnEnd { .. }) {
                meter.fold_turn(&events[start..=index]);
                start = index + 1;
            }
        }
        meter.turn_usage()
    }

    /// O(1):下一个 turn 号(维护的计数器,不再遍历日志)。
    pub fn next_turn_number(&self) -> u32 {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .last_turn
            + 1
    }

    /// O(1):最近一次落库的系统提示词正文。
    pub fn last_system_prompt(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .last_system_prompt
            .clone()
    }

    /// 从内存事件重建 token-meter(load 后调用一次;之后由 `append`
    /// 增量维护)。
    ///
    /// **与模型面同一份投影**:历史投影排除的事件(旧子代理的父运行态与
    /// 自动注入)不进表面计量。这里不自己判可见性,走
    /// `crate::append::meter_visible` 这一个判据。
    ///
    /// 顺序依赖(别调换):`Session::load` 先 `parse_file`(它在里面带了
    /// 一次 meter 折叠)再 `load_history_projection` 再走到这里。parse_file
    /// 那一次折叠看不到投影文件(还没读)——它靠本函数覆盖掉;若将来把本
    /// 函数提到 `load_history_projection` 之前,旧 child 的 meter 会把被
    /// 投影排除的注入重新折回表面,而且只在重载后发作。
    pub(super) fn refresh_meter_from_log(&self) -> Result<(), SessionError> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // 借日志而不是克隆:与 `events` 同时驻留一份完整副本会让加载长会话的
        // 峰值内存翻倍(实测 29MB 级会话即多出几十 MB)。`meter` 与 `events`
        // 同为 inner 的字段,拆借用取出。
        let SessionInner {
            events,
            meter,
            pending_turn,
            history_projection,
            ..
        } = &mut *inner;
        *meter = ContextMeter::new();
        pending_turn.clear();
        for envelope in events.iter() {
            if matches!(&envelope.event, SessionEvent::TurnStart { .. }) {
                pending_turn.clear();
            }
            if let Some(sample) = usage_envelope(envelope) {
                pending_turn.push(sample);
            }
            if matches!(&envelope.event, SessionEvent::TurnEnd { .. }) {
                meter.fold_turn(pending_turn);
                pending_turn.clear();
            }
            let model_visible =
                crate::append::meter_visible(history_projection.as_ref(), envelope.seq);
            meter.apply_one_projected(envelope, model_visible);
        }
        pending_turn.clear();
        // 记下“表面是按哪一份投影折的”:下一次 append 靠这个指纹判断投影是否
        // 晚于本表建立(旧子代理首次继续)而需要重建。
        meter.set_folded_projection(
            history_projection
                .as_ref()
                .map(crate::append::projection_stamp),
        );
        Ok(())
    }

    /// 更新工具声明 token(供 token-meter;server 在请求启动后通知 fold)。
    pub fn set_tools_tokens(&self, tokens: u64) {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .meter
            .set_tools_tokens(tokens);
    }

    /// 更新系统提示词 token(driver 喂 framed 版;同 provider 所见)。
    pub fn set_system_tokens(&self, tokens: u64) {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .meter
            .set_system_tokens(tokens);
    }

    /// 当前上下文 token 拆分快照(纯启发式)。
    pub fn context_breakdown(&self) -> ContextBreakdown {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .meter
            .breakdown()
    }

    /// 当前 provider 精确 usage 累计。
    pub fn turn_token_usage(&self) -> TurnTokenUsage {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .meter
            .turn_usage()
    }

    /// 当前上下文压力(锚点 + 启发式;圆环面板用此值除以窗口得到百分比)。
    pub fn context_pressure(&self) -> ContextPressure {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .meter
            .context_pressure()
    }

    /// 当前权限模式(由事件 fold,O(1),不遍历日志)。
    pub fn permission_mode(&self) -> PermissionMode {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .permission_mode
    }

    /// 当前会话的 agent preset(由事件 fold,O(1));`None` = 未指定,
    /// 按部署默认值组装。
    pub fn agent_preset(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .agent_preset
            .clone()
    }

    /// 切换会话的 agent preset:追加 agent-preset 事件(事件源折叠,
    /// O(1) 生效)。是否允许切换由调用方判定——只有尚未产出内容的会话
    /// 可以换工具面,事后换会让已记录的工具调用无工具可执行。
    pub fn set_agent_preset(&self, preset: &str) -> Result<SessionEnvelope, SessionError> {
        self.append(SessionEvent::AgentPreset {
            preset: preset.to_string(),
        })
    }

    /// 当前会话目标(goal 事件折叠,O(1));`None` = 无目标。
    pub fn goal(&self) -> Option<GoalState> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .goal
            .clone()
    }

    /// 目标记账(减法):激活后的 token 消耗 = 会话精确累计 − 激活时快照。
    /// 无目标时返回 `None`。
    pub fn goal_tokens_used(&self) -> Option<u64> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let goal = inner.goal.as_ref()?;
        Some(
            inner
                .meter
                .turn_usage()
                .total()
                .saturating_sub(goal.base_tokens),
        )
    }

    /// 写一个 goal 操作事件(事件源折叠,O(1) 生效);调用方负责在写前
    /// 校验状态转换合法(fold 端对非法转换做忽略兜底)。
    pub fn apply_goal(&self, op: GoalOp) -> Result<SessionEnvelope, SessionError> {
        self.append(SessionEvent::Goal { op })
    }

    /// 当前任务账本(任务事件折叠;`None` = 日志里没有任务事件)。
    ///
    /// 热/冷态都可用:冷加载在解析时就折好了这份聚合(见 `recovery::parse_file`),
    /// 不需要先 `ensure_hot`。
    pub fn task(&self) -> Option<TaskState> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .task
            .clone()
    }

    /// 任务折叠作用域:本会话身份 + rewind 报废的 revision。
    ///
    /// 会话内部的折叠走这里;任何在别处自己折任务账本的调用方(工具面的
    /// 账本宿主)也必须用同一个作用域 —— 否则 rewind 之后旧分支的 revision
    /// 身份会复活,父子会话的结论归属也失去判据。
    pub fn task_fold_scope(&self) -> TaskFoldScope {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        task_fold_scope(&self.header.id, &inner.retired_revisions)
    }

    /// 切换会话权限模式:追加 permission-mode 事件(事件源折叠,O(1) 生效)。
    pub fn set_permission_mode(
        &self,
        mode: PermissionMode,
    ) -> Result<SessionEnvelope, SessionError> {
        self.append(SessionEvent::PermissionMode { mode })
    }

    /// 当前会话标题(session-title 事件折叠;None = 尚未生成)。
    pub fn title(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .title
            .clone()
    }

    /// 写入会话标题:追加 session-title 事件(latest-wins,事件源折叠,
    /// O(1) 生效);append 自带落盘 flush,标题必须立即可被磁盘读者看到。
    pub fn set_title(&self, title: String) -> Result<SessionEnvelope, SessionError> {
        self.append(SessionEvent::SessionTitle { title })
    }

    /// The model-facing history projected from the log.
    pub fn derive_messages(&self) -> Vec<ChatMessage> {
        self.derive_surface()
            .iter()
            .map(|item| item.message.clone())
            .collect()
    }

    /// 派生带 seq 的表面消息(微压缩/压缩需要 seq 定位 `replaces` 目标)。
    ///
    /// 结果按日志版本号缓存并交给调用方共享:`derive_messages` 与
    /// `derive_surface` 是每 step 必跑的热路径,而事件是 append-only 的,
    /// 同一份日志派生结果恒等。缓存把"每 step 两次全量重建"降为"每个
    /// 新事件一次"。
    pub fn derive_surface(&self) -> Arc<[denia_core::session::SurfaceMessage]> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(cached) = &inner.derived_surface
            && inner.derived_revision == inner.log_revision
        {
            return cached.clone();
        }
        let projection = inner.history_projection.clone();
        let surface: Arc<[denia_core::session::SurfaceMessage]> = match &projection {
            None => denia_core::session::derive_surface(&inner.events).into(),
            // 历史投影（旧子代理）：模型面丢掉旧父运行态与自动注入，审计
            // UI 仍走原始日志。只在这条路径多一次过滤（非旧 child 不付代价）。
            Some(projection) => {
                let visible: Vec<SessionEnvelope> = inner
                    .events
                    .iter()
                    .filter(|envelope| !projection.drops(envelope.seq))
                    .cloned()
                    .collect();
                denia_core::session::derive_surface(&visible).into()
            }
        };
        inner.derived_surface = Some(surface.clone());
        inner.derived_revision = inner.log_revision;
        surface
    }

    /// 距最后一条 assistant 消息落盘过了多少分钟(微压缩的空闲触发用)。
    ///
    /// 日志里没有 assistant 消息时返回 `None`(没有"闲置"可言)。
    /// 时间戳是 epoch 毫秒;系统时钟回拨时按 0 处理,不产生负值。
    pub fn last_assistant_age_minutes(&self) -> Option<f64> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let last_ms = inner
            .events
            .iter()
            .rev()
            .find(|envelope| matches!(envelope.event, SessionEvent::AssistantMessage { .. }))
            .map(|envelope| envelope.time)?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_millis() as u64)?;
        Some(now_ms.saturating_sub(last_ms) as f64 / 60_000.0)
    }

    /// The first user prompt, trimmed, for list views.
    pub fn first_prompt_excerpt(&self, max_chars: usize) -> Option<String> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if inner.cold {
            return inner
                .first_prompt_excerpt
                .as_ref()
                .map(|text| excerpt_text(text, max_chars));
        }
        let text = inner
            .events
            .iter()
            .find_map(|envelope| match &envelope.event {
                SessionEvent::UserMessage { text, .. } => Some(text),
                _ => None,
            })?;
        Some(excerpt_text(text, max_chars))
    }
}
