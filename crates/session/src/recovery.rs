//! Session recovery responsibilities.
use super::*;

/// 读取 `rewinds.jsonl` 里累计的报废 revision 集合。
///
/// 为什么是它:`session.jsonl` 被 rewind **物理截断**,被截掉的身份在日志里
/// 已经查不到;`rewinds.jsonl` 与它同级、只追加、不被截断,是同目录里唯一
/// 活得过截断与重启的载体。所以回退记录里带上"这次截掉了哪些身份",加载时
/// 取并集 —— 选择写在这里是为了让下次改回退审计的人知道它有下游读者。
///
/// 边界:审计文件成了折叠作用域的一个输入,文件被删或被手改,报废集合就只剩
/// 进程内积累的那部分(保护退化,不报错,也不挡住旧日志的读取)。所以解析
/// 失败一律按空集处理 —— 审计文件不是真值来源,读不到只表示"没有已知报废
/// 身份"。
fn read_retired_revisions(file: &Path) -> Vec<RevisionId> {
    let path = file.with_file_name("rewinds.jsonl");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut out: Vec<RevisionId> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(items) = record.get("retiredRevisions").and_then(|v| v.as_array()) else {
            continue;
        };
        for item in items {
            if let Some(id) = item.as_str()
                && !out.iter().any(|known| known.as_str() == id)
            {
                out.push(RevisionId::new(id));
            }
        }
    }
    out
}

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
        session.load_history_projection()?;
        session.close_orphaned_turn()?;
        session.close_orphaned_interactions()?;
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
        let session = Session {
            header,
            file: file.to_path_buf(),
            inner: Mutex::new(inner),
        };
        // 冷态同样读投影文件:它只影响模型面派生，不依赖事件是否驻留；
        // 之后 ensure_hot 会走 load() 再读一次（幂等）。
        session.load_history_projection()?;
        Ok(session)
    }

    /// 读取 `<session-dir>/history-projection.json`（不存在则保持 None）。
    pub(super) fn load_history_projection(&self) -> Result<(), SessionError> {
        let path = self.file.with_file_name(HISTORY_PROJECTION_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(SessionError::Io(error)),
        };
        let projection: history::HistoryProjection = serde_json::from_str(&text)
            .map_err(|e| SessionError::Corrupt(format!("bad history projection: {e}")))?;
        if projection.version != history::LEGACY_HISTORY_PROJECTION_VERSION {
            return Err(SessionError::Corrupt(format!(
                "unsupported history projection version {}",
                projection.version
            )));
        }
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        inner.history_projection = Some(projection);
        inner.derived_surface = None;
        Ok(())
    }

    /// 建立并持久化历史投影（旧子代理首次继续前调用）。
    ///
    /// 幂等：已有同版本投影则直接复用（返回 `false`），在线与冷恢复一致。
    /// 返回 `true` 表示本次新建并落盘。
    pub fn apply_history_projection(&self) -> Result<bool, SessionError> {
        if self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .history_projection
            .is_some()
        {
            return Ok(false);
        }
        let projection = self.with_events(|events| history::legacy_subagent_drop_set(events));
        let path = self.file.with_file_name(HISTORY_PROJECTION_FILE);
        let text = serde_json::to_string_pretty(&projection)
            .map_err(|e| SessionError::Corrupt(e.to_string()))?;
        std::fs::write(&path, text)?;
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        inner.history_projection = Some(projection);
        inner.derived_surface = None;
        inner.log_revision += 1;
        Ok(true)
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
        // 任务账本折叠所需的极小切片:只放折叠器真正读的事件,口径只有一处
        // (`task_projection::task_fold_input`)—— `SessionEvent::Task`,以及写入类
        // 工具的 `ToolCall`(只留目标路径,参数正文不进内存)。冷态不驻留事件,
        // 但折叠需要完整的任务脉络;热态也只保留这一类,于是冷/热两条路径折出的
        // 账本逐字段相同 —— parity 由"两条路径用同一份切片"保证,而不是靠两处都
        // 记得改。
        let mut task_events: Vec<SessionEnvelope> = Vec::new();
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
                    if let Some(reduced) = crate::task_projection::task_fold_input(&envelope) {
                        task_events.push(reduced);
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
        // 任务账本折叠(冷热同一份输入,见上面的 `task_events`)。报废身份
        // 来自 `rewinds.jsonl`:被截断掉的 revision 在日志里已经查不到。
        let retired_revisions = read_retired_revisions(file);
        let task = project_task_scoped(
            &task_events,
            &task_fold_scope(&header.id, &retired_revisions),
        );
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
            task,
            retired_revisions,
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
            history_projection: None,
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

    /// 未决交互的收敛：`ask-requested` 没有 `ask-resolved`、`approval-asked`
    /// 没有 `approval-decided` 时补一条终局事件。
    ///
    /// 应答通道（oneshot）只活在内存里：进程重启或会话重新加载后，日志里的卡片
    /// 依旧存在，但已经不可能被结算。没有这一步，UI 会一直显示一张"能点提交、
    /// 提交必然失败"的卡片（规格 10.3）。收敛写成 `unavailable` 并带上可执行的
    /// 原因，模型与用户都能看到明确结局。
    ///
    /// 幂等：收敛后日志里已有终局事件，第二次加载不会再补。
    pub(super) fn close_orphaned_interactions(&self) -> Result<(), SessionError> {
        let (pending_asks, pending_approvals, last_time) = {
            let inner = self
                .inner
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let mut asked: Vec<String> = Vec::new();
            let mut answered: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut approval_asked: Vec<String> = Vec::new();
            let mut approval_settled: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            let mut last_time: u64 = 0;
            for envelope in &inner.events {
                last_time = envelope.time;
                match &envelope.event {
                    SessionEvent::AskRequested { request_id, .. } => {
                        asked.push(request_id.clone());
                    }
                    SessionEvent::AskResolved { request_id, .. } => {
                        answered.insert(request_id.clone());
                    }
                    SessionEvent::ApprovalAsked { request_id, .. } => {
                        approval_asked.push(request_id.clone());
                    }
                    SessionEvent::ApprovalDecided { request_id, .. } => {
                        approval_settled.insert(request_id.clone());
                    }
                    _ => {}
                }
            }
            let pending_asks: Vec<String> = asked
                .into_iter()
                .filter(|request_id| !answered.contains(request_id))
                .collect();
            let pending_approvals: Vec<String> = approval_asked
                .into_iter()
                .filter(|request_id| !approval_settled.contains(request_id))
                .collect();
            (pending_asks, pending_approvals, last_time)
        };
        if pending_asks.is_empty() && pending_approvals.is_empty() {
            return Ok(());
        }
        for request_id in pending_asks {
            self.append_with_time(
                SessionEvent::AskResolved {
                    request_id,
                    resolution: AskResolution {
                        outcome: AskOutcome::Unavailable,
                        answers: Vec::new(),
                        reason: Some(ORPHAN_INTERACTION_REASON.to_string()),
                    },
                },
                last_time,
            )?;
        }
        for request_id in pending_approvals {
            self.append_with_time(
                SessionEvent::ApprovalDecided {
                    request_id,
                    outcome: ApprovalOutcome::Unavailable,
                },
                last_time,
            )?;
        }
        Ok(())
    }
}

/// 未决交互在加载期收敛时给出的原因（模型与 UI 共用同一句话）。
pub const ORPHAN_INTERACTION_REASON: &str =
    "该交互请求在服务重启或会话重新加载前没有被应答,应答通道已失效;请重新发起操作或询问用户。";

/// 未决交互收敛的回归网：重启后不能留下"可提交但必然失败"的卡片。
#[cfg(test)]
mod interaction_tests {
    use super::*;

    fn setup(name: &str) -> (PathBuf, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("denia-recovery-{name}-{}", uuid::Uuid::new_v4()));
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        (root, work)
    }

    fn event(value: serde_json::Value) -> SessionEvent {
        serde_json::from_value(value).expect("测试事件形状合法")
    }

    fn ask_requested(id: &str) -> SessionEvent {
        event(serde_json::json!({
            "type": "ask-requested",
            "request_id": id,
            "call_id": "call-1",
            "questions": [{"id": "q1", "question": "继续吗"}],
            "timeout_ms": 60_000
        }))
    }

    fn approval_asked(id: &str) -> SessionEvent {
        event(serde_json::json!({
            "type": "approval-asked",
            "request_id": id,
            "call_id": "call-2",
            "tool": "write_file",
            "args_preview": "{}"
        }))
    }

    /// 未应答的提问与审批在加载时各补一条终局事件（unavailable）。
    #[test]
    fn load_settles_orphaned_interactions() {
        let (root, work) = setup("settle");
        let store = crate::SessionStore::open(&root).unwrap();
        let session = store.create(&work, true).unwrap();
        session.append(ask_requested("ask-1")).unwrap();
        session.append(approval_asked("appr-1")).unwrap();
        session.flush().unwrap();
        let path = session.file().to_path_buf();
        drop(session);

        let loaded = Session::load(&path).unwrap();
        let events = loaded.events();
        let ask_resolution = events.iter().find_map(|item| match &item.event {
            SessionEvent::AskResolved {
                request_id,
                resolution,
            } if request_id == "ask-1" => Some(resolution.clone()),
            _ => None,
        });
        let resolution = ask_resolution.expect("未决提问必须被收敛");
        assert_eq!(resolution.outcome, AskOutcome::Unavailable);
        assert!(resolution.reason.is_some());
        assert!(
            events.iter().any(|item| matches!(
                &item.event,
                SessionEvent::ApprovalDecided {
                    request_id,
                    outcome: ApprovalOutcome::Unavailable,
                } if request_id == "appr-1"
            )),
            "未决审批必须被收敛"
        );

        // 幂等：再次加载不再追加新的终局事件。
        let before = events.len();
        drop(loaded);
        let reloaded = Session::load(&path).unwrap();
        assert_eq!(
            reloaded.events().len(),
            before,
            "第二次加载不应再补终局事件"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// 已经结算过的交互不会被二次收敛。
    #[test]
    fn already_settled_interactions_are_left_alone() {
        let (root, work) = setup("settled");
        let store = crate::SessionStore::open(&root).unwrap();
        let session = store.create(&work, true).unwrap();
        session.append(ask_requested("ask-1")).unwrap();
        session
            .append(event(serde_json::json!({
                "type": "ask-resolved",
                "request_id": "ask-1",
                "resolution": {"outcome": "answered", "answers": [{"id": "q1", "custom": "好"}]}
            })))
            .unwrap();
        session.append(approval_asked("appr-1")).unwrap();
        session
            .append(event(serde_json::json!({
                "type": "approval-decided",
                "request_id": "appr-1",
                "outcome": "allowed-once"
            })))
            .unwrap();
        session.flush().unwrap();
        let path = session.file().to_path_buf();
        let before = session.events().len();
        drop(session);

        let loaded = Session::load(&path).unwrap();
        assert_eq!(loaded.events().len(), before, "不得改写已结算的交互");
        assert_eq!(
            loaded
                .events()
                .iter()
                .filter(|item| matches!(item.event, SessionEvent::AskResolved { .. }))
                .count(),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// 冷载切片必须带上**写入类工具调用**:否则热态折出"结论被后来的写入推翻",
    /// 冷载却折出"验收通过" —— 同一条日志两个结局。
    #[test]
    fn cold_load_sees_the_write_tool_calls_that_invalidate_a_conclusion() {
        use denia_core::task::{
            CheckRun, FileFingerprint, RevisionId, TaskOp, TaskOutcome, ValidationResult,
        };

        let (root, work) = setup("task-writes");
        let store = crate::SessionStore::open(&root).unwrap();
        let session = store.create(&work, true).unwrap();
        let id = session.id().to_string();
        let result = ValidationResult::from_check_runs(
            RevisionId::new("rev-a"),
            vec![CheckRun {
                command: "cargo test".into(),
                exit_code: Some(0),
                expect_exit_code: 0,
                workdir: ".".into(),
                fingerprints: vec![FileFingerprint {
                    path: "src/lib.rs".into(),
                    digest: "d1".into(),
                }],
            }],
            Some(id.clone()),
            2_000,
        )
        .expect("有检查运行才构成结论");
        session
            .append(SessionEvent::Task {
                op: TaskOp::RecordValidation { result },
            })
            .unwrap();
        // 结论之后补一次 edit:热态立刻失效;冷载必须折出同一份账本。
        session
            .append(SessionEvent::ToolCall {
                turn: 1,
                step: 1,
                call_id: "call-1".into(),
                name: "edit".into(),
                arguments: serde_json::json!({
                    "path": "src/lib.rs",
                    "old_string": "a",
                    "new_string": "b",
                    "content": "x".repeat(4_000),
                })
                .to_string(),
            })
            .unwrap();
        let hot = session.task().expect("账本在");
        assert_eq!(hot.outcome(), TaskOutcome::Unverified, "热态:写入推翻结论");
        session.flush().unwrap();
        drop(session);

        let cold = store.open_cold(&id).unwrap();
        assert!(!cold.is_hot());
        assert_eq!(
            cold.task().as_ref(),
            Some(&hot),
            "冷热折出的账本必须逐字段相同"
        );
        assert_eq!(
            cold.task().map(|state| state.outcome()),
            Some(TaskOutcome::Unverified),
            "冷载也必须看到这次写入"
        );
        drop(cold);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
