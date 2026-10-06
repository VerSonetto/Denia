//! Session history responsibilities.
use super::*;

/// fork 到子代理时的历史投影结果。
#[derive(Debug, Clone, Default)]
pub struct SubagentSeed {
    /// 投影后的闭合对话事件（已重新编号前的原始 seq 顺序）。
    pub events: Vec<SessionEnvelope>,
    /// 截取到的截止 seq（最后一个闭合轮次）。
    pub cut_seq: u64,
    /// 被丢弃的来源类别（审计与迁移诊断用）。
    pub dropped: Vec<String>,
}

/// 子代理 fork 的历史投影：**只**保留闭合轮次里的普通对话内容。
///
/// 与 [`Session::seed_from`]（用户分支原样复制前缀）不同，子代理种子不能
/// 携带父会话的运行态与自动注入，否则子代理会在自己的作用域里看到：
///
/// - 用户全局 `AGENTS.md` 的注入文本（父会话的自动通道，子代理不该有）；
/// - 父 SystemPrompt / 能力快照 / 定义目录 / 技能目录 / 记忆索引；
/// - goal、权限模式、preset、审批与提问等父侧交互历史；
/// - 旧 `CompactionSummary` 及其压缩状态（区间引用在新日志里没有意义）。
///
/// 丢掉这些之后，子代理的 token-meter 与基线由新会话自己从零建立。
/// 模型历史里保留的是真实用户消息、助手回复与完整的工具调用/结果对。
pub fn build_subagent_seed(source: &[SessionEnvelope]) -> Result<SubagentSeed, String> {
    let cut = source
        .iter()
        .rposition(|envelope| matches!(envelope.event, SessionEvent::TurnEnd { .. }))
        .map(|index| index + 1)
        .unwrap_or(0);
    if cut == 0 {
        return Err(
            "subagent/no-closed-turn: 父会话没有已闭合的轮次可供 fork；\
             请改用 spawn_agent 并把需要的上下文写进 prompt。"
                .to_string(),
        );
    }
    let mut events: Vec<SessionEnvelope> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    let note_dropped = |name: &str, dropped: &mut Vec<String>| {
        if !dropped.iter().any(|item| item == name) {
            dropped.push(name.to_string());
        }
    };
    for envelope in &source[..cut] {
        let event = match &envelope.event {
            // 真实用户消息与助手回复：保留（usage 去掉，Billing 不跨会话累计）。
            SessionEvent::UserMessage {
                injected: false,
                text,
                images,
                ..
            } => SessionEvent::UserMessage {
                text: text.clone(),
                injected: false,
                channel: None,
                images: images.clone(),
            },
            SessionEvent::UserMessage { channel, .. } => {
                note_dropped(
                    channel.as_deref().unwrap_or("injected-user-message"),
                    &mut dropped,
                );
                continue;
            }
            SessionEvent::AssistantMessage {
                turn,
                step,
                blocks,
                interrupted,
                first_token_time,
                ..
            } => SessionEvent::AssistantMessage {
                turn: *turn,
                step: *step,
                blocks: blocks.clone(),
                usage: None,
                interrupted: *interrupted,
                // chunk 事件不复制，指向它们的引用一并清空。
                source_event_seqs: Vec::new(),
                first_token_time: *first_token_time,
            },
            SessionEvent::ToolCall { .. }
            | SessionEvent::ToolResult { .. }
            | SessionEvent::ArgsCleared { .. }
            | SessionEvent::TurnStart { .. }
            | SessionEvent::TurnEnd { .. }
            | SessionEvent::StepStart { .. }
            | SessionEvent::StepEnd { .. } => envelope.event.clone(),
            SessionEvent::SystemPrompt { .. } => {
                note_dropped("system-prompt", &mut dropped);
                continue;
            }
            SessionEvent::AssistantChunk { .. } => continue,
            SessionEvent::CompactionSummary { .. } => {
                note_dropped("compaction-summary", &mut dropped);
                continue;
            }
            SessionEvent::TodoWrite { .. } => {
                note_dropped("todo-write", &mut dropped);
                continue;
            }
            SessionEvent::Goal { .. } => {
                note_dropped("goal", &mut dropped);
                continue;
            }
            SessionEvent::CommandRun { .. } => {
                note_dropped("command-run", &mut dropped);
                continue;
            }
            SessionEvent::PermissionMode { .. } => {
                note_dropped("permission-mode", &mut dropped);
                continue;
            }
            SessionEvent::SessionTitle { .. } => {
                note_dropped("session-title", &mut dropped);
                continue;
            }
            SessionEvent::AgentPreset { .. } => {
                note_dropped("agent-preset", &mut dropped);
                continue;
            }
            SessionEvent::ApprovalPolicy { .. }
            | SessionEvent::ApprovalAsked { .. }
            | SessionEvent::ApprovalDecided { .. } => {
                note_dropped("approval", &mut dropped);
                continue;
            }
            SessionEvent::AskRequested { .. } | SessionEvent::AskResolved { .. } => {
                note_dropped("ask", &mut dropped);
                continue;
            }
            SessionEvent::RequestHeader { .. }
            | SessionEvent::RequestContext { .. }
            | SessionEvent::RetryAttempt { .. } => {
                note_dropped("request-metadata", &mut dropped);
                continue;
            }
            SessionEvent::AgentInbox { .. } | SessionEvent::AgentDelivery { .. } => {
                note_dropped("agent-inbox", &mut dropped);
                continue;
            }
        };
        events.push(SessionEnvelope {
            seq: envelope.seq,
            time: envelope.time,
            event,
        });
    }
    let has_conversation = events.iter().any(|envelope| {
        matches!(
            envelope.event,
            SessionEvent::UserMessage { .. } | SessionEvent::AssistantMessage { .. }
        )
    });
    if !has_conversation {
        return Err(
            "subagent/empty-projection: 父会话闭合轮次里没有可投影的对话内容；\
             请改用 spawn_agent 并把需要的上下文写进 prompt。"
                .to_string(),
        );
    }
    Ok(SubagentSeed {
        events,
        cut_seq: source[cut - 1].seq,
        dropped,
    })
}

impl Session {
    /// 分支种子回放:把源日志前缀原样写入新会话(重新从 1 连续编号,
    /// 保留源事件时间戳),last_turn/last_system_prompt/token-meter 随
    /// `append_with_time` 一致重建。回放后新会话 `derive_messages` 与
    /// 源会话前缀逐字一致——model-visible == logged 的不变量不破坏。
    pub fn seed_from(&self, source: &[SessionEnvelope]) -> Result<(), SessionError> {
        for envelope in source {
            self.append_with_time(envelope.event.clone(), envelope.time)?;
        }
        Ok(())
    }

    /// 本会话的子代理运行快照(普通会话与旧日志都没有)。
    /// 快照含角色补充提示正文(上限 64 KiB),因此它落在会话目录的
    /// [`denia_core::subagent::SNAPSHOT_FILE`] 里,不进会话头——头部只留引用,
    /// 冷扫描与会话列表都不会把这段正文读出来。引用不存在或校验失败时
    /// 返回 `None`,由装配层拒绝启动而不是回退默认。
    pub fn subagent_snapshot(&self) -> Option<denia_core::subagent::SubagentSnapshotFile> {
        let path = self
            .directory()
            .join(denia_core::subagent::SNAPSHOT_FILE);
        let text = std::fs::read_to_string(path).ok()?;
        let file: denia_core::subagent::SubagentSnapshotFile = serde_json::from_str(&text).ok()?;
        (file.hash() == file.profile.hash).then_some(file)
    }

    /// 物理回退:截断会话日志到目标用户消息**之前**,并把回退审计追加到
    /// `rewinds.jsonl`。内存事件/token-meter/计数器同步重建。
    ///
    /// 这是破坏性操作:目标 seq 之后的事件从磁盘移除,不可恢复。
    pub fn rewind(&self, to_seq: u64) -> Result<RewindOutcome, SessionError> {
        // 破坏性写路径强制热态:截断依赖完整事件与 offsets 对齐。
        self.ensure_hot()?;
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // seq 定位:事件表不驻留 chunk(seq 有空洞),下标 != seq-1。
        if to_seq == 0 {
            return Err(SessionError::NotARewindPoint(to_seq));
        }
        let target_idx = inner
            .events
            .partition_point(|envelope| envelope.seq < to_seq);
        let target = match inner.events.get(target_idx) {
            Some(envelope) if envelope.seq == to_seq => envelope,
            _ => return Err(SessionError::NotARewindPoint(to_seq)),
        };
        let to_message = match &target.event {
            SessionEvent::UserMessage {
                text,
                injected: false,
                ..
            } => Some(text.clone()),
            _ => return Err(SessionError::NotARewindPoint(to_seq)),
        };

        let removed_events = inner.events.len() - target_idx;
        // 先 flush,确保所有已写事件落盘;再按最后一个保留事件的字节偏移截断。
        inner.writer.flush()?;
        let truncate_offset = if target_idx == 0 {
            inner.base_offset
        } else {
            *inner
                .offsets
                .get(target_idx - 1)
                .ok_or(SessionError::NotARewindPoint(to_seq))?
        };
        {
            let fh = OpenOptions::new().write(true).open(&self.file)?;
            fh.set_len(truncate_offset)?;
        }

        // 内存与派生状态回退。
        inner.events.truncate(target_idx);
        inner.events.shrink_to_fit();
        inner.offsets.truncate(target_idx);
        inner.offsets.shrink_to_fit();
        inner.next_offset = truncate_offset;
        // 驻留字节随截断重算:offsets 只登记非 chunk 事件的行尾偏移,
        // 差值即这些事件在内存里的近似体积。chunk 在轮次闭合时已清扫,
        // 被截断的区间里也不含驻留的 chunk,因此这里不需要 transient 账。
        inner.resident_bytes = if target_idx == 0 {
            0
        } else {
            inner
                .offsets
                .get(target_idx - 1)
                .copied()
                .unwrap_or(0)
                .saturating_sub(inner.base_offset)
        };
        inner.transient_bytes = 0;
        // 派生面缓存随截断作废(版本号自增,即使截断后长度恰好等于某个
        // 旧缓存时的长度,也不会误命中)。
        inner.derived_surface = None;
        inner.log_revision += 1;
        inner.last_turn = 0;
        inner.last_system_prompt = None;
        inner.permission_mode = PermissionMode::AutoEdit;
        inner.goal = None;
        inner.meter = ContextMeter::new();
        inner.pending_turn.clear();
        inner.first_prompt_excerpt = None;
        // 取出日志就地回放(而不是克隆一份):29MB 级的长会话下,这份副本
        // 是回退时峰值内存的主要来源。回放完放回原位。
        let kept = std::mem::take(&mut inner.events);
        for envelope in &kept {
            match &envelope.event {
                SessionEvent::TurnStart { turn } => inner.last_turn = inner.last_turn.max(*turn),
                SessionEvent::SystemPrompt { text, .. } => {
                    inner.last_system_prompt = Some(text.clone())
                }
                SessionEvent::PermissionMode { mode } => {
                    inner.permission_mode = *mode;
                }
                SessionEvent::AgentPreset { preset } => {
                    inner.agent_preset = Some(preset.clone());
                }
                _ => {}
            }
            if matches!(&envelope.event, SessionEvent::TurnStart { .. }) {
                inner.pending_turn.clear();
            }
            if let SessionEvent::UserMessage { text, .. } = &envelope.event
                && inner.first_prompt_excerpt.is_none()
            {
                inner.first_prompt_excerpt = Some(excerpt_text(text, 80));
            }
            if let Some(sample) = usage_envelope(envelope) {
                inner.pending_turn.push(sample);
            }
            if matches!(&envelope.event, SessionEvent::TurnEnd { .. }) {
                let slice = std::mem::take(&mut inner.pending_turn);
                inner.meter.fold_turn(&slice);
            }
            inner.meter.apply_one(envelope);
            if let SessionEvent::Goal { op } = &envelope.event {
                let total = inner.meter.turn_usage().total();
                inner.goal = apply_goal_op(inner.goal.take(), op, envelope.time, total);
            }
        }
        inner.events = kept;
        inner.last_seq = inner
            .events
            .last()
            .map(|envelope| envelope.seq)
            .unwrap_or(0);

        // 回退审计:独立于 session.jsonl 追加,物理截断不会抹掉这段记录。
        let rewind_file = self.file.with_file_name("rewinds.jsonl");
        let record = serde_json::json!({
            "time": now_millis(),
            "to_seq": to_seq,
            "to_message": to_message,
            "removed_events": removed_events,
        });
        {
            let mut fh = OpenOptions::new()
                .create(true)
                .append(true)
                .open(rewind_file)?;
            writeln!(fh, "{record}")?;
        }

        Ok(RewindOutcome {
            to_seq,
            to_message,
            removed_events,
        })
    }
}
