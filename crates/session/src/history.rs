//! Session history responsibilities.
use super::*;

/// 收集一次任务操作里引用到的 revision id。
///
/// 只按"这条事件提到了哪些身份"收,不判断它是不是被折叠采纳 —— 报废集合
/// 宁多勿少:多收一个只会让"重用一个本就不该重用的 id"被拒,少收一个却会
/// 让旧分支的验证结论复活。
fn collect_revision_ids(op: &TaskOp, out: &mut Vec<RevisionId>) {
    match op {
        TaskOp::Open { revision, .. } | TaskOp::Revise { revision, .. } => {
            push_unique_revision(out, revision)
        }
        TaskOp::RecordEvidence { evidence } => push_unique_revision(out, &evidence.revision),
        TaskOp::RecordValidation { result } => push_unique_revision(out, result.revision()),
        _ => {}
    }
}

/// 追加一个尚未登记的身份(报废集合是有序并集,重复项不重复记)。
fn push_unique_revision(out: &mut Vec<RevisionId>, id: &RevisionId) {
    if !out.iter().any(|known| known == id) {
        out.push(id.clone());
    }
}

/// fork 投影版本：投影规则变化时自增，旧快照按版本解释。
pub const SUBAGENT_SEED_VERSION: u32 = 1;

/// 子代理 fork 种子：只含"重新投影后"的对话事件，以及投影审计。
#[derive(Debug, Clone, Default)]
pub struct SubagentSeed {
    /// 可回放的事件（已重编号由 `seed_from` 负责）。
    pub events: Vec<SessionEnvelope>,
    /// 截取到最后一个闭合轮次的日志序号（0 = 没有任何闭合轮次）。
    pub cut_seq: u64,
    /// 被丢弃的来源类别（审计可见，便于解释 child 为什么看不到某段父历史）。
    pub dropped: Vec<String>,
}

/// 子代理专用种子投影（**不**用于普通用户分支）。
///
/// 只保留已闭合轮次里的对话事实：
/// - 丢弃父 `SystemPrompt`、**所有** `injected: true` 的 UserMessage
///   （工作区指令/能力快照/技能目录/记忆索引/目标/运行时上下文/系统提示更新
///   等全部自动注入通道）、`RequestHeader`/`RequestContext`、`CompactionSummary`、
///   `Goal`/`PermissionMode`/`AgentPreset` 等父运行态；
/// - 丢弃 `AgentInbox`/`AgentDelivery`（父的持久收件箱与领取记录）；
/// - 丢弃审批/提问的请求与结局（子代理重新开始自己的交互记录）；
/// - 复制 `AssistantMessage` 时把用量清零：父历史的 token 不是 child 的新消耗，
///   否则 fork 后会重复记账；
/// - 不改动普通用户分支的种子语义（`seed_from` 保持原样）。
///
/// 当前正在运行的轮次（最后一个 `TurnEnd` 之后的事件）不复制，保证工具调用与
/// 结果成对。
pub fn build_subagent_seed(events: &[SessionEnvelope]) -> SubagentSeed {
    let cut = events
        .iter()
        .rposition(|envelope| matches!(envelope.event, SessionEvent::TurnEnd { .. }))
        .map_or(0, |index| index + 1);
    let closed = &events[..cut];
    let cut_seq = closed.last().map(|envelope| envelope.seq).unwrap_or(0);
    let mut dropped: Vec<String> = Vec::new();
    let mark = |name: &str, dropped: &mut Vec<String>| {
        if !dropped.iter().any(|item| item == name) {
            dropped.push(name.to_string());
        }
    };
    let mut out: Vec<SessionEnvelope> = Vec::with_capacity(closed.len());
    for envelope in closed {
        match &envelope.event {
            SessionEvent::SystemPrompt { .. } => mark("system-prompt", &mut dropped),
            SessionEvent::UserMessage { injected: true, .. } => {
                mark("workspace-and-runtime-injections", &mut dropped)
            }
            SessionEvent::CompactionSummary { .. } => mark("compaction-summary", &mut dropped),
            SessionEvent::AgentInbox { .. } | SessionEvent::AgentDelivery { .. } => {
                mark("agent-inbox", &mut dropped)
            }
            SessionEvent::Goal { .. } => mark("goal-state", &mut dropped),
            // 任务账本是父会话的验证结论与约束脉络;子代理重新开自己的账本,
            // 不继承父的未决问题、待完成与验收结论(继承过来的结论也会被
            // `TaskState::verification` 按来源挡掉,这里直接不让它进种子)。
            SessionEvent::Task { .. } => mark("task-ledger", &mut dropped),
            SessionEvent::PermissionMode { .. } => mark("permission-state", &mut dropped),
            SessionEvent::AgentPreset { .. } => mark("agent-preset", &mut dropped),
            SessionEvent::ApprovalPolicy { .. }
            | SessionEvent::ApprovalAsked { .. }
            | SessionEvent::ApprovalDecided { .. } => mark("approvals", &mut dropped),
            SessionEvent::AskRequested { .. } | SessionEvent::AskResolved { .. } => {
                mark("asks", &mut dropped)
            }
            SessionEvent::RequestHeader { .. } | SessionEvent::RequestContext { .. } => {
                mark("request-metadata", &mut dropped)
            }
            SessionEvent::SessionTitle { .. } => mark("session-title", &mut dropped),
            SessionEvent::CommandRun { .. } => mark("command-runs", &mut dropped),
            SessionEvent::AssistantChunk { .. } => mark("transient-chunks", &mut dropped),
            SessionEvent::AssistantMessage { .. } => {
                // 复制对话内容，但不复制用量：父历史的计费不是 child 的新消耗。
                let mut cloned = envelope.clone();
                if let SessionEvent::AssistantMessage { usage, .. } = &mut cloned.event {
                    *usage = None;
                }
                out.push(cloned);
            }
            _ => out.push(envelope.clone()),
        }
    }
    SubagentSeed {
        events: out,
        cut_seq,
        dropped,
    }
}

/// 旧子代理历史投影的版本：投影规则变化时自增。
pub const LEGACY_HISTORY_PROJECTION_VERSION: u32 = 1;

/// 旧子代理的**模型历史投影**（计划 9.4）。
///
/// 旧 child 的日志里混着父运行态与自动注入（父系统提示、全局/项目 AGENTS.md、
/// 能力快照、技能目录、记忆索引、目标、运行时上下文、旧压缩摘要），但旧描述符
/// 没有可复现的指令投影。为了让它能**安全继续**，首次继续前把"哪些事件不再进
/// 模型历史"落盘成一份显式清单：
///
/// - 只影响**模型面**（`derive_messages`/注入基线），审计 UI 仍看完整日志；
/// - 持久化后在线加载与冷恢复（先冷后热）得到同一投影；
/// - 与 fork 种子的区别：**保留** `AgentInbox`/`AgentDelivery`——那是这个 child
///   自己的任务与汇报通道，不是父的运行态。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HistoryProjection {
    pub version: u32,
    /// 被排除进模型历史的事件序号（升序，稳定可审计）。
    pub drop_seqs: Vec<u64>,
    /// 计算时的日志末尾序号：比它更新的事件从未在投影计算里出现。
    pub source_last_seq: u64,
    /// 被排除的来源类别（解释"这个 child 为什么看不到某段历史"）。
    pub dropped: Vec<String>,
    pub created_at: u64,
}

impl HistoryProjection {
    pub fn drops(&self, seq: u64) -> bool {
        self.drop_seqs.binary_search(&seq).is_ok()
    }
}

/// 算出旧子代理的模型历史投影选择。
///
/// 排除规则与 fork 种子同源（父系统提示、**所有** `injected: true` 的用户
/// 消息即各自动注入通道、旧压缩摘要、请求元数据、父运行态、交互记录、瞬时
/// 事件），但**不做闭合轮次截断**（继续运行要保留正在进行的轮次），也**不排除
/// AgentInbox/AgentDelivery**（child 自己的任务与汇报）。
///
/// 按内容判断的一律不做：这里只按事件类别决策，规则可复现、可审计。
pub fn legacy_subagent_drop_set(events: &[SessionEnvelope]) -> HistoryProjection {
    let mut dropped: Vec<String> = Vec::new();
    let mut mark = |name: &str, dropped: &mut Vec<String>| {
        if !dropped.iter().any(|item| item == name) {
            dropped.push(name.to_string());
        }
    };
    let mut drop_seqs: Vec<u64> = Vec::new();
    for envelope in events {
        match &envelope.event {
            SessionEvent::SystemPrompt { .. } => {
                mark("system-prompt", &mut dropped);
                drop_seqs.push(envelope.seq);
            }
            SessionEvent::UserMessage { injected: true, .. } => {
                // 这里同时覆盖"全局规则可能被写进注入块"的情形：旧日志无法
                // 区分全局与项目来源，整个自动注入通道都退出模型历史；下一步
                // 起按当前项目级规则重新注入（基线也按投影读取）。
                mark("workspace-and-runtime-injections", &mut dropped);
                drop_seqs.push(envelope.seq);
            }
            SessionEvent::CompactionSummary { .. } => {
                mark("compaction-summary", &mut dropped);
                drop_seqs.push(envelope.seq);
            }
            SessionEvent::RequestHeader { .. } | SessionEvent::RequestContext { .. } => {
                mark("request-metadata", &mut dropped);
                drop_seqs.push(envelope.seq);
            }
            SessionEvent::Goal { .. }
            | SessionEvent::PermissionMode { .. }
            | SessionEvent::AgentPreset { .. }
            | SessionEvent::ApprovalPolicy { .. } => {
                mark("parent-runtime-state", &mut dropped);
                drop_seqs.push(envelope.seq);
            }
            SessionEvent::ApprovalAsked { .. }
            | SessionEvent::ApprovalDecided { .. }
            | SessionEvent::AskRequested { .. }
            | SessionEvent::AskResolved { .. } => {
                mark("interaction-records", &mut dropped);
                drop_seqs.push(envelope.seq);
            }
            SessionEvent::SessionTitle { .. }
            | SessionEvent::CommandRun { .. }
            | SessionEvent::AssistantChunk { .. } => {
                mark("transient-or-log-only", &mut dropped);
                drop_seqs.push(envelope.seq);
            }
            _ => {}
        }
    }
    drop_seqs.sort_unstable();
    drop_seqs.dedup();
    HistoryProjection {
        version: LEGACY_HISTORY_PROJECTION_VERSION,
        drop_seqs,
        source_last_seq: events.last().map(|envelope| envelope.seq).unwrap_or(0),
        dropped,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    }
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
        // rewind 是**物理截断**:被截掉的分支在日志里不复存在,折叠层再也拒绝
        // 不了"旧 revision id 复活"。所以先把截断区间里出现过的身份收成报废
        // 集合 —— 它随回退审计落盘(见函数末尾),并在下面的回放里作为折叠
        // 作用域生效。
        let mut retired: Vec<RevisionId> = Vec::new();
        for envelope in &inner.events[target_idx..] {
            if let SessionEvent::Task { op } = &envelope.event {
                collect_revision_ids(op, &mut retired);
            }
        }
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
        inner.task = None;
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
            // 回放同样吃历史投影(判据只有 `meter_visible` 一处):重放完的
            // meter 必须与增量路径、加载路径给出同一份表面。
            let model_visible =
                crate::append::meter_visible(inner.history_projection.as_ref(), envelope.seq);
            inner.meter.apply_one_projected(envelope, model_visible);
            if let SessionEvent::Goal { op } = &envelope.event {
                let total = inner.meter.turn_usage().total();
                inner.goal = apply_goal_op(inner.goal.take(), op, envelope.time, total);
            }
        }
        inner.events = kept;
        // 报废身份并入后重放任务账本:作用域里带上被截断区间的 id,旧分支的
        // 验证结论因此不可能"复用同一个 revision id"带回新分支。集合只增不减
        // —— 已经报废的身份不会因为后来没人提它而复活。
        for id in &retired {
            push_unique_revision(&mut inner.retired_revisions, id);
        }
        inner.refold_task(&self.header.id);
        inner.last_seq = inner
            .events
            .last()
            .map(|envelope| envelope.seq)
            .unwrap_or(0);

        // 回退审计:独立于 session.jsonl 追加,物理截断不会抹掉这段记录。
        // `retiredRevisions` 只记**本次截掉的**身份(历次取并集即完整报废
        // 集合):加载时 `read_retired_revisions` 正是按并集读的。
        let rewind_file = self.file.with_file_name("rewinds.jsonl");
        let record = serde_json::json!({
            "time": now_millis(),
            "to_seq": to_seq,
            "to_message": to_message,
            "removed_events": removed_events,
            "retiredRevisions": retired
                .iter()
                .map(|id| id.as_str())
                .collect::<Vec<&str>>(),
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

/// 子代理 fork 种子的回归网：注入通道、父运行态、旧摘要与父用量都必须被
/// 挡在 child 之外，同时工具调用/结果保持成对。
#[cfg(test)]
mod seed_tests {
    use super::*;
    use denia_core::session::TurnEndReason;
    use denia_core::stream::TokenUsage;

    fn envelope(seq: u64, event: serde_json::Value) -> SessionEnvelope {
        SessionEnvelope {
            seq,
            time: 1_000 + seq,
            event: serde_json::from_value(event).expect("test event is well-formed"),
        }
    }

    fn user(seq: u64, text: &str, injected: bool) -> SessionEnvelope {
        envelope(
            seq,
            serde_json::json!({
                "type": "user-message",
                "text": text,
                "injected": injected,
                "channel": if injected { Some("workspace-instructions") } else { None },
                "images": []
            }),
        )
    }

    fn assistant(seq: u64, text: &str, usage: Option<TokenUsage>) -> SessionEnvelope {
        envelope(
            seq,
            serde_json::json!({
                "type": "assistant-message",
                "turn": 1,
                "step": 1,
                "blocks": [{"type": "text", "text": text}],
                "usage": usage,
                "interrupted": false,
                "source_event_seqs": []
            }),
        )
    }

    fn turn_end(seq: u64) -> SessionEnvelope {
        envelope(
            seq,
            serde_json::json!({"type": "turn-end", "turn": 1, "reason": {"kind": "completed"}}),
        )
    }

    /// 一次开账事件(任务账本的入口)。
    fn task_open(seq: u64) -> SessionEnvelope {
        envelope(
            seq,
            serde_json::json!({
                "type": "task",
                "op": {
                    "kind": "open",
                    "task_id": "task-sentinel",
                    "revision": "rev-sentinel",
                    "goal": "修好解析器",
                    "requirements": []
                }
            }),
        )
    }

    fn kind_of(event: &SessionEvent) -> &'static str {
        match event {
            SessionEvent::TurnStart { .. } => "turn-start",
            SessionEvent::TurnEnd { .. } => "turn-end",
            SessionEvent::StepStart { .. } => "step-start",
            SessionEvent::StepEnd { .. } => "step-end",
            SessionEvent::UserMessage { .. } => "user",
            SessionEvent::AssistantMessage { .. } => "assistant",
            SessionEvent::ToolCall { .. } => "tool-call",
            SessionEvent::ToolResult { .. } => "tool-result",
            SessionEvent::SystemPrompt { .. } => "system-prompt",
            SessionEvent::Goal { .. } => "goal",
            SessionEvent::Task { .. } => "task",
            SessionEvent::PermissionMode { .. } => "permission",
            SessionEvent::AgentPreset { .. } => "agent-preset",
            SessionEvent::AgentInbox { .. } => "agent-inbox",
            SessionEvent::AgentDelivery { .. } => "agent-delivery",
            SessionEvent::CompactionSummary { .. } => "compaction-summary",
            SessionEvent::RequestHeader { .. } => "request-header",
            SessionEvent::RequestContext { .. } => "request-context",
            SessionEvent::SessionTitle { .. } => "session-title",
            SessionEvent::CommandRun { .. } => "command-run",
            SessionEvent::ApprovalPolicy { .. } => "approval-policy",
            SessionEvent::ApprovalAsked { .. } => "approval-asked",
            SessionEvent::ApprovalDecided { .. } => "approval-decided",
            SessionEvent::AskRequested { .. } => "ask-requested",
            SessionEvent::AskResolved { .. } => "ask-resolved",
            SessionEvent::AssistantChunk { .. } => "chunk",
            SessionEvent::ToolOutputChunk { .. } => "tool-output-chunk",
            SessionEvent::RetryAttempt { .. } => "retry",
            SessionEvent::TodoWrite { .. } => "todo",
            SessionEvent::ArgsCleared { .. } => "args-cleared",
        }
    }

    #[test]
    fn seed_keeps_closed_dialogue_and_drops_every_automatic_channel() {
        let usage = TokenUsage {
            input_tokens: 9_000,
            output_tokens: 500,
            cache_read_tokens: None,
            reasoning_tokens: None,
        };
        let events = vec![
            envelope(1, serde_json::json!({"type": "turn-start", "turn": 1})),
            envelope(
                2,
                serde_json::json!({"type": "step-start", "turn": 1, "step": 1}),
            ),
            envelope(
                3,
                serde_json::json!({"type": "system-prompt", "turn": 1, "step": 1, "text": "父系统提示"}),
            ),
            user(4, "全局规则哨兵 GLOBAL-SENTINEL", true),
            user(5, "真实用户请求", false),
            assistant(6, "开始调查", Some(usage)),
            envelope(
                7,
                serde_json::json!({
                    "type": "tool-call", "turn": 1, "step": 1, "call_id": "c1",
                    "name": "read_file", "arguments": "{}"
                }),
            ),
            envelope(
                8,
                serde_json::json!({
                    "type": "tool-result", "turn": 1, "step": 1, "call_id": "c1",
                    "content": "文件内容", "is_error": false
                }),
            ),
            envelope(
                9,
                serde_json::json!({"type": "step-end", "turn": 1, "step": 1}),
            ),
            // 父会话的任务账本：子代理自己开账，不继承父的账本与验收结论。
            task_open(10),
            turn_end(11),
            // 未闭合的当前轮次：不复制。
            envelope(12, serde_json::json!({"type": "turn-start", "turn": 2})),
            user(13, "正在进行的请求", false),
        ];
        let seed = build_subagent_seed(&events);
        assert_eq!(seed.cut_seq, 11, "只截取到最后一个闭合轮次");
        let kinds: Vec<&'static str> = seed
            .events
            .iter()
            .map(|item| kind_of(&item.event))
            .collect();
        assert!(
            kinds.contains(&"user") && kinds.contains(&"assistant"),
            "闭合对话必须保留：{kinds:?}"
        );
        for forbidden in [
            "system-prompt",
            "compaction-summary",
            "request-header",
            "agent-inbox",
            "approval-asked",
            "ask-requested",
            "task",
            "chunk",
        ] {
            assert!(
                !kinds.contains(&forbidden),
                "{forbidden} 不得进入 child 种子：{kinds:?}"
            );
        }
        // 注入消息（全局规则哨兵）不得出现；真实用户消息必须保留。
        let texts: Vec<&str> = seed
            .events
            .iter()
            .filter_map(|item| match &item.event {
                SessionEvent::UserMessage { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["真实用户请求"]);
        assert!(
            !seed
                .events
                .iter()
                .any(|item| serde_json::to_string(&item.event)
                    .unwrap_or_default()
                    .contains("GLOBAL-SENTINEL")),
            "自动通道里的全局规则不得随 fork 进入 child"
        );
        // 工具调用与结果成对。
        assert_eq!(
            kinds.iter().filter(|kind| **kind == "tool-call").count(),
            kinds.iter().filter(|kind| **kind == "tool-result").count()
        );
        assert!(
            seed.dropped
                .iter()
                .any(|item| item == "workspace-and-runtime-injections")
        );
        assert!(seed.dropped.iter().any(|item| item == "system-prompt"));
        assert!(
            seed.dropped.iter().any(|item| item == "task-ledger"),
            "父会话的任务账本必须被标记为丢弃：{:?}",
            seed.dropped
        );
    }

    #[test]
    fn seed_zeroes_parent_usage_and_never_copies_a_summary() {
        let usage = TokenUsage {
            input_tokens: 12_345,
            output_tokens: 678,
            cache_read_tokens: Some(100),
            reasoning_tokens: None,
        };
        let events = vec![
            envelope(1, serde_json::json!({"type": "turn-start", "turn": 1})),
            user(2, "第一轮", false),
            assistant(3, "回答", Some(usage)),
            turn_end(4),
            envelope(
                5,
                serde_json::json!({
                    "type": "compaction-summary", "turn": 1, "step": 1,
                    "summary": "旧摘要", "replaces_from": 1, "replaces_to": 4,
                    "keep_from": 3, "pre_tokens": 900, "post_tokens": 100
                }),
            ),
            envelope(6, serde_json::json!({"type": "turn-start", "turn": 2})),
            user(7, "第二轮", false),
            assistant(8, "回答二", None),
            turn_end(9),
        ];
        let seed = build_subagent_seed(&events);
        assert_eq!(
            seed.events
                .iter()
                .filter(|item| matches!(item.event, SessionEvent::CompactionSummary { .. }))
                .count(),
            0,
            "旧压缩摘要不得复制"
        );
        assert!(seed.dropped.iter().any(|item| item == "compaction-summary"));
        for item in &seed.events {
            if let SessionEvent::AssistantMessage { usage, .. } = &item.event {
                assert!(usage.is_none(), "父历史用量必须清零，避免 fork 后重复记账");
            }
        }
        assert_eq!(
            seed.events
                .iter()
                .filter(|item| matches!(
                    &item.event,
                    SessionEvent::UserMessage {
                        injected: false,
                        ..
                    }
                ))
                .count(),
            2
        );
        assert_eq!(
            seed.events
                .iter()
                .filter(|item| matches!(item.event, SessionEvent::TurnEnd { .. }))
                .count(),
            2
        );
        assert!(seed.events.iter().any(|item| matches!(
            &item.event,
            SessionEvent::TurnEnd {
                reason: TurnEndReason::Completed,
                ..
            }
        )));
    }

    #[test]
    fn seed_of_a_session_without_closed_turns_is_empty() {
        let events = vec![
            envelope(1, serde_json::json!({"type": "turn-start", "turn": 1})),
            user(2, "进行中", false),
        ];
        let seed = build_subagent_seed(&events);
        assert!(seed.events.is_empty());
        assert_eq!(seed.cut_seq, 0);
        assert_eq!(SUBAGENT_SEED_VERSION, 1);
    }
}

/// 旧子代理的历史投影（计划 9.4）：模型面排除、审计面保留、落盘后在线与冷
/// 恢复一致。
#[cfg(test)]
mod projection_tests {
    use super::*;

    fn envelope(seq: u64, event: serde_json::Value) -> SessionEnvelope {
        SessionEnvelope {
            seq,
            time: 1_000 + seq,
            event: serde_json::from_value(event).expect("test event is well-formed"),
        }
    }

    fn legacy_log() -> Vec<SessionEnvelope> {
        vec![
            envelope(1, serde_json::json!({"type": "turn-start", "turn": 1})),
            envelope(
                2,
                serde_json::json!({"type": "system-prompt", "turn": 1, "step": 1, "text": "父系统提示"}),
            ),
            envelope(
                3,
                serde_json::json!({
                    "type": "user-message",
                    "text": "全局规则哨兵 GLOBAL-SENTINEL",
                    "injected": true,
                    "channel": "workspace-instructions",
                    "images": []
                }),
            ),
            envelope(
                4,
                serde_json::json!({
                    "type": "agent-delivery",
                    "id": "m1",
                    "text": "[父代理 委派任务]\n调查登录流程",
                    "source": "agent:parent"
                }),
            ),
            envelope(
                5,
                serde_json::json!({
                    "type": "assistant-message",
                    "turn": 1,
                    "step": 1,
                    "blocks": [{"type": "text", "text": "开始调查"}],
                    "usage": null,
                    "interrupted": false,
                    "source_event_seqs": []
                }),
            ),
            envelope(
                6,
                serde_json::json!({
                    "type": "compaction-summary",
                    "turn": 1,
                    "step": 1,
                    "summary": "压缩摘要 SENTINEL-SUMMARY",
                    "replaces_from": 1,
                    "replaces_to": 4,
                    "keep_from": 5
                }),
            ),
            envelope(
                7,
                serde_json::json!({"type": "session-title", "title": "旧会话标题"}),
            ),
            envelope(
                8,
                serde_json::json!({"type": "permission-mode", "mode": "read-only"}),
            ),
            envelope(
                9,
                serde_json::json!({"type": "turn-end", "turn": 1, "reason": {"kind": "completed"}}),
            ),
        ]
    }

    #[test]
    fn drop_set_excludes_parent_state_and_keeps_the_task_channel() {
        let projection = legacy_subagent_drop_set(&legacy_log());
        // 排除：父系统提示、自动注入、旧摘要、仅日志事件、父运行态。
        for seq in [2u64, 3, 6, 7, 8] {
            assert!(projection.drops(seq), "seq {seq} 必须退出模型历史");
        }
        // 保留：child 自己的任务投递、助手消息、轮次闭合。
        for seq in [1u64, 4, 5, 9] {
            assert!(!projection.drops(seq), "seq {seq} 必须保留");
        }
        assert_eq!(projection.version, LEGACY_HISTORY_PROJECTION_VERSION);
        assert_eq!(projection.source_last_seq, 9);
        assert!(projection.dropped.contains(&"system-prompt".to_string()));
        assert!(
            projection
                .dropped
                .contains(&"workspace-and-runtime-injections".to_string())
        );
        assert!(
            projection
                .dropped
                .contains(&"compaction-summary".to_string())
        );
        assert!(
            projection
                .dropped
                .contains(&"parent-runtime-state".to_string())
        );
        assert!(
            projection
                .drop_seqs
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
    }

    /// 落盘 → 重新加载：模型面看不到旧全局规则，审计面完整保留。
    #[test]
    fn projection_persists_and_only_filters_the_model_view() {
        let root = std::env::temp_dir().join(format!("denia-projection-{}", uuid::Uuid::new_v4()));
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let store = crate::SessionStore::open(&root).unwrap();
        let session = store
            .create_subagent(
                &work,
                true,
                "parent",
                denia_core::session::SubagentDescriptor::legacy(
                    "legacy",
                    1,
                    "default",
                    denia_core::config::ModelSelection {
                        provider: "p".into(),
                        model: "m".into(),
                        reasoning_effort: None,
                    },
                    None,
                    None,
                ),
            )
            .unwrap();
        session.seed_from(&legacy_log()).unwrap();
        session.flush().unwrap();
        let path = session.file().to_path_buf();
        assert!(session.apply_history_projection().unwrap());
        // 幂等：同版本投影不重建。
        assert!(!session.apply_history_projection().unwrap());
        drop(session);

        let loaded = Session::load(&path).unwrap();
        assert!(loaded.history_projection().is_some(), "投影必须持久化");
        let model_text: String = loaded.with_model_events(|events| {
            events
                .iter()
                .filter_map(|item| match &item.event {
                    SessionEvent::UserMessage {
                        text,
                        injected: false,
                        ..
                    } => Some(text.clone()),
                    SessionEvent::UserMessage {
                        text,
                        injected: true,
                        ..
                    } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        });
        assert!(
            !model_text.contains("GLOBAL-SENTINEL"),
            "旧自动注入不得继续进模型：{model_text}"
        );
        // 旧压缩摘要同样退出模型历史（重新投影，而不是把摘要当历史继续送）。
        let summary_visible = loaded.with_model_events(|events| {
            events
                .iter()
                .any(|item| matches!(&item.event, SessionEvent::CompactionSummary { .. }))
        });
        assert!(!summary_visible, "旧压缩摘要不得继续进模型");
        // 任务投递（agent-delivery）在模型面保留：它是 child 自己的任务。
        let has_task = loaded.with_model_events(|events| {
            events
                .iter()
                .any(|item| matches!(&item.event, SessionEvent::AgentDelivery { .. }))
        });
        assert!(has_task, "child 的任务投递必须保留");
        // 审计面（with_events / 界面）仍能看到完整日志。
        let audit_has_sentinel = loaded.with_events(|events| {
            events.iter().any(|item| {
                matches!(
                    &item.event,
                    SessionEvent::UserMessage { text, .. } if text.contains("GLOBAL-SENTINEL")
                )
            })
        });
        assert!(audit_has_sentinel, "审计日志必须保留原始注入");
        // 冷打开同样带上投影（在线/冷恢复一致）。
        drop(loaded);
        let cold = Session::open_cold(&path).unwrap();
        assert!(cold.history_projection().is_some(), "冷打开必须读同一投影");
        std::fs::remove_dir_all(root).unwrap();
    }
}
