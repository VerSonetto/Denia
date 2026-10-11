//! 每 step 的背景注入管线(抄 dsh agent-instructions / tool-skill 语义)。
//!
//! 四条独立幂等的注入通道(文本即身份,通道字段 `channel` 优先、旧日志
//! 文本前缀 fallback):工作区指令(AGENTS.md)、能力上下文、技能目录、
//! 项目记忆索引。刷新失败不阻断轮次(记日志跳过);`runtime.drain` 失败
//! 视为硬错误。

use std::path::PathBuf;

use denia_core::message::ChatRole;
use denia_core::session::{GoalState, GoalStatus, SessionEvent, SurfaceMessage};
use denia_session::Session;

use crate::workspace_instructions::{SKILL_CATALOG_PREFIX, WORKSPACE_PREFIX, render_skill_catalog};
use crate::{SessionDriver, TurnState, append};

/// 目标状态注入通道名;goal 模式唯一的模型侧目标消息通道。
pub const GOAL_CHANNEL: &str = "goal";

/// 能力上下文注入通道名。
const CAPABILITY_CHANNEL: &str = "capability";
/// 能力上下文注入文本前缀(早期日志无 `channel` 字段时的识别方式)。
const CAPABILITY_PREFIX: &str = "[denia 能力上下文]";
/// 工作区指令(AGENTS.md)注入通道名。
const WORKSPACE_CHANNEL: &str = "workspace-instructions";
/// 技能目录注入通道名。
const SKILL_CHANNEL: &str = "skill-catalog";

/// 系统提示更新通道:system 字节冻结后,变化的新提示词全文经此通道
/// 追加到已缓存历史之后(对齐 dsh `systemPromptUpdate: 'in-history'`)。
pub const SYSTEM_UPDATE_CHANNEL: &str = "system-prompt-update";

/// 项目记忆索引注入通道名(MEMORY.md 全文,记忆启用时主会话可见)。
pub const MEMORY_CHANNEL: &str = "project-memory";

/// 子代理派遣目录注入通道名（父代理专用；子代理永不注入）。
pub const SUBAGENT_CATALOG_CHANNEL: &str = "subagent-catalog";

/// 子代理是否被授予某工具。
///
/// 优先读派遣时冻结的 `effectiveTools`；旧描述符没有该字段时按历史只读上限
/// 保守构造。这保证"注入通道的可见性"与"执行层授权"同源。
fn child_allows(session: &Session, name: &str) -> bool {
    match session.header().subagent.as_ref() {
        None => true,
        Some(child) => match &child.effective_tools {
            Some(tools) => tools.iter().any(|item| item == name),
            None => denia_core::subagent::legacy_child_tools(child.allowed_tools.as_deref())
                .iter()
                .any(|item| item == name),
        },
    }
}

/// 目标被清除后的终局通知(一次性;此后通道内容与基准一致,不再重发)。
const GOAL_CLEARED_TEXT: &str = "[denia 目标] 当前会话目标已清除,无需再关注目标。";

/// 注入通道的本轮基准:模型面(surface)上最后一条本通道注入文本,
/// 内容未变不重发。
pub(crate) struct InjectionBaselines {
    /// 能力上下文(`[denia 能力上下文]` 前缀)。
    pub capability_context: Option<String>,
    /// 工作区指令(`<system-reminder>\n工作区指令:` 前缀)。
    pub workspace_baseline: Option<String>,
    /// 技能目录(`<system-reminder>\n技能目录:` 前缀)。
    pub skill_catalog: Option<String>,
    /// 项目记忆索引(`<system-reminder>\n项目记忆:` 前缀)。
    pub project_memory: Option<String>,
    /// 会话目标状态块(`[denia 目标]` 前缀)。
    pub goal: Option<String>,
    /// 子代理派遣目录（父代理专用）。
    pub subagent_catalog: Option<String>,
    /// 系统提示更新(`<system-reminder>\n系统提示更新:` 全文)。
    ///
    /// 该通道与其余通道同源(surface):system 字节冻结在 `RequestHeader` 里的
    /// 旧值后,提示词变化只经这条注入追加。压缩把注入消息挤出模型面时,system
    /// 字段仍是冻结的旧版——不重发全文,模型就再也看不到当前生效的提示词。
    /// 冻结语义不变:提示词与冻结值一致时本来就不该发。
    pub system_update: Option<String>,
    /// 基准对齐到 surface 时日志的末尾 seq。
    ///
    /// 压缩会把注入块挤出模型面而日志仍是 append-only:一旦轮内压缩过,
    /// 光比"内容变没变"会在压缩后的所有 step 里认为已注入过。带上对齐点,
    /// 每步只扫"上一步之后新落的事件"就能发现需要重新对齐。
    synced_seq: u64,
}

impl InjectionBaselines {
    /// 从**派生 surface**恢复注入通道的基准(通道字段优先,旧日志走前缀判断)。
    ///
    /// 基准是"模型面(surface)上还有没有本通道的最新文本",不是"日志里写过
    /// 没有":压缩(`CompactionSummary`)把被压区间的事件从 surface 移除,日志
    /// 却保持 append-only。读日志会得出"已经注入过"的错误结论,而注入块
    /// (工作区指令、目标、记忆索引、技能目录)通常正落在最老的那一段——
    /// 压缩之后模型就再也收不到它们,只能指望摘要恰好带上。读 surface 后,
    /// 注入块被压掉 = 本通道在 surface 上没有文本 = 下一 step 自然重发。
    pub(crate) fn restore(session: &Session) -> Self {
        // 派生面本身有缓存(按日志版本号),这里只多一次读共享的 Arc,
        // 不做二次克隆;历史投影(旧子代理)已由 `derive_surface` 应用。
        let surface = session.derive_surface();
        let synced_seq = session.with_events(|events| last_seq(events));
        Self {
            capability_context: injected_text(&surface, CAPABILITY_CHANNEL, CAPABILITY_PREFIX),
            workspace_baseline: injected_text(&surface, WORKSPACE_CHANNEL, WORKSPACE_PREFIX),
            skill_catalog: injected_text(&surface, SKILL_CHANNEL, SKILL_CATALOG_PREFIX),
            project_memory: injected_text(&surface, MEMORY_CHANNEL, ""),
            goal: injected_text(&surface, GOAL_CHANNEL, ""),
            subagent_catalog: injected_text(&surface, SUBAGENT_CATALOG_CHANNEL, ""),
            // 通道字段优先;历史日志里这条消息也一直带 channel,无前缀兜底。
            system_update: injected_text(&surface, SYSTEM_UPDATE_CHANNEL, ""),
            synced_seq,
        }
    }

    /// 上一步之后新落了压缩事件时,把基准重新对齐到当前表面。返回是否对齐过。
    ///
    /// 轮内基准只在轮次开始恢复过一次,而自动压缩发生在 step 之间——压缩把
    /// 注入块挤出表面后,同一轮剩下的 step 会拿一个模型已经看不到的文本判定
    /// "内容没变",注入块要拖到下一轮才回来。对齐只在真的落了压缩时做:
    /// 多一次的全量派生不该落在未压缩会话的每 step 热路径上。
    pub(crate) fn realign_if_compacted(&mut self, session: &Session) -> bool {
        let (compacted, last) = compaction_since(session, self.synced_seq);
        self.synced_seq = last;
        if !compacted {
            return false;
        }
        *self = Self::restore(session);
        true
    }
}

/// `synced_seq` 之后是否落了压缩事件;返回 (是否压缩, 日志末尾 seq)。
///
/// 轮内基准(注入通道、运行时快照)都只在轮次开始对齐过一次,而自动压缩
/// 发生在 step 之间:压缩把注入块移出模型面后,同一轮剩下的 step 会拿一个
/// 模型已经看不到的文本判定"内容没变"。两张基准共用这一条检测,判据不会
/// 分叉;且只扫对齐点之后新落的事件,未压缩会话的每 step 代价是 O(新增),
/// 全量派生只在真的压缩过时才做。
pub(crate) fn compaction_since(session: &Session, synced_seq: u64) -> (bool, u64) {
    session.with_events(|events| {
        let compacted = events
            .iter()
            .rev()
            .take_while(|envelope| envelope.seq > synced_seq)
            .any(|envelope| matches!(envelope.event, SessionEvent::CompactionSummary { .. }));
        (compacted, last_seq(events))
    })
}

/// 日志末尾的事件 seq(空日志为 0)。
pub(crate) fn last_seq(events: &[denia_core::session::SessionEnvelope]) -> u64 {
    events.last().map(|envelope| envelope.seq).unwrap_or(0)
}

/// 从派生 surface 逆序取最后一条本通道的注入文本(幂等基准)。
///
/// `legacy_prefix` 为空串表示本通道没有前缀兜底(只用通道字段)。非空时
/// 只在消息以该前缀开头时兜底识别早期日志:两条判据合在一次逆序扫描里,
/// 不会出现"通道命中旧值、前缀命中新值"的错位。
fn injected_text(surface: &[SurfaceMessage], channel: &str, legacy_prefix: &str) -> Option<String> {
    surface.iter().rev().find_map(|item| {
        if item.message.role != ChatRole::User {
            return None;
        }
        let text = &item.message.content;
        let matched = item.channel.as_deref() == Some(channel)
            || (!legacy_prefix.is_empty() && text.starts_with(legacy_prefix));
        matched.then(|| text.clone())
    })
}

/// 日志里最后一条**真实**用户消息文本(注入块不算):项目记忆选择层的
/// 关键词来源。
///
/// 走 append-only 日志而不是派生 surface:压缩把旧消息移出模型面时用户的
/// 诉求没有变,选择结果也不该跟着变——否则每次压缩都会换一次注入正文,
/// 白白多一次重发。
fn last_user_text(session: &Session) -> Option<String> {
    session.with_events(|events| {
        events
            .iter()
            .rev()
            .find_map(|envelope| match &envelope.event {
                SessionEvent::UserMessage {
                    text,
                    injected: false,
                    ..
                } => Some(text.clone()),
                _ => None,
            })
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
    // 上一步之间压缩把注入块挤出模型面的话,先把基准对齐到当前表面:
    // 否则本轮剩下的 step 会拿模型已经看不到的文本判定"内容没变"。
    baselines.realign_if_compacted(session);
    // 会话组装的功能开关:装配层按它摘工具与纪律段,这里按它关掉纯通道。
    // 两处读的是同一份声明,features 关闭的功能不会以任何形态泄漏给模型。
    let preset_features = driver.features_for(session);

    // ① 工作区指令(AGENTS.md):发现/预算/替换语义在 runtime 侧;
    // restore 的旧文本作为 previous 传入,由正文比较决定幂等与"取代"引导语。
    // 组装关闭 AGENTS.md 注入时整条通道跳过(对齐 dsh:不带 agent-instructions
    // 行的组装完全不注入)。
    if preset_features.agents_md {
        match runtime
            .workspace_instructions(
                session.id(),
                &cwd,
                touched,
                baselines.workspace_baseline.as_deref(),
            )
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
                            channel: Some(WORKSPACE_CHANNEL.into()),
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
                channel: Some(CAPABILITY_CHANNEL.into()),
                images: Vec::new(),
            },
        )?;
        baselines.capability_context = Some(context);
    }

    // ③ 技能目录:仅当组装开启技能且 skill 工具对该会话可见(子代理按
    // 冻结授权同装配过滤);从未发布且为空则不发消息,整块替换语义同工作区指令。
    let skill_tool_visible = preset_features.skills && child_allows(session, "skill");
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
                            channel: Some(SKILL_CHANNEL.into()),
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

    // ④ 项目记忆索引:仅组装开启记忆的主代理会话注入(子代理不烧这份
    // token,提取子代理的素材由任务提示词自带);内容不变不重发,后台提取
    // 更新索引后下一个 step 自动带出最新版。
    // 选择层按本轮已触碰的路径与最新一条用户消息挑相关条目——同一输入
    // 给出同一正文(见 read_index_for_injection 的确定性口径)。
    let memory_visible = preset_features.memory && session.header().subagent.is_none();
    if memory_visible {
        let recent_user = last_user_text(session);
        match runtime
            .project_memory_index(&cwd, touched, recent_user.as_deref())
            .await
        {
            Ok(Some(text)) => {
                if baselines.project_memory.as_deref() != Some(&text) {
                    append(
                        session,
                        &state.emit,
                        SessionEvent::UserMessage {
                            text: text.clone(),
                            injected: true,
                            channel: Some(MEMORY_CHANNEL.into()),
                            images: Vec::new(),
                        },
                    )?;
                    baselines.project_memory = Some(text);
                }
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(
                session_id = session.id(),
                error = %error,
                "project memory index refresh failed"
            ),
        }
    }

    // ⑤ 会话目标:状态块仅对开启 goal 且 goal 工具可见的会话注入(子代理按
    // 冻结授权同装配过滤;子代理默认不含 get_goal)。内容不变不重发——turn
    // 运行中用户编辑目标(steering)或状态转换后,下一 step 自动带出新状态;
    // 目标被清除后发一次终局通知。
    let goal_tool_visible = preset_features.goal && child_allows(session, "get_goal");
    if goal_tool_visible {
        let goal_text = match session.goal() {
            Some(goal) => Some(render_goal_block(
                &goal,
                session.goal_tokens_used().unwrap_or(0),
            )),
            None => baselines
                .goal
                .as_ref()
                .map(|_| GOAL_CLEARED_TEXT.to_string()),
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

    // ⑥ 子代理派遣目录：只对能派遣的父会话注入（组装关闭 subagents 或本身是
    // 子代理时整条通道不出现——目录与派遣纪律同进退）。目录内容跟随 profile
    // revision 变化，整块替换语义同工作区指令。
    if preset_features.subagents && session.header().subagent.is_none() {
        match runtime.subagent_catalog(session.id()).await {
            Ok(Some(text)) => {
                if baselines.subagent_catalog.as_deref() != Some(&text) {
                    append(
                        session,
                        &state.emit,
                        SessionEvent::UserMessage {
                            text: text.clone(),
                            injected: true,
                            channel: Some(SUBAGENT_CATALOG_CHANNEL.into()),
                            images: Vec::new(),
                        },
                    )?;
                    baselines.subagent_catalog = Some(text);
                }
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(
                session_id = session.id(),
                error = %error,
                "subagent catalog refresh failed"
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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use denia_core::message::ChatMessage;
    use std::sync::{Arc, Mutex};

    fn item(seq: u64, text: &str, channel: Option<&str>) -> SurfaceMessage {
        SurfaceMessage {
            seq,
            message: ChatMessage::user(text),
            is_error: false,
            channel: channel.map(str::to_string),
        }
    }

    #[test]
    fn baseline_takes_last_text_of_the_channel_or_of_the_legacy_prefix() {
        // 现代日志:通道字段命中,普通用户消息被跳过。
        let surface = vec![
            item(1, "<system-reminder>\n工作区指令:旧", None),
            item(2, "普通用户消息", None),
            item(
                3,
                "<system-reminder>\n工作区指令:新",
                Some(WORKSPACE_CHANNEL),
            ),
        ];
        assert_eq!(
            injected_text(&surface, WORKSPACE_CHANNEL, WORKSPACE_PREFIX).as_deref(),
            Some("<system-reminder>\n工作区指令:新")
        );

        // 旧日志(无 channel 字段)走前缀兜底;较新的一条即基准——
        // 判据合在一次逆序扫描里,不会挑到更早的通道命中。
        let surface = vec![
            item(
                1,
                "<system-reminder>\n工作区指令:通道",
                Some(WORKSPACE_CHANNEL),
            ),
            item(2, "<system-reminder>\n工作区指令:前缀", None),
        ];
        assert_eq!(
            injected_text(&surface, WORKSPACE_CHANNEL, WORKSPACE_PREFIX).as_deref(),
            Some("<system-reminder>\n工作区指令:前缀")
        );

        // 没有前缀兜底的通道:只认通道字段。
        assert_eq!(injected_text(&surface, GOAL_CHANNEL, ""), None);
        assert_eq!(
            injected_text(&[], WORKSPACE_CHANNEL, WORKSPACE_PREFIX),
            None
        );
    }

    fn temp_session() -> Session {
        let dir = std::env::temp_dir().join(format!(
            "denia-injections-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Session::create(&dir, uuid::Uuid::new_v4().to_string(), &dir, true, None).unwrap()
    }

    /// 压缩落在轮次中途时的重新对齐:同一轮剩下的 step 必须知道注入块
    /// 已经不在模型面上(否则要等到下一轮才重发);没有新压缩时不得
    /// 反复重新对齐(对齐点必须真的推进,扫描不回头)。
    #[test]
    fn baselines_realign_when_a_compaction_lands_mid_turn() {
        let session = temp_session();
        let injected = |text: &str| SessionEvent::UserMessage {
            text: text.to_string(),
            injected: true,
            channel: Some(SKILL_CHANNEL.into()),
            images: Vec::new(),
        };
        session
            .append(injected("<system-reminder>\n技能目录:目录A"))
            .unwrap();
        let mut baselines = InjectionBaselines::restore(&session);
        assert_eq!(
            baselines.skill_catalog.as_deref(),
            Some("<system-reminder>\n技能目录:目录A")
        );

        // 日志往前走了但没压缩:基准不动。
        session
            .append(SessionEvent::UserMessage {
                text: "普通消息".into(),
                injected: false,
                channel: None,
                images: Vec::new(),
            })
            .unwrap();
        assert!(!baselines.realign_if_compacted(&session));
        assert_eq!(
            baselines.skill_catalog.as_deref(),
            Some("<system-reminder>\n技能目录:目录A")
        );

        // 压缩把注入块移出模型面:同一轮内重新对齐成"模型面上没有"。
        let last = session.with_events(|events| events.last().unwrap().seq);
        session
            .append(SessionEvent::CompactionSummary {
                turn: 1,
                step: 1,
                summary: "总结".into(),
                replaces_from: 1,
                replaces_to: last,
                keep_from: last + 1,
                pre_tokens: 0,
                post_tokens: 0,
            })
            .unwrap();
        assert!(
            baselines.realign_if_compacted(&session),
            "压缩后必须重新对齐"
        );
        assert_eq!(baselines.skill_catalog, None, "注入块已不在模型面");

        // 重发(append 后基准同步更新)之后,不得把旧压缩再当成新压缩。
        let text = "<system-reminder>\n技能目录:目录A";
        session.append(injected(text)).unwrap();
        baselines.skill_catalog = Some(text.to_string());
        assert!(
            !baselines.realign_if_compacted(&session),
            "没有新压缩不得反复重新对齐"
        );
        assert_eq!(baselines.skill_catalog.as_deref(), Some(text));
    }

    // —— 项目记忆通道:选择依据的传递与"内容未变不重发" ——

    /// 记忆通道的桩:记录每次调用收到的 (touched, recent_user),正文由触碰
    /// 路径数派生——于是"输入未变 → 正文未变 → 不重发"可以在通道层被观察,
    /// 而"输入变了"必然换一次正文(最坏情况的注入频率)。
    struct MemoryRuntime {
        index: String,
        calls: Mutex<Vec<(Vec<PathBuf>, Option<String>)>>,
    }

    impl MemoryRuntime {
        fn new(index: &str) -> Self {
            Self {
                index: index.to_string(),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(Vec<PathBuf>, Option<String>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl denia_tools::capabilities::AgentRuntime for MemoryRuntime {
        async fn execute_command(
            &self,
            _command: denia_tools::runtime_command::RuntimeCommand,
            _ctx: &denia_tools::ToolContext,
        ) -> Result<serde_json::Value, String> {
            Err("记忆桩没有可执行命令".to_string())
        }

        async fn context(
            &self,
            _session: &str,
            _cwd: &std::path::Path,
        ) -> Result<Vec<String>, String> {
            Ok(Vec::new())
        }

        async fn drain(&self, _session: &str) -> Result<Vec<String>, String> {
            Ok(Vec::new())
        }

        async fn project_memory_index(
            &self,
            _cwd: &std::path::Path,
            touched: &[PathBuf],
            recent_user: Option<&str>,
        ) -> Result<Option<String>, String> {
            self.calls
                .lock()
                .unwrap()
                .push((touched.to_vec(), recent_user.map(str::to_string)));
            Ok(Some(format!(
                "<system-reminder>\n{}\n(本轮已触碰 {} 个路径)\n</system-reminder>",
                self.index,
                touched.len()
            )))
        }
    }

    /// 只装配注入通道所需的驱动:无模型适配器(本测试不发起请求)。
    fn stub_driver(runtime: Arc<dyn denia_tools::capabilities::AgentRuntime>) -> SessionDriver {
        let prompt =
            denia_system_prompt::SystemPrompt::new(denia_system_prompt::SystemPromptConfig {
                include_runtime_context: false,
                ..Default::default()
            });
        SessionDriver::new(
            Arc::new(denia_llm::LlmRegistry::new()),
            Arc::new(denia_tools::ToolRegistry::default()),
            Arc::new(arc_swap::ArcSwap::from_pointee(prompt)),
        )
        .with_runtime(runtime)
    }

    fn stub_state(driver: &SessionDriver, session: Arc<Session>) -> TurnState {
        TurnState::new(
            driver,
            session,
            Arc::new(|_| {}),
            denia_core::config::ModelSelection {
                provider: "stub".to_string(),
                model: "stub-1".to_string(),
                reasoning_effort: None,
            },
            false,
            tokio_util::sync::CancellationToken::new(),
            crate::loop_guard::LoopGuard::default(),
            denia_tools::read_state::shared(),
        )
    }

    /// 日志里每一条项目记忆索引注入的文本。
    fn memory_injections(session: &Session) -> Vec<String> {
        session.with_events(|events| {
            events
                .iter()
                .filter_map(|envelope| match &envelope.event {
                    SessionEvent::UserMessage {
                        text,
                        injected: true,
                        channel: Some(name),
                        ..
                    } if name == MEMORY_CHANNEL => Some(text.clone()),
                    _ => None,
                })
                .collect()
        })
    }

    /// 稳定性:选择依据(cwd/touched/最近用户消息/索引文本)一字未变时,
    /// 注入通道不得产生新的注入文本;依据变了才发一次,而且只发一次。
    /// 顺带覆盖传递链路:通道把本轮 `touched` 与最新一条真实用户消息交给了
    /// 实现方(压缩不影响它——它读 append-only 日志)。
    #[tokio::test]
    async fn memory_channel_republishes_only_when_selection_inputs_change() {
        let runtime = Arc::new(MemoryRuntime::new("固定索引正文"));
        let driver = stub_driver(runtime.clone());
        let session = Arc::new(temp_session());
        session
            .append(SessionEvent::UserMessage {
                text: "看一下 injections.rs".to_string(),
                injected: false,
                channel: None,
                images: Vec::new(),
            })
            .unwrap();
        let mut state = stub_state(&driver, session.clone());
        let mut baselines = InjectionBaselines::restore(&session);

        let empty: Vec<PathBuf> = Vec::new();
        refresh_background_injections(&driver, &mut state, &mut baselines, &empty)
            .await
            .unwrap();
        assert_eq!(memory_injections(&session).len(), 1, "首个 step 注入一次");
        assert!(memory_injections(&session)[0].contains("固定索引正文"));

        // 同一输入连续两步:正文一字未变 → 不重发。
        refresh_background_injections(&driver, &mut state, &mut baselines, &empty)
            .await
            .unwrap();
        refresh_background_injections(&driver, &mut state, &mut baselines, &empty)
            .await
            .unwrap();
        assert_eq!(memory_injections(&session).len(), 1, "输入未变不得重发");

        // 触碰路径变了(选择依据变了)→ 发一次,此后同一输入再刷也不重发。
        let touched = vec![PathBuf::from("src/injections.rs")];
        refresh_background_injections(&driver, &mut state, &mut baselines, &touched)
            .await
            .unwrap();
        assert_eq!(memory_injections(&session).len(), 2);
        refresh_background_injections(&driver, &mut state, &mut baselines, &touched)
            .await
            .unwrap();
        assert_eq!(
            memory_injections(&session).len(),
            2,
            "最坏情况也只是每个新输入一次"
        );

        // 传递链路:实现方收到的 touched 与最近用户消息就是通道收到的。
        let calls = runtime.calls();
        assert_eq!(calls.len(), 5, "每个 step 都问一次实现方");
        assert_eq!(calls[0].0, Vec::<PathBuf>::new());
        assert_eq!(calls[0].1.as_deref(), Some("看一下 injections.rs"));
        assert_eq!(calls[4].0, touched);
    }
}
