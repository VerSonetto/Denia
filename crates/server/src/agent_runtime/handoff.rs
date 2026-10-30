use super::*;

/// 子代理终态的三分类(宿主读子会话终态事件 + 任务账本算出)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalOutcome {
    /// 本轮正常结束。**注意**:它不等于验收通过 —— 宿主证据是否支持见
    /// [`TerminalState::host_verified`]。
    Completed,
    /// 失败:本轮以错误 / 死循环 / 输出预算耗尽结束,或账本上的结论是验收失败。
    Failed,
    /// 没有正常终态(被取消 / 中断 / 无终态事件):需要父代理处置。
    NeedsDecision,
}

impl TerminalOutcome {
    /// 同一套字面量:JSON 枚举名与通知文本共用一个来源,不各写一份。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::NeedsDecision => "needs_decision",
        }
    }
}

/// 终态的依据:把"子代理自己说完成"与"宿主证据说了算"分开。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalBasis {
    /// 宿主证据支持:账本上绑当前 revision 的验收结论是通过。
    HostVerified,
    /// 只有子代理自己的轮次终态(它最后那句话);宿主拿不到可核验的结论。
    SelfReported,
    /// 本轮被取消 / 中断(含崩溃孤儿轮次的合成闭合)。
    Interrupted,
    /// 子会话里没有终态事件。
    NoTerminalEvent,
}

impl TerminalBasis {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HostVerified => "host_verified",
            Self::SelfReported => "self_reported",
            Self::Interrupted => "interrupted",
            Self::NoTerminalEvent => "no_terminal_event",
        }
    }
}

/// 子代理的终态:枚举 + 依据 + 原因 + 一句人读交代。
///
/// `outcome == completed` 与 `host_verified == true` 是两件事:前者可能是子代理
/// 自称的("这轮没发工具调用"),后者要求宿主账本上绑当前 revision 的验收结论
/// 是通过。两者的差别必须能一眼看出来,否则"自称完成"就会被当成验收通过。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalState {
    pub outcome: TerminalOutcome,
    pub basis: TerminalBasis,
    /// 子会话轮次终态的中文标签(如"已完成");`None` = 没有终态事件。
    pub reason: Option<String>,
    /// 宿主证据是否支持"完成"。只有账本上绑当前 revision 的验收结论是通过时
    /// 才为 `true`。
    pub host_verified: bool,
    /// `false` = 子代理仍在运行,以下终态取自当前日志快照,不是最终结果。
    pub finalized: bool,
    /// 一句人读交代。
    pub detail: String,
}

/// 结构化的子代理用量(provider 精确 usage 累计),不是拼进文本的字符串。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffUsage {
    pub uncached_input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    /// 计费口径的全量(五分量之和)。
    pub total_tokens: u64,
}

/// 子代理交接信息。
///
/// **单一计算来源**:完成通知(注入父会话的那条 `AgentDelivery` 文本)与
/// `wait_agent` 的返回对象都从这一份结构渲染(见 [`subagent_handoff`])。两处各
/// 算一份必然会漂移 —— 父代理在通知里读到的字段与查询到的字段必须逐项相等。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentHandoff {
    /// 子会话 id。
    pub id: String,
    /// 展示名(定义名优先,回退标签)。
    pub name: String,
    /// profile qualifiedId;`-` = 旧描述符没有 profile。
    pub profile: String,
    pub terminal: TerminalState,
    pub usage: HandoffUsage,
    /// 子代理改动过的文件;口径见 [`SubagentHandoff::changed_paths_note`]。
    pub changed_paths: Vec<String>,
    /// `changedPaths` 的口径,以及"为什么可能是空的"。
    pub changed_paths_note: String,
    /// 子会话产生的产物引用。
    pub artifacts: Vec<ArtifactRef>,
    /// 任务账本的验证状态;`None` = 拿不到可用结论(没有账本,或账本上没跑过
    /// / 跑过但结论已失效)。**复用** `core::task::VerificationState`,不另造一套。
    pub verification: Option<VerificationState>,
    /// 验证面的人读交代(拿不到结论时说清原因)。
    pub verification_note: String,
    /// 父代理必须看到的提醒(未验证 / 验收失败);为空 = 确有可核验的通过结论。
    pub advisories: Vec<String>,
}

impl SubagentHandoff {
    /// 交接字段表(对象):`wait_agent` 的返回**直接用这一份**,不手抄一份键表
    /// —— 手抄的键表会与通知文本漂移,而这里就是同源的接缝。
    pub fn fields(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({}))
    }

    /// 完成通知正文(父会话里那条 `AgentDelivery` 的文本)。
    ///
    /// 所有字段都从本结构取:通知里报的用量、终态、变更文件必须与 `wait_agent`
    /// 返回的 JSON 逐项一致。`body` / `truncated` / `cancelled_note` 由调用方
    /// 提供(摘要正文、截断说明、被取消的后台工作说明)。
    pub fn notice_text(&self, body: &str, truncated: &str, cancelled_note: &str) -> String {
        let changed = if self.changed_paths.is_empty() {
            "（无）".to_string()
        } else {
            self.changed_paths.join("、")
        };
        let artifacts = if self.artifacts.is_empty() {
            "（无）".to_string()
        } else {
            self.artifacts
                .iter()
                .map(|item| format!("{}（{}）", item.output_id, item.tool))
                .collect::<Vec<_>>()
                .join("、")
        };
        let reason = self
            .terminal
            .reason
            .clone()
            .unwrap_or_else(|| "未知（没有终态事件）".to_string());
        let mut text = format!(
            "[子代理执行结束] {name}\n\
             子代理：{id}（{profile}）\n\
             结果：{reason}\n\
             终态：{outcome}（依据 {basis}，宿主验证 {verified}{provisional}）—— {detail}\n\
             本次子代理用量：{total} tokens（输入 {input} / 输出 {output} / 缓存读 {cache_read} / 推理 {reasoning}）\n\
             变更文件：{changed}\n\
             产物：{artifacts}\n\
             验证：{verification}\n\
             结果引用：会话 {id} 的日志与产物（read_tool_output 只能读本会话，父代理需要全文时用 wait_agent 查看该子代理）",
            name = self.name,
            id = self.id,
            profile = self.profile,
            outcome = self.terminal.outcome.as_str(),
            basis = self.terminal.basis.as_str(),
            verified = if self.terminal.host_verified {
                "有"
            } else {
                "无"
            },
            provisional = if self.terminal.finalized {
                ""
            } else {
                "，非最终"
            },
            detail = self.terminal.detail,
            total = self.usage.total_tokens,
            input = self.usage.uncached_input_tokens,
            output = self.usage.output_tokens,
            cache_read = self.usage.cache_read_tokens,
            reasoning = self.usage.reasoning_tokens,
            verification = self.verification_note,
        );
        for advisory in &self.advisories {
            text.push('\n');
            text.push_str(advisory);
        }
        // 口径说明永远要写:空数组也必须能区分"没有改动"与"读不到改动"。
        text.push_str("\n变更文件口径：");
        text.push_str(&self.changed_paths_note);
        text.push('\n');
        text.push_str(body);
        text.push_str(truncated);
        text.push_str(cancelled_note);
        text
    }
}

/// 从子会话算出一份交接信息(**唯一入口**)。
///
/// `settle` 的完成通知与 `wait_agent` 的返回都调它;`running` 为 `true` 时终态
/// 只是当前快照(`finalized = false`),不能当成最终结果。
pub(super) fn subagent_handoff(
    child: Option<&Child>,
    session: &denia_session::Session,
    running: bool,
) -> SubagentHandoff {
    subagent_handoff_at(child, session, running, &subagent_events(session))
}

pub(super) fn subagent_handoff_at(
    child: Option<&Child>,
    session: &denia_session::Session,
    running: bool,
    events: &[SessionEnvelope],
) -> SubagentHandoff {
    let id = session.id().to_string();
    let (name, profile) = match child {
        Some(child) => (
            child
                .descriptor
                .name
                .clone()
                .unwrap_or_else(|| child.descriptor.label.clone()),
            child
                .descriptor
                .profile
                .as_ref()
                .map(|profile| profile.qualified_id.clone())
                .unwrap_or_else(|| "-".to_string()),
        ),
        None => (id.clone(), "-".to_string()),
    };
    // 冻结授权里有没有账本工具:决定"天然不会产生验证结论"是解释还是异常。
    let ledger_tools = child.is_some_and(|child| {
        child
            .descriptor
            .effective_tools
            .as_ref()
            .is_some_and(|tools| tools.iter().any(|name| name == "run_checks"))
    });
    let task =
        denia_session::task_projection::project_task_scoped(events, &session.task_fold_scope());
    let verification = task.as_ref().and_then(|state| match state.verification() {
        verified @ VerificationState::Verified { .. } => Some(verified),
        // 拿不到可用结论一律给 `None`:父代理要判断的是"有没有可核验的结论",
        // 失效原因放在 `verification_note` 里,不混进这个字段的语义。
        VerificationState::Unverified { .. } => None,
    });
    let verification_note = verification_note(task.as_ref(), verification.as_ref(), ledger_tools);
    // 日志取一次、三处共用(终态 / 变更文件 / 产物),不重复扫描。
    let reason = last_turn_end_reason(events);
    let terminal = terminal_state(reason.as_ref(), verification.as_ref(), running);
    // `denia-server` 不直接依赖 `denia-token-meter`:这里不写 `TurnTokenUsage`
    // 这个类型名,只读它暴露的五个分量与总数(`total()` 是计费口径的唯一来源,
    // 不在这里重算一份会漂移的总量)。
    let raw_usage = denia_session::Session::project_turn_token_usage(events);
    let usage = HandoffUsage {
        uncached_input_tokens: raw_usage.uncached_input_tokens,
        output_tokens: raw_usage.output_tokens,
        cache_read_tokens: raw_usage.cache_read_tokens,
        cache_write_tokens: raw_usage.cache_write_tokens,
        reasoning_tokens: raw_usage.reasoning_tokens,
        total_tokens: raw_usage.total(),
    };
    let (changed_paths, changed_paths_note) = changed_paths(events, session);
    let artifacts = artifacts(events);
    let advisories = advisories(&terminal, verification.as_ref(), &verification_note);
    SubagentHandoff {
        id,
        name,
        profile,
        terminal,
        usage,
        changed_paths,
        changed_paths_note,
        artifacts,
        verification,
        verification_note,
        advisories,
    }
}

/// 取子会话日志(热/冷两条路唯一的入口)。
///
/// 热会话从内存取;冷会话的事件表**不驻留**(见 `state::LiveSessions::get_or_load`
/// 的冷加载),`events_after` 会走磁盘回放。用 `events()` 会把"冷会话读不到事件"
/// 静默说成"没有改动 / 没有终态" —— 那是假话,交接信息宁可说清楚读不到。
/// 代价是一次全量克隆(冷会话再加一次日志扫描),与本路径上已有的
/// `derive_messages` 相比不是瓶颈。
pub(super) fn subagent_events(session: &denia_session::Session) -> Vec<SessionEnvelope> {
    session.events_after(0)
}

/// 子会话最近一次轮次终态(`None` = 日志里没有终态事件)。
pub(super) fn last_turn_end_reason(events: &[SessionEnvelope]) -> Option<TurnEndReason> {
    events
        .iter()
        .rev()
        .find_map(|envelope| match &envelope.event {
            SessionEvent::TurnEnd { reason, .. } => Some(reason.clone()),
            _ => None,
        })
}

/// 终态判定:账本证据优先于子代理自己的轮次终态。
pub(super) fn terminal_state(
    reason: Option<&TurnEndReason>,
    verification: Option<&VerificationState>,
    running: bool,
) -> TerminalState {
    // 结论与结局同源:账本上"可用结论"的唯一来源是宿主写入的验证结论,而
    // `TaskState::outcome` 也由它算出 —— 所以"结论通过"与"结局通过"是同一件
    // 事,不在这里再各判一遍。也正因如此,原来那条"账本标记外部受阻"的终态没有
    // 依据了(记账层删掉后,没有任何写入点能产生"受阻")。
    let verdict = match verification {
        Some(VerificationState::Verified { verdict, .. }) => Some(*verdict),
        _ => None,
    };
    // 失败型轮次终态:账本上的失败结论比子代理自己的话更硬。
    let failure_basis = if verdict == Some(ValidationVerdict::Failed) {
        TerminalBasis::HostVerified
    } else {
        TerminalBasis::SelfReported
    };
    let (outcome, basis, host_verified, detail) = match reason {
        Some(TurnEndReason::Completed) => match verdict {
            Some(ValidationVerdict::Passed) => (
                TerminalOutcome::Completed,
                TerminalBasis::HostVerified,
                true,
                "子代理正常结束，且任务账本上绑当前 revision 的验收结论是通过（验收通过）。"
                    .to_string(),
            ),
            Some(ValidationVerdict::Failed) => (
                TerminalOutcome::Failed,
                TerminalBasis::HostVerified,
                false,
                "子代理正常结束，但任务账本上绑当前 revision 的结论是验收失败。".to_string(),
            ),
            None => (
                TerminalOutcome::Completed,
                TerminalBasis::SelfReported,
                false,
                "子代理自称完成（本轮正常结束），宿主没有可核验的验证结论——这个\"完成\"只来自它自己的最后发言。"
                    .to_string(),
            ),
        },
        Some(TurnEndReason::MaxTokens) => (
            TerminalOutcome::Failed,
            failure_basis,
            false,
            "子代理输出预算耗尽，本轮没有正常结束。".to_string(),
        ),
        Some(TurnEndReason::LoopDetected { repeats }) => (
            TerminalOutcome::Failed,
            failure_basis,
            false,
            format!("宿主检测到死循环（连续 {repeats} 次重复）后强制中断本轮。"),
        ),
        Some(TurnEndReason::Error { failure }) => (
            TerminalOutcome::Failed,
            failure_basis,
            false,
            format!("子代理本轮失败：{}（{}）。", failure.message, failure.code),
        ),
        Some(TurnEndReason::Aborted { .. }) | Some(TurnEndReason::Interrupted) => (
            TerminalOutcome::NeedsDecision,
            TerminalBasis::Interrupted,
            false,
            "子代理本轮被取消 / 中断，没有正常终态，需要父代理决定是否继续。".to_string(),
        ),
        None => (
            TerminalOutcome::NeedsDecision,
            TerminalBasis::NoTerminalEvent,
            false,
            "子会话里没有终态事件，无法判定子代理是否真的结束，需要父代理确认。".to_string(),
        ),
    };
    TerminalState {
        outcome,
        basis,
        reason: reason.map(turn_end_label),
        host_verified,
        finalized: !running,
        detail: if running {
            format!("[进行中，非最终] {detail}")
        } else {
            detail
        },
    }
}

/// 验证面的人读交代。
pub(super) fn verification_note(
    task: Option<&TaskState>,
    verification: Option<&VerificationState>,
    ledger_tools: bool,
) -> String {
    match (task, verification) {
        (_, Some(VerificationState::Verified {
            revision,
            verdict,
            fingerprints,
            at,
        })) => format!(
            "{}（revision {}，冻结 {} 个输入指纹，时间戳 {}）",
            match verdict {
                ValidationVerdict::Passed => "验收通过",
                ValidationVerdict::Failed => "验收失败",
            },
            revision.as_str(),
            fingerprints.len(),
            at
        ),
        (Some(state), _) => match state.verification() {
            VerificationState::Unverified { reason } => format!(
                "无：账本在，但当前 revision 上拿不到可用结论（{}）。",
                stale_reason_label(&reason)
            ),
            VerificationState::Verified { .. } => "无：账本在，但当前 revision 上拿不到可用结论。".to_string(),
        },
        (None, _) if ledger_tools => {
            "无：该子代理的授权里含 run_checks，但账本上没有任何记录，没有可核验的验证结论。"
                .to_string()
        }
        (None, _) => "无：该子代理没有任务账本（内置的 develop / explore / verify 授权里都不含 run_checks），天然不会产生验证结论。"
            .to_string(),
    }
}

/// [`StaleReason`] 的中文口径(tools crate 里的那份是私有的,这里只给交接文案用)。
pub(super) fn stale_reason_label(reason: &StaleReason) -> String {
    match reason {
        StaleReason::NeverRun => "当前 revision 上还没跑过任何验证".to_string(),
        StaleReason::RevisionChanged => "只在别的 revision 上跑过验证".to_string(),
        StaleReason::InputsChanged { paths } => {
            format!("验证之后这些文件又被改过：{}", paths.join("、"))
        }
        StaleReason::NoCheckRun => "有记录但没有可用的检查运行（空报告）".to_string(),
        StaleReason::ForeignOrigin => "结论来自别的会话，不参与本会话的裁决".to_string(),
    }
}

/// 父代理侧的提醒:没有可核验结论或结论是失败时必须显式说出来。
///
/// 这条提醒会随完成通知落进父会话的上下文(`AgentDelivery` 的文本) —— 注入通道
/// 不变,标注加在文本里。它存在的理由很具体:子代理的"完成"只由它自己的轮次
/// 终态决定([`TurnEndReason::Completed`] 的含义是"这轮没发工具调用"),宿主侧没有
/// 验收,父代理不能据此判定验收通过。
pub(super) fn advisories(
    terminal: &TerminalState,
    verification: Option<&VerificationState>,
    verification_note: &str,
) -> Vec<String> {
    if !terminal.finalized {
        return vec![
            "[进行中] 子代理仍未结束，以下交接信息取自当前日志快照，不是最终结果。".to_string(),
        ];
    }
    match verification {
        None => vec![format!(
            "[未验证] 该子代理未产生可核验的验证结论（{verification_note}）；它的\"完成\"不等于验收通过，父代理不得据此判定完成，请自行核验（读回改动 / 跑测试 / 看子会话日志）。"
        )],
        Some(VerificationState::Verified {
            verdict: ValidationVerdict::Failed,
            ..
        }) => vec![
            "[验收失败] 宿主账本上的验证结论是验收失败，不能按已完成的子任务集成。".to_string(),
        ],
        Some(_) => Vec::new(),
    }
}

/// 归集"该子会话写过的文件"与它的口径说明。
///
/// 为什么是这个来源:宿主的变更账本(原来的 `RecordChange`)已随记账层一起删除,
/// 账本里不再有变更范围可读;文件历史后端只按会话持有 `tracked` 且不对外暴露
/// —— 所以从**子会话日志**归集:
/// `write_file` / `edit` 调用里带 `path` 且**执行成功**(对应 `ToolResult` 不是
/// 错误)的那些。被拒的调用不算改动(否则只读子代理的幻觉写也会被报成更改),
/// `bash` 的重定向 / 删除 / 脚本写入推断不出来(命令是任意程序),一律不计入
/// —— 宁可少报也不猜。
pub(super) fn changed_paths(
    events: &[SessionEnvelope],
    session: &denia_session::Session,
) -> (Vec<String>, String) {
    let cwd = PathBuf::from(session.header().cwd.clone());
    let mut paths: Vec<String> = Vec::new();
    let mut saw_log_write = false;
    {
        let mut succeeded: HashSet<&str> = HashSet::new();
        for envelope in events {
            if let SessionEvent::ToolResult {
                call_id, is_error, ..
            } = &envelope.event
                && !*is_error
            {
                succeeded.insert(call_id.as_str());
            }
        }
        for envelope in events {
            let SessionEvent::ToolCall {
                call_id,
                name,
                arguments,
                ..
            } = &envelope.event
            else {
                continue;
            };
            if !matches!(name.as_str(), "write_file" | "edit")
                || !succeeded.contains(call_id.as_str())
            {
                continue;
            }
            let Some(path) = tool_path_argument(arguments) else {
                continue;
            };
            saw_log_write = true;
            push_unique(&mut paths, display_path(&cwd, &path));
        }
    }
    let mut note = if events.is_empty() {
        "口径：会话日志为空（子会话没有任何事件）—— 无法判断落盘改动。".to_string()
    } else if saw_log_write {
        "口径：子会话日志里执行成功的 write_file / edit 调用的 path（去重）。".to_string()
    } else {
        "口径：子会话日志里没有执行成功的 write_file / edit 调用。".to_string()
    };
    note.push_str("bash 的重定向 / 删除 / 脚本写入推断不出来，不计入。");
    if paths.is_empty() {
        note.push_str("本字段为空 = 没有可识别的落盘改动，不是读取失败。");
    }
    (paths, note)
}

/// UTF-8 边界截断：预算切在多字节序列中间时回退到前一个字符边界。
pub(super) fn truncate_utf8(text: &str, budget: usize) -> &str {
    if text.len() <= budget {
        return text;
    }
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

pub(super) fn turn_end_label(reason: &denia_core::session::TurnEndReason) -> String {
    use denia_core::session::TurnEndReason;
    match reason {
        TurnEndReason::Completed => "已完成".to_string(),
        TurnEndReason::Aborted { .. } => "被取消".to_string(),
        TurnEndReason::MaxTokens => "输出预算耗尽".to_string(),
        TurnEndReason::LoopDetected { repeats } => format!("检测到死循环（连续 {repeats} 次）"),
        TurnEndReason::Interrupted => "中断".to_string(),
        TurnEndReason::Error { failure } => {
            format!("失败：{}（{}）", failure.message, failure.code)
        }
    }
}
