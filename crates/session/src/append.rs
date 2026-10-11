//! Session append responsibilities.
use super::*;
use denia_token_meter::ProjectionStamp;

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
        // 投影晚于 meter 建立时(旧子代理首次继续)先把表面拉回同一份投影。
        self.sync_meter_projection();
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
        // 3) 占用估算必须与主请求**同一份投影**:被历史投影排除的事件(旧
        //    子代理的父运行态与自动注入)不进模型面,也不进表面计量,否则
        //    "模型看不见的历史"仍在推高占用面板与压缩闸门。判据只有一处:
        //    [`meter_visible`]。
        // 4) turn 缓冲不吃投影:usage 是账单事实(provider 真花了这些
        //    token),不是历史投影的对象;投影规则本身也不排除 usage 事件。
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
        let model_visible = meter_visible(inner.history_projection.as_ref(), envelope.seq);
        inner.meter.apply_one_projected(&envelope, model_visible);
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

    /// 把 meter 的表面拉回当前历史投影(投影晚于 meter 建立时整条重建)。
    ///
    /// 投影是**事后**建立的:旧子代理首次继续时才落盘
    /// (`Session::apply_history_projection`,server 的 resume 路径),而 meter
    /// 里已经折进了它要排除的父运行态与自动注入。没人重建的话,这些内容会
    /// 一直推高占用面板与压缩闸门,直到某次重载才消失——"只在重载后才变"
    /// 的偏差最难排查,所以补齐在这里:指纹一致时只是一次 Option 比较,
    /// 重建只发生在投影刚建立/更换的那一次。
    ///
    /// 重建走 `refresh_meter_from_log`:它自己按投影过滤,这里不另写一套判据。
    fn sync_meter_projection(&self) {
        let stale = {
            let inner = self
                .inner
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let stamp = inner.history_projection.as_ref().map(projection_stamp);
            inner.meter.folded_projection() != stamp
        };
        if stale {
            // 重建只影响面板与压缩闸门的精度,不该阻断写入;该函数目前不会
            // 失败(保留 Result 是为了与其他重建入口同形)。
            let _ = self.refresh_meter_from_log();
        }
    }
}

/// 事件是否进 token-meter 的表面(与主请求同一份历史投影)。
///
/// 喂 meter 的入口有多处(增量 append、加载/冷→热重建
/// `Session::refresh_meter_from_log`、回退重放 `Session::rewind`),它们
/// **共用这一处判据**:谁都不许自己再判一次"哪些事件进模型面",否则口径
/// 立刻分叉——而这类分叉只在重载后出现,最难排查。
///
/// 投影为 `None`(普通会话)时全部可见,不付代价。
pub(crate) fn meter_visible(projection: Option<&HistoryProjection>, seq: u64) -> bool {
    match projection {
        Some(projection) => !projection.drops(seq),
        None => true,
    }
}

/// 历史投影指纹:`(source_last_seq, drop_seqs.len())`。
///
/// meter 靠它判断"这份投影是否已经按到我的表上"(见
/// `denia_token_meter::ContextMeter::folded_projection`)。投影落盘后不可变,
/// 这两个数足够区分"还没建 / 刚建 / 换了一份"——不必克隆整份 drop_seqs
/// (热路径不付这份代价)。
pub(crate) fn projection_stamp(projection: &HistoryProjection) -> ProjectionStamp {
    (projection.source_last_seq, projection.drop_seqs.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::message::ImageData;
    use denia_token_meter::estimate_message;

    /// meter 的表面总量(占用面板与压缩闸门读的就是它,未经锚点校准)。
    fn meter_surface(session: &Session) -> u64 {
        session
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .meter
            .surface_tokens()
    }

    /// 模型面的启发式总量:`derive_surface` 派生出来的消息集合的规模。
    fn model_surface(session: &Session) -> u64 {
        session.derive_surface().iter().fold(0u64, |acc, item| {
            acc.saturating_add(estimate_message(&item.message))
        })
    }

    fn temp_session(tag: &str) -> (std::path::PathBuf, Session) {
        let root = std::env::temp_dir().join(format!("{tag}-{}", uuid::Uuid::new_v4()));
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let store = SessionStore::open(&root).unwrap();
        let session = store.create(&work, true).unwrap();
        (root, session)
    }

    fn user_event(text: &str, injected: bool) -> SessionEvent {
        SessionEvent::UserMessage {
            text: text.into(),
            injected,
            images: Vec::new(),
            channel: None,
        }
    }

    fn call_event(
        turn: u32,
        step: u32,
        call_id: &str,
        name: &str,
        arguments: String,
    ) -> SessionEvent {
        SessionEvent::AssistantMessage {
            turn,
            step,
            blocks: vec![ContentBlock::ToolCall {
                id: call_id.into(),
                name: name.into(),
                arguments,
                incomplete: false,
            }],
            usage: None,
            interrupted: false,
            source_event_seqs: Vec::new(),
            first_token_time: None,
        }
    }

    fn result_event(call_id: &str, content: String) -> SessionEvent {
        SessionEvent::ToolResult {
            turn: 1,
            step: 1,
            call_id: call_id.into(),
            content,
            is_error: false,
            error: None,
            error_identity: None,
            meta: None,
            replaces: None,
            truncation: None,
        }
    }

    /// 旧子代理的历史投影必须同时作用于模型面与占用估算:被排除的自动注入
    /// 既不进模型面,也不进表面计量。三条喂 meter 的路径(增量 append、投影
    /// 刚建立的那一次、重载)必须给出同一个表面。
    #[test]
    fn projection_filters_meter_on_append_and_reload() {
        let (root, session) = temp_session("denia-meter-projection");
        // 旧 child 的日志形态:父系统提示 + 自动注入(大块正文)+ 真实对话。
        session
            .append(SessionEvent::SystemPrompt {
                turn: 1,
                step: 1,
                text: "PARENT-SYSTEM-PROMPT".into(),
            })
            .unwrap();
        session
            .append(user_event(
                &format!("GLOBAL-SENTINEL {}", "x".repeat(4_000)),
                true,
            ))
            .unwrap();
        session.append(user_event("真实任务", false)).unwrap();
        let before = meter_surface(&session);
        assert!(before > 0, "未建投影时注入照常计价(它是那时的模型面)");

        // 建立投影 == server 的 resume 路径(`apply_history_projection`)。
        assert!(session.apply_history_projection().unwrap());
        let path = session.file().to_path_buf();
        assert!(
            meter_surface(&session) > model_surface(&session),
            "投影刚建立时 meter 还留着被排除的历史:这正是要修的那一段"
        );

        // 投影建立后的第一次写入必须已经把表面拉回同一份投影,而不是把注入
        // 一直算到某次重载才消失。
        session.append(user_event("继续", false)).unwrap();
        let after = meter_surface(&session);
        assert!(
            after < before,
            "被投影排除的注入必须退出表面计量: before={before} after={after}"
        );
        assert_eq!(
            after,
            model_surface(&session),
            "增量路径的表面必须与模型面一致"
        );

        // 重载(冷→热升级走的也是这条路径)给出同一个表面。
        drop(session);
        let loaded = Session::load(&path).unwrap();
        assert_eq!(
            meter_surface(&loaded),
            model_surface(&loaded),
            "重载后的表面必须与模型面一致"
        );
        assert_eq!(meter_surface(&loaded), after);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// 回退会让新事件复用被投影排除过的 seq(日志回退把编号退回中途,这是
    /// 既有语义)。此时"这个 seq 进不进模型面"必须由**同一处判据**决定:
    /// 模型面(`derive_surface`)与表面计量(`meter_visible`)不能各判一次,
    /// 否则占用面板会把一条模型看不见的消息算进去(或反过来)。
    #[test]
    fn projection_judgement_is_shared_by_model_surface_and_meter() {
        let (root, session) = temp_session("denia-meter-rewind");
        session.append(user_event("第一条", false)).unwrap();
        session.append(user_event("第二条", false)).unwrap();
        session.append(user_event("第三条", false)).unwrap();
        let path = session.file().to_path_buf();
        drop(session);
        // 手写一份把 seq 2 排除的投影(等价于回退**之前**算出的清单)。
        let projection = HistoryProjection {
            version: crate::LEGACY_HISTORY_PROJECTION_VERSION,
            drop_seqs: vec![2],
            source_last_seq: 3,
            dropped: vec!["runtime-injections".into()],
            created_at: 0,
        };
        std::fs::write(
            path.with_file_name(crate::HISTORY_PROJECTION_FILE),
            serde_json::to_string(&projection).unwrap(),
        )
        .unwrap();
        let session = Session::load(&path).unwrap();
        assert!(session.history_projection().unwrap().drops(2));
        // 回退到 seq 2 之前,再写一条新消息:它的编号正好是 2(复用)。
        session.rewind(2).unwrap();
        session.append(user_event("回退后的新消息", false)).unwrap();
        assert_eq!(session.events().last().unwrap().seq, 2);
        // 模型面与表面计量给出同一判断(都排除它)。
        assert!(
            !session
                .derive_messages()
                .iter()
                .any(|message| message.content.contains("回退后的新消息")),
            "模型面按同一份投影排除该 seq"
        );
        assert_eq!(
            meter_surface(&session),
            model_surface(&session),
            "表面计量不得与模型面分叉"
        );
        assert_eq!(
            meter_surface(&session),
            estimate_message(&ChatMessage::user("第一条")),
            "被排除的 seq 不进表面,保留的那条照常计价"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// 走真实 `append` 路径的跨口径回归:参数打桩(c)、带图结果并进
    /// tool-result(e)、未应答调用的合成中断结果(d)全部经过 append → meter,
    /// 每一步都必须与 `derive_surface` 一致。
    #[test]
    fn meter_follows_model_surface_through_the_append_path() {
        let (root, session) = temp_session("denia-meter-append");
        let steps = vec![
            user_event("把这段写进文件", false),
            call_event(
                1,
                1,
                "call_w",
                "write_file",
                format!("{{\"content\":\"{}\"}}", "z".repeat(8_000)),
            ),
            result_event("call_w", "已写入".into()),
            // 超长参数打桩:参数在模型面上被原位换成占位符。
            SessionEvent::ArgsCleared {
                turn: 1,
                step: 1,
                call_id: "call_w".into(),
                placeholder: "[参数已打桩:write_file 8KB]".into(),
            },
            // 新的一次调用悬着,下面那条带图消息因此不进表面。
            call_event(
                1,
                1,
                "call_b",
                "browser",
                "{\"action\":\"screenshot\"}".into(),
            ),
            SessionEvent::UserMessage {
                text: "这是截屏".into(),
                injected: false,
                images: vec![ImageData {
                    mime: "image/png".into(),
                    data: "QUJD".repeat(120),
                    path: None,
                }],
                channel: None,
            },
            // 结果一到,暂存的图并进这条 tool-result。
            result_event("call_b", "已截图".into()),
            // 最后一条调用没人应答:模型面要补合成中断结果。
            call_event(1, 1, "call_c", "bash", "{\"command\":\"sleep 1\"}".into()),
        ];
        for (index, event) in steps.into_iter().enumerate() {
            session.append(event).unwrap();
            assert_eq!(
                meter_surface(&session),
                model_surface(&session),
                "第 {index} 条之后 meter 与模型面分叉"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
