//! Session append responsibilities.
use super::*;

impl Session {
    /// Appends one event, assigning contiguous seq and wall-clock time. The
    /// log lock is held only for this write, so concurrent readers observe
    /// live progress instead of waiting for the whole turn.
    ///
    /// 写入走 BufWriter:高频流式帧不逐条 flush,崩溃尾部由 torn-tail 修复。
    pub fn append(&self, event: SessionEvent) -> Result<SessionEnvelope, SessionError> {
        self.append_with_time(event, now_millis())
    }

    /// 把缓冲的日志前缀刷到磁盘(对齐 dsh checkpoint-policy 的
    /// "模型请求前"检查点):agent-loop 在模型请求 dispatched 之前调用,
    /// 保证请求前缀(step-start/system-prompt/request-header/context)已
    /// 落盘,崩溃后能完整重建请求上下文;fail-closed——flush 失败则
    /// 不派发请求。
    pub fn flush(&self) -> Result<(), SessionError> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        inner.writer.flush()?;
        Ok(())
    }

    /// [`Session::append`] 的时间显式版:分支种子回放保留源事件时间戳,
    /// 轨迹时间轴在子会话里保持真实。
    pub(super) fn append_with_time(
        &self,
        event: SessionEvent,
        time: u64,
    ) -> Result<SessionEnvelope, SessionError> {
        // 写路径强制热态:冷会话的事件 Vec 为空,seq 分配与派生面都会错。
        self.ensure_hot()?;
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // last_seq 兜底:极旧日志存在 seq 跳变/乱序(历史回退遗留),
        // 事件数可能小于最大 seq,只按 len+1 分配会撞号。
        let seq = inner.last_seq.max(inner.events.len() as u64) + 1;
        let envelope = SessionEnvelope { seq, time, event };
        let line = serde_json::to_string(&envelope)?;
        writeln!(inner.writer, "{line}")?;
        // 落盘策略:步骤边界/工具结果/todo 快照/权限切换立即 flush(耐久性
        // 边界——权限档位是安全语义,必须立即可被磁盘读者看到),流式 chunk
        // 只进 buffer,超 16KB 自动落盘(高频帧零系统调用)。
        //
        // 判据补充(界面一致性):凡是"已经完整呈现在界面上"或"会让本轮阻塞
        // 等待(等工具跑完/等用户回答)"的事件,都必须在等待开始前落到磁盘 ——
        // 断线重快照读的是磁盘分页(`store::read_page`),这类事件若还留在
        // buffer 里,一次重连就会把刚结算的正文、工具行、待答卡片从视图里
        // 抹掉,直到下一个 flush 点(ToolResult/TurnEnd)才追回来。结算消息与
        // 工具调用同理:工具执行期间磁盘尾停在模型请求前,重建出来就没有它们。
        match &envelope.event {
            SessionEvent::AssistantMessage { .. }
            | SessionEvent::ToolCall { .. }
            | SessionEvent::ApprovalAsked { .. }
            | SessionEvent::ApprovalDecided { .. }
            | SessionEvent::AskRequested { .. }
            | SessionEvent::AskResolved { .. }
            | SessionEvent::TurnEnd { .. }
            | SessionEvent::StepEnd { .. }
            | SessionEvent::ToolResult { .. }
            | SessionEvent::TodoWrite { .. }
            | SessionEvent::PermissionMode { .. }
            | SessionEvent::AgentPreset { .. }
            | SessionEvent::SessionTitle { .. }
            | SessionEvent::Goal { .. }
            | SessionEvent::Task { .. }
            | SessionEvent::CommandRun { .. } => {
                inner.writer.flush()?;
            }
            _ => {
                if inner.writer.buffered_len() >= 16 * 1024 {
                    inner.writer.flush()?;
                }
            }
        }
        let transient = is_transient_event(&envelope.event);
        let next_offset = inner.next_offset + line.len() as u64 + 1;
        inner.next_offset = next_offset;
        if !transient {
            inner.offsets.push(next_offset);
        }
        inner.last_seq = seq;
        // 维护 last_turn / last_system_prompt 的 O(1) 投影。
        match &envelope.event {
            SessionEvent::TurnStart { turn } => {
                inner.last_turn = inner.last_turn.max(*turn);
            }
            SessionEvent::SystemPrompt { text, .. } => {
                inner.last_system_prompt = Some(text.clone());
            }
            SessionEvent::PermissionMode { mode } => {
                inner.permission_mode = *mode;
            }
            SessionEvent::AgentPreset { preset } => {
                inner.agent_preset = Some(preset.clone());
            }
            SessionEvent::SessionTitle { title } => {
                inner.title = Some(title.clone());
            }
            SessionEvent::UserMessage { text, .. } if inner.first_prompt_excerpt.is_none() => {
                inner.first_prompt_excerpt = Some(excerpt_text(text, 80));
            }
            _ => {}
        }
        // 维护 token-meter:
        // 1) 每个事件都贡献 message/system 启发式 fold(apply_one 内部按角色累计)。
        // 2) `TurnStart` 重置本轮 envelope 缓冲;`TurnEnd` 闭合时把整段
        //    喂给 `meter.fold_turn`,成功则并入精确 usage 与 anchor。
        // chunk(transient)不进缓冲:token-meter 与派生面都不读它。
        if matches!(&envelope.event, SessionEvent::TurnStart { .. }) {
            inner.pending_turn.clear();
        }
        if let Some(sample) = usage_envelope(&envelope) {
            inner.pending_turn.push(sample);
        }
        if matches!(&envelope.event, SessionEvent::TurnEnd { .. }) {
            let slice = std::mem::take(&mut inner.pending_turn);
            inner.meter.fold_turn(&slice);
        }
        inner.meter.apply_one(&envelope);
        // goal 折叠放在 meter 维护之后:`Set` 的记账基数取此刻的精确累计,
        // 与 load 回放(`apply_one` 之后折叠)时序一致。
        if let SessionEvent::Goal { op } = &envelope.event {
            let total = inner.meter.turn_usage().total();
            inner.goal = apply_goal_op(inner.goal.take(), op, envelope.time, total);
        }

        // 当前步骤的 chunk 支持实时回放;消息结算或轮次闭合后只保留终稿。
        let line_bytes = line.len() as u64 + 1;
        inner.events.push(envelope.clone());
        inner.resident_bytes += line_bytes;
        if transient {
            inner.transient_bytes += line_bytes;
        }
        if matches!(
            &envelope.event,
            SessionEvent::AssistantMessage { .. } | SessionEvent::TurnEnd { .. }
        ) {
            inner.events.retain(|item| !is_transient_event(&item.event));
            inner.events.shrink_to_fit();
            // 清扫的正是本轮的 chunk,按记账扣减(饱和,防历史回退遗留的偏差)。
            inner.resident_bytes = inner.resident_bytes.saturating_sub(inner.transient_bytes);
            inner.transient_bytes = 0;
        }
        // 任务账本折叠:与 goal 同一条原则(操作即意图,状态由折叠得出),
        // 但这里是**重放整条日志**而不是状态机单步 —— 折叠器核对事实引用时
        // 要看日志里的用户原话,那一步只有整条日志才做得出来。放在事件入列
        // 之后:折叠器必须看到这条事件本身。低频事件(每个约束/换版/收口
        // 一条)承担一次重放,派生面热路径不受影响。
        if matches!(&envelope.event, SessionEvent::Task { .. }) {
            inner.refold_task(&self.header.id);
        }
        // 日志变了:派生面缓存失效,版本号自增(回退截断也走这里)。
        inner.derived_surface = None;
        inner.log_revision += 1;
        Ok(envelope)
    }
}
