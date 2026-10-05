//! Session recovery responsibilities.
use super::*;

impl Session {
    /// Loads one session, repairing torn tails and orphaned turns.
    ///
    /// 流式逐行读取:不把整个日志读成 String(数百 MB 大会话会多出一份
    /// 等体量的瞬时拷贝),峰值内存 = 解析后的事件 + 一个行缓冲。
    pub fn load(file: &Path) -> Result<Session, SessionError> {
        let (header, inner) = Self::parse_file(file, true)?;
        let session = Session {
            header,
            file: file.to_path_buf(),
            inner: Mutex::new(inner),
        };
        session.close_orphaned_turn()?;
        session.refresh_meter_from_log()?;
        Ok(session)
    }

    /// 冷打开(仅浏览路径):同样完整解析一遍日志 —— meter/goal/权限/
    /// preset/标题等聚合投影照常保留 —— 但事件与 offsets **不驻留**。
    ///
    /// 内存从 O(全部事件) 降到 O(聚合);代价是失去事件 Vec,事件需求
    /// 由 [`Session::events_after`] 的磁盘回放或 [`Session::ensure_hot`]
    /// 升级满足。孤儿轮闭合是写盘修复,延迟到 ensure_hot(任何写路径都
    /// 必先升级,修复因此总在首次写入前落地)。
    pub fn open_cold(file: &Path) -> Result<Session, SessionError> {
        let (header, inner) = Self::parse_file(file, false)?;
        Ok(Session {
            header,
            file: file.to_path_buf(),
            inner: Mutex::new(inner),
        })
    }

    /// 流式解析日志:retain=true 全量驻留(热),false 只留聚合(冷)。
    pub(super) fn parse_file(
        file: &Path,
        retain: bool,
    ) -> Result<(SessionHeader, SessionInner), SessionError> {
        let mut reader = BufReader::new(File::open(file)?);
        let mut raw = String::new();

        raw.clear();
        let header_read = reader.read_line(&mut raw)?;
        if header_read == 0 {
            return Err(SessionError::Corrupt("empty session log".into()));
        }
        let header: SessionHeader = serde_json::from_str(raw.trim_end_matches(['\n', '\r']))
            .map_err(|e| SessionError::Corrupt(format!("bad header: {e}")))?;
        if header.version != SESSION_FORMAT_VERSION {
            return Err(SessionError::Corrupt(format!(
                "unsupported session format version {}",
                header.version
            )));
        }

        // Byte offsets track the committed prefix for torn-tail truncation.
        let consumed_base = header_read as u64;
        let mut consumed = consumed_base;
        let mut offsets: Vec<u64> = Vec::new();
        let mut events: Vec<SessionEnvelope> = Vec::new();
        let mut torn_at: Option<usize> = None;
        let mut last_turn = 0u32;
        let mut last_system_prompt: Option<String> = None;
        let mut permission_mode = PermissionMode::AutoEdit;
        let mut agent_preset: Option<String> = None;
        let mut goal: Option<GoalState> = None;
        let mut title: Option<String> = None;
        let mut meter = ContextMeter::new();
        let mut pending_turn: Vec<SessionEnvelope> = Vec::new();
        let mut last_seq = 0u64;
        let mut first_prompt_excerpt = None;
        // 驻留事件的内存近似:逐行累计(chunk 只在打开的轮次里驻留,
        // 加载时日志里的旧 chunk 一律不进内存,故此处只记非 chunk 行)。
        let mut resident_bytes = 0u64;
        loop {
            raw.clear();
            let read = reader.read_line(&mut raw)?;
            if read == 0 {
                break;
            }
            let line = raw.trim_end_matches(['\n', '\r']);
            if line.trim().is_empty() {
                consumed += read as u64;
                continue;
            }
            match serde_json::from_str::<SessionEnvelope>(line) {
                Ok(envelope) => {
                    let transient = is_transient_event(&envelope.event);
                    if retain && !transient {
                        offsets.push(consumed + read as u64);
                    }
                    last_seq = last_seq.max(envelope.seq);
                    match &envelope.event {
                        SessionEvent::TurnStart { turn } => last_turn = last_turn.max(*turn),
                        SessionEvent::SystemPrompt { text, .. } => {
                            last_system_prompt = Some(text.clone())
                        }
                        SessionEvent::PermissionMode { mode } => permission_mode = *mode,
                        SessionEvent::AgentPreset { preset } => agent_preset = Some(preset.clone()),
                        SessionEvent::SessionTitle { title: t } => title = Some(t.clone()),
                        SessionEvent::UserMessage { text, .. }
                            if first_prompt_excerpt.is_none() =>
                        {
                            first_prompt_excerpt = Some(excerpt_text(text, 80));
                        }
                        _ => {}
                    }
                    // meter/goal 的聚合折叠冷热一致;chunk 不进任何折叠
                    // (token-meter 与派生面都不读它),只有驻留是可选项。
                    if matches!(envelope.event, SessionEvent::TurnStart { .. }) {
                        pending_turn.clear();
                    }
                    if let Some(sample) = usage_envelope(&envelope) {
                        pending_turn.push(sample);
                    }
                    if matches!(envelope.event, SessionEvent::TurnEnd { .. }) {
                        let slice = std::mem::take(&mut pending_turn);
                        meter.fold_turn(&slice);
                    }
                    meter.apply_one(&envelope);
                    if let SessionEvent::Goal { op } = &envelope.event {
                        goal = apply_goal_op(goal, op, envelope.time, meter.turn_usage().total());
                    }
                    if retain && !transient {
                        events.push(envelope);
                        resident_bytes += read as u64;
                    }
                }
                Err(_) => {
                    if line.trim_end().ends_with('}') {
                        // Retired or unknown event types are skipped so older
                        // logs keep loading.
                        consumed += read as u64;
                        continue;
                    }
                    torn_at = Some(consumed as usize);
                    break;
                }
            }
            consumed += read as u64;
        }

        if let Some(torn) = torn_at {
            // Truncate to the last good boundary; the torn tail is discarded.
            let file_handle = OpenOptions::new().write(true).open(file)?;
            file_handle.set_len(torn as u64)?;
        }
        if !retain {
            // 冷态不驻留事件;turn 缓冲同样只服务热态的 meter 精确折叠。
            pending_turn.clear();
        }
        let inner = SessionInner {
            events,
            writer: open_append_writer(file)?,
            offsets,
            base_offset: consumed_base,
            next_offset: consumed,
            last_turn,
            last_system_prompt,
            meter,
            pending_turn,
            permission_mode,
            agent_preset,
            goal,
            title,
            derived_surface: None,
            derived_revision: 0,
            log_revision: 0,
            cold: !retain,
            last_seq,
            // 热态的事件内存近似 = 驻留各行的字节之和(与 append 同口径)。
            resident_bytes,
            transient_bytes: 0,
            first_prompt_excerpt,
        };
        Ok((header, inner))
    }

    /// Crash recovery: a `turn-start` without `turn/end` gets a synthetic
    /// interrupted close(对齐 dsh `interruptedTurnClosers`):先为已落库但
    /// 未答复的工具调用补合成错误结果,再补 `step/end`,最后补
    /// `turn/end { interrupted }`;时间戳复用最后真实事件时间戳(确定性,
    /// 不发明未来时间)。用户取消的 `aborted` 与崩溃闭合区分开。
    pub(super) fn close_orphaned_turn(&self) -> Result<(), SessionError> {
        let (open_turn, last_step, pending_calls, last_time) = {
            let inner = self
                .inner
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let mut open_turn: Option<u32> = None;
            let mut last_step: Option<u32> = None;
            // 集合化:工具调用多的会话(含压缩前的历史)用 Vec 做 contains/any
            // 是平方级扫描,而这段每次 load 会话都要跑。
            let mut answered: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut announced_ids: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            let mut announced: Vec<(String, u32, u32)> = Vec::new();
            let mut last_time: u64 = 0;
            for envelope in &inner.events {
                last_time = envelope.time;
                match &envelope.event {
                    SessionEvent::TurnStart { turn } => {
                        open_turn = Some(*turn);
                        last_step = None;
                    }
                    SessionEvent::TurnEnd { .. } => {
                        open_turn = None;
                        last_step = None;
                    }
                    SessionEvent::StepStart { turn, step } => {
                        if Some(*turn) == open_turn {
                            last_step = Some(*step);
                        }
                    }
                    SessionEvent::AssistantMessage {
                        blocks, turn, step, ..
                    } => {
                        if Some(*turn) == open_turn {
                            for block in blocks {
                                if let ContentBlock::ToolCall { id, .. } = block
                                    && announced_ids.insert(id.clone())
                                {
                                    announced.push((id.clone(), *turn, *step));
                                }
                            }
                        }
                    }
                    SessionEvent::ToolResult { call_id, .. } => {
                        answered.insert(call_id.clone());
                    }
                    _ => {}
                }
            }
            let pending_calls: Vec<(String, u32, u32)> = announced
                .into_iter()
                .filter(|(call, _, _)| !answered.contains(call))
                .collect();
            (open_turn, last_step, pending_calls, last_time)
        };
        let Some(turn) = open_turn else {
            return Ok(());
        };
        for (call_id, call_turn, call_step) in &pending_calls {
            self.append_with_time(
                SessionEvent::ToolResult {
                    turn: *call_turn,
                    step: *call_step,
                    call_id: call_id.clone(),
                    content: ORPHAN_TOOL_RESULT.to_string(),
                    is_error: true,
                    error: None,
                    error_identity: None,
                    meta: None,
                    replaces: None,
                    truncation: None,
                },
                last_time,
            )?;
        }
        if let Some(step) = last_step {
            self.append_with_time(SessionEvent::StepEnd { turn, step }, last_time)?;
        }
        self.append_with_time(
            SessionEvent::TurnEnd {
                turn,
                reason: TurnEndReason::Interrupted,
            },
            last_time,
        )?;
        Ok(())
    }
}
