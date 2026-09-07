//! 工具执行:并行滚动池 + 升权审批 + 结果兜底截断。
//!
//! - 一次 step 内模型返回的多个工具调用并发执行(学 codex
//!   `ToolCallRuntime` + `FuturesOrdered` 滚动池),受 `max_parallel_tool_calls`
//!   约束;结果按 model-order 提交,每个 call 恰好一条 `ToolResult` 事件
//!   (事件源不变量)。
//! - 需要审批的调用(升权)串行化——审批是交互式阻塞,并发弹窗会交错事件。
//! - 工具结果落盘前统一过输出预算(`apply_output_budget`):工具内部只做
//!   业务限量,字符级截断只有一个权威出口,截断事实进事件供 UI 展示。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use denia_core::message::ToolCallRef;
use denia_core::session::{ApprovalOutcome, PermissionMode, SessionEvent};
use denia_session::Session;
use denia_tools::ToolOutput;
use futures::StreamExt;

use crate::{SessionDriver, TurnState, append};

/// 执行一次 step 的全部工具调用(并行池 + 升权串行),结果逐条落盘。
pub(crate) async fn execute_calls(
    driver: &SessionDriver,
    state: &mut TurnState,
    step: u32,
    calls: &[ToolCallRef],
    touched: &mut Vec<PathBuf>,
) -> Result<(), denia_core::error::LlmFailure> {
    let cwd = state.cwd();
    let max_parallel = driver.parallel.max_parallel_tool_calls.max(1);
    let mut in_flight: futures::stream::FuturesOrdered<
        futures::future::BoxFuture<'static, (usize, ToolOutput)>,
    > = futures::stream::FuturesOrdered::new();
    let mut next = 0usize;
    while next < calls.len() || !in_flight.is_empty() {
        // 调度:填充滚动池(升权调用不入池,等池空后串行执行)。
        while next < calls.len() && in_flight.len() < max_parallel {
            let call = &calls[next];
            let needs_escalation = matches!(call.name.as_str(), "bash" | "write_file" | "edit")
                && escalation_fields(&call.arguments).is_some();
            if needs_escalation {
                break;
            }
            append(
                &state.session,
                &state.emit,
                SessionEvent::ToolCall {
                    turn: state.turn,
                    step,
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                },
            )?;
            let index = next;
            let future = dispatch_tool_call(driver, state, &cwd, call);
            in_flight.push_back(Box::pin(async move { (index, future.await) }));
            next += 1;
        }
        if in_flight.is_empty() {
            if next < calls.len() {
                // 升权调用:串行执行(审批交互式,不并发)。
                let call = &calls[next];
                append(
                    &state.session,
                    &state.emit,
                    SessionEvent::ToolCall {
                        turn: state.turn,
                        step,
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    },
                )?;
                let output = dispatch_escalated_tool_call(driver, state, &cwd, call).await;
                commit_result(state, step, call, output, touched, &cwd).await?;
                next += 1;
                continue;
            }
            break;
        }
        // 提交:按 model-order 取回结果并落 `ToolResult`。
        if let Some((index, output)) = in_flight.next().await {
            let call = &calls[index];
            commit_result(state, step, call, output, touched, &cwd).await?;
        }
    }
    Ok(())
}

/// 结果落盘:成功的读写类调用记录触碰路径;内容过统一输出预算;
/// 截断事实进事件。
async fn commit_result(
    state: &TurnState,
    step: u32,
    call: &ToolCallRef,
    output: ToolOutput,
    touched: &mut Vec<PathBuf>,
    cwd: &Path,
) -> Result<(), denia_core::error::LlmFailure> {
    if !output.is_error && matches!(call.name.as_str(), "read_file" | "write_file" | "edit") {
        if let Some(path) = crate::workspace_instructions::touched_path(cwd, &call.arguments) {
            touched.push(path);
        }
    }
    let (content, truncation) = denia_tools::support::apply_output_budget(&output.content);
    append(
        &state.session,
        &state.emit,
        SessionEvent::ToolResult {
            turn: state.turn,
            step,
            call_id: call.id.clone(),
            content,
            is_error: output.is_error,
            error: None,
            error_identity: None,
            meta: None,
            truncation,
            replaces: None,
        },
    )?;
    Ok(())
}

/// 调度一次工具调用(无升权路径),返回 boxed future 供并行池使用。
/// 内部处理取消、子代理白名单、未知工具、权限与审批(普通被拒)。
fn dispatch_tool_call(
    driver: &SessionDriver,
    state: &TurnState,
    cwd: &Path,
    call: &ToolCallRef,
) -> futures::future::BoxFuture<'static, ToolOutput> {
    let session = state.session.clone();
    let emit = state.emit.clone();
    let selection = state.selection.clone();
    let cwd = cwd.to_path_buf();
    let call = call.clone();
    let tools = driver.tools.clone();
    let cancel = state.cancel.clone();
    let file_history = state.file_history.clone();
    let vision_supported = state.vision_supported;
    Box::pin(async move {
        if cancel.is_cancelled() {
            return ToolOutput::error("工具调用在派发前已被中断");
        }
        if session
            .header()
            .subagent
            .as_ref()
            .and_then(|s| s.allowed_tools.as_ref())
            .is_some_and(|allowed| !allowed.contains(&call.name))
        {
            return ToolOutput::error("该工具不在当前子代理允许的工具集合中");
        }
        let Some(tool) = tools.get(&call.name) else {
            return ToolOutput::error(format!(
                "unknown tool: {}\n该工具不存在;可用工具见系统提示的工具列表,核对名称后重试。",
                call.name
            ));
        };
        let current_mode = session.permission_mode();
        let sink_session = session.clone();
        let sink_emit = emit.clone();
        let context = denia_tools::ToolContext {
            session_id: Some(session.id().to_string()),
            selection: Some(selection),
            cwd: cwd.clone(),
            cancel: cancel.child_token(),
            confined: !current_mode.is_full() && session.header().sandbox,
            vision_supported,
            emit_event: Some(Arc::new(move |event: SessionEvent| {
                if let Ok(envelope) = sink_session.append(event) {
                    sink_emit(&envelope);
                }
            })),
            file_history,
            permission_mode: current_mode,
            permission_override: None,
        };
        let execute = tool.execute(&call.arguments, &context);
        tokio::pin!(execute);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => ToolOutput::error("工具执行被中断"),
            output = &mut execute => output,
        }
    })
}

/// 执行一次需升权的工具调用(串行):校验 → 审批 → 执行。
async fn dispatch_escalated_tool_call(
    driver: &SessionDriver,
    state: &TurnState,
    cwd: &Path,
    call: &ToolCallRef,
) -> ToolOutput {
    if state.cancel.is_cancelled() {
        return ToolOutput::error("工具调用在派发前已被中断");
    }
    if state
        .session
        .header()
        .subagent
        .as_ref()
        .and_then(|s| s.allowed_tools.as_ref())
        .is_some_and(|allowed| !allowed.contains(&call.name))
    {
        return ToolOutput::error("该工具不在当前子代理允许的工具集合中");
    }
    let Some(tool) = driver.tools.get(&call.name) else {
        return ToolOutput::error(format!(
            "unknown tool: {}\n该工具不存在;可用工具见系统提示的工具列表,核对名称后重试。",
            call.name
        ));
    };
    let current_mode = state.session.permission_mode();
    let mut permission_override = None;
    if let Some((requested_raw, justification)) = escalation_fields(&call.arguments) {
        match resolve_escalation(
            driver,
            state,
            call,
            current_mode,
            &requested_raw,
            &justification,
            escalation_subject(&call.name),
        )
        .await
        {
            Ok(mode) => permission_override = Some(mode),
            Err(message) => return ToolOutput::error(message),
        }
    }
    let sink_session = state.session.clone();
    let sink_emit = state.emit.clone();
    let context = denia_tools::ToolContext {
        session_id: Some(state.session.id().to_string()),
        selection: Some(state.selection.clone()),
        cwd: cwd.to_path_buf(),
        cancel: state.cancel.child_token(),
        confined: !current_mode.is_full() && state.session.header().sandbox,
        vision_supported: state.vision_supported,
        emit_event: Some(Arc::new(move |event: SessionEvent| {
            if let Ok(envelope) = sink_session.append(event) {
                sink_emit(&envelope);
            }
        })),
        file_history: state.file_history.clone(),
        permission_mode: current_mode,
        permission_override,
    };
    let execute = tool.execute(&call.arguments, &context);
    tokio::pin!(execute);
    tokio::select! {
        biased;
        _ = state.cancel.cancelled() => ToolOutput::error("工具执行被中断"),
        output = &mut execute => output,
    }
}

/// 从工具原始参数里提取升权请求字段(宽松解析:取不到就视为无升权)。
pub(crate) fn escalation_fields(raw: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(raw.trim()).ok()?;
    let permissions = value.get("sandbox_permissions")?.as_str()?.to_string();
    let justification = value.get("justification")?.as_str()?.to_string();
    Some((permissions, justification))
}

/// 处理一次工具升权请求:校验 → 落 approval/asked → 等用户决策 →
/// 落 approval/decided → 把结果映射为允许模式或错误文本。
///
/// 仅在模型显式携带 `sandbox_permissions` + `justification` 时调用;
/// 普通被拒调用由工具自身返回 `[sandbox: …]` 标记,模型再据此重试。
async fn resolve_escalation(
    driver: &SessionDriver,
    state: &TurnState,
    call: &ToolCallRef,
    current_mode: PermissionMode,
    requested_raw: &str,
    justification: &str,
    subject: &str,
) -> Result<PermissionMode, String> {
    if let Err(message) =
        denia_tools::permission::validate_escalation_args(Some(requested_raw), Some(justification))
    {
        return Err(message);
    }
    let requested = denia_tools::permission::parse_permission_mode(requested_raw)
        .ok_or_else(|| format!("unknown sandbox mode \"{requested_raw}\""))?;
    if !denia_tools::permission::is_strictly_wider(current_mode, requested) {
        return Err(format!(
            "sandbox escalation to \"{}\" is not strictly wider than this call's current \"{}\" mode",
            requested.as_str(),
            current_mode.as_str(),
        ));
    }
    let Some(approval) = &driver.approval else {
        return Err(format!(
            "sandbox escalation to \"{}\" requires approval, but no approval channel is available",
            requested.as_str()
        ));
    };
    let request_id = uuid::Uuid::new_v4().to_string();
    let reason = format!(
        "escalate sandbox to {}: {}",
        requested.as_str(),
        justification.trim()
    );
    append(
        &state.session,
        &state.emit,
        SessionEvent::ApprovalAsked {
            request_id: request_id.clone(),
            call_id: call.id.clone(),
            tool: call.name.clone(),
            args_preview: call.arguments.clone(),
            reason: Some(reason.clone()),
        },
    )
    .map_err(|error| format!("[{}] {}", error.code, error.message))?;
    let outcome = approval
        .request(state.session.id(), &request_id, state.cancel.clone())
        .await;
    append(
        &state.session,
        &state.emit,
        SessionEvent::ApprovalDecided {
            request_id: request_id.clone(),
            outcome,
        },
    )
    .map_err(|error| format!("[{}] {}", error.code, error.message))?;
    match outcome {
        ApprovalOutcome::AllowedOnce => Ok(requested),
        ApprovalOutcome::Rejected => Err(format!(
            "the user rejected escalating this {subject} to \"{}\"",
            requested.as_str()
        )),
        ApprovalOutcome::Cancelled => Err(format!(
            "approval for escalating to \"{}\" was cancelled",
            requested.as_str()
        )),
        ApprovalOutcome::Unavailable => Err(format!(
            "sandbox escalation to \"{}\" requires approval, but no approval channel is available",
            requested.as_str()
        )),
    }
}

/// 工具升权审批的模型侧主题(与 dsh escalation hint 一致)。
fn escalation_subject(tool_name: &str) -> &'static str {
    match tool_name {
        "bash" => "command",
        "write_file" | "edit" => "operation",
        _ => "operation",
    }
}
