//! 工具执行:并行滚动池 + 策略门控(Allow/Ask/Deny)+ 结果兜底截断。
//!
//! - 一次 step 内模型返回的多个工具调用并发执行(学 codex
//!   `ToolCallRuntime` + `FuturesOrdered` 滚动池),受 `max_parallel_tool_calls`
//!   约束;结果按 model-order 提交,每个 call 恰好一条 `ToolResult` 事件
//!   (事件源不变量)。
//! - 策略门控统一在派发处(见 `denia_tools::permission::decide`):
//!   Deny 直接合成错误结果,Ask(越界写文件、计划提交)串行化走审批——
//!   审批是交互式阻塞,并发弹窗会交错事件。工具内部不再自行判权。
//! - 工具结果落盘前统一过输出预算(`apply_output_budget`):工具内部只做
//!   业务限量,字符级截断只有一个权威出口,截断事实进事件供 UI 展示。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use denia_core::message::ToolCallRef;
use denia_core::session::{ApprovalOutcome, PermissionMode, PlanReviewDecision, SessionEvent};
use denia_tools::permission::{ActionClass, Decision};
use denia_tools::ToolOutput;
use futures::StreamExt;

use crate::{SessionDriver, TurnState, append};

/// 执行一次 step 的全部工具调用(并行池 + 策略门控),结果逐条落盘。
pub(crate) async fn execute_calls(
    driver: &SessionDriver,
    state: &mut TurnState,
    step: u32,
    calls: &[ToolCallRef],
    touched: &mut Vec<PathBuf>,
) -> Result<(), denia_core::error::LlmFailure> {
    let cwd = state.cwd();
    // 项目记忆目录一次解析(未启用为 None):写类分类与子代理写收敛共用。
    let memory_root = driver
        .runtime
        .as_ref()
        .and_then(|runtime| runtime.memory_root_for(&cwd));
    let max_parallel = driver.parallel.max_parallel_tool_calls.max(1);
    let mut in_flight: futures::stream::FuturesOrdered<
        futures::future::BoxFuture<'static, (usize, ToolOutput)>,
    > = futures::stream::FuturesOrdered::new();
    let mut next = 0usize;
    while next < calls.len() || !in_flight.is_empty() {
        // 调度:逐个判定(白名单/未知工具/策略),Allow 入池,Deny 内联
        // 合成错误,Ask 留给池空后的串行审批路径。
        while next < calls.len() && in_flight.len() < max_parallel {
            let call = &calls[next];
            if let Some(output) = reject_before_dispatch(driver, state, call) {
                append_call(state, step, call)?;
                commit_result(state, step, call, output, touched, &cwd).await?;
                next += 1;
                continue;
            }
            match decide_for(state, &cwd, call, memory_root.as_deref()) {
                Decision::Allow => {
                    append_call(state, step, call)?;
                    let index = next;
                    let future = dispatch_tool_call(driver, state, &cwd, call);
                    in_flight.push_back(Box::pin(async move { (index, future.await) }));
                    next += 1;
                }
                Decision::Deny(reason) => {
                    append_call(state, step, call)?;
                    commit_result(
                        state,
                        step,
                        call,
                        ToolOutput::error(reason),
                        touched,
                        &cwd,
                    )
                    .await?;
                    next += 1;
                }
                Decision::Ask => break,
            }
        }
        if in_flight.is_empty() {
            if next < calls.len() {
                // Ask 调用:串行执行(审批交互式,不并发)。
                let call = &calls[next];
                append_call(state, step, call)?;
                let output = dispatch_asked_tool_call(driver, state, &cwd, call).await;
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

/// 落一条 ToolCall 事件(过程透明:被拒/待审批的调用同样先落调用行)。
fn append_call(
    state: &TurnState,
    step: u32,
    call: &ToolCallRef,
) -> Result<(), denia_core::error::LlmFailure> {
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

/// 派发前的硬性校验:子代理白名单与未知工具。命中则返回错误结果,
/// 调用不进入策略判定(子代理绝不能弹出审批)。
fn reject_before_dispatch(
    driver: &SessionDriver,
    state: &TurnState,
    call: &ToolCallRef,
) -> Option<ToolOutput> {
    if state
        .session
        .header()
        .subagent
        .as_ref()
        .and_then(|s| s.allowed_tools.as_ref())
        .is_some_and(|allowed| !allowed.contains(&call.name))
    {
        return Some(ToolOutput::error("该工具不在当前子代理允许的工具集合中"));
    }
    if driver.tools().get(&call.name).is_none() {
        return Some(ToolOutput::error(format!(
            "unknown tool: {}\n该工具不存在;可用工具见系统提示的工具列表,核对名称后重试。",
            call.name
        )));
    }
    None
}

/// 策略判定:当前会话模式 × 调用类别 → Allow/Ask/Deny。
///
/// `memory_root` 是本会话工作区对应的项目记忆目录(None = 记忆未启用):
/// 命中的 `.md` 写分类为 MemoryWrite(四档放行,敏感段拒绝);子代理的
/// 其余写一律拒绝(提取子代理不能被诱导写记忆目录之外,子代理也不弹审批)。
fn decide_for(state: &TurnState, cwd: &Path, call: &ToolCallRef, memory_root: Option<&Path>) -> Decision {
    let mode = state.permission_mode();
    let confined = !mode.is_full() && state.session.header().sandbox;
    let class = classify_call(cwd, call, confined, memory_root);
    let is_memory_write = class == ActionClass::MemoryWrite;
    if is_memory_write {
        // 敏感段(git 钩子/依赖树/其他 harness 的技能目录)直接拒绝,
        // 即使落点算在记忆目录内(防符号链接与名字伪装的兜底)。
        if let Some(path) = crate::workspace_instructions::touched_path(cwd, &call.arguments)
            && denia_tools::permission::memory_path_is_sensitive(&path)
        {
            return Decision::Deny(format!(
                "路径 {} 命中敏感目录(git 钩子/依赖/技能等),禁止写入。",
                path.display()
            ));
        }
        return denia_tools::permission::decide(mode, class);
    }
    if state.session.header().subagent.is_some()
        && matches!(call.name.as_str(), "write_file" | "edit")
    {
        return Decision::Deny(
            "子代理的文件写仅限记忆目录内的 .md 文件(记忆提取);其余写操作留在父代理。".into(),
        );
    }
    denia_tools::permission::decide(mode, class)
}

/// 判定一次工具调用的操作类别(策略引擎输入)。
///
/// 分类宽容:参数取不到时按读类/区内放行,让工具自身的参数错误兜底,
/// 避免分类失败伪装成权限问题。
fn classify_call(cwd: &Path, call: &ToolCallRef, confined: bool, memory_root: Option<&Path>) -> ActionClass {
    match call.name.as_str() {
        "exit_plan" => ActionClass::PlanSubmit,
        "write_file" | "edit" => match crate::workspace_instructions::touched_path(cwd, &call.arguments)
        {
            // 记忆目录内的 .md 写:独立类别(harness 行为,四档放行)。
            Some(path)
                if memory_root.is_some_and(|root| path.starts_with(root))
                    && path.extension().is_some_and(|ext| ext == "md") =>
            {
                ActionClass::MemoryWrite
            }
            Some(path) if path.starts_with(cwd) => ActionClass::WriteInside,
            // 真越界(confined 时工具本就会拒绝 `..` 逃逸,按区内处理)。
            Some(_) if !confined => ActionClass::WriteOutside,
            _ => ActionClass::WriteInside,
        },
        "bash" | "job_start" => {
            let command = serde_json::from_str::<serde_json::Value>(call.arguments.trim())
                .ok()
                .and_then(|value| {
                    value
                        .get("command")
                        .and_then(|c| c.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_default();
            if denia_tools::permission::bash_may_write(&command) {
                ActionClass::BashWrite
            } else {
                ActionClass::Read
            }
        }
        _ => ActionClass::Read,
    }
}

/// 判定一次调用的路径参数是否锚定在项目记忆目录内(读写皆含):
/// `write_file/edit/read_file/ls/glob/grep` 的 `path` 解析后落在
/// `memory_root` 内即命中。命中者对沙箱 confined 豁免(见
/// [`dispatch_tool_call`] 内注释);无 path 参数(如 grep 缺省全工作区)
/// 与非 path 型工具(如 bash,本就不受 confined 约束)不在此列。
fn memory_anchored(cwd: &Path, call: &ToolCallRef, memory_root: Option<&Path>) -> bool {
    let Some(root) = memory_root else {
        return false;
    };
    if !matches!(
        call.name.as_str(),
        "write_file" | "edit" | "read_file" | "ls" | "glob" | "grep"
    ) {
        return false;
    }
    crate::workspace_instructions::touched_path(cwd, &call.arguments)
        .is_some_and(|path| path.starts_with(root))
}

/// 调度一次 Allow 类工具调用,返回 boxed future 供并行池使用。
/// 取消与审批(Ask)路径在别处处理。
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
    let tools: Arc<denia_tools::ToolRegistry> = driver.tools();
    let cancel = state.cancel.clone();
    let file_history = state.file_history.clone();
    let vision_supported = state.vision_supported;
    let read_state = state.read_state.clone();
    let ask = driver.ask.clone();
    let call_id = call.id.clone();
    // 记忆域沙箱豁免:锚定记忆目录的读写不受 confined 限制,否则默认
    // 沙箱会话按 tool:memory 纪律读写记忆会在工具路径解析层被拦(权限
    // 层早已放行,纪律段成为空头支票)。豁免口径与权限放行口径一致:
    // 写边界仍由 MemoryWrite 分类(敏感段拒绝、子代理仅限记忆目录)收敛。
    // 在 Box::pin 之前计算:闭包是 'static,不能借用 driver。
    let confined = !state.permission_mode().is_full()
        && state.session.header().sandbox
        && !memory_anchored(
            &cwd,
            &call,
            driver
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.memory_root_for(&cwd))
                .as_deref(),
        );
    Box::pin(async move {
        if cancel.is_cancelled() {
            return ToolOutput::error("工具调用在派发前已被中断");
        }
        let tool = match tools.get(&call.name) {
            Some(tool) => tool,
            None => return ToolOutput::error("unknown tool"), // 派发前已校验,理论不可达
        };
        let current_mode = session.permission_mode();
        let sink_session = session.clone();
        let sink_emit = emit.clone();
        let goal_session = session.clone();
        let context = denia_tools::ToolContext {
            session_id: Some(session.id().to_string()),
            selection: Some(selection),
            cwd: cwd.clone(),
            cancel: cancel.child_token(),
            confined,
            vision_supported,
            emit_event: Some(Arc::new(move |event: SessionEvent| {
                if let Ok(envelope) = sink_session.append(event) {
                    sink_emit(&envelope);
                }
            })),
            file_history,
            permission_mode: current_mode,
            ask,
            call_id: Some(call_id),
            goal_reader: Some(Arc::new(move || {
                let goal = goal_session.goal()?;
                let used = goal_session.goal_tokens_used().unwrap_or(0);
                Some((goal, used))
            })),
            read_state: Some(read_state),
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

/// 执行一次策略 Ask 的调用(串行):落审批事件 → 等用户决策 → 落决策
/// 事件 → 执行或合成结果。`exit_plan` 的批准会切换执行档位与模型。
async fn dispatch_asked_tool_call(
    driver: &SessionDriver,
    state: &mut TurnState,
    cwd: &Path,
    call: &ToolCallRef,
) -> ToolOutput {
    if state.cancel.is_cancelled() {
        return ToolOutput::error("工具调用在派发前已被中断");
    }
    let is_plan = call.name == "exit_plan";
    // 计划参数先校验:空计划不该弹出审批卡打扰用户。
    if is_plan
        && let Err(message) = denia_tools::ExitPlanArgs::from_raw(&call.arguments)
    {
        return ToolOutput::error(format!("计划提交参数无效:{message}"));
    }
    let Some(approval) = &driver.approval else {
        return ToolOutput::error(if is_plan {
            "计划审批需要用户审批通道,但当前没有可用的审批通道;请把计划直接写在回复里,请用户手动切换权限模式。".to_string()
        } else {
            "该操作需要用户审批,但当前没有可用的审批通道;请改用工作区内的路径,或请用户调整权限模式。".to_string()
        });
    };
    let request_id = uuid::Uuid::new_v4().to_string();
    let reason = if is_plan {
        "计划审批:批准后会话自动切换执行档位并继续执行".to_string()
    } else {
        "该操作要写工作区之外的路径,需要用户确认".to_string()
    };
    if let Err(error) = append(
        &state.session,
        &state.emit,
        SessionEvent::ApprovalAsked {
            request_id: request_id.clone(),
            call_id: call.id.clone(),
            tool: call.name.clone(),
            args_preview: call.arguments.clone(),
            reason: Some(reason),
        },
    ) {
        return ToolOutput::error(format!("[{}] {}", error.code, error.message));
    }
    let decision = approval
        .request(state.session.id(), &request_id, state.cancel.clone())
        .await;
    if let Err(error) = append(
        &state.session,
        &state.emit,
        SessionEvent::ApprovalDecided {
            request_id: request_id.clone(),
            outcome: decision.outcome,
        },
    ) {
        return ToolOutput::error(format!("[{}] {}", error.code, error.message));
    }
    match decision.outcome {
        ApprovalOutcome::AllowedOnce => {
            if is_plan {
                apply_plan_approval(state, decision)
            } else {
                dispatch_tool_call(driver, state, cwd, call).await
            }
        }
        ApprovalOutcome::Rejected => {
            if is_plan {
                let mut text = String::from("用户拒绝了该计划。");
                let feedback = decision
                    .feedback
                    .as_deref()
                    .map(str::trim)
                    .filter(|f| !f.is_empty());
                match feedback {
                    Some(feedback) => {
                        text.push_str("请按以下补充建议修订计划后重新提交:\n");
                        text.push_str(feedback);
                    }
                    None => text.push_str(
                        "请修订计划后重新提交;不确定用户的顾虑时,先用 ask 工具询问。",
                    ),
                }
                ToolOutput::text(text)
            } else {
                ToolOutput::error("用户拒绝了该操作;请改用工作区内的方案,或先询问用户。")
            }
        }
        ApprovalOutcome::Cancelled => ToolOutput::error("审批已被取消(用户退出或轮次被中断)。"),
        ApprovalOutcome::Unavailable => {
            ToolOutput::error("该操作需要用户审批,但当前没有可用的审批通道。")
        }
    }
}

/// 落计划批准的副作用:切换执行档位(事件)与执行模型(TurnState),
/// 并合成给模型的继续执行指令。
fn apply_plan_approval(state: &mut TurnState, decision: PlanReviewDecision) -> ToolOutput {
    // 执行档位只接受自动编辑/完全访问;批准进计划档没有意义,兜底为自动编辑。
    let mode = match decision.execute_mode {
        Some(PermissionMode::ReadOnly) | None => PermissionMode::AutoEdit,
        Some(mode) => mode,
    };
    if mode != state.permission_mode()
        && let Err(error) = append(
            &state.session,
            &state.emit,
            SessionEvent::PermissionMode { mode },
        )
    {
        return ToolOutput::error(format!("[{}] {}", error.code, error.message));
    }
    let switched_model = decision.selection.is_some();
    if let Some(selection) = decision.selection {
        state.selection = selection;
    }
    if let Some(vision) = decision.vision_supported {
        state.vision_supported = vision;
    }
    let mut text = format!("计划已批准,会话已切换至 {} 模式,请开始执行。", mode.as_str());
    if switched_model {
        text.push_str("执行模型已按用户选择切换。");
    }
    if let Some(feedback) = decision
        .feedback
        .as_deref()
        .map(str::trim)
        .filter(|f| !f.is_empty())
    {
        text.push_str("\n用户补充建议(执行时一并落实):\n");
        text.push_str(feedback);
    }
    ToolOutput::text(text)
}

#[cfg(test)]
mod memory_anchor_tests {
    use super::*;

    fn call(name: &str, arguments: &str) -> ToolCallRef {
        ToolCallRef {
            id: "c1".into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    const ROOT: &str = "/home/u/.denia/memories/x/memory";

    #[test]
    fn anchored_only_for_memory_root_paths_of_path_tools() {
        let cwd = Path::new("/ws");
        let root = Some(Path::new(ROOT));
        // 落点在记忆目录内:path 型读写工具全部豁免。
        for name in ["write_file", "edit", "read_file", "ls", "glob", "grep"] {
            assert!(
                memory_anchored(cwd, &call(name, r#"{"path":"/home/u/.denia/memories/x/memory/a.md"}"#), root),
                "{name} 锚定记忆目录应豁免沙箱"
            );
        }
        // 落点在记忆目录外/无 path 参数/非 path 型工具:不豁免。
        assert!(!memory_anchored(
            cwd,
            &call("write_file", r#"{"path":"/ws/a.md"}"#),
            root
        ));
        assert!(!memory_anchored(
            cwd,
            &call("read_file", r#"{"path":"/etc/passwd"}"#),
            root
        ));
        assert!(!memory_anchored(cwd, &call("grep", r#"{"pattern":"x"}"#), root));
        assert!(!memory_anchored(cwd, &call("bash", r#"{"command":"echo hi"}"#), root));
        // 记忆未启用(None):一律不豁免。
        assert!(!memory_anchored(
            cwd,
            &call("write_file", r#"{"path":"/home/u/.denia/memories/x/memory/a.md"}"#),
            None
        ));
        // 相对路径消解到记忆目录内同样命中(以记忆目录为锚的相对写法)。
        assert!(memory_anchored(
            Path::new(ROOT),
            &call("read_file", r#"{"path":"MEMORY.md"}"#),
            root
        ));
    }
}
