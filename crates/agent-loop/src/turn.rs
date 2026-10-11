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
use denia_core::stream::FinishReason;
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
    /// system 字节冻结基准:日志最后一条请求头携带的 system 全文,即
    /// 提供方缓存前缀的第一段。None = 会话尚未发过任何请求。
    system_frozen: Option<String>,
    /// 子代理运行快照（角色补充 + 冻结授权）；非子代理为 None。
    subagent: Option<denia_tools::capabilities::SubagentPrompt>,
}

/// 子代理角色段：定义 instructions + 任务职责，**追加**在父基础提示之后。
pub(crate) const SUBAGENT_ROLE_SECTION: &str = "subagent:role";
/// 子代理约束段：禁止派遣、汇报方式、身份与权限边界。
pub(crate) const SUBAGENT_CONSTRAINTS_SECTION: &str = "subagent:constraints";

/// 没有定义管理能力的部署（或旧会话）的降级路径：按旧描述符构造保守快照。
fn legacy_subagent_prompt(
    descriptor: &denia_core::session::SubagentDescriptor,
) -> denia_tools::capabilities::SubagentPrompt {
    denia_tools::capabilities::SubagentPrompt {
        name: descriptor
            .name
            .clone()
            .or_else(|| Some(descriptor.label.clone())),
        instructions: descriptor.persona.clone().unwrap_or_default(),
        effective_tools: descriptor.effective_tools.clone().unwrap_or_else(|| {
            denia_core::subagent::legacy_child_tools(descriptor.allowed_tools.as_deref())
        }),
        permission_ceiling: Some(descriptor.permission_ceiling.as_str().to_string()),
        parent_preset_persona: descriptor.parent_preset_persona.clone(),
        legacy: true,
    }
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
    // 子代理运行快照：角色正文与冻结授权。**读取校验失败即拒绝本轮**，不回退
    // 到默认角色或更宽的工具面（计划 5.3）。
    let subagent = match (
        driver.runtime.as_ref(),
        state.session.header().subagent.as_ref(),
    ) {
        (Some(runtime), Some(descriptor)) => {
            // 本轮已尝试处理的派遣消息先结算；校验期间或之后到达的消息留给后续领取。
            runtime
                .drain(state.session.id())
                .await
                .map_err(|error| LlmFailure::new(codes::UNKNOWN, error))?;
            match runtime.subagent_prompt(state.session.id()).await {
                Ok(Some(parts)) => Some(parts),
                Ok(None) => Some(legacy_subagent_prompt(descriptor)),
                Err(error) => {
                    let reason = TurnEndReason::Error {
                        failure: LlmFailure::new(
                            codes::UNKNOWN,
                            format!("子代理运行快照不可用，已拒绝启动：{error}"),
                        ),
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
            }
        }
        _ => None,
    };
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
        baselines: InjectionBaselines::restore(&state.session),
        projection: RuntimeContextProjection::restore(&state.session),
        touched: Vec::new(),
        gesture_skill: detect_gesture_skill(driver, state, prompt, &cwd).await,
        system_frozen: last_request_header_system(&state.session),
        subagent,
    };

    'step_loop: loop {
        // —— 执行预算准入(步边界)——
        // 超限走现有收尾路径(TurnEnd + Error),不新造 TurnEndReason:
        // 上层(goal 续跑/失败计数)读的是同一套结局分类。
        if let Err(refusal) = state.budget.admit_step() {
            return close_budget(state, &refusal);
        }
        // —— 背景注入(工作区指令/能力上下文/技能目录),失败不阻断 ——
        refresh_background_injections(driver, state, &mut assembly.baselines, &assembly.touched)
            .await?;
        let real_input = state.session.with_events(|events| {
            events
                .iter()
                .rev()
                .find_map(|event| {
                    matches!(
                        &event.event,
                        SessionEvent::UserMessage {
                            injected: false,
                            ..
                        }
                    )
                    .then_some(event.seq)
                })
                .unwrap_or(0)
        });
        if real_input != state.last_real_input {
            state.loop_guard.reset();
            state.last_real_input = real_input;
        }
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
        let prompt_assembly = match assemble_step(driver, state, assembly.subagent.as_ref()) {
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
        let framed_current = frame_system_prompt_for_model(&model_prompt);
        // —— 系统提示变更 in-history 追加(对齐 dsh `systemPromptUpdate: 'in-history'`)——
        // wire 层把 system 序列化为请求首条消息:已发过请求的会话里改写它,
        // 等于从第 0 个 token 打碎提供方前缀缓存。因此 system 字节冻结在
        // 最后一次发出的值;提示词变化(权限模式段位进退 / SYSTEM.md 热更新 /
        // 模型名插值)把新提示词全文以注入消息追加到已缓存历史之后,声明
        // 取代旧版。首个请求直接采用当前提示词(RequestHeader 落盘即基准)。
        // 是否"已经发过"看**模型面**还留不留着这条注入(`InjectionBaselines::system_update`
        // 从 surface 恢复):压缩只把事件移出模型面,日志仍 append-only,读日志会
        // 得出"已经发过"的错误结论。冻结基准本身仍来自 `RequestHeader`——提示词
        // 与冻结值一致时本来就不该发,这条语义不变。
        let framed_system = match assembly.system_frozen.as_deref() {
            Some(frozen) if frozen != framed_current => {
                let already_sent = assembly
                    .baselines
                    .system_update
                    .as_deref()
                    .and_then(extract_system_update)
                    == Some(framed_current.as_str());
                if !already_sent {
                    let text = format!(
                        "<system-reminder>\n系统提示更新:以下为当前生效的完整系统指令,\
                         取代此前系统消息中的旧版本;与旧版本冲突时以本更新为准。\n\n\
                         {framed_current}\n</system-reminder>"
                    );
                    append(
                        &state.session,
                        &state.emit,
                        SessionEvent::UserMessage {
                            text: text.clone(),
                            injected: true,
                            channel: Some(crate::injections::SYSTEM_UPDATE_CHANNEL.into()),
                            images: Vec::new(),
                        },
                    )?;
                    assembly.baselines.system_update = Some(text);
                }
                frozen.to_string()
            }
            _ => framed_current,
        };
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
                let mut calls = denia_core::message::assistant_from_blocks(&blocks)
                    .map(|message| message.tool_calls)
                    .unwrap_or_default();
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

                if calls.is_empty() && hit_max_tokens && state.continuations < 3 {
                    state.continuations += 1;
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
                        SessionEvent::RetryAttempt {
                            turn: state.turn,
                            step,
                            attempt: state.continuations,
                            code: "MAX_TOKENS_CONTINUATION".into(),
                            message: "输出达到预算，已保存部分响应，继续生成；最多三次。".into(),
                            delay_ms: 0,
                        },
                    )?;
                    append(&state.session, &state.emit, SessionEvent::UserMessage { text: "上次响应因输出预算截断。请从已有内容继续，不要重复已完成操作。截断或不完整的工具调用未执行，如仍需要该动作，请重新生成完整合法的调用。".into(), injected: true, channel: Some("output-continuation".into()), images: Vec::new() })?;
                    continue 'step_loop;
                }
                if calls.is_empty() {
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
                let results = state.session.with_events(|events| {
                    calls
                        .iter()
                        .filter_map(|call| {
                            events.iter().rev().find_map(|event| match &event.event {
                                SessionEvent::ToolResult {
                                    call_id,
                                    content,
                                    is_error,
                                    meta,
                                    replaces: None,
                                    ..
                                } if call_id == &call.id => {
                                    Some((content.clone(), *is_error, meta.clone()))
                                }
                                _ => None,
                            })
                        })
                        .collect::<Vec<_>>()
                });
                match state.loop_guard.observe(&calls, &results) {
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

                // 计一次工具轮次:rapid-refill 断路器靠它判断"压缩后多久又满"。
                state
                    .compaction_state
                    .tool_turns
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

/// 预算拒绝的收尾:与 `close_error` 同一条路径(落 TurnEnd + 返回 Error),
/// 但消息不走 `summarize_failure` —— 分类表里没有预算行,重写会把完整的
/// 预算结论降级成“未知错误”;账本给的消息本来就是完整句子。
fn close_budget(
    state: &TurnState,
    refusal: &denia_token_meter::BudgetRefusal,
) -> Result<TurnEndReason, LlmFailure> {
    let reason = TurnEndReason::Error {
        failure: LlmFailure::new(codes::BUDGET_EXHAUSTED, refusal.message.clone()),
    };
    append(
        &state.session,
        &state.emit,
        SessionEvent::TurnEnd {
            turn: state.turn,
            reason: reason.clone(),
        },
    )?;
    Ok(reason)
}

/// turn 开始前的输入注入:上传文件通知 → 轨迹引用 → 图片通知 → 真实用户消息
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
    if !images.is_empty() {
        // 图片通知:与 file-notice 同构的显式留痕。图片只挂在下一条真实用户
        // 消息的 images 字段上,模型侧没有对应文本;一旦下游(协议转换层、
        // provider)把图片 part 丢弃,上下文里就只剩用户那句纯文本提问,表现为
        // "模型不知道有图"且日志里看不出丢过。这条注入让"有几张图"成为文本事实。
        let kinds = image_kind_summary(&images);
        // 图片通知:内联 data URL 是首选视觉通道,但转换层/提供方一旦丢弃图片
        // part,模型上下文里就只剩那句纯文本提问。把落盘路径写成文本事实,
        // 模型就有 read_file 这条自救路径;都拿不到时才要求它明说,不许猜。
        let lines = image_paths_list(&images);
        append(
            &state.session,
            &state.emit,
            SessionEvent::UserMessage {
                text: format!(
                    "[harness] 用户上传了 {} 张图片({}),已作为图片附在紧随其后的用户消息中。\
                     {lines}\n若你在该消息里看不到图片内容,请用 read_file 读取上述路径;仍取不到\
                     再告知用户\"未收到图片\",不要猜测其内容。",
                    images.len(),
                    kinds
                ),
                injected: true,
                channel: Some("image-notice".into()),
                images: Vec::new(),
            },
        )?;
    }
    // 只发图不打字时 prompt 为空,但图片仍必须随消息落库(否则整条丢失)。
    if !prompt.is_empty() || !images.is_empty() {
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

/// 图片落盘路径清单(`\n- <path>` 形式);一张都没路径时返回空串。
pub(crate) fn image_paths_list(images: &[denia_core::message::ImageData]) -> String {
    let paths = images
        .iter()
        .filter_map(|image| image.path.as_deref())
        .map(|path| format!("- {path}"))
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return String::new();
    }
    format!("\n已同时保存到本地,路径如下:\n{}\n", paths.join("\n"))
}

/// 单张图的类型标签:常见 MIME 用惯用名,其余取 `image/` 后缀大写。
fn image_kind_label(mime: &str) -> String {
    match mime {
        "image/png" => "PNG".to_string(),
        "image/jpeg" => "JPEG".to_string(),
        "image/gif" => "GIF".to_string(),
        "image/webp" => "WEBP".to_string(),
        "image/bmp" => "BMP".to_string(),
        other => other.strip_prefix("image/").unwrap_or(other).to_uppercase(),
    }
}

/// 图片类型摘要(按 MIME 归类、保持首现顺序):`PNG×2、JPEG`。
pub(crate) fn image_kind_summary(images: &[denia_core::message::ImageData]) -> String {
    let mut labels: Vec<String> = Vec::new();
    let mut counts: Vec<usize> = Vec::new();
    for image in images {
        let label = image_kind_label(&image.mime);
        match labels.iter().position(|existing| *existing == label) {
            Some(index) => counts[index] += 1,
            None => {
                labels.push(label);
                counts.push(1);
            }
        }
    }
    labels
        .iter()
        .zip(counts)
        .map(|(label, count)| {
            if count > 1 {
                format!("{label}×{count}")
            } else {
                label.clone()
            }
        })
        .collect::<Vec<_>>()
        .join("、")
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

/// 日志中最后一条请求头携带的 system 字节:已发过请求的会话里,这就是
/// 提供方缓存前缀的第一段(in-history 追加的冻结基准)。
/// None = 会话尚未发过任何请求,当前提示词即为基准。
fn last_request_header_system(session: &Session) -> Option<String> {
    // 模型面：历史投影丢掉的旧请求头不作为冻结基准（否则会拿父代理的
    // 系统提示当本会话前缀）。
    session.with_model_events(|events| {
        events
            .iter()
            .rev()
            .find_map(|envelope| match &envelope.event {
                SessionEvent::RequestHeader { header, .. } => header.system.clone(),
                _ => None,
            })
    })
}

/// 从系统提示更新注入消息中提取携带的提示词全文(与写入时的
/// `framed_current` 逐字节一致),供恢复后的幂等比较。结构:
/// `<system-reminder>\n{头行}\n\n{framed_current}\n</system-reminder>`。
fn extract_system_update(message: &str) -> Option<&str> {
    let inner = message
        .strip_prefix("<system-reminder>\n")?
        .strip_suffix("\n</system-reminder>")?;
    inner.split_once("\n\n").map(|(_, body)| body)
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
        "tool:output" => &["read_tool_output"],
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
        "tool:task" => &["get_task", "update_task", "run_checks"],
        "tool:plan" => &["exit_plan"],
        "tool:preset" => &["create_preset"],
        "tool:webfetch" => &["web_fetch"],
        // 记忆沉淀复用 write_file/edit;映射让提取子代理(白名单含这两个
        // 写工具)能看到纪律段,只读子代理看不到。
        "tool:memory" => &["write_file", "edit"],
        _ => return None,
    })
}

/// 把会话选中的 agent preset 应用到本 step 的装配上。
///
/// 单独成函数是为了可测:装配管线其余部分依赖完整 `TurnState`,而这一步
/// 只依赖 driver 的 preset 名册。名册缺失(无 preset 的部署)时装配维持
/// 出厂全量组装。
pub(crate) fn apply_session_preset(
    driver: &SessionDriver,
    session_preset: Option<&str>,
    assembly: &mut PromptAssembly,
) {
    if let Some(preset) = driver.preset_for(session_preset) {
        crate::preset::apply_preset(assembly, &preset);
    }
}

/// 模型可见的 MCP 工具名前缀(与 `denia_mcp::qualify_tool_name` 的输出一致)。
pub(crate) const MCP_TOOL_PREFIX: &str = "mcp__";

/// 目录化装配:未装载的 `mcp__*` 工具整体移出请求工具面,只留目录工具
/// `mcp_list` 与已装载的条目。外部服务器的工具清单与 inputSchema 都不再
/// 常驻每个请求——模型需要外部能力时先用 `mcp_list` 发现,看中的工具
/// 直接按名调用,首次调用被 exec 层拦截返回参数定义并装载(见
/// `exec::mcp_load_intercept`),之后该工具全量进请求。
pub(crate) fn retain_loaded_mcp_tools(
    driver: &SessionDriver,
    session_id: &str,
    assembly: &mut PromptAssembly,
) {
    assembly.tools.retain(|schema| {
        !schema.name.starts_with(MCP_TOOL_PREFIX) || driver.mcp_loaded(session_id, &schema.name)
    });
}

/// 装配本 step 的系统提示与工具集。
///
/// 系统提示热更新不丢能力(bash schema 回填实际注册表版本)。
///
/// 子代理装配遵循"结构化继承，而非替换 persona"（计划 9.1）：
/// 1. 父基础提示（部署身份 + 主 preset persona）保持不动；
/// 2. 按子有效工具重建工具纪律段（拿不到的工具其纪律段不出现）；
/// 3. 追加 `subagent:role`（定义 instructions，追加不覆盖）；
/// 4. 追加 `subagent:constraints`（禁止派遣、汇报方式、身份与权限边界）。
///
/// 追加发生在 persona 独占过滤之后：即使父 preset 是"persona 独占"，
/// child 的强制约束段也必须存在。
fn assemble_step(
    driver: &SessionDriver,
    state: &TurnState,
    subagent: Option<&denia_tools::capabilities::SubagentPrompt>,
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
    // 会话选中的 agent preset 先收窄(工具面 + persona),子代理的
    // `allowed_tools`/角色提示词在其后继续收窄——继承即收窄,绝不放大。
    // 放在能力 schema 追加之后:preset 说"不给 bash"就得对追加进来的
    // 扩展工具同样生效。
    apply_session_preset(
        driver,
        state.session.agent_preset().as_deref(),
        &mut assembly,
    );
    // MCP 目录化:未装载的 `mcp__*` 工具整体移出工具面,模型经 mcp_list
    // 发现、按名调用、首次调用装载。放白名单收窄之后:被 preset/子代理
    // 白名单排除的 MCP 工具同样不进请求。
    retain_loaded_mcp_tools(driver, state.session.id(), &mut assembly);
    if let Some(child) = subagent {
        apply_subagent_context(&mut assembly, child);
    }
    // 只读档工具面收窄:bash 与写文件工具不开放(只留 ls/read_file/glob/grep
    // 等只读类),纪律段同步摘除。执行层 decide 矩阵仍兜底幻觉调用。子代理
    // 不在此列:其工具面由 allowed_tools 白名单收窄,且记忆提取子代理的写
    // 落点限定记忆目录(MemoryWrite 类放行),收掉写工具会弄断记忆沉淀。
    // 执行档(auto-edit/plan/full)之间维持跨模式字节稳定,不按模式增删。
    if state.session.permission_mode().is_read_only() && state.session.header().subagent.is_none() {
        crate::preset::apply_tool_blocklist(&mut assembly, &["bash", "write_file", "edit"]);
    }
    // 执行档(auto-edit/plan/full)之间**不**增删 schema 与纪律段(计划档
    // 保留写工具、执行档保留 exit_plan):工具面与系统提示跨模式字节稳定,
    // plan↔执行互切零缓存代价。越权由权限引擎在执行时拒绝(decide 矩阵:
    // 计划档拒绝一切写、非计划档拒绝 PlanSubmit),模型拿 isError 自纠正;
    // 当前模式语义由 `harness:permission` 运行时快照承担(变化走注入追加,
    // 不碰前缀)。只读档是例外:bash 与写文件工具在上方直接收窄,不给模型。
    // 项目记忆:段随记忆启用与否进退(runtime.memory_root_for 与权限层、
    // 注入通道同源);启用时替换为带真实路径的完整段,模型在任何 step 都
    // 能直接看到记忆目录。同会话 cwd 不可变 → 路径恒定 → 段文本字节稳定,
    // 不破坏 system 冻结;未注册(如无 runtime 部署)自然不存在。
    let session_cwd = std::path::PathBuf::from(state.session.header().cwd.clone());
    if let Some(position) = assembly
        .sections
        .iter()
        .position(|section| section.name == "tool:memory")
    {
        match driver
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.memory_root_for(&session_cwd))
        {
            Some(root) => {
                assembly.sections[position].text = denia_tools::render_memory_section(Some(&root))
            }
            None => {
                assembly.sections.remove(position);
            }
        }
    }
    let tools_tokens = serde_json::to_string(&assembly.tools)
        .map(|json| denia_token_meter::estimate_tools_tokens(&json))
        .unwrap_or(0);
    state.session.set_tools_tokens(tools_tokens);
    Ok(assembly)
}

/// 把子代理上下文落到本 step 的装配上（结构化继承，而非替换 persona）。
///
/// 四步，顺序即语义：
/// 1. 父 preset persona **快照**：父 preset 文件后来被删/损坏时 child 不回退
///    部署默认人格；
/// 2. 按有效工具重建工具纪律段（拿不到的工具其纪律段不出现）；
/// 3. 追加 `subagent:role`（定义 instructions，**追加不覆盖**）；
/// 4. 追加 `subagent:constraints`（禁止派遣、项目级指令、权限边界、汇报方式）。
///
/// 因为 3/4 只做 `push`，父 persona 段（包括"persona 独占"留下的那一段）一定
/// 保留；约束段也不受任何 persona 设置影响。
pub(crate) fn apply_subagent_context(
    assembly: &mut PromptAssembly,
    child: &denia_tools::capabilities::SubagentPrompt,
) {
    // 父 persona 快照覆盖 deployment:persona。父 preset 是“persona 独占”时,
    // 本装配已在 apply_session_preset 里被收成只剩 persona 一段,语言纪律随之
    // 消失——快照必须把它一起带回来;装配里还有 `harness:communication` 时则
    // 不重复补,权威段已经讲过了。
    let persona_only = !assembly
        .sections
        .iter()
        .any(|section| section.name == "harness:communication");
    if let Some(persona) = child
        .parent_preset_persona
        .as_deref()
        .filter(|text| !text.trim().is_empty())
        && let Some(section) = assembly
            .sections
            .iter_mut()
            .find(|s| s.name == "deployment:persona")
    {
        section.text = if persona_only {
            format!("{persona}\n{}", denia_tools::prompt::LANGUAGE_RULE)
        } else {
            persona.to_string()
        };
    }
    crate::preset::apply_tool_allowlist(assembly, &child.effective_tools);
    // 后台命令的入口是 `job_start`：没被授予它时，bash 的 run_in_background
    // 参数必须一起消失（执行层 `exec::reject_before_dispatch` 同样拒绝），
    // 否则"只禁 job_start"等于留了一条绕过通道。
    if !child.effective_tools.iter().any(|name| name == "job_start") {
        crate::preset::strip_background_param(assembly);
    }
    let name = child.name.as_deref().unwrap_or("子代理");
    let mut role = format!("# 你的角色（子代理）\n名称：{name}\n");
    if child.instructions.trim().is_empty() {
        role.push_str("本次派遣只给了任务说明：按用户消息里的任务要求工作。\n");
    } else {
        role.push_str(child.instructions.trim_end());
        role.push('\n');
    }
    assembly
        .sections
        .push(denia_system_prompt::AssembledSection {
            name: SUBAGENT_ROLE_SECTION.to_string(),
            text: role,
            audience: denia_system_prompt::SectionAudience::Model,
        });
    let ceiling = child.permission_ceiling.as_deref().unwrap_or("inherit");
    let tools_note = if child.effective_tools.is_empty() {
        "本次没有授予任何工具：只能用你已有的知识完成推理，并在结果里说明需要的工具。".to_string()
    } else {
        format!("可用工具仅限：{}。", child.effective_tools.join("、"))
    };
    let constraints = format!(
        "# 子代理约束\n\
         - 你是子代理，**不能派遣子代理**，也不能使用宿主配置或会话主控工具；\n\
         - 不会自动加载全局 AGENTS.md，只自动发现项目级规则；工作目录、权限模式与工作区沙箱与父代理一致；\n\
         - {tools_note}\n\
         - 权限上限：{ceiling}{}；\n\
         - 无法完成的动作不要假装完成：把阻塞、未覆盖范围与需要的决策写进最终结果。\n\
         - 汇报时区分三种情形：成功结果（附证据）、任务失败（附原因）、等待父代理决策。",
        if ceiling == "read-only" {
            "（写文件与命令一律被拒绝）"
        } else {
            "（写操作仍按当前权限模式走正常审批）"
        }
    );
    assembly
        .sections
        .push(denia_system_prompt::AssembledSection {
            name: SUBAGENT_CONSTRAINTS_SECTION.to_string(),
            text: constraints,
            audience: denia_system_prompt::SectionAudience::Model,
        });
}

/// 本 turn 的下一个 step 号:反向扫描到 TurnStart 为止,统计本 turn 已有
/// 的 step 数(通常个位数,扫描成本可忽略)。
impl TurnState {
    pub(crate) fn turn_step_next(&self) -> u32 {
        self.session.with_events(|events| {
            events
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
        }) + 1
    }
}

/// 保留对 Session 的类型引用(工具 emit_event sink 由 exec 模块使用)。
#[allow(unused)]
fn _session_type_witness(_s: &Session) {}

/// 子代理装配的回归网（H01）：父 persona 保留、角色段与约束段存在、工具说明
/// 与有效授权一致、零工具与只读上限都有明确文案。不需要 driver 即可断言。
#[cfg(test)]
mod subagent_section_tests {
    use super::*;
    use denia_system_prompt::{AssembleContext, SectionAudience};
    use denia_tools::capabilities::SubagentPrompt;
    use denia_tools::prompt::LANGUAGE_RULE;

    /// 同一份装配里语言纪律出现的次数:权威段是 `harness:communication`,
    /// persona 只在“收窄到只剩 persona”的档位里把它随身带走。
    fn language_rule_count(assembly: &PromptAssembly) -> usize {
        assembly
            .sections
            .iter()
            .map(|section| section.text.matches(LANGUAGE_RULE).count())
            .sum()
    }

    fn assembly() -> PromptAssembly {
        let (prompt, _registry) = denia_tools::default_shipped();
        prompt
            .assemble(&AssembleContext::default())
            .expect("出厂组装可装配")
    }

    fn child(instructions: &str, tools: Vec<&str>, ceiling: &str) -> SubagentPrompt {
        SubagentPrompt {
            name: Some("验证员".to_string()),
            instructions: instructions.to_string(),
            effective_tools: tools.into_iter().map(str::to_string).collect(),
            permission_ceiling: Some(ceiling.to_string()),
            parent_preset_persona: None,
            legacy: false,
        }
    }

    #[test]
    fn role_and_constraints_are_appended_and_the_parent_persona_survives() {
        let mut a = assembly();
        let persona_before = a
            .sections
            .iter()
            .find(|section| section.name == "deployment:persona")
            .map(|section| section.text.clone())
            .expect("出厂组装必须有 deployment:persona");
        apply_subagent_context(
            &mut a,
            &child(
                "角色哨兵 ROLE-SENTINEL",
                vec!["read_file", "bash"],
                "inherit",
            ),
        );
        assert_eq!(
            a.sections
                .iter()
                .find(|section| section.name == "deployment:persona")
                .map(|section| section.text.clone()),
            Some(persona_before),
            "子代理角色是追加，绝不替换父 persona"
        );
        let role = a
            .sections
            .iter()
            .find(|section| section.name == SUBAGENT_ROLE_SECTION)
            .expect("角色段必须存在");
        assert!(role.text.contains("ROLE-SENTINEL"));
        assert!(role.text.contains("验证员"));
        assert_eq!(role.audience, SectionAudience::Model);
        let constraints = a
            .sections
            .iter()
            .find(|section| section.name == SUBAGENT_CONSTRAINTS_SECTION)
            .expect("约束段必须存在");
        assert!(constraints.text.contains("不能派遣子代理"));
        assert!(constraints.text.contains("项目级规则"));
        assert!(constraints.text.contains("read_file") && constraints.text.contains("bash"));
        assert!(constraints.text.contains("正常审批"));
        // 工具纪律段与授权同源：没授予 bash 时 tool:bash 段必须消失。
        let mut narrowed = assembly();
        apply_subagent_context(&mut narrowed, &child("", vec!["read_file"], "read-only"));
        assert!(
            narrowed
                .sections
                .iter()
                .any(|section| section.name == "tool:read"),
            "被授予的工具其纪律段保留"
        );
        assert!(
            !narrowed
                .sections
                .iter()
                .any(|section| section.name == "tool:bash"),
            "未授予的工具其纪律段必须移除"
        );
        assert!(
            !narrowed.tools.iter().any(|schema| schema.name == "bash"),
            "未授予的工具其 schema 必须移除"
        );
    }

    #[test]
    fn zero_tools_and_read_only_ceiling_are_spelled_out() {
        let mut a = assembly();
        apply_subagent_context(&mut a, &child("", Vec::new(), "read-only"));
        let constraints = a
            .sections
            .iter()
            .find(|section| section.name == SUBAGENT_CONSTRAINTS_SECTION)
            .expect("约束段必须存在");
        assert!(constraints.text.contains("没有授予任何工具"));
        assert!(constraints.text.contains("read-only"));
        assert!(constraints.text.contains("写文件与命令一律被拒绝"));
        // 零工具时请求工具面也必须为空（schema 与授权一致）。
        assert!(a.tools.is_empty(), "零工具定义不得拿到任何 schema");
    }

    #[test]
    fn parent_preset_persona_snapshot_wins_over_a_later_preset_change() {
        let mut a = assembly();
        let mut child = child("", vec!["read_file"], "inherit");
        child.parent_preset_persona = Some("父 preset 的快照 persona".to_string());
        apply_subagent_context(&mut a, &child);
        let persona = a
            .sections
            .iter()
            .find(|section| section.name == "deployment:persona")
            .expect("persona 段仍在");
        assert!(persona.text.starts_with("父 preset 的快照 persona"));
        // 装配里还有 harness:communication,中文纪律不追写第二遍。
        assert!(!persona.text.contains(LANGUAGE_RULE), "{}", persona.text);
        assert_eq!(language_rule_count(&a), 1, "语言纪律只该出现一次");
    }

    #[test]
    fn persona_only_assembly_keeps_the_language_rule_once() {
        // 父 preset 是 persona 独占时,子代理装配里只剩 deployment:persona
        // (APPLY_SESSION_PRESET 已把它收窄),语言纪律只能随父 persona 快照
        // 一起回来——少了这一手,该子代理就彻底没有语言约束。
        let mut a = assembly();
        a.sections
            .retain(|section| section.name == denia_system_prompt::PERSONA_SECTION);
        let mut child = child("", vec!["read_file"], "inherit");
        child.parent_preset_persona = Some("父 preset 的快照 persona".to_string());
        apply_subagent_context(&mut a, &child);
        let persona = a
            .sections
            .iter()
            .find(|section| section.name == "deployment:persona")
            .expect("persona 段仍在");
        assert!(persona.text.starts_with("父 preset 的快照 persona"));
        assert!(
            persona.text.contains(LANGUAGE_RULE),
            "persona 独占档丢了语言纪律:{}",
            persona.text
        );
        assert_eq!(language_rule_count(&a), 1, "语言纪律只该出现一次");
    }

    /// 后台命令的入口是 `job_start`：没有授予它时，bash 的 `run_in_background`
    /// 参数必须从 schema 里消失（"只禁 job_start"不是收口）。
    #[test]
    fn bash_background_param_follows_the_jobs_grant() {
        // 出厂 shipped 组装里的 bash schema 不带 run_in_background（它由 server
        // 侧带 runtime 的 BashTool 注入），这里补上以模拟真实部署的工具面。
        let with_background_bash = || {
            let mut a = assembly();
            if let Some(bash) = a.tools.iter_mut().find(|schema| schema.name == "bash") {
                bash.parameters["properties"]["run_in_background"] =
                    serde_json::json!({"type": "boolean"});
            } else {
                panic!("出厂组装必须含 bash");
            }
            a
        };

        let mut without_jobs = with_background_bash();
        apply_subagent_context(&mut without_jobs, &child("", vec!["bash"], "inherit"));
        let bash = without_jobs
            .tools
            .iter()
            .find(|schema| schema.name == "bash")
            .expect("bash 仍在工具面内");
        assert!(
            bash.parameters["properties"]
                .get("run_in_background")
                .is_none(),
            "未授予 job_start 时 run_in_background 必须消失"
        );

        let mut with_jobs = with_background_bash();
        apply_subagent_context(
            &mut with_jobs,
            &child("", vec!["bash", "job_start"], "inherit"),
        );
        let bash = with_jobs
            .tools
            .iter()
            .find(|schema| schema.name == "bash")
            .expect("bash 仍在工具面内");
        assert!(
            bash.parameters["properties"]
                .get("run_in_background")
                .is_some(),
            "授予 job_start 时后台参数保留"
        );
    }
}

/// 执行预算(阶段五)的端到端回归:三道边界各自"拒绝并走收尾",以及来源区分。
///
/// 断言只看用户可见的结局(turn-end 的失败码与消息、是否还有下一步、请求
/// 有没有真的发出去),不看账本内部计数——那些由 token-meter 的单元测试钉。
#[cfg(test)]
mod budget_tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    use denia_core::config::ModelSelection;
    use denia_core::error::LlmError;
    use denia_core::stream::{BlockType, ContentBlock, StreamChunk, TokenUsage};
    use denia_llm::{
        ChunkStream, GenerateRequest, LlmAdapter, LlmModelInfo, LlmRegistry, LlmResolvedModelInfo,
        ProviderInfo, RequestAdmission, RetryPolicy,
    };
    use denia_session::Session;
    use denia_token_meter::{Clock, ExecutionBudget, ExecutionLimits};
    use denia_tools::ToolRegistry;
    use tokio_util::sync::CancellationToken;

    /// 按脚本逐次返回响应(脚本用完重复最后一条),并记录请求次数。
    struct ScriptedAdapter {
        scripts: Mutex<VecDeque<Vec<StreamChunk>>>,
        calls: AtomicU32,
    }

    #[async_trait::async_trait]
    impl LlmAdapter for ScriptedAdapter {
        fn provider_info(&self, provider: &str) -> ProviderInfo {
            ProviderInfo {
                id: provider.to_string(),
                name: provider.to_string(),
            }
        }

        async fn list_models(&self, _provider: &str) -> Result<Vec<LlmModelInfo>, LlmError> {
            Ok(Vec::new())
        }

        async fn resolve_model(
            &self,
            provider: &str,
            model: &str,
        ) -> Result<LlmResolvedModelInfo, LlmError> {
            Ok(LlmResolvedModelInfo {
                info: LlmModelInfo {
                    provider: provider.to_string(),
                    id: model.to_string(),
                    name: model.to_string(),
                    description: None,
                    input_modalities: vec!["text".to_string()],
                },
                context_window: Some(200_000),
                default_max_tokens: Some(4_096),
                reasoning: None,
            })
        }

        async fn stream(
            &self,
            _provider: &str,
            _request: &GenerateRequest,
        ) -> Result<ChunkStream, LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let script = {
                let mut scripts = self.scripts.lock().unwrap();
                if scripts.len() > 1 {
                    scripts.pop_front().unwrap_or_default()
                } else {
                    scripts.front().cloned().unwrap_or_default()
                }
            };
            let items: Vec<Result<StreamChunk, LlmFailure>> = script.into_iter().map(Ok).collect();
            Ok(Box::pin(futures::stream::iter(items)))
        }
    }

    fn text_script(text: &str, finish: FinishReason) -> Vec<StreamChunk> {
        vec![
            StreamChunk::BlockStart {
                index: 0,
                block_type: BlockType::Text,
            },
            StreamChunk::TextDelta {
                index: 0,
                text: text.to_string(),
            },
            StreamChunk::BlockEnd {
                index: 0,
                block: ContentBlock::Text {
                    text: text.to_string(),
                },
            },
            StreamChunk::Finish { reason: finish },
        ]
    }

    fn usage_chunk(input: u64, output: u64) -> StreamChunk {
        StreamChunk::Usage {
            usage: TokenUsage {
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: None,
                reasoning_tokens: None,
            },
        }
    }

    fn temp_session() -> Arc<Session> {
        let dir = std::env::temp_dir().join(format!(
            "denia-budget-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Arc::new(Session::create(&dir, uuid::Uuid::new_v4().to_string(), &dir, true, None).unwrap())
    }

    /// 装配一个只有 mock 路由、无工具的驱动器:预算边界不依赖工具面。
    fn scripted_driver(
        scripts: Vec<Vec<StreamChunk>>,
        limits: ExecutionLimits,
        clock: Option<Clock>,
    ) -> (SessionDriver, Arc<ScriptedAdapter>, Arc<Session>) {
        let registry = Arc::new(LlmRegistry::new());
        let adapter = Arc::new(ScriptedAdapter {
            scripts: Mutex::new(VecDeque::from(scripts)),
            calls: AtomicU32::new(0),
        });
        registry
            .register(
                &["mock".to_string()],
                adapter.clone(),
                RetryPolicy {
                    // 注册表内部重试不睡:测试只验证准入次数,不验证退避曲线。
                    max_retries: 2,
                    initial_delay_ms: 0,
                    max_delay_ms: 0,
                    ..RetryPolicy::default()
                },
            )
            .unwrap();
        let mut prompt =
            denia_system_prompt::SystemPrompt::new(denia_system_prompt::SystemPromptConfig {
                include_runtime_context: false,
                ..Default::default()
            });
        prompt
            .variable("cwd", |context| context.cwd.clone())
            .unwrap();
        prompt
            .variable("model", |context| context.model.clone())
            .unwrap();
        prompt
            .variable("provider", |context| context.provider.clone())
            .unwrap();
        let mut driver = SessionDriver::new(
            registry,
            Arc::new(ToolRegistry::default()),
            Arc::new(arc_swap::ArcSwap::from_pointee(prompt)),
        );
        driver.set_execution_limits_source(Arc::new(move || limits));
        if let Some(clock) = clock {
            driver = driver.with_budget_clock(clock);
        }
        (driver, adapter, temp_session())
    }

    async fn run_one_turn(driver: &SessionDriver, session: &Arc<Session>) -> TurnEndReason {
        driver
            .run_turn(
                session,
                &ModelSelection {
                    provider: "mock".to_string(),
                    model: "mock-1".to_string(),
                    reasoning_effort: None,
                },
                "开始",
                Vec::new(),
                Vec::new(),
                Vec::new(),
                false,
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
    }

    fn last_turn_end(session: &Session) -> TurnEndReason {
        session.with_events(|events| {
            events
                .iter()
                .rev()
                .find_map(|envelope| match &envelope.event {
                    SessionEvent::TurnEnd { reason, .. } => Some(reason.clone()),
                    _ => None,
                })
                .expect("轮次必须闭合")
        })
    }

    fn steps_started(session: &Session) -> usize {
        session.with_events(|events| {
            events
                .iter()
                .filter(|envelope| {
                    matches!(envelope.event, SessionEvent::StepStart { turn: 1, .. })
                })
                .count()
        })
    }

    fn budget_failure(reason: &TurnEndReason) -> &LlmFailure {
        match reason {
            TurnEndReason::Error { failure } => failure,
            other => panic!("预算耗尽必须走现有错误收尾路径,得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn step_budget_refuses_and_closes_the_turn() {
        // 步数上限 2;脚本持续输出截断,驱动会一直开新 step(续写路径)。
        let limits = ExecutionLimits {
            max_steps: 2,
            ..ExecutionLimits::unbounded()
        };
        let (driver, adapter, session) = scripted_driver(
            vec![text_script("部分输出", FinishReason::MaxTokens)],
            limits,
            None,
        );
        let reason = run_one_turn(&driver, &session).await;
        let failure = budget_failure(&reason);
        assert_eq!(failure.code, codes::BUDGET_EXHAUSTED);
        assert!(failure.message.contains("模型步骤"), "{}", failure.message);
        assert_eq!(steps_started(&session), 2, "第 3 个 step 不该开始");
        assert_eq!(
            adapter.calls.load(Ordering::SeqCst),
            2,
            "超限之后不得再发请求"
        );
    }

    #[tokio::test]
    async fn request_budget_refuses_before_the_next_attempt() {
        // 请求尝试上限 1;第一次流失败后 step 内重试,第二次尝试必须被拒。
        let limits = ExecutionLimits {
            max_requests: 1,
            ..ExecutionLimits::unbounded()
        };
        let failing = vec![StreamChunk::Finish {
            reason: FinishReason::Error {
                failure: LlmFailure::new(codes::EMPTY_RESPONSE, "empty response"),
            },
        }];
        let (driver, adapter, session) = scripted_driver(
            vec![failing, text_script("恢复", FinishReason::Stop)],
            limits,
            None,
        );
        let reason = run_one_turn(&driver, &session).await;
        let failure = budget_failure(&reason);
        assert_eq!(failure.code, codes::BUDGET_EXHAUSTED);
        assert!(failure.message.contains("模型请求"), "{}", failure.message);
        assert_eq!(
            adapter.calls.load(Ordering::SeqCst),
            1,
            "被拒绝的尝试不得发给提供方"
        );
    }

    #[tokio::test]
    async fn explicit_retries_fit_inside_the_request_budget() {
        // 反向的另一半:额度够时,显式重试是**新身份**,不该被当成重复调用
        // 卡住(去重不能把重试判成"重复")。
        let limits = ExecutionLimits {
            max_requests: 4,
            ..ExecutionLimits::unbounded()
        };
        let failing = vec![StreamChunk::Finish {
            reason: FinishReason::Error {
                failure: LlmFailure::new(codes::EMPTY_RESPONSE, "empty response"),
            },
        }];
        let (driver, adapter, session) = scripted_driver(
            vec![failing, text_script("重试成功", FinishReason::Stop)],
            limits,
            None,
        );
        let reason = run_one_turn(&driver, &session).await;
        assert_eq!(reason, TurnEndReason::Completed, "重试应当被容纳并正常完成");
        assert_eq!(
            adapter.calls.load(Ordering::SeqCst),
            2,
            "两次物理请求都发出"
        );
    }

    #[tokio::test]
    async fn active_time_budget_refuses_at_the_step_boundary() {
        // 假时钟每次读取前进 20 分钟;主动时间上限 60 分钟 → 第 2 个 step 的
        // 准入就已经超限。等待用户的区间由账本的 pause/resume 排除(单元测试
        // 钉住),这里验证超限确实走收尾而不是继续发请求。
        let ticks = Arc::new(AtomicU64::new(0));
        let clock: Clock = {
            let ticks = ticks.clone();
            Arc::new(move || ticks.fetch_add(20 * 60_000, Ordering::SeqCst))
        };
        let limits = ExecutionLimits {
            max_active_ms: 60 * 60_000,
            ..ExecutionLimits::unbounded()
        };
        let (driver, adapter, session) = scripted_driver(
            vec![text_script("分段输出", FinishReason::MaxTokens)],
            limits,
            Some(clock),
        );
        let reason = run_one_turn(&driver, &session).await;
        let failure = budget_failure(&reason);
        assert_eq!(failure.code, codes::BUDGET_EXHAUSTED);
        assert!(
            failure.message.contains("主动执行时间"),
            "{}",
            failure.message
        );
        assert_eq!(
            adapter.calls.load(Ordering::SeqCst),
            1,
            "超时后不得再发请求"
        );
    }

    #[test]
    fn bypass_sources_stay_out_of_the_user_budget() {
        // 会话标题、连通测试、git 摘要不是用户任务的执行成本:额度为 0 时
        // 它们照样放行,用户请求照样被拒。
        let budget = ExecutionBudget::new(ExecutionLimits {
            max_requests: 0,
            ..ExecutionLimits::unbounded()
        });
        let gate = crate::BudgetGate(budget.clone());
        for kind in [
            denia_llm::RequestKind::SessionTitle,
            denia_llm::RequestKind::Connectivity,
            denia_llm::RequestKind::Git,
        ] {
            let call = denia_llm::RequestCall {
                session: "s1".to_string(),
                turn: 1,
                step: 1,
                kind,
                sequence: 0,
                attempt: 1,
            };
            assert!(
                gate.admit(&call).is_ok(),
                "{kind:?} 是旁路,不该被用户预算拦住"
            );
        }
        let user = denia_llm::RequestCall {
            session: "s1".to_string(),
            turn: 1,
            step: 1,
            kind: denia_llm::RequestKind::UserStep,
            sequence: 1,
            attempt: 1,
        };
        let refusal = gate.admit(&user).expect_err("用户请求额度为 0,必须被拒");
        assert!(refusal.message.contains("执行预算已用尽"));
        let counters = budget.counters();
        assert_eq!(counters.metered_requests, 0, "旁路不吃用户预算");
        assert_eq!(counters.bypass_requests, 3, "旁路只累计观测计数");
    }

    #[tokio::test]
    async fn compaction_summary_usage_is_booked() {
        // 主请求先撞上下文超限 → 强制压缩;摘要请求上报 usage。旧路径把这块
        // 计费整个丢掉(compact.rs 不消费 Usage),于是"摘要花的钱"不在账上。
        let overflow = vec![StreamChunk::Finish {
            reason: FinishReason::Error {
                failure: LlmFailure::new(codes::CONTEXT_WINDOW_EXCEEDED, "maximum context"),
            },
        }];
        let mut summary = text_script("既往工作摘要", FinishReason::Stop);
        summary.insert(3, usage_chunk(900, 120));
        let (driver, _adapter, session) = scripted_driver(
            vec![overflow, summary, text_script("收尾", FinishReason::Stop)],
            ExecutionLimits::unbounded(),
            None,
        );
        let driver = driver.with_compaction(crate::CompactionSettings {
            min_keep_tokens: 1,
            max_keep_tokens: 1,
            min_text_messages: 1,
            ..Default::default()
        });
        for index in 0..15 {
            session
                .append(SessionEvent::UserMessage {
                    text: format!("old-{index}"),
                    injected: false,
                    channel: None,
                    images: Vec::new(),
                })
                .unwrap();
        }
        let _ = run_one_turn(&driver, &session).await;

        let booked: Vec<denia_token_meter::AccountedUsage> = session.with_events(|events| {
            events
                .iter()
                .filter_map(|envelope| denia_token_meter::AccountedUsage::parse(&envelope.event))
                .collect()
        });
        let exact: Vec<&denia_token_meter::AccountedUsage> =
            booked.iter().filter(|usage| !usage.estimated).collect();
        assert_eq!(exact.len(), 1, "摘要请求的精确用量必须落账:{booked:?}");
        assert_eq!(exact[0].uncached_input_tokens, 900);
        assert_eq!(exact[0].output_tokens, 120);
        // 同一轮里主请求的脚本没有报 usage,所以还会有一条估算标注(见下一个
        // 测试):两条账目共存,互不覆盖。
        assert_eq!(booked.len(), 2, "精确账目与估算标注各管一段:{booked:?}");
        assert!(
            session.turn_token_usage().uncached_input_tokens >= 900,
            "账本要真的把这笔算进去:{:?}",
            session.turn_token_usage()
        );
    }

    #[tokio::test]
    async fn a_usage_less_step_is_estimated_and_marked() {
        // provider 不报 usage:不再让整轮折账(旧行为),而是保守估算 + 事件
        // 上标注。断言标注落盘、且账本确实有非零用量。
        let mut script = text_script("没有 usage 的回复", FinishReason::Stop);
        script.retain(|chunk| !matches!(chunk, StreamChunk::Usage { .. }));
        let (driver, _adapter, session) =
            scripted_driver(vec![script], ExecutionLimits::unbounded(), None);
        let reason = run_one_turn(&driver, &session).await;
        assert_eq!(reason, TurnEndReason::Completed);
        let marked: Vec<denia_token_meter::AccountedUsage> = session.with_events(|events| {
            events
                .iter()
                .filter_map(|envelope| denia_token_meter::AccountedUsage::parse(&envelope.event))
                .collect()
        });
        assert_eq!(marked.len(), 1, "缺 usage 必须留一条估算标注:{marked:?}");
        assert!(marked[0].estimated, "标注必须说明是估算");
        assert!(marked[0].total() > 0, "估算不能是 0");
        assert!(
            session.turn_token_usage().total() > 0,
            "估算要计入账本:{:?}",
            session.turn_token_usage()
        );
    }
}
