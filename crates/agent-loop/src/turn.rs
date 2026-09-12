//! turn/step 主编排:一个 turn 从用户输入到轮次闭合的全过程。
//!
//! step 循环体:背景注入 → 装配系统提示/工具 → 请求派发(attempt 重试环)
//! → LoopGuard 死循环检测 → 伪调用救援 → 工具执行。
//! 轮次终止条件:无工具调用(Completed)、MaxTokens、错误(自纠耗尽/配置
//! 问题/存储故障)、用户取消、死循环检测触发。

use std::path::PathBuf;

use denia_core::error::{LlmFailure, codes};
use denia_core::message::ToolCallRef;
use denia_core::session::{SessionEvent, TurnEndReason};
use denia_core::stream::{ContentBlock, FinishReason};
use denia_session::Session;
use denia_system_prompt::{
    AssembleContext, PromptAssembly, frame_system_prompt_for_model, render_prompt,
    render_prompt_for_user,
};
use denia_token_meter::estimate_system_tokens;

use crate::errors::MAX_FEEDBACK;
use crate::injections::{InjectionBaselines, refresh_background_injections};
use crate::rescue::{detect_fake_names, fake_tool_call_feedback, log_rescued, rescue_from_blocks};
use crate::runtime_context::RuntimeContextProjection;
use crate::workspace_instructions::skill_gesture;
use crate::{SessionDriver, TurnState, append, should_log_system_prompt};

/// 一次 turn 内跨 step 的可变装配状态(注入基准、touched 路径、手势正文)。
struct TurnAssembly {
    baselines: InjectionBaselines,
    projection: RuntimeContextProjection,
    touched: Vec<PathBuf>,
    gesture_skill: Option<(String, String, String)>,
}

pub(crate) async fn run_turn_inner(
    driver: &SessionDriver,
    state: &mut TurnState,
    prompt: &str,
    images: Vec<denia_core::message::ImageData>,
    files: Vec<String>,
    quoted: Vec<(String, String)>,
) -> Result<TurnEndReason, LlmFailure> {
    state.turn = state.session.next_turn_number();
    let cwd = state.cwd();
    state.file_history = match &driver.file_history {
        Some(provider) => provider.backend(state.session.id(), &cwd).await,
        None => None,
    };
    prepare_turn_input(driver, state, prompt, images, files, quoted, &cwd).await?;
    // 路由容量解析一次(解析失败不影响主流程;报错摘要与 request-context 用)。
    state.context_window = driver
        .registry
        .resolve_call(
            &state.selection.provider,
            &state.selection.model,
            state.selection.reasoning_effort.as_deref(),
        )
        .await
        .ok()
        .and_then(|resolved| resolved.context_window);

    let mut assembly = TurnAssembly {
        baselines: InjectionBaselines::restore(state),
        projection: RuntimeContextProjection::restore(&state.session),
        touched: Vec::new(),
        gesture_skill: detect_gesture_skill(driver, state, prompt, &cwd).await,
    };

    'step_loop: loop {
        // —— 背景注入(工作区指令/能力上下文/技能目录),失败不阻断 ——
        refresh_background_injections(driver, state, &mut assembly.baselines, &assembly.touched)
            .await?;
        let step = state.turn_step_next();
        append(
            &state.session,
            &state.emit,
            SessionEvent::StepStart {
                turn: state.turn,
                step,
            },
        )?;

        // —— 装配系统提示 + 工具集 ——
        let prompt_assembly = match assemble_step(driver, state) {
            Ok(assembly) => assembly,
            Err(error) => {
                // 日志平衡:step 已开,补 step-end + turn-end(对齐 dsh 的
                // finally 配对语义;不再让轮次裸开等 load 合成)。
                let failure = LlmFailure::new(codes::UNKNOWN, error);
                append(
                    &state.session,
                    &state.emit,
                    SessionEvent::StepEnd {
                        turn: state.turn,
                        step,
                    },
                )?;
                let reason = TurnEndReason::Error {
                    failure: failure.clone(),
                };
                append(
                    &state.session,
                    &state.emit,
                    SessionEvent::TurnEnd {
                        turn: state.turn,
                        reason: reason.clone(),
                    },
                )?;
                return Ok(reason);
            }
        };

        // 运行时上下文快照:渲染变化才注入(通道幂等)。
        if let Some(snapshot) = assembly.projection.project(&prompt_assembly) {
            append(
                &state.session,
                &state.emit,
                SessionEvent::UserMessage {
                    text: snapshot,
                    injected: true,
                    channel: Some("runtime-context".into()),
                    images: Vec::new(),
                },
            )?;
        }

        // UI 副本只展示 User audience 的 sections(身份 + persona),
        // 工具纪律/工具使用说明等 Model audience 段不入日志副本;
        // model 实际收到的是完整 prompt(全部 audience)+ 权威框架。
        let prompt_body = render_prompt_for_user(&prompt_assembly);
        if should_log_system_prompt(&state.session, step, &prompt_body) {
            append(
                &state.session,
                &state.emit,
                SessionEvent::SystemPrompt {
                    turn: state.turn,
                    step,
                    text: prompt_body,
                },
            )?;
        }
        let model_prompt = render_prompt(&prompt_assembly);
        let framed_system = frame_system_prompt_for_model(&model_prompt);
        state
            .session
            .set_system_tokens(estimate_system_tokens(&framed_system));

        // 用户 /技能名 手势的正文:全部背景注入之后,最贴近模型的回答
        // (dsh material-last 顺序);只发一次(第一个 step)。
        if let Some((name, source, body)) = assembly.gesture_skill.take() {
            append(
                &state.session,
                &state.emit,
                SessionEvent::UserMessage {
                    text: format!("[技能正文 {name}（来源：{source}）]\n{body}"),
                    injected: true,
                    channel: Some("gesture-skill".into()),
                    images: Vec::new(),
                },
            )?;
        }

        // —— 请求派发(attempt 重试环;头部/上下文/压缩在 dispatch 前落盘)——
        let outcome = crate::request::dispatch_request(
            driver,
            state,
            step,
            &framed_system,
            &prompt_assembly.tools,
        )
        .await?;
        match outcome {
            crate::request::RequestOutcome::TurnEnded(reason) => return Ok(reason),
            crate::request::RequestOutcome::RestartStep => continue 'step_loop,
            crate::request::RequestOutcome::Step { blocks, finish } => {
                // —— 死循环检测:两级处置(先提醒、后中断)——
                // 连续第 3 次相同 → 注入提醒让模型自纠(工作继续);
                // 连续第 4 次相同 → 强制中断本轮(提醒无效时的兜底)。
                match state.loop_guard.observe(&blocks) {
                    crate::loop_guard::LoopVerdict::Continue => {}
                    crate::loop_guard::LoopVerdict::Warn { .. } => {
                        append(
                            &state.session,
                            &state.emit,
                            SessionEvent::UserMessage {
                                text: crate::loop_guard::LOOP_WARN_TEXT.to_string(),
                                injected: true,
                                channel: Some("loop-warning".into()),
                                images: Vec::new(),
                            },
                        )?;
                    }
                    crate::loop_guard::LoopVerdict::Block { repeats } => {
                        append(
                            &state.session,
                            &state.emit,
                            SessionEvent::StepEnd {
                                turn: state.turn,
                                step,
                            },
                        )?;
                        let reason = TurnEndReason::LoopDetected { repeats };
                        append(
                            &state.session,
                            &state.emit,
                            SessionEvent::TurnEnd {
                                turn: state.turn,
                                reason: reason.clone(),
                            },
                        )?;
                        return Ok(reason);
                    }
                }

                let mut calls: Vec<ToolCallRef> = blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                        } => Some(ToolCallRef {
                            id: id.clone(),
                            name: name.clone(),
                            arguments: arguments.clone(),
                        }),
                        _ => None,
                    })
                    .collect();
                let hit_max_tokens = matches!(finish, Some(FinishReason::MaxTokens));

                if calls.is_empty() && !hit_max_tokens {
                    // —— 文本伪调用救援 ——
                    // 标签可完整解析时直接代为执行(走同一条 dispatch,
                    // 权限/沙箱/审批一视同仁);解析失败退回注入自纠;
                    // 配额耗尽按原样收尾。
                    if let Some(rescued) = rescue_from_blocks(&blocks) {
                        log_rescued(&rescued);
                        calls = rescued
                            .into_iter()
                            .map(|call| ToolCallRef {
                                id: uuid::Uuid::new_v4().to_string(),
                                name: call.name,
                                arguments: serde_json::to_string(&call.arguments)
                                    .unwrap_or_else(|_| "{}".to_string()),
                            })
                            .collect();
                    } else {
                        let fake_names = detect_fake_names(&blocks);
                        if !fake_names.is_empty() && state.feedback < MAX_FEEDBACK {
                            state.feedback += 1;
                            append(
                                &state.session,
                                &state.emit,
                                SessionEvent::StepEnd {
                                    turn: state.turn,
                                    step,
                                },
                            )?;
                            append(
                                &state.session,
                                &state.emit,
                                SessionEvent::UserMessage {
                                    text: fake_tool_call_feedback(&fake_names),
                                    injected: true,
                                    channel: Some("feedback".into()),
                                    images: Vec::new(),
                                },
                            )?;
                            continue 'step_loop;
                        }
                    }
                }

                if calls.is_empty() || hit_max_tokens {
                    append(
                        &state.session,
                        &state.emit,
                        SessionEvent::StepEnd {
                            turn: state.turn,
                            step,
                        },
                    )?;
                    let reason = if hit_max_tokens {
                        TurnEndReason::MaxTokens
                    } else {
                        TurnEndReason::Completed
                    };
                    append(
                        &state.session,
                        &state.emit,
                        SessionEvent::TurnEnd {
                            turn: state.turn,
                            reason: reason.clone(),
                        },
                    )?;
                    return Ok(reason);
                }

                // —— 工具并行执行(滚动池;升权串行;结果兜底截断)——
                crate::exec::execute_calls(driver, state, step, &calls, &mut assembly.touched)
                    .await?;
                // 计一次工具轮次:rapid-refill 断路器靠它判断"压缩后多久又满"。
                driver
                    .tool_turns_since_compact
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // 本 step 闭合(工具与结果全部落盘后)。
                append(
                    &state.session,
                    &state.emit,
                    SessionEvent::StepEnd {
                        turn: state.turn,
                        step,
                    },
                )?;
            }
        }
    }
}

/// turn 开始前的输入注入:上传文件通知 → 轨迹引用 → 真实用户消息
/// (文件快照) → turn-start。顺序即模型看到的顺序。
async fn prepare_turn_input(
    driver: &SessionDriver,
    state: &mut TurnState,
    prompt: &str,
    images: Vec<denia_core::message::ImageData>,
    files: Vec<String>,
    quoted: Vec<(String, String)>,
    cwd: &std::path::Path,
) -> Result<(), LlmFailure> {
    if !files.is_empty() {
        let list = files
            .iter()
            .map(|path| format!("- {path}"))
            .collect::<Vec<_>>()
            .join("\n");
        append(
            &state.session,
            &state.emit,
            SessionEvent::UserMessage {
                text: format!(
                    "[harness] 用户上传了文件:\n{list}\n这些文件已保存,可随时用工具读取。"
                ),
                injected: true,
                channel: Some("file-notice".into()),
                images: Vec::new(),
            },
        )?;
    }
    if !quoted.is_empty() {
        // 轨迹引用:与文件通知同一注入通道,真实用户消息之前落库,
        // 模型在看到提问前先看到引用内容。
        let body = quoted
            .iter()
            .map(|(title, text)| format!("---\n[{title}]\n{text}\n"))
            .collect::<Vec<_>>()
            .join("\n");
        append(
            &state.session,
            &state.emit,
            SessionEvent::UserMessage {
                text: format!(
                    "[harness] 用户从轨迹视图引用了以下记录,请结合这些内容回答:\n\n{body}"
                ),
                injected: true,
                channel: Some("quote".into()),
                images: Vec::new(),
            },
        )?;
    }
    if !prompt.is_empty() {
        let user_envelope = append(
            &state.session,
            &state.emit,
            SessionEvent::UserMessage {
                text: prompt.to_string(),
                injected: false,
                channel: None,
                images,
            },
        )?;
        if let Some(provider) = &driver.file_history
            && let Err(error) = provider
                .snapshot(state.session.id(), cwd, user_envelope.seq)
                .await
        {
            // 快照失败不阻断本轮对话;但该回退点会缺失文件历史,记录日志便于排查。
            tracing::warn!(
                session_id = state.session.id(),
                seq = user_envelope.seq,
                error = %error,
                "file history snapshot failed"
            );
        }
    }
    append(
        &state.session,
        &state.emit,
        SessionEvent::TurnStart { turn: state.turn },
    )?;
    Ok(())
}

/// 用户手势:真实用户消息(injected=false,注入文本无法伪造)首行
/// /技能名 直接加载技能正文;未知名与非 user_invocable 技能保持普通文本。
async fn detect_gesture_skill(
    driver: &SessionDriver,
    state: &TurnState,
    prompt: &str,
    cwd: &std::path::Path,
) -> Option<(String, String, String)> {
    let runtime = driver.runtime.as_ref()?;
    let name = skill_gesture(prompt)?;
    match runtime.user_skill(state.session.id(), &name, cwd).await {
        Ok(Some((source, body))) => Some((name, source, body)),
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(
                session_id = state.session.id(),
                skill = %name,
                error = %error,
                "skill gesture load failed"
            );
            None
        }
    }
}

/// 工具纪律段名 → 它对应的工具名(可能多个)。
///
/// 段与工具严格同步是 AGENTS.md 的硬要求;子代理按 `allowed_tools` 过滤时,
/// 工具和它的纪律段必须同进退。返回 `None` 表示该段不绑定具体工具
/// (`harness:`/`context:`/`deployment:` 等),不参与过滤。
///
/// 新增 `tool:<族>` 段时必须在此登记,否则子代理会读到不存在的工具纪律
/// (有单测覆盖:`subagent_sections_follow_tool_grant`)。
pub(crate) fn section_tools(section: &str) -> Option<&'static [&'static str]> {
    Some(match section {
        "tool:bash" => &["bash"],
        "tool:ls" => &["ls"],
        "tool:read" => &["read_file"],
        "tool:write" | "tool:todo" => &["write_file", "todo_write"],
        "tool:glob" => &["glob"],
        "tool:grep" => &["grep"],
        "tool:edit" => &["edit"],
        "tool:agents" => &["spawn_agent", "fork_agent", "send_message"],
        "tool:jobs" => &["job_start", "job_output", "job_kill"],
        "tool:skill" => &["skill"],
        "tool:browser" => &["browser"],
        "tool:ask" => &["ask"],
        "tool:goal" => &["get_goal", "update_goal"],
        "tool:plan" => &["exit_plan"],
        // 记忆沉淀复用 write_file/edit;映射让提取子代理(白名单含这两个
        // 写工具)能看到纪律段,只读子代理看不到。
        "tool:memory" => &["write_file", "edit"],
        _ => return None,
    })
}

/// 装配本 step 的系统提示与工具集。
/// 系统提示热更新不丢能力(bash schema 回填实际注册表版本);
/// 子代理 persona 覆盖 + 工具白名单过滤。
fn assemble_step(
    driver: &SessionDriver,
    state: &TurnState,
) -> Result<PromptAssembly, String> {
    let cwd = state.session.header().cwd.clone();
    let mut assembly = driver.system_prompt.load().assemble(&AssembleContext {
        cwd: Some(cwd),
        model: Some(state.selection.model.clone()),
        provider: Some(state.selection.provider.clone()),
        permission_mode: Some(state.session.permission_mode().as_str().to_string()),
    })?;
    // 扩展工具随实际注册表装配,自定义 SYSTEM.md 热更新不会丢失能力。
    if driver.runtime.is_some() {
        if let (Some(schema), Some(tool)) = (
            assembly.tools.iter_mut().find(|s| s.name == "bash"),
            driver.tools().get("bash"),
        ) {
            *schema = tool.schema().clone();
        }
        for schema in denia_tools::capabilities::schemas() {
            if !assembly.tools.iter().any(|s| s.name == schema.name) {
                assembly.tools.push(schema);
            }
        }
    }
    if let Some(child) = &state.session.header().subagent {
        if let Some(persona) = &child.persona
            && let Some(section) = assembly
                .sections
                .iter_mut()
                .find(|s| s.name == "deployment:persona")
        {
            section.text =
                format!("{persona}\n始终使用简体中文回复，除非用户明确要求其他语言。");
        }
        if let Some(allowed) = &child.allowed_tools {
            assembly.tools.retain(|s| allowed.contains(&s.name));
            // 纪律段与工具同进退(AGENTS.md 的同步要求):子代理拿不到的工具,
            // 其纪律段不得注入——否则模型读到 bash/ask/write 的纪律却找不到
            // 对应工具,既浪费 token 又误导。段名到工具的映射见
            // `section_tools`;context:/harness: 段不受工具集影响。
            assembly
                .sections
                .retain(|section| match section_tools(&section.name) {
                    Some(tools) => tools.iter().any(|name| allowed.contains(&name.to_string())),
                    None => true,
                });
        }
    }
    // 权限模式决定工具面:计划档隐藏 write_file/edit(执行面只读,bash
    // 保留给只读命令);非计划档隐藏 exit_plan。schema 与纪律段同进退
    // (AGENTS.md 同步要求,映射见 `section_tools`)。
    let mode = state.session.permission_mode();
    let mode_denies = |name: &str| match mode {
        denia_core::session::PermissionMode::Plan => name == "write_file" || name == "edit",
        _ => name == "exit_plan",
    };
    if assembly.tools.iter().any(|s| mode_denies(&s.name)) {
        assembly.tools.retain(|s| !mode_denies(&s.name));
        assembly
            .sections
            .retain(|section| match section_tools(&section.name) {
                Some(tools) => tools.iter().any(|name| !mode_denies(name)),
                None => true,
            });
    }
    // 项目记忆:纪律段仅在记忆启用时注入(runtime.memory_root_for 与
    // 权限层、注入通道同源);未注册(如无 runtime 部署)自然不存在。
    let session_cwd = std::path::PathBuf::from(state.session.header().cwd.clone());
    if assembly.sections.iter().any(|section| section.name == "tool:memory")
        && !driver
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.memory_root_for(&session_cwd).is_some())
    {
        assembly
            .sections
            .retain(|section| section.name != "tool:memory");
    }
    let tools_tokens = serde_json::to_string(&assembly.tools)
        .map(|json| denia_token_meter::estimate_tools_tokens(&json))
        .unwrap_or(0);
    state.session.set_tools_tokens(tools_tokens);
    Ok(assembly)
}

/// 本 turn 的下一个 step 号:反向扫描到 TurnStart 为止,统计本 turn 已有
/// 的 step 数(通常个位数,扫描成本可忽略)。
impl TurnState {
    pub(crate) fn turn_step_next(&self) -> u32 {
        self.session
            .events()
            .iter()
            .rev()
            .take_while(|envelope| !matches!(envelope.event, SessionEvent::TurnStart { .. }))
            .filter(|envelope| {
                matches!(
                    envelope.event,
                    SessionEvent::StepStart { turn, .. } if turn == self.turn
                )
            })
            .count() as u32
            + 1
    }
}

/// 保留对 Session 的类型引用(工具 emit_event sink 由 exec 模块使用)。
#[allow(unused)]
fn _session_type_witness(_s: &Session) {}
