//! 每 step 的背景注入管线(抄 dsh agent-instructions / tool-skill 语义)。
//!
//! 三条独立幂等的注入通道(文本即身份,通道字段 `channel` 优先、旧日志
//! 文本前缀 fallback):工作区指令(AGENTS.md)、能力上下文、技能目录。
//! 刷新失败不阻断轮次(记日志跳过);`runtime.drain` 失败视为硬错误。

use std::path::PathBuf;

use denia_core::session::SessionEvent;
use denia_session::Session;

use crate::workspace_instructions::{
    SKILL_CATALOG_PREFIX, WORKSPACE_PREFIX, render_skill_catalog, restore_injected_text,
};
use crate::{SessionDriver, TurnState, append};

/// 三条注入通道的本轮基准:日志中最后一条本通道注入文本,内容未变不重发。
pub(crate) struct InjectionBaselines {
    /// 能力上下文(`[denia 能力上下文]` 前缀)。
    pub capability_context: Option<String>,
    /// 工作区指令(`<system-reminder>\n工作区指令:` 前缀)。
    pub workspace_baseline: Option<String>,
    /// 技能目录(`<system-reminder>\n技能目录:` 前缀)。
    pub skill_catalog: Option<String>,
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

    // 领取待处理的代理/任务通知(与文本投影原子落盘);失败 = 硬错误。
    runtime
        .drain(session.id())
        .await
        .map_err(|e| denia_core::error::LlmFailure::new(denia_core::error::codes::UNKNOWN, e))?;
    Ok(())
}
