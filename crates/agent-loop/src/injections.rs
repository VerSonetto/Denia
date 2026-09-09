//! 每 step 的背景注入管线(抄 dsh agent-instructions / tool-skill 语义)。
//!
//! 三条独立幂等的注入通道(文本即身份,通道字段 `channel` 优先、旧日志
//! 文本前缀 fallback):工作区指令(AGENTS.md)、能力上下文、技能目录。
//! 刷新失败不阻断轮次(记日志跳过);`runtime.drain` 失败视为硬错误。

use std::path::PathBuf;

use denia_core::session::{GoalState, GoalStatus, SessionEvent};
use denia_session::Session;

use crate::workspace_instructions::{
    SKILL_CATALOG_PREFIX, WORKSPACE_PREFIX, render_skill_catalog, restore_injected_text,
};
use crate::{SessionDriver, TurnState, append};

/// 目标状态注入通道名;goal 模式唯一的模型侧目标消息通道。
pub const GOAL_CHANNEL: &str = "goal";

/// 目标被清除后的终局通知(一次性;此后通道内容与基准一致,不再重发)。
const GOAL_CLEARED_TEXT: &str = "[denia 目标] 当前会话目标已清除,无需再关注目标。";

/// 三条注入通道的本轮基准:日志中最后一条本通道注入文本,内容未变不重发。
pub(crate) struct InjectionBaselines {
    /// 能力上下文(`[denia 能力上下文]` 前缀)。
    pub capability_context: Option<String>,
    /// 工作区指令(`<system-reminder>\n工作区指令:` 前缀)。
    pub workspace_baseline: Option<String>,
    /// 技能目录(`<system-reminder>\n技能目录:` 前缀)。
    pub skill_catalog: Option<String>,
    /// 会话目标状态块(`[denia 目标]` 前缀)。
    pub goal: Option<String>,
}

impl InjectionBaselines {
    /// 从日志恢复三条通道的基准(通道字段优先,旧日志走前缀判断)。
    pub(crate) fn restore(state: &TurnState) -> Self {
        Self {
            capability_context: last_injected_channel(&state.session, "capability").or_else(
                || {
                    state.session.events().iter().rev().find_map(|e| match &e.event {
                        SessionEvent::UserMessage {
                            text,
                            injected: true,
                            ..
                        } if text.starts_with("[denia 能力上下文]") => Some(text.clone()),
                        _ => None,
                    })
                },
            ),
            workspace_baseline: restore_injected_text(&state.session.events(), WORKSPACE_PREFIX),
            skill_catalog: restore_injected_text(&state.session.events(), SKILL_CATALOG_PREFIX),
            goal: last_injected_channel(&state.session, GOAL_CHANNEL),
        }
    }
}

/// 按通道名取日志中最后一条注入消息(新事件模型:channel 字段)。
fn last_injected_channel(session: &Session, channel: &str) -> Option<String> {
    session.events().iter().rev().find_map(|envelope| match &envelope.event {
        SessionEvent::UserMessage {
            text,
            injected: true,
            channel: Some(name),
            ..
        } if name == channel => Some(text.clone()),
        _ => None,
    })
}

/// 每 step 刷新背景注入。任何一条通道刷新失败只记日志;`runtime.drain`
/// 失败(通知领取不可用)返回错误终止本轮。
pub(crate) async fn refresh_background_injections(
    driver: &SessionDriver,
    state: &mut TurnState,
    baselines: &mut InjectionBaselines,
    touched: &[PathBuf],
) -> Result<(), denia_core::error::LlmFailure> {
    let Some(runtime) = &driver.runtime else {
        return Ok(());
    };
    let session = &state.session;
    let cwd = state.cwd();

    // ① 工作区指令(AGENTS.md):发现/预算/替换语义在 runtime 侧;
    // restore 的旧文本作为 previous 传入,由正文比较决定幂等与"取代"引导语。
    match runtime
        .workspace_instructions(&cwd, touched, baselines.workspace_baseline.as_deref())
        .await
    {
        Ok(Some(text)) => {
            if baselines.workspace_baseline.as_deref() != Some(&text) {
                append(
                    session,
                    &state.emit,
                    SessionEvent::UserMessage {
                        text: text.clone(),
                        injected: true,
                        channel: Some("workspace-instructions".into()),
                        images: Vec::new(),
                    },
                )?;
                baselines.workspace_baseline = Some(text);
            }
        }
        Ok(None) => {}
        Err(error) => tracing::warn!(
            session_id = session.id(),
            error = %error,
            "workspace instructions refresh failed"
        ),
    }

    // ② 能力上下文。
    let context = match runtime.context(session.id(), &cwd).await {
        Ok(parts) => parts.join("\n"),
        Err(error) => format!("[denia 能力上下文]\n上下文生成失败：{error}。"),
    };
    if baselines.capability_context.as_deref() != Some(&context) {
        append(
            session,
            &state.emit,
            SessionEvent::UserMessage {
                text: context.clone(),
                injected: true,
                channel: Some("capability".into()),
                images: Vec::new(),
            },
        )?;
        baselines.capability_context = Some(context);
    }

    // ③ 技能目录:仅当 skill 工具对该会话可见(子代理白名单同装配过滤);
    // 从未发布且为空则不发消息,整块替换语义同工作区指令。
    let skill_tool_visible = session
        .header()
        .subagent
        .as_ref()
        .and_then(|s| s.allowed_tools.as_ref())
        .is_none_or(|allowed| allowed.iter().any(|name| name == "skill"));
    if skill_tool_visible {
        match runtime.skill_catalog(session.id(), &cwd).await {
            Ok(entries) => {
                if let Some(text) =
                    render_skill_catalog(&entries, baselines.skill_catalog.as_deref())
                    && baselines.skill_catalog.as_deref() != Some(&text)
                {
                    append(
                        session,
                        &state.emit,
                        SessionEvent::UserMessage {
                            text: text.clone(),
                            injected: true,
                            channel: Some("skill-catalog".into()),
                            images: Vec::new(),
                        },
                    )?;
                    baselines.skill_catalog = Some(text);
                }
            }
            Err(error) => tracing::warn!(
                session_id = session.id(),
                error = %error,
                "skill catalog refresh failed"
            ),
        }
    }

    // ④ 会话目标:状态块仅对 goal 工具可见的会话注入(子代理白名单同
    // 装配过滤)。内容不变不重发——turn 运行中用户编辑目标(steering)或
    // 状态转换后,下一 step 自动带出新状态;目标被清除后发一次终局通知。
    let goal_tool_visible = session
        .header()
        .subagent
        .as_ref()
        .and_then(|s| s.allowed_tools.as_ref())
        .is_none_or(|allowed| allowed.iter().any(|name| name == "get_goal"));
    if goal_tool_visible {
        let goal_text = match session.goal() {
            Some(goal) => Some(render_goal_block(&goal, session.goal_tokens_used().unwrap_or(0))),
            None => baselines.goal.as_ref().map(|_| GOAL_CLEARED_TEXT.to_string()),
        };
        if let Some(text) = goal_text
            && baselines.goal.as_deref() != Some(&text)
        {
            append(
                session,
                &state.emit,
                SessionEvent::UserMessage {
                    text: text.clone(),
                    injected: true,
                    channel: Some(GOAL_CHANNEL.into()),
                    images: Vec::new(),
                },
            )?;
            baselines.goal = Some(text);
        }
    }

    // 领取待处理的代理/任务通知(与文本投影原子落盘);失败 = 硬错误。
    runtime
        .drain(session.id())
        .await
        .map_err(|e| denia_core::error::LlmFailure::new(denia_core::error::codes::UNKNOWN, e))?;
    Ok(())
}

/// 目标状态块(背景注入,goal 模式唯一的模型侧目标通道):objective +
/// 状态 + 轮次 + 预算用量 + 状态相关的行动指引。续跑轮不再单独发
/// `<goal_round>` 消息——用量每轮变化使本块每轮重注入一次,天然承担
/// 轮次开始提示;turn 内用量不变(fold_turn 在轮闭合才并入),不会逐
/// step 重发。
pub fn render_goal_block(goal: &GoalState, tokens_used: u64) -> String {
    let mut lines = vec![
        "[denia 目标]".to_string(),
        format!("目标:{}", goal.objective),
    ];
    lines.push(match &goal.blocked_reason {
        Some(reason) if goal.status == GoalStatus::Blocked => {
            format!("状态:受阻({reason})")
        }
        _ => format!("状态:{}", goal.status.label()),
    });
    lines.push(format!("已发起续跑轮数:{}", goal.rounds_started));
    lines.push(match goal.token_budget {
        Some(budget) => format!("预算用量:{tokens_used}/{budget} tokens"),
        None => format!("预算用量:{tokens_used} tokens(未设预算)"),
    });
    lines.push(match goal.status {
        GoalStatus::Active => "请继续朝目标自主工作:评估当前进展,决定并执行下一步;目标达成时立即用 update_goal 标记 complete,无法推进时用 blocked 标记并说明原因。".to_string(),
        GoalStatus::Paused => "目标已暂停:自动续跑停止,等待用户恢复后再继续朝目标工作。".to_string(),
        GoalStatus::Blocked => "目标受阻:自动续跑停止;阻塞解除或用户给出新指示后继续。".to_string(),
        GoalStatus::BudgetLimited => "目标预算已耗尽:本轮请总结进展并收尾,不要开启新的大步骤。".to_string(),
        GoalStatus::Complete => "目标已完成:无需再为目标做任何工作。".to_string(),
    });
    lines.join("\n")
}
