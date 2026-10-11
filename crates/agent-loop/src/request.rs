//! 请求派发:attempt 重试环 + 失败分流 + 请求头落盘 + 压缩闸门。
//!
//! 一次 step 的模型请求全流程:
//! 1. 请求头/路由元数据按需落盘(快照相等则跳过,dsh 对齐);
//! 2. 压缩闸门:高压力时 LLM 总结压缩(失败熔断,不阻断主流程);
//! 3. flush 检查点(fail-closed:请求前缀没落盘就不发请求);
//! 4. attempt 循环:建流(select 包裹,取消即刻断)→ 流消费(逐 chunk
//!    落盘)→ 失败分流(可重试退避重试 / 可自纠注入反馈 / 终止并生成
//!    用户可读的中文摘要)。

use std::sync::Arc;

use denia_core::error::LlmFailure;
use denia_core::session::{AbortCause, RequestHeaderReason, SessionEvent, TurnEndReason};
use denia_core::stream::{ContentBlock, FinishReason, StreamChunk, TokenUsage};
use denia_core::tool::ToolSchema;
use denia_llm::GenerateRequest;
use futures::StreamExt;

use crate::SessionDriver;
use crate::compact::should_compact;
use crate::errors::{MAX_FEEDBACK, feedback_eligible, feedback_text, summarize_failure};
use crate::flush_before_dispatch;
use crate::{TurnState, append, build_header_snapshot};

/// 一次成功 step 的模型输出。
pub(crate) struct StepOutput {
    pub blocks: Vec<ContentBlock>,
    pub finish: Option<FinishReason>,
}

/// dispatch 的三种去向:
/// - `Step`:模型消息已落盘,主循环继续(死循环检测 → 救援 → 工具执行);
/// - `RestartStep`:自纠反馈已注入,直接开新 step(旧 step 已闭合);
/// - `TurnEnded`:轮次已闭合(StepEnd/TurnEnd 均已落盘),主循环直接返回。
pub(crate) enum RequestOutcome {
    Step {
        blocks: Vec<ContentBlock>,
        finish: Option<FinishReason>,
    },
    TurnEnded(TurnEndReason),
    RestartStep,
}

/// 流失败后的重试决策。
///
/// 与 [`denia_llm::retry::with_retry`] 同语义地把"能不能重试"与"等多久"
/// 收在一个判定里:`RetryAfterTooLong` 明确表示**放弃重试**,调用方不得
/// 兜成 0 毫秒——provider 要求等更久时立刻重发只会再撞同一堵墙。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryDecision {
    /// 等待指定毫秒后重发。
    Wait(u64),
    /// 错误码不在可重试集。
    NotRetryable,
    /// 重试预算已耗尽。
    BudgetExhausted,
    /// provider 要求的等待超过本地退避上限 → 放弃重试。
    RetryAfterTooLong,
}

/// 流中断/失败后的重试决策。
fn stream_retry_decision(
    policy: &denia_llm::RetryPolicy,
    failure: &LlmFailure,
    attempts_used: u32,
) -> RetryDecision {
    if !policy.is_retryable(&failure.code) {
        return RetryDecision::NotRetryable;
    }
    if attempts_used >= policy.max_retries {
        return RetryDecision::BudgetExhausted;
    }
    let attempt = attempts_used + 1;
    let connection = policy.is_connection_error(&failure.code);
    match policy.delay_ms_for(attempt, failure.provider_retry_after_ms, connection) {
        Some(delay) => RetryDecision::Wait(delay),
        None => RetryDecision::RetryAfterTooLong,
    }
}

/// 派发一次模型请求并返回去向。`Err` 只保留给 append/存储类硬失败
/// (轮次裸开,由 load 合成闭合);所有模型侧失败都转为 `TurnEnded`。
pub(crate) async fn dispatch_request(
    driver: &SessionDriver,
    state: &mut TurnState,
    step: u32,
    framed_system: &str,
    tools: &[ToolSchema],
) -> Result<RequestOutcome, LlmFailure> {
    state.output_budget = driver
        .registry
        .output_budget(&state.selection.provider, &state.selection.model, None)
        .await
        .map_err(|error| error.failure)?;
    log_request_headers(state, step, framed_system, tools)?;
    // 先做廉价的工具结果微压缩(不调模型),再做压力驱动的 LLM 摘要。
    // 顺序:先轻量清理,再重量摘要。
    // 轻量清理能把压力降下来,很多时候就不必动用昂贵的摘要了。
    run_microcompact_gate(driver, state, step).await;
    run_compaction_gate(driver, state, step, framed_system, tools, false).await;

    let mut request = GenerateRequest {
        model: state.selection.model.clone(),
        reasoning_effort: state.selection.reasoning_effort.clone(),
        messages: state.session.derive_messages(),
        system: Some(framed_system.to_string()),
        tools: tools.to_vec(),
        temperature: None,
        max_tokens: Some(state.output_budget),
        stop: Vec::new(),
    };
    let retry_sink: Option<denia_llm::RetrySink> = Some(Arc::new({
        let session = state.session.clone();
        let emit = state.emit.clone();
        let turn = state.turn;
        move |attempt: &denia_llm::RetryAttempt| {
            // 重试轨迹落盘(对齐 dsh llm-retry 事件化):失败不阻断主流程。
            if let Err(error) = append(
                &session,
                &emit,
                SessionEvent::RetryAttempt {
                    turn,
                    step,
                    attempt: attempt.attempt,
                    code: attempt.code.clone(),
                    message: attempt.message.clone(),
                    delay_ms: attempt.delay_ms,
                },
            ) {
                tracing::warn!(
                    session_id = session.id(),
                    error_code = %error.code,
                    "retry attempt append failed"
                );
            }
        }
    }));

    // —— attempt 循环:dsh 对齐的 step 内重试 ——
    // setup 失败:registry 内部已按 RetryPolicy 退避重试(maxRetries=5),
    // 耗尽后分流——模型输出问题注入自纠反馈;提供方/配置问题直接 error
    // 终止(dsh 语义:setup 失败不进重试环)。
    // finish 错误(可重试码、预算内、未取消)→ 同 step 内退避重试。
    let retry_policy = denia_llm::RetryPolicy::default();
    let mut step_retries: u32 = 0;
    let mut context_recovered = false;
    // 本 step 里第几次建流:建立失败、上下文超限强压、replay 降级都会回到
    // 循环头重发,那是一次**新的逻辑调用**(也真的是新的一次计费)。预算
    // 去重键带上它,才不会把重发误判成“重复调用”。
    let mut dispatch_sequence: u32 = 0;
    'attempts: loop {
        // 请求前检查点(对齐 dsh checkpoint-policy):请求前缀刷盘
        // 成功才派发;失败 fail-closed(不发出请求)。
        flush_before_dispatch(&state.session)?;
        // 请求建立期同样响应取消:网关/代理黑洞挂起时,点停止必须立刻能断。
        let attempt_request = request.clone();
        dispatch_sequence = dispatch_sequence.saturating_add(1);
        let gate = crate::BudgetGate(state.budget.clone());
        let ticket = denia_llm::RequestTicket::new(
            &gate,
            denia_llm::RequestCall {
                session: state.session.id().to_string(),
                turn: state.turn,
                step,
                kind: if is_continuation_step(state) {
                    denia_llm::RequestKind::Continuation
                } else {
                    denia_llm::RequestKind::UserStep
                },
                sequence: dispatch_sequence,
                attempt: 0,
            },
        );
        let stream_setup = driver.registry.stream_with_replay_admitted(
            &state.selection.provider,
            &attempt_request,
            retry_sink.clone(),
            &state.replay,
            Some(&ticket),
        );
        tokio::pin!(stream_setup);
        let mut stream = tokio::select! {
            biased;
            _ = state.cancel.cancelled() => {
                return close_aborted(state, step);
            }
            result = &mut stream_setup => match result {
                Ok(stream) => stream,
                Err(error) => {
                    let failure = error.failure.clone();
                    if failure.code == denia_core::error::codes::CONTEXT_WINDOW_EXCEEDED && !context_recovered {
                        context_recovered = true;
                        append(&state.session, &state.emit, SessionEvent::RetryAttempt { turn: state.turn, step, attempt: 1, code: failure.code.clone(), message: "输入上下文超限，尝试本逻辑步骤唯一一次强制压缩；关闭、失败或断路时停止。".into(), delay_ms: 0 })?;
                        if run_compaction_gate(driver, state, step, framed_system, tools, true).await {
                            request.messages = state.session.derive_messages();
                            continue 'attempts;
                        }
                    }
                    append(
                        &state.session,
                        &state.emit,
                        SessionEvent::StepEnd { turn: state.turn, step },
                    )?;
                    if feedback_eligible(&failure.code) && state.feedback < MAX_FEEDBACK {
                        state.feedback += 1;
                        append(
                            &state.session,
                            &state.emit,
                            SessionEvent::UserMessage {
                                text: feedback_text(&failure),
                                injected: true,
                                channel: Some("feedback".into()),
                                images: Vec::new(),
                            },
                        )?;
                        return Ok(RequestOutcome::RestartStep);
                    }
                    return Ok(RequestOutcome::TurnEnded(close_error(state, step, failure, 0)));
                }
            },
        };

        // —— 流消费:每个 chunk 先落盘再处理 ——
        let mut blocks: Vec<ContentBlock> = Vec::new();
        let mut source_event_seqs: Vec<u64> = Vec::new();
        let mut usage: Option<TokenUsage> = None;
        let mut finish: Option<FinishReason> = None;
        let mut stream_error: Option<LlmFailure> = None;
        let mut interrupted = false;
        // 首个 token 帧的落盘时刻:首字延迟与解码吞吐的锚点。框架帧
        // (block-start 等)不算 token,锚定第一个真正的输出增量。
        let mut first_token_time: Option<u64> = None;
        loop {
            let next = tokio::select! {
                biased;
                _ = state.cancel.cancelled() => {
                    interrupted = true;
                    break;
                }
                item = stream.next() => item,
            };
            match next {
                Some(Ok(chunk)) => {
                    let envelope = append(
                        &state.session,
                        &state.emit,
                        SessionEvent::AssistantChunk {
                            turn: state.turn,
                            step,
                            chunk: chunk.clone(),
                        },
                    )?;
                    source_event_seqs.push(envelope.seq);
                    if first_token_time.is_none() && chunk.is_token_delta() {
                        first_token_time = Some(envelope.time);
                    }
                    match chunk {
                        StreamChunk::BlockEnd { block, .. } => blocks.push(block),
                        StreamChunk::Usage { usage: next_usage } => usage = Some(next_usage),
                        StreamChunk::Finish { reason } => finish = Some(reason),
                        _ => {}
                    }
                }
                Some(Err(failure)) => {
                    stream_error = Some(failure);
                    break;
                }
                None => break,
            }
        }

        if interrupted {
            append(
                &state.session,
                &state.emit,
                SessionEvent::AssistantMessage {
                    turn: state.turn,
                    step,
                    blocks: blocks.clone(),
                    usage,
                    interrupted: true,
                    source_event_seqs: source_event_seqs.clone(),
                    first_token_time,
                },
            )?;
            return close_aborted(state, step);
        }

        // 错误统一分流:流错误 或 finish { Error | Aborted }。
        let failure = stream_error.clone().or_else(|| match &finish {
            Some(FinishReason::Error { failure }) | Some(FinishReason::Aborted { failure }) => {
                Some(failure.clone())
            }
            _ => None,
        });
        if let Some(failure) = failure {
            let route = driver
                .registry
                .route_identity(&state.selection.provider, &request.model);
            if state.replay.downgrade(&route, &request, &failure) {
                append(
                    &state.session,
                    &state.emit,
                    SessionEvent::RetryAttempt {
                        turn: state.turn,
                        step,
                        attempt: 1,
                        code: denia_llm::REASONING_REJECTED.into(),
                        message: format!(
                            "接口拒绝历史思考，仅本 turn 移除思考重试：{}",
                            failure.message
                        ),
                        delay_ms: 0,
                    },
                )?;
                continue 'attempts;
            }
            if failure.code == denia_core::error::codes::CONTEXT_WINDOW_EXCEEDED
                && !context_recovered
            {
                context_recovered = true;
                append(
                    &state.session,
                    &state.emit,
                    SessionEvent::RetryAttempt {
                        turn: state.turn,
                        step,
                        attempt: 1,
                        code: failure.code.clone(),
                        message: "输入上下文超限，尝试本逻辑步骤唯一一次强制压缩。".into(),
                        delay_ms: 0,
                    },
                )?;
                if run_compaction_gate(driver, state, step, framed_system, tools, true).await {
                    request.messages = state.session.derive_messages();
                    continue 'attempts;
                }
            }
            let has_chunks = !source_event_seqs.is_empty();
            // 可重试:finish 错误与流错误都重试(dsh:llm-retry 丢弃失败
            // 尝试的 chunk)。重放 LLM 请求无副作用:半成品 chunk 留在日志
            // 但不进派生历史,工具尚未执行;失败尝试的部分输出由新 attempt
            // 的 blocks 整体取代。曾限制"仅无输出时重试",实测被网关断流
            // 掐死的收尾步白白死亡——有输出重放是安全的,故取消该限制。
            let cancelled = state.cancel.is_cancelled();
            let decision = stream_retry_decision(&retry_policy, &failure, step_retries);
            let retry_delay = match decision {
                RetryDecision::Wait(delay) if !cancelled => Some(delay),
                _ => None,
            };
            if let Some(delay) = retry_delay {
                step_retries += 1;
                state.step_retries = step_retries;
                append(
                    &state.session,
                    &state.emit,
                    SessionEvent::RetryAttempt {
                        turn: state.turn,
                        step,
                        attempt: step_retries,
                        code: failure.code.clone(),
                        message: failure.message.clone(),
                        delay_ms: delay,
                    },
                )?;
                let sleep = tokio::time::sleep(std::time::Duration::from_millis(delay));
                tokio::select! {
                    biased;
                    // 取消压倒重试(dsh:signal.abort 优先于 retry)。
                    _ = state.cancel.cancelled() => {}
                    _ = sleep => {}
                }
                if state.cancel.is_cancelled() {
                    if has_chunks {
                        append(
                            &state.session,
                            &state.emit,
                            SessionEvent::AssistantMessage {
                                turn: state.turn,
                                step,
                                blocks: blocks.clone(),
                                usage,
                                interrupted: true,
                                source_event_seqs: source_event_seqs.clone(),
                                first_token_time,
                            },
                        )?;
                    }
                    return close_aborted(state, step);
                }
                continue 'attempts;
            }
            if decision == RetryDecision::RetryAfterTooLong && !cancelled {
                // 不能零延迟重发:provider 明确要求等更久,等待上限是本地
                // 保护(与 llm::retry::with_retry 同语义)。
                tracing::warn!(
                    session_id = state.session.id(),
                    attempt = step_retries + 1,
                    code = %failure.code,
                    retry_after_ms = ?failure.provider_retry_after_ms,
                    "provider retry-after exceeds max delay; giving up retry"
                );
            }
            // 不可重试/预算耗尽:失败尝试不产出终稿消息(dsh:流错误
            // rethrow 不终稿;已落盘的 chunk 保留在日志),分流终止。
            append(
                &state.session,
                &state.emit,
                SessionEvent::StepEnd {
                    turn: state.turn,
                    step,
                },
            )?;
            if feedback_eligible(&failure.code) && state.feedback < MAX_FEEDBACK {
                state.feedback += 1;
                append(
                    &state.session,
                    &state.emit,
                    SessionEvent::UserMessage {
                        text: feedback_text(&failure),
                        injected: true,
                        channel: Some("feedback".into()),
                        images: Vec::new(),
                    },
                )?;
                return Ok(RequestOutcome::RestartStep);
            }
            let retries_spent = step_retries;
            return Ok(RequestOutcome::TurnEnded(close_error(
                state,
                step,
                failure,
                retries_spent,
            )));
        }

        // 成功:终稿消息落盘(source_event_seqs 只引用本次 attempt 的 chunk,
        // 失败尝试的 chunk 自动不进派生历史)。
        state.step_retries = step_retries;
        if usage.is_none() {
            // provider 没报 usage:保守估算并**在事件上标注**。旧行为是让
            // token-meter 整轮拒绝折账——“没报数”于是变成了“不花钱”,目标
            // 预算永远用不完。估算口径与面板表面同源(同 density、同 system
            // 与工具声明),但天然不含缓存/思考明细,所以必须标明是估算。
            let accounted = estimate_step_usage(state, framed_system, tools, &blocks);
            append(
                &state.session,
                &state.emit,
                accounted.event(state.turn, step),
            )?;
        }
        append(
            &state.session,
            &state.emit,
            SessionEvent::AssistantMessage {
                turn: state.turn,
                step,
                blocks: blocks.clone(),
                usage,
                interrupted: false,
                source_event_seqs,
                first_token_time,
            },
        )?;
        return Ok(RequestOutcome::Step { blocks, finish });
    }
}

/// 本 step 是不是“输出截断续写”触发的:续写分支会把那条指令作为最后一条
/// 事件追加后回到 step 循环头。两者都吃用户预算,分类只影响对账与观测。
fn is_continuation_step(state: &TurnState) -> bool {
    state.session.with_events(|events| {
        events.last().is_some_and(|envelope| {
            matches!(
                &envelope.event,
                SessionEvent::UserMessage {
                    injected: true,
                    channel: Some(channel),
                    ..
                } if channel == "output-continuation"
            )
        })
    })
}

/// 缺 provider usage 时的保守估算(prompt 侧 + 输出侧)。
///
/// prompt 侧 = 请求**实际发出去**的那条前缀:派生消息 + system + 工具声明,
/// 与占用面板同一份估算函数(`denia_token_meter::estimate_message` 系列)。
/// 输出侧按回复正文 / 思考 / 工具调用同源估算。
///
/// “保守”的方向是**宁可高估**:高估只会让用户早一点看到预算被拦住(可见、
/// 可调),漏账则让预算静默失效——那正是这次要修的东西。标注靠
/// `AccountedUsage::estimated`,账本据此与精确用量分开对账。
fn estimate_step_usage(
    state: &TurnState,
    framed_system: &str,
    tools: &[ToolSchema],
    blocks: &[ContentBlock],
) -> denia_token_meter::AccountedUsage {
    let messages = state
        .session
        .derive_messages()
        .iter()
        .fold(0u64, |acc, message| {
            acc.saturating_add(denia_token_meter::estimate_message(message))
        });
    let tools_tokens = serde_json::to_string(tools)
        .map(|json| denia_token_meter::estimate_tools_tokens(&json))
        .unwrap_or(0);
    let prompt_tokens = messages
        .saturating_add(denia_token_meter::estimate_system_tokens(framed_system))
        .saturating_add(tools_tokens);
    let output_tokens = denia_core::message::assistant_from_blocks(blocks)
        .map(|message| denia_token_meter::estimate_message(&message))
        .unwrap_or(0);
    denia_token_meter::AccountedUsage::estimated_only(prompt_tokens, output_tokens)
}

/// 请求头/路由元数据按需落盘:头相同的连续请求不重复写(最近快照即
/// 重建);context 仅在路由或容量变化时写。
fn log_request_headers(
    state: &mut TurnState,
    step: u32,
    framed_system: &str,
    tools: &[ToolSchema],
) -> Result<(), LlmFailure> {
    let mut snapshot = build_header_snapshot(&state.selection, framed_system, tools);
    snapshot.config.max_tokens = Some(state.output_budget);
    if state.last_header.as_ref() != Some(&snapshot) {
        let reason = match &state.last_header {
            None if !state.has_request_header => RequestHeaderReason::Initial,
            None => RequestHeaderReason::Resume,
            Some(_) => RequestHeaderReason::Change,
        };
        append(
            &state.session,
            &state.emit,
            SessionEvent::RequestHeader {
                turn: state.turn,
                step,
                header: snapshot.clone(),
                reason,
                starts_series: reason == RequestHeaderReason::Change,
            },
        )?;
        state.last_header = Some(snapshot);
        state.has_request_header = true;
    }
    let context = (
        state.selection.provider.clone(),
        state.selection.model.clone(),
        state.context_window,
    );
    if state.last_context.as_ref() != Some(&context) {
        append(
            &state.session,
            &state.emit,
            SessionEvent::RequestContext {
                turn: state.turn,
                step,
                provider: context.0.clone(),
                model: context.1.clone(),
                context_window: context.2,
            },
        )?;
        state.last_context = Some(context);
    }
    Ok(())
}

/// 被清理结果需要沿用到替换事件上的事实。
///
/// 微压缩只清内容,不改变结果本身:错误状态、失败身份、产物引用与截断
/// 事实都必须跟着替换事件走,否则模型和 UI 都失去"这次到底成功没有、完整
/// 输出在哪里"的依据。
#[derive(Clone, Default)]
struct ClearedCarry {
    is_error: bool,
    error_identity: Option<denia_core::session::ToolFailureIdentity>,
    meta: Option<serde_json::Value>,
    truncation: Option<denia_core::session::TruncationInfo>,
}

impl ClearedCarry {
    fn from_event(event: &SessionEvent) -> Self {
        match event {
            SessionEvent::ToolResult {
                is_error,
                error_identity,
                meta,
                truncation,
                ..
            } => Self {
                is_error: *is_error,
                error_identity: error_identity.clone(),
                meta: meta.clone(),
                truncation: truncation.clone(),
            },
            _ => Self::default(),
        }
    }

    /// 清理后的模型可见文本。
    ///
    /// 必须以 [`crate::microcompact::CLEARED_PLACEHOLDER`] 开头:幂等保护
    /// (`already_cleared`)按前缀判断,否则带引用的占位符会被当成未清理项
    /// 反复重清。有产物引用时额外给出回读入口 —— 内容被清掉不代表证据
    /// 消失,只是从上下文挪到了产物里。
    fn placeholder(&self) -> String {
        let output_id = self
            .meta
            .as_ref()
            .and_then(|meta| meta.get("outputArtifact"))
            .and_then(|artifact| artifact.get("output_id"))
            .and_then(|id| id.as_str());
        match output_id {
            Some(id) => format!(
                "{}[完整输出保留为产物 {id},需要原文时用 read_tool_output 回读]",
                crate::microcompact::CLEARED_PLACEHOLDER
            ),
            None => crate::microcompact::CLEARED_PLACEHOLDER.to_string(),
        }
    }
}

/// —— 工具结果微压缩闸门 ——
///
/// 在 LLM 摘要之前的一层廉价清理:把已经用过的旧工具结果内容替换为
/// 占位符。不调模型、不重写历史——落盘为带 `replaces` 的 `ToolResult`
/// 事件,派生时原位替换,**日志保持 append-only**。
///
/// 触发条件(两者之一):空闲超时 / token 压力达到摘要阈值的一定比例。
/// `min_savings` 保护前缀缓存:省不到阈值就不动手。
async fn run_microcompact_gate(driver: &SessionDriver, state: &TurnState, step: u32) {
    let settings = &state.settings.microcompact;
    if !settings.enabled {
        return;
    }
    // 组装关闭压缩功能时,微压缩作为压缩的前置清理一并跳过。
    if !driver.features_for(&state.session).compaction {
        return;
    }

    let surface = state.session.derive_surface();
    if surface.is_empty() {
        return;
    }

    // 空闲时长:从最后一条 assistant 消息的落盘时间算起。
    let idle_minutes = state.session.last_assistant_age_minutes().map(|m| m as u64);
    // token 压力与阈值:微压缩的阈值取摘要阈值的 90%(
    // 让轻量清理总是先于重量摘要发生)。
    let pressure = state.session.context_pressure();
    let pressure_tokens = pressure.projected_tokens;
    let threshold_tokens = driver
        .compaction_threshold_tokens(&pressure, &state.settings.compaction)
        .map(|t| (t as f64 * 0.9) as u64);

    let decision = crate::microcompact::plan(
        &surface,
        settings,
        idle_minutes,
        pressure_tokens,
        threshold_tokens,
    );

    let crate::microcompact::MicrocompactDecision::Applied {
        trigger,
        cleared,
        kept,
        cleared_seqs,
        estimated_savings,
    } = decision
    else {
        return;
    };

    // 落盘:每个被清理项追加一条带 `replaces` 的 ToolResult 事件,
    // 派生时原位替换(内容不再进模型,但日志与前端仍保留完整历史)。
    let seq_to_call: std::collections::HashMap<u64, &str> = surface
        .iter()
        .filter_map(|item| {
            item.message
                .tool_call_id
                .as_deref()
                .map(|id| (item.seq, id))
        })
        .collect();

    // 原事件索引:替换事件只清内容,结果本身的其余事实必须沿用。
    let wanted: std::collections::HashSet<u64> = cleared_seqs.iter().copied().collect();
    let carry: std::collections::HashMap<u64, ClearedCarry> = state
        .session
        .events()
        .iter()
        .filter(|envelope| wanted.contains(&envelope.seq))
        .map(|envelope| (envelope.seq, ClearedCarry::from_event(&envelope.event)))
        .collect();

    let mut cleared_count = 0usize;
    let mut cleared_calls: Vec<&str> = Vec::new();
    for seq in &cleared_seqs {
        let Some(call_id) = seq_to_call.get(seq) else {
            continue;
        };
        let facts = carry.get(seq).cloned().unwrap_or_default();
        let event = SessionEvent::ToolResult {
            turn: state.turn,
            step,
            call_id: (*call_id).to_string(),
            content: facts.placeholder(),
            is_error: facts.is_error,
            error: None,
            error_identity: facts.error_identity,
            meta: facts.meta,
            truncation: facts.truncation,
            replaces: Some(*seq),
        };
        if append(&state.session, &state.emit, event).is_err() {
            // 落盘失败不阻断请求:清理是优化,不是正确性依赖。
            break;
        }
        cleared_count += 1;
        cleared_calls.push(*call_id);
    }

    if cleared_count > 0 {
        // 读状态失效(只在确实清掉东西之后做):
        //
        // 被清的内容既然已从模型上下文里消失,read_file 的去重提示
        // ("自上次读取以来未变更,内容同上一条读取结果")就指向了一条
        // 已不存在的记录——模型既看不到内容、又被拦着不重读。按**产生该
        // 结果的工具调用 id** 精确失效:只有内容被清掉的文件失去去重收益,
        // 其余文件(以及写后记录)照旧命中缓存。
        let invalidated = match state.read_state.lock() {
            Ok(mut read_state) => read_state.invalidate_calls(cleared_calls.iter().copied()),
            // 锁中毒只影响去重优化,不阻断请求。
            Err(_) => 0,
        };
        tracing::info!(
            session_id = state.session.id(),
            turn = state.turn,
            step,
            trigger = ?trigger,
            cleared = cleared_count,
            kept = kept.len(),
            estimated_savings,
            read_state_invalidated = invalidated,
            cleared_tool_calls = ?cleared,
            "microcompact applied"
        );
    }
}

/// 压缩后的读状态恢复:重新注入压缩前读过的文件内容。
///
/// 两件事一起做:
///
/// 1. **重新注入内容**:从 read_state 里挑最近读过的文件(最多 5 个、
///    单文件 5k token、总量 50k token),以注入消息的形式放回上下文;
/// 2. **清空 read_state**:摘要已接管旧内容的记忆职责。不清空的话,模型
///    会看到"读过"的标记、却看不到内容,写文件时基于不存在的记忆做决策。
fn restore_read_state_after_compact(driver: &SessionDriver, state: &TurnState, step: u32) {
    let shared = state.read_state.clone();
    let entries = match shared.lock() {
        Ok(state) => state.recent_writable(crate::compact::POST_COMPACT_MAX_FILES),
        Err(_) => return,
    };

    // 组装"路径 + 内容"列表(只取保留了完整内容的条目)。
    let pairs: Vec<(String, String)> = entries
        .into_iter()
        .filter_map(|(key, entry)| {
            let content = entry.content?;
            Some((key.path.to_string_lossy().replace('\\', "/"), content))
        })
        .collect();

    if !pairs.is_empty() {
        let texts = crate::compact::build_post_compact_read_state(
            &pairs,
            crate::compact::POST_COMPACT_MAX_FILES,
            crate::compact::POST_COMPACT_MAX_FILE_TOKENS,
            crate::compact::POST_COMPACT_MAX_TOTAL_TOKENS,
        );
        for text in texts {
            let _ = append(
                &state.session,
                &state.emit,
                SessionEvent::UserMessage {
                    text,
                    injected: true,
                    channel: Some("post-compact-read-state".into()),
                    images: Vec::new(),
                },
            );
        }
    }

    // 清空:旧内容要么已重新注入、要么已明确告知"需重读"。
    driver.clear_read_state(state.session.id());
    let _ = step;
}

/// rapid-refill 断路器:连续多少次"压缩后立刻回填"就停止自动压缩。
pub const RAPID_REFILL_MAX_CONSECUTIVE: u32 = 3;

/// 判定"回填过快"的工具轮次阈值:压缩后经过少于这么多轮又撞阈值,
/// 计一次 rapid-refill。
pub const RAPID_REFILL_TOOL_TURN_THRESHOLD: u32 = 3;

/// —— 层叠上下文管理(学 dsh 压力驱动 + Claude Code compact)——
/// 1. 低压力:什么都不做 —— 请求内容与日志逐字一致,provider 前缀缓存
///    持续命中;
/// 2. 高压力:LLM 总结压缩,把旧事件区间折叠成摘要(落盘
///    compaction-summary,日志保持 append-only)。失败熔断:连续失败达
///    max_attempts 后不再尝试,带着全量历史继续。
///
/// 另有 **rapid-refill 断路器**:
/// 压缩刚做完、没经过几个工具轮次又撞上阈值,说明有超大输出在持续灌入。
/// 连续发生 3 次就停止自动压缩并明确报错——比无限压缩烧钱更有用。
async fn run_compaction_gate(
    driver: &SessionDriver,
    state: &TurnState,
    step: u32,
    framed_system: &str,
    tools: &[ToolSchema],
    forced: bool,
) -> bool {
    // 组装关闭压缩功能时整个自动闸门跳过(手动 /compact 由 compact_manually
    // 按同一开关拒绝)。
    if !driver.features_for(&state.session).compaction {
        return false;
    }
    let pressure = state.session.context_pressure();
    let mut reserved_pressure = pressure;
    reserved_pressure.context_window = reserved_pressure
        .context_window
        .map(|window| window.saturating_sub(state.output_budget));
    if !state.settings.compaction.compact_enabled
        || (!forced && !should_compact(&reserved_pressure, &state.settings.compaction))
        || state
            .compaction_state
            .failures
            .load(std::sync::atomic::Ordering::SeqCst)
            >= state.settings.compaction.max_attempts
    {
        return false;
    }
    // 断路器:连续快速回填已达上限时不再尝试,并把原因明确告知用户。
    if state
        .compaction_state
        .rapid_refills
        .load(std::sync::atomic::Ordering::SeqCst)
        >= RAPID_REFILL_MAX_CONSECUTIVE
    {
        let _ = append(
            &state.session,
            &state.emit,
            SessionEvent::UserMessage {
                text: format!(
                    "[denia] 自动压缩已停止:连续 {RAPID_REFILL_MAX_CONSECUTIVE} 次压缩后、\
                     不到 {RAPID_REFILL_TOOL_TURN_THRESHOLD} 个工具轮次上下文就被重新填满。\
                     可能有某个文件或命令的输出过大。请分块读取,或开一个新会话继续。"
                ),
                injected: true,
                channel: Some("compact-breaker".into()),
                images: Vec::new(),
            },
        );
        return false;
    }
    match driver
        .compact_context(
            &state.session,
            &state.selection,
            framed_system,
            tools,
            state.turn,
            step,
            &state.cancel,
            Some(&state.replay),
            &state.settings.compaction,
            // 摘要请求计入本轮预算:内部压缩不是绕过预算的暗道。
            Some(state.budget.clone()),
        )
        .await
    {
        Ok(Some(outcome)) => {
            if append(
                &state.session,
                &state.emit,
                SessionEvent::CompactionSummary {
                    turn: state.turn,
                    step,
                    summary: outcome.summary,
                    replaces_from: outcome.replaces_from,
                    replaces_to: outcome.replaces_to,
                    keep_from: outcome.keep_from,
                    pre_tokens: outcome.pre_tokens,
                    post_tokens: outcome.post_tokens,
                },
            )
            .is_err()
            {
                return false;
            }
            let refills = state
                .compaction_state
                .record_success(RAPID_REFILL_TOOL_TURN_THRESHOLD);
            if refills > 0 {
                tracing::warn!(
                    session_id = state.session.id(),
                    consecutive_rapid_refills = refills,
                    "compaction refilled quickly"
                );
            }
            // 压缩后读状态恢复:
            // 把压缩前读过的文件内容按预算重新注入,避免模型"忘了看过什么"
            // 而立刻重新读一遍——那正是压缩后窗口二次膨胀的根源。
            restore_read_state_after_compact(driver, state, step);
            return true;
        }
        Ok(None) => {
            // 无可压缩区间(历史太短/窗口选择失败):不计数。
        }
        Err(error) => {
            state
                .compaction_state
                .failures
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tracing::warn!(
                session_id = state.session.id(),
                error_code = %error.code,
                error_message = %error.message,
                failures = state.compaction_state.failures.load(std::sync::atomic::Ordering::SeqCst),
                "llm compaction failed; skipping and continuing with full history"
            );
        }
    }
    false
}

/// 用户取消的轮次闭合:补 StepEnd + TurnEnd(Aborted),返回 TurnEnded。
fn close_aborted(state: &TurnState, step: u32) -> Result<RequestOutcome, LlmFailure> {
    append(
        &state.session,
        &state.emit,
        SessionEvent::StepEnd {
            turn: state.turn,
            step,
        },
    )?;
    let reason = TurnEndReason::Aborted {
        cause: Some(AbortCause::User),
    };
    append(
        &state.session,
        &state.emit,
        SessionEvent::TurnEnd {
            turn: state.turn,
            reason: reason.clone(),
        },
    )?;
    Ok(RequestOutcome::TurnEnded(reason))
}

/// 错误终止的轮次闭合:失败摘要中文化后落 TurnEnd(Error)。
fn close_error(state: &TurnState, _step: u32, failure: LlmFailure, retries: u32) -> TurnEndReason {
    let reason = TurnEndReason::Error {
        failure: summarize_failure(&failure, retries),
    };
    // TurnEnd 落盘失败已无路可走(存储故障),保留在日志语义里由 load 兜底。
    let _ = append(
        &state.session,
        &state.emit,
        SessionEvent::TurnEnd {
            turn: state.turn,
            reason: reason.clone(),
        },
    );
    reason
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::error::codes;
    use denia_llm::RetryPolicy;

    #[test]
    fn long_retry_after_gives_up_instead_of_zero_delay_retry() {
        // provider 给的 Retry-After 超过本地等待上限时,`delay_ms_for` 返回
        // None。此处必须判为"放弃重试",而不是兑成 0 毫秒立刻重发。
        let policy = RetryPolicy::default();
        let failure = LlmFailure::new(codes::RATE_LIMIT, "slow down")
            .with_retry_after_ms(policy.max_delay_ms + 1);
        assert_eq!(
            stream_retry_decision(&policy, &failure, 0),
            RetryDecision::RetryAfterTooLong
        );

        // 连接类错误走更长的退避曲线,上限也更高:以它自己的上限判定。
        let connection = LlmFailure::new(codes::TIMEOUT, "timeout")
            .with_retry_after_ms(policy.connection_max_delay_ms + 1);
        assert_eq!(
            stream_retry_decision(&policy, &connection, 0),
            RetryDecision::RetryAfterTooLong
        );
    }

    #[test]
    fn retry_within_limits_waits_and_other_cases_stop() {
        let policy = RetryPolicy::default();
        // 上限内:退避等待(非零),正常重试。
        let hint = LlmFailure::new(codes::RATE_LIMIT, "slow down").with_retry_after_ms(1_500);
        assert_eq!(
            stream_retry_decision(&policy, &hint, 0),
            RetryDecision::Wait(1_500)
        );
        // 不可重试的错误码。
        let fatal = LlmFailure::new(codes::AUTH, "bad key");
        assert_eq!(
            stream_retry_decision(&policy, &fatal, 0),
            RetryDecision::NotRetryable
        );
        // 预算耗尽。
        let plain = LlmFailure::new(codes::SERVER, "5xx");
        assert_eq!(
            stream_retry_decision(&policy, &plain, policy.max_retries),
            RetryDecision::BudgetExhausted
        );
    }

    #[test]
    fn cleared_carry_keeps_result_facts_and_output_reference() {
        // 清理只该清内容:错误状态、失败身份、产物引用与截断事实必须沿用,
        // 否则模型与 UI 都失去"这次到底成功没有、完整输出在哪里"的依据。
        let event = SessionEvent::ToolResult {
            turn: 1,
            step: 2,
            call_id: "call_1".into(),
            content: "原文".into(),
            is_error: true,
            error: None,
            error_identity: Some(denia_core::session::ToolFailureIdentity {
                name: "read_file".into(),
                code: "E_NOT_FOUND".into(),
            }),
            meta: Some(serde_json::json!({
                "outputArtifact": { "output_id": "art-7", "complete": true }
            })),
            truncation: Some(denia_core::session::TruncationInfo {
                total_chars: 400_000,
                shown_chars: 16_000,
            }),
            replaces: None,
        };
        let carry = ClearedCarry::from_event(&event);
        assert!(carry.is_error);
        assert_eq!(
            carry.error_identity.as_ref().map(|id| id.code.as_str()),
            Some("E_NOT_FOUND")
        );
        assert_eq!(
            carry.truncation.as_ref().map(|t| t.total_chars),
            Some(400_000)
        );
        let text = carry.placeholder();
        assert!(
            text.starts_with(crate::microcompact::CLEARED_PLACEHOLDER),
            "必须保持前缀,幂等判断依赖它: {text}"
        );
        assert!(text.contains("art-7"), "占位符要保留产物引用: {text}");
        assert!(
            text.contains("read_tool_output"),
            "占位符要给出回读入口: {text}"
        );
    }

    /// 微压缩闸门的落盘定位在"有被折叠注入块"的会话上仍然正确。
    ///
    /// 闸门要用投影面的 seq 做三件事:映射回 `call_id`、从原始日志取出该结果的
    /// 其余事实(`ClearedCarry`)、把 `replaces` 写回那个 seq。基线通道的注入块
    /// 被投影折叠后,投影面的 seq 集合与原始日志不再一致——只要其中一环按
    /// 日志下标而不是 seq 定位,就会写出一条指向错误节点的替换事件(或者静默
    /// 地什么都不清)。本测试把闸门的三步照搬下来跑一遍。
    #[test]
    fn microcompact_replacement_targets_resolve_on_a_folded_surface() {
        let envelope = |seq: u64, event: SessionEvent| denia_core::session::SessionEnvelope {
            seq,
            time: 1_700_000_000_000 + seq,
            event,
        };
        let user_msg = |seq: u64, text: &str| {
            envelope(
                seq,
                SessionEvent::UserMessage {
                    text: text.into(),
                    injected: false,
                    images: Vec::new(),
                    channel: None,
                },
            )
        };
        let injection = |seq: u64, text: &str| {
            envelope(
                seq,
                SessionEvent::UserMessage {
                    text: text.into(),
                    injected: true,
                    images: Vec::new(),
                    channel: Some("capability".into()),
                },
            )
        };
        let call = |seq: u64, id: &str| {
            envelope(
                seq,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 1,
                    blocks: vec![denia_core::stream::ContentBlock::ToolCall {
                        id: id.into(),
                        name: "bash".into(),
                        arguments: "{}".into(),
                        incomplete: false,
                    }],
                    usage: None,
                    interrupted: false,
                    source_event_seqs: Vec::new(),
                    first_token_time: None,
                },
            )
        };
        let result = |seq: u64, id: &str, content: String| {
            envelope(
                seq,
                SessionEvent::ToolResult {
                    turn: 1,
                    step: 1,
                    call_id: id.into(),
                    content,
                    is_error: false,
                    error: None,
                    error_identity: None,
                    meta: None,
                    replaces: None,
                    truncation: None,
                },
            )
        };

        let events = vec![
            user_msg(1, "先跑一遍"),
            call(2, "call_old"),
            result(3, "call_old", "旧结果".repeat(2_000)),
            // 同一通道的能力上下文两条:旧的(seq 4)被投影折叠掉。
            injection(4, &format!("[denia 能力上下文]{}", "x".repeat(2_000))),
            injection(5, "[denia 能力上下文]v2"),
            call(6, "call_new"),
            result(7, "call_new", "新结果".into()),
        ];

        let surface = denia_core::session::derive_surface(&events);
        assert!(
            surface.iter().all(|item| item.seq != 4),
            "旧基线必须离开模型面(否则本测试什么也没钉)"
        );
        let seqs: Vec<u64> = surface.iter().map(|item| item.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3, 5, 6, 7]);

        // 保留最近 1 组 + 空闲触发:seq 3 那条旧结果进候选(5/6/7 是新组)。
        let decision = crate::microcompact::plan(
            &surface,
            &crate::microcompact::MicrocompactSettings {
                keep_recent_groups: 1,
                min_savings: 0,
                ..Default::default()
            },
            Some(120),
            None,
            None,
        );
        let crate::microcompact::MicrocompactDecision::Applied { cleared_seqs, .. } = decision
        else {
            panic!("空闲超时 + 保留 1 组应当清理旧结果:{decision:?}");
        };
        assert_eq!(cleared_seqs, vec![3], "只清模型面上真实存在的那条旧结果");

        // —— 以下三步照搬 `run_microcompact_gate` ——
        let seq_to_call: std::collections::HashMap<u64, &str> = surface
            .iter()
            .filter_map(|item| {
                item.message
                    .tool_call_id
                    .as_deref()
                    .map(|id| (item.seq, id))
            })
            .collect();
        let wanted: std::collections::HashSet<u64> = cleared_seqs.iter().copied().collect();
        let carry: std::collections::HashMap<u64, ClearedCarry> = events
            .iter()
            .filter(|envelope| wanted.contains(&envelope.seq))
            .map(|envelope| (envelope.seq, ClearedCarry::from_event(&envelope.event)))
            .collect();

        let mut replaced = events.clone();
        for seq in &cleared_seqs {
            // 定位失败就是闸门的静默失败模式:清了 0 条而不报错。
            let call_id = seq_to_call
                .get(seq)
                .expect("被清项必须在投影面上找得到 call_id");
            let facts = carry.get(seq).cloned().unwrap_or_default();
            replaced.push(envelope(
                seq + 100,
                SessionEvent::ToolResult {
                    turn: 1,
                    step: 2,
                    call_id: (*call_id).to_string(),
                    content: facts.placeholder(),
                    is_error: facts.is_error,
                    error: None,
                    error_identity: facts.error_identity,
                    meta: facts.meta,
                    truncation: facts.truncation,
                    replaces: Some(*seq),
                },
            ));
        }

        // 原位替换:节点数不变、原 seq 保位、被折叠的旧基线不因新事件复活。
        let after = denia_core::session::derive_surface(&replaced);
        assert_eq!(after.len(), surface.len(), "替换必须原位:不新增节点");
        assert_eq!(after.iter().map(|item| item.seq).collect::<Vec<_>>(), seqs);
        let cleared = after
            .iter()
            .find(|item| item.seq == 3)
            .expect("被清节点保持原 seq");
        assert_eq!(
            cleared.message.content,
            crate::microcompact::CLEARED_PLACEHOLDER
        );
        assert_eq!(
            cleared.message.tool_call_id.as_deref(),
            Some("call_old"),
            "替换事件必须写回那条结果自己的 call_id"
        );
    }

    /// core 的旧日志前缀表必须与注入侧的识别口径逐字一致。
    ///
    /// 两边分叉时,同一个通道的老块(无 `channel` 字段)与新块会被算成两个
    /// 通道:基准按前缀认、投影不认——模型面上就是两份基线并存。前缀常量
    /// 分居两个 crate,只能靠这条测试钉住(core 不能反向依赖 agent-loop)。
    #[test]
    fn legacy_injection_prefixes_match_the_injection_side() {
        use denia_core::session::{BASELINE_INJECTION_CHANNELS, injection_channel};
        for (prefix, channel) in [
            (
                crate::workspace_instructions::WORKSPACE_PREFIX,
                "workspace-instructions",
            ),
            (
                crate::workspace_instructions::SKILL_CATALOG_PREFIX,
                "skill-catalog",
            ),
        ] {
            assert_eq!(
                injection_channel(prefix, true, None),
                Some(channel),
                "前缀识别必须与注入侧同口径:{prefix}"
            );
            assert!(
                BASELINE_INJECTION_CHANNELS.contains(&channel),
                "前缀认出的通道必须在折叠名单里:{channel}"
            );
        }
    }

    #[test]
    fn cleared_carry_without_artifact_is_plain_placeholder() {
        let event = SessionEvent::ToolResult {
            turn: 1,
            step: 2,
            call_id: "call_1".into(),
            content: "原文".into(),
            is_error: false,
            error: None,
            error_identity: None,
            meta: None,
            truncation: None,
            replaces: None,
        };
        let carry = ClearedCarry::from_event(&event);
        assert_eq!(
            carry.placeholder(),
            crate::microcompact::CLEARED_PLACEHOLDER
        );
    }
}
