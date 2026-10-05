//! Session history responsibilities.
use super::*;

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
