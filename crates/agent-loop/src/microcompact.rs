//! 工具结果微压缩(microcompact):把旧的工具结果内容替换为占位符。
//!
//! 这是**在 LLM 摘要压缩之前**的一层低成本清理:不调模型、不重写历史,
//! 只是把已经用过的大块工具输出换成一句占位符,让上下文增速变缓。
//!
//! 与 denia 架构的契合点:落盘为带 `replaces` 的 `ToolResult` 事件,
//! 派生时原位替换(`derive_surface` 的 `surfaceOp.replace` 语义)——
//! **日志保持 append-only,前端仍能看到完整历史**,不破坏
//! "日志即真相"的不变量。
//!
//! # 为什么需要这一层
//!
//! 实测某会话 426 个 step 涨到 243K token、全程零压缩:只在压力达到
//! 90% 窗口时才做 LLM 摘要,而那时体验已经很差。在阈值之前就持续做
//! 这种廉价清理,能把增长曲线压平。
//!
//! # 关键保护(每条都有理由)
//!
//! - `min_savings = 256`:省不到这么多 token 就不动手。**保护 provider
//!   前缀缓存**——无谓的扰动会让缓存失效,得不偿失(实测缓存命中率
//!   99.2%,这个保护尤其重要)。
//! - **错误结果默认不动**:错误信息往往是模型纠正自己的唯一线索。
//! - **含图片/视频/文件的结果永不清**:多模态内容无法用文本占位符替代。
//! - **保留最近 N 组**:最近的工具结果几乎肯定还在被引用。
//! - **按轮次分组**:不把一轮内的多次工具调用切开,保持语义完整。

use std::collections::HashSet;

use denia_core::message::ChatRole;
use denia_core::session::SurfaceMessage;

/// 占位符文本(替换后的旧工具结果只留这一句)。
pub const CLEARED_PLACEHOLDER: &str = "[旧工具结果内容已清理]";

/// 微压缩配置。
#[derive(Debug, Clone, PartialEq)]
pub struct MicrocompactSettings {
    /// 总开关。
    pub enabled: bool,
    /// 保留最近多少**组**工具结果(一组 = 一次 assistant 消息发起的全部调用)。
    pub keep_recent_groups: usize,
    /// 空闲触发阈值(分钟):模型闲置超过这么久,下次进入时清理一次。
    pub idle_threshold_minutes: u64,
    /// 最小节省 token 数:省不到这么多就不动手(保护前缀缓存)。
    pub min_savings: u64,
    /// 可清理的工具白名单。
    pub compactable_tools: Vec<String>,
    /// 是否也清理错误结果(默认否)。
    pub clear_error_results: bool,
}

impl Default for MicrocompactSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            // 保留最近 5 组:再往前的旧结果基本不会再被引用。
            keep_recent_groups: 5,
            // 空闲 60 分钟:用户离开一段时间后回来,旧结果多半已无关。
            idle_threshold_minutes: 60,
            // 省不到 256 token 不动手:小改动会扰动前缀缓存,得不偿失。
            min_savings: 256,
            compactable_tools: default_compactable_tools(),
            clear_error_results: false,
        }
    }
}

/// 默认可清理工具白名单(能产生大块文本输出、且内容可被"重读"替代的工具)。
pub fn default_compactable_tools() -> Vec<String> {
    [
        "read_file",
        "bash",
        "grep",
        "glob",
        "ls",
        "edit",
        "write_file",
        "browser",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// 微压缩触发原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrocompactTrigger {
    /// 空闲超时(模型闲置很久,旧结果大概率不再需要)。
    TimeBased,
    /// token 压力(达到阈值)。
    TokenPressure,
}

/// 微压缩决策结果(用于日志与可观测性)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MicrocompactDecision {
    /// 已应用:给出被清理的 tool_call_id 列表。
    Applied {
        trigger: MicrocompactTrigger,
        cleared: Vec<String>,
        kept: Vec<String>,
        /// 被清理项在 surface 中的 seq(落盘 `replaces` 用)。
        cleared_seqs: Vec<u64>,
        estimated_savings: u64,
    },
    /// 未触发。
    NotTriggered(&'static str),
}

/// 一次待清理项。
#[derive(Debug, Clone)]
struct Candidate {
    /// surface 中的 seq(落盘 `replaces` 的定位符)。
    seq: u64,
    tool_call_id: String,
}

/// 判断某个工具结果是否已经被清理过(幂等保护)。
fn already_cleared(content: &str) -> bool {
    content.trim() == CLEARED_PLACEHOLDER
}

/// 粗略 token 估算:与 token-meter 的口径对齐(4 字符 ≈ 1 token)。
fn rough_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

/// 收集可清理的候选(按 assistant 轮次分组,组内按出现顺序)。
///
/// 一轮 = 一条带 tool_calls 的 assistant 消息 + 其后跟随的若干 tool 结果。
/// **不跨轮切分**,保证"一次发起的多个调用"要么全清、要么全留。
fn collect_candidates(
    surface: &[SurfaceMessage],
    settings: &MicrocompactSettings,
) -> Vec<Vec<Candidate>> {
    let compactable: HashSet<&str> = settings
        .compactable_tools
        .iter()
        .map(|s| s.as_str())
        .collect();

    let mut groups: Vec<Vec<Candidate>> = Vec::new();
    let mut current: Option<Vec<Candidate>> = None;

    for item in surface {
        let message = &item.message;
        match message.role {
            // 新的 assistant 轮次:收束上一组,开启新组。
            ChatRole::Assistant if !message.tool_calls.is_empty() => {
                if let Some(group) = current.take()
                    && !group.is_empty()
                {
                    groups.push(group);
                }
                current = Some(Vec::new());
            }
            ChatRole::Tool => {
                let Some(call_id) = message.tool_call_id.as_deref() else {
                    continue;
                };
                // 错误结果默认不动——它是模型纠正自己的线索。
                if item.is_error && !settings.clear_error_results {
                    continue;
                }
                // 幂等:已清理过的跳过。
                if already_cleared(&message.content) {
                    continue;
                }
                // 多模态内容永不清:图片/视频无法用文本占位符替代。
                if !message.images.is_empty() {
                    continue;
                }
                // 只清理白名单内的工具。工具名从 tool_call 映射里取不到时,
                // 用 call_id 前缀无从判断——保守起见跳过。
                let Some(tool_name) = tool_name_for(surface, call_id) else {
                    continue;
                };
                if !compactable.contains(tool_name) {
                    continue;
                }
                let candidate = Candidate {
                    seq: item.seq,
                    tool_call_id: call_id.to_string(),
                };
                match current.as_mut() {
                    Some(group) => group.push(candidate),
                    // 没有前导 assistant(理论上不该发生):自成一组。
                    None => groups.push(vec![candidate]),
                }
            }
            _ => {}
        }
    }
    if let Some(group) = current.take()
        && !group.is_empty()
    {
        groups.push(group);
    }
    groups
}

/// 从 surface 里找出某个 tool_call_id 对应的工具名。
fn tool_name_for<'a>(surface: &'a [SurfaceMessage], call_id: &str) -> Option<&'a str> {
    surface
        .iter()
        .flat_map(|item| item.message.tool_calls.iter())
        .find(|call| call.id == call_id)
        .map(|call| call.name.as_str())
}

/// 规划一次微压缩:返回待清理项与节省估算。
///
/// `pressure_tokens` / `threshold_tokens` 提供 token 压力触发条件;
/// `idle_minutes` 提供空闲触发条件。两者都不满足时返回 `NotTriggered`。
pub fn plan(
    surface: &[SurfaceMessage],
    settings: &MicrocompactSettings,
    idle_minutes: Option<u64>,
    pressure_tokens: Option<u64>,
    threshold_tokens: Option<u64>,
) -> MicrocompactDecision {
    if !settings.enabled {
        return MicrocompactDecision::NotTriggered("disabled");
    }

    // 触发判定(先空闲,后压力:空闲是更强的"确实不需要了"信号)。
    let trigger = if let Some(idle) = idle_minutes
        && idle > settings.idle_threshold_minutes
    {
        Some(MicrocompactTrigger::TimeBased)
    } else if let (Some(pressure), Some(threshold)) = (pressure_tokens, threshold_tokens)
        && threshold > 0
        && pressure >= threshold
    {
        Some(MicrocompactTrigger::TokenPressure)
    } else {
        None
    };
    let Some(trigger) = trigger else {
        return MicrocompactDecision::NotTriggered("not_triggered");
    };

    let groups = collect_candidates(surface, settings);
    if groups.is_empty() {
        return MicrocompactDecision::NotTriggered("no_candidates");
    }

    // 保留最近 N 组,清理其余(从最老的开始)。
    let keep = settings.keep_recent_groups.max(1);
    let clear_count = groups.len().saturating_sub(keep);
    if clear_count == 0 {
        return MicrocompactDecision::NotTriggered("nothing_to_clear");
    }

    let to_clear: Vec<Candidate> = groups[..clear_count].iter().flatten().cloned().collect();
    let to_keep: Vec<Candidate> = groups[clear_count..].iter().flatten().cloned().collect();
    if to_clear.is_empty() {
        return MicrocompactDecision::NotTriggered("nothing_to_clear");
    }

    // 估算节省:被清理项当前内容的 token 数,减去占位符的 token 数。
    let cleared_ids: HashSet<&str> = to_clear.iter().map(|c| c.tool_call_id.as_str()).collect();
    let saved: u64 = surface
        .iter()
        .filter(|item| {
            item.message.role == ChatRole::Tool
                && item
                    .message
                    .tool_call_id
                    .as_deref()
                    .is_some_and(|id| cleared_ids.contains(id))
        })
        .map(|item| {
            rough_tokens(&item.message.content)
                .saturating_sub(rough_tokens(CLEARED_PLACEHOLDER))
        })
        .sum();

    if saved < settings.min_savings {
        return MicrocompactDecision::NotTriggered("below_min_savings");
    }

    MicrocompactDecision::Applied {
        trigger,
        cleared: to_clear.iter().map(|c| c.tool_call_id.clone()).collect(),
        kept: to_keep.iter().map(|c| c.tool_call_id.clone()).collect(),
        cleared_seqs: to_clear.iter().map(|c| c.seq).collect(),
        estimated_savings: saved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::message::{ChatMessage, ToolCallRef};

    fn surface_with_rounds(rounds: usize, payload: &str) -> Vec<SurfaceMessage> {
        let mut surface = Vec::new();
        let mut seq = 1u64;
        for round in 0..rounds {
            let call_id = format!("call_{round}");
            surface.push(SurfaceMessage {
                seq,
                is_error: false,
                message: ChatMessage::assistant(
                    format!("第 {round} 轮"),
                    None,
                    vec![ToolCallRef {
                        id: call_id.clone(),
                        name: "read_file".into(),
                        arguments: format!(r#"{{"path":"f{round}.rs"}}"#),
                    }],
                ),
            });
            seq += 1;
            surface.push(SurfaceMessage {
                seq,
                message: ChatMessage::tool_result(call_id, payload),
                is_error: false,
            });
            seq += 1;
        }
        surface
    }

    fn settings() -> MicrocompactSettings {
        MicrocompactSettings {
            min_savings: 0,
            ..Default::default()
        }
    }

    #[test]
    fn keeps_recent_groups_and_clears_older() {
        let surface = surface_with_rounds(10, &"x".repeat(2000));
        let decision = plan(&surface, &settings(), None, Some(1000), Some(500));
        match decision {
            MicrocompactDecision::Applied {
                cleared,
                kept,
                trigger,
                ..
            } => {
                assert_eq!(trigger, MicrocompactTrigger::TokenPressure);
                assert_eq!(cleared.len(), 5, "10 组保留 5 组,清 5 组");
                assert_eq!(kept.len(), 5);
                assert_eq!(cleared[0], "call_0");
                assert_eq!(kept[4], "call_9");
            }
            other => panic!("应当触发清理,实际 {other:?}"),
        }
    }

    #[test]
    fn not_triggered_without_pressure() {
        let surface = surface_with_rounds(10, "small");
        let decision = plan(&surface, &settings(), None, Some(10), Some(500));
        assert_eq!(decision, MicrocompactDecision::NotTriggered("not_triggered"));
    }

    #[test]
    fn idle_triggers_cleanup() {
        let surface = surface_with_rounds(10, &"x".repeat(2000));
        let decision = plan(&surface, &settings(), Some(120), None, None);
        assert!(matches!(
            decision,
            MicrocompactDecision::Applied {
                trigger: MicrocompactTrigger::TimeBased,
                ..
            }
        ));
    }

    #[test]
    fn disabled_never_clears() {
        let surface = surface_with_rounds(10, &"x".repeat(2000));
        let cfg = MicrocompactSettings {
            enabled: false,
            ..settings()
        };
        assert_eq!(
            plan(&surface, &cfg, Some(999), Some(9999), Some(1)),
            MicrocompactDecision::NotTriggered("disabled")
        );
    }

    #[test]
    fn error_results_are_preserved() {
        let mut surface = surface_with_rounds(3, &"x".repeat(2000));
        // 把第 0 轮的结果改成错误。
        surface[1].is_error = true;
        let cfg = MicrocompactSettings {
            keep_recent_groups: 1,
            min_savings: 0,
            ..Default::default()
        };
        match plan(&surface, &cfg, None, Some(1000), Some(500)) {
            MicrocompactDecision::Applied { cleared, .. } => {
                assert!(
                    !cleared.contains(&"call_0".to_string()),
                    "错误结果不得被清理"
                );
            }
            other => panic!("应当触发,实际 {other:?}"),
        }
    }

    #[test]
    fn already_cleared_is_idempotent() {
        let mut surface = surface_with_rounds(3, &"x".repeat(2000));
        surface[1].message.content = CLEARED_PLACEHOLDER.to_string();
        let cfg = MicrocompactSettings {
            keep_recent_groups: 1,
            min_savings: 0,
            ..Default::default()
        };
        match plan(&surface, &cfg, None, Some(1000), Some(500)) {
            MicrocompactDecision::Applied { cleared, .. } => {
                assert!(!cleared.contains(&"call_0".to_string()));
            }
            other => panic!("应当触发,实际 {other:?}"),
        }
    }

    #[test]
    fn multimodal_results_are_preserved() {
        let mut surface = surface_with_rounds(3, &"x".repeat(2000));
        surface[1].message.images = vec![denia_core::message::ImageData {
            mime: "image/png".into(),
            data: "AAAA".into(),
        }];
        let cfg = MicrocompactSettings {
            keep_recent_groups: 1,
            min_savings: 0,
            ..Default::default()
        };
        match plan(&surface, &cfg, None, Some(1000), Some(500)) {
            MicrocompactDecision::Applied { cleared, .. } => {
                assert!(!cleared.contains(&"call_0".to_string()), "带图结果不得清理");
            }
            other => panic!("应当触发,实际 {other:?}"),
        }
    }

    #[test]
    fn below_min_savings_skips_cleanup() {
        // 内容很小,省不到 256 token → 不动手(保护前缀缓存)。
        let surface = surface_with_rounds(10, "tiny");
        let cfg = MicrocompactSettings::default();
        assert_eq!(
            plan(&surface, &cfg, None, Some(1000), Some(500)),
            MicrocompactDecision::NotTriggered("below_min_savings")
        );
    }

    #[test]
    fn non_compactable_tool_is_skipped() {
        let mut surface = Vec::new();
        surface.push(SurfaceMessage {
            seq: 1,
            message: ChatMessage::assistant(
                "t".to_string(),
                None,
                vec![ToolCallRef {
                    id: "c1".into(),
                    name: "todo_write".into(), // 不在白名单
                    arguments: "{}".into(),
                }],
            ),
            is_error: false,
        });
        surface.push(SurfaceMessage {
            seq: 2,
            message: ChatMessage::tool_result("c1", "x".repeat(5000)),
            is_error: false,
        });
        let cfg = MicrocompactSettings {
            keep_recent_groups: 1,
            min_savings: 0,
            ..Default::default()
        };
        assert_eq!(
            plan(&surface, &cfg, None, Some(9999), Some(1)),
            MicrocompactDecision::NotTriggered("no_candidates")
        );
    }

    #[test]
    fn keeps_recent_when_fewer_groups_than_keep() {
        let surface = surface_with_rounds(3, &"x".repeat(2000));
        let decision = plan(&surface, &settings(), None, Some(9999), Some(1));
        assert_eq!(
            decision,
            MicrocompactDecision::NotTriggered("nothing_to_clear")
        );
    }

    #[test]
    fn cleared_seqs_are_reported_for_replaces() {
        let surface = surface_with_rounds(4, &"x".repeat(2000));
        let cfg = MicrocompactSettings {
            keep_recent_groups: 1,
            min_savings: 0,
            ..Default::default()
        };
        match plan(&surface, &cfg, None, Some(9999), Some(1)) {
            MicrocompactDecision::Applied { cleared_seqs, .. } => {
                // 3 组被清,每组的 tool result seq 分别是 2/4/6。
                assert_eq!(cleared_seqs, vec![2, 4, 6]);
            }
            other => panic!("应当触发,实际 {other:?}"),
        }
    }
}
