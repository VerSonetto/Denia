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

use denia_core::config::LlmCallConfig;
use denia_core::error::LlmFailure;
use denia_core::session::{
    AbortCause, RequestHeaderReason, SessionEvent, TurnEndReason,
};
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
    Step { blocks: Vec<ContentBlock>, finish: Option<FinishReason> },
    TurnEnded(TurnEndReason),
    RestartStep,
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
    log_request_headers(state, step, framed_system, tools)?;
    run_compaction_gate(driver, state, step, framed_system, tools).await;

    let request = GenerateRequest {
        model: state.selection.model.clone(),
        reasoning_effort: state.selection.reasoning_effort.clone(),
        messages: state.session.derive_messages(),
        system: Some(framed_system.to_string()),
        tools: tools.to_vec(),
        temperature: None,
        max_tokens: None,
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
    'attempts: loop {
        // 请求前检查点(对齐 dsh checkpoint-policy):请求前缀刷盘
        // 成功才派发;失败 fail-closed(不发出请求)。
        flush_before_dispatch(&state.session)?;
        // 请求建立期同样响应取消:网关/代理黑洞挂起时,点停止必须立刻能断。
        let stream_setup = driver
            .registry
            .stream(&state.selection.provider, &request, retry_sink.clone());
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
            let has_chunks = !source_event_seqs.is_empty();
            let retryable = retry_policy.is_retryable(&failure.code)
                && step_retries < retry_policy.max_retries
                && !state.cancel.is_cancelled();
            // 可重试:finish 错误与流错误都重试(dsh:llm-retry 丢弃失败
            // 尝试的 chunk)。重放 LLM 请求无副作用:半成品 chunk 留在日志
            // 但不进派生历史,工具尚未执行;失败尝试的部分输出由新 attempt
            // 的 blocks 整体取代。曾限制"仅无输出时重试",实测被网关断流
            // 掐死的收尾步白白死亡——有输出重放是安全的,故取消该限制。
            if retryable {
                step_retries += 1;
                state.step_retries = step_retries;
                let connection = retry_policy.is_connection_error(&failure.code);
                let delay = retry_policy
                    .delay_ms_for(step_retries, failure.provider_retry_after_ms, connection)
                    .unwrap_or(0);
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
                            },
                        )?;
                    }
                    return close_aborted(state, step);
                }
                continue 'attempts;
            }
            // 不可重试/预算耗尽:失败尝试不产出终稿消息(dsh:流错误
            // rethrow 不终稿;已落盘的 chunk 保留在日志),分流终止。
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
            },
        )?;
        return Ok(RequestOutcome::Step { blocks, finish });
    }
}

/// 请求头/路由元数据按需落盘:头相同的连续请求不重复写(最近快照即
/// 重建);context 仅在路由或容量变化时写。
fn log_request_headers(
    state: &mut TurnState,
    step: u32,
    framed_system: &str,
    tools: &[ToolSchema],
) -> Result<(), LlmFailure> {
    let snapshot = build_header_snapshot(&state.selection, framed_system, tools);
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

/// —— 层叠上下文管理(学 dsh 压力驱动 + Claude Code compact)——
/// 1. 低压力:什么都不做 —— 请求内容与日志逐字一致,provider 前缀缓存
///    持续命中;
/// 2. 高压力:LLM 总结压缩,把旧事件区间折叠成摘要(落盘
///    compaction-summary,日志保持 append-only)。失败熔断:连续失败达
///    max_attempts 后不再尝试,带着全量历史继续。
async fn run_compaction_gate(
    driver: &SessionDriver,
    state: &TurnState,
    step: u32,
    framed_system: &str,
    tools: &[ToolSchema],
) {
    let pressure = state.session.context_pressure();
    if !should_compact(&pressure, &driver.compaction)
        || driver.compact_failures.load(std::sync::atomic::Ordering::SeqCst)
            >= driver.compaction.max_attempts
    {
        return;
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
        )
        .await
    {
        Ok(Some(outcome)) => {
            // 压缩成功:熔断清零,落盘事件由 append 广播给前端。
            driver
                .compact_failures
                .store(0, std::sync::atomic::Ordering::SeqCst);
            let _ = append(
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
            );
        }
        Ok(None) => {
            // 无可压缩区间(历史太短/窗口选择失败):不计数。
        }
        Err(error) => {
            driver
                .compact_failures
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tracing::warn!(
                session_id = state.session.id(),
                error_code = %error.code,
                error_message = %error.message,
                failures = driver.compact_failures.load(std::sync::atomic::Ordering::SeqCst),
                "llm compaction failed; skipping and continuing with full history"
            );
        }
    }
}

/// 用户取消的轮次闭合:补 StepEnd + TurnEnd(Aborted),返回 TurnEnded。
fn close_aborted(state: &TurnState, step: u32) -> Result<RequestOutcome, LlmFailure> {
    append(
        &state.session,
        &state.emit,
        SessionEvent::StepEnd { turn: state.turn, step },
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
fn close_error(
    state: &TurnState,
    _step: u32,
    failure: LlmFailure,
    retries: u32,
) -> TurnEndReason {
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
