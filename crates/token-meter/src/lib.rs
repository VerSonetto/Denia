//! dsh `@deepseek-ai/dsh-token-meter` 三层投影的 Rust 镜像。
//!
//! 1. `ContextBreakdown` —— **纯启发式**:固定密度 4 char/token + 块/角色框,
//!    拆成 system / tools / message 三行。同 dsh:不汇总,也不指望与
//!    `contextPressure` 相加。
//! 2. `TurnTokenUsage` —— **精确**,provider 上报的 usage 累加。`usage`
//!    缺失或 lifecycle 缺段的轮次不进总和(同 dsh `deriveTurnTokenUsage`:
//!    任何不完整则整个 Turn 跳过)。
//! 3. `ContextPressure` —— **锚点 + 表面增量**(wire 形状对齐 dsh)。最近
//!    一次 provider usage 提供精确的 prompt 侧锚点;锚点之后消息的启发式
//!    增量叠加出 `projectedTokens`(回答下一次请求的 prompt 规模);路由
//!    容量来自 `request/context` 记录。没有 usage 样本就没有锚点,不做
//!    启发式兜底 —— provider 没报数就不显示占用,同 dsh。

use std::collections::HashMap;

use denia_core::message::{ChatMessage, ChatRole, ToolCallRef};
use denia_core::session::{SessionEnvelope, SessionEvent};
use denia_core::stream::{ContentBlock, TokenUsage};

/// 固定密度启发式:文本按 `len / 4` 向上取整换算 token。
pub const CHARS_PER_TOKEN: u64 = 4;
/// 内容块结构开销(JSON 框与类型标签)。
pub const BLOCK_OVERHEAD: u64 = 4;
/// 每条消息角色框开销。
pub const ROLE_OVERHEAD: u64 = 4;

/// 一段上下文的 token 组成(纯估算)。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextBreakdown {
    pub system_tokens: u64,
    pub tools_tokens: u64,
    pub message_tokens: u64,
}

/// 一个 Turn 上累加的精确 usage(同 dsh `TurnTokenUsage`)。
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnTokenUsage {
    pub uncached_input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
}

/// 上下文压力投影(对齐 dsh `contextPressure` 的 wire 形状,字段各自
/// last-wins、可缺省,None 时不序列化)。
///
/// - `contextWindow`:最新 `request/context` 记录的路由容量。
/// - `pressureTokens`:最近一次 provider usage 的 prompt 侧总量
///   (input + cache_read + cache_write,不含输出);没有 usage 样本就没有
///   该字段 —— dsh 语义:provider 没报数就不显示占用,不做启发式兜底。
/// - `projectedTokens`:锚点加上锚点之后表面的启发式增量,回答"下一次
///   请求的 prompt 有多大",而不是"上一次有多大"。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextPressure {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pressure_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projected_tokens: Option<u64>,
}

fn estimate_text(text: &str) -> u64 {
    (text.len() as u64).div_ceil(CHARS_PER_TOKEN)
}

fn estimate_tool_calls(calls: &[ToolCallRef]) -> u64 {
    let mut tokens = 0u64;
    for call in calls {
        tokens = tokens
            .saturating_add(BLOCK_OVERHEAD)
            .saturating_add(estimate_text(&call.id))
            .saturating_add(estimate_text(&call.name))
            .saturating_add(estimate_text(&call.arguments));
    }
    tokens
}

fn estimate_message(message: &ChatMessage) -> u64 {
    let mut tokens = ROLE_OVERHEAD;
    tokens = tokens.saturating_add(estimate_text(&message.content));
    if matches!(message.role, ChatRole::Tool) {
        return tokens;
    }
    if let Some(reasoning) = &message.reasoning_content {
        tokens = tokens
            .saturating_add(BLOCK_OVERHEAD)
            .saturating_add(estimate_text(reasoning));
    }
    tokens = tokens.saturating_add(estimate_tool_calls(&message.tool_calls));
    for image in &message.images {
        tokens = tokens
            .saturating_add(BLOCK_OVERHEAD)
            .saturating_add(estimate_text(&image.mime))
            .saturating_add(estimate_text(&image.data));
    }
    tokens
}

fn extract_assistant_message(blocks: &[ContentBlock]) -> Option<ChatMessage> {
    let text: String = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let calls: Vec<ToolCallRef> = blocks
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
    if text.is_empty() && calls.is_empty() {
        None
    } else {
        // 与 derive_messages 一致:模型历史剥离 reasoning_content,表面估算
        // 也只按实际回传的 assistant 消息计价。
        Some(ChatMessage::assistant(text, None, calls))
    }
}

/// 输入为 "input + cache_read + cache_write",等价于 provider 的 prompt 计费桶。
fn prompt_tokens(usage: &TokenUsage) -> u64 {
    let cache = usage.cache_read_tokens.unwrap_or(0);
    let cache_write = usage.cache_write_tokens_opt().unwrap_or(0);
    usage.input_tokens.saturating_add(cache).saturating_add(cache_write)
}

/// 用 dsh `deriveTurnTokenUsage` 思路 fold 一个 turn 的精确 usage。
///
/// 接受一个 turn 起止区间内的所有 envelope(turn-start 到 turn-end),用每个
/// 步骤的 AssistantMessage.usage 累加(缺失的步骤整 turn 跳过)。每个 step
/// 的 input/cache_read/cache_write/output 各求和,reasoning 同。
fn derive_turn_token_usage(events: &[SessionEnvelope]) -> Option<TurnTokenUsage> {
    let mut usage = TurnTokenUsage::default();
    let mut open_turn: Option<u32> = None;
    let mut step_attempt: Option<(u32, u32)> = None;
    let mut step_usage_sample: Option<TokenUsage> = None;
    let mut invalid = false;
    let mut found_turn = false;

    fn close_attempt(
        usage: &mut TurnTokenUsage,
        step_usage_sample: &mut Option<TokenUsage>,
        step_attempt: &mut Option<(u32, u32)>,
        invalid: &mut bool,
    ) {
        let sample = step_usage_sample.take();
        // 取走 step_attempt,若 sample 缺失整 turn 拒绝。
        if step_attempt.take().is_some() && sample.is_none() {
            *invalid = true;
            return;
        }
        if let Some(sample) = sample {
            usage.uncached_input_tokens = usage
                .uncached_input_tokens
                .saturating_add(sample.input_tokens);
            usage.output_tokens = usage.output_tokens.saturating_add(sample.output_tokens);
            if let Some(cache) = sample.cache_read_tokens {
                usage.cache_read_tokens = usage.cache_read_tokens.saturating_add(cache);
            }
            if let Some(write) = sample.cache_write_tokens_opt() {
                usage.cache_write_tokens = usage.cache_write_tokens.saturating_add(write);
            }
            if let Some(reasoning) = sample.reasoning_tokens {
                usage.reasoning_tokens = usage.reasoning_tokens.saturating_add(reasoning);
            }
        }
    }

    for envelope in events {
        match &envelope.event {
            SessionEvent::TurnStart { turn } => {
                if open_turn.is_some() {
                    invalid = true;
                    break;
                }
                open_turn = Some(*turn);
                found_turn = true;
            }
            SessionEvent::TurnEnd { turn, .. } => {
                if Some(*turn) != open_turn {
                    invalid = true;
                    break;
                }
                close_attempt(&mut usage, &mut step_usage_sample, &mut step_attempt, &mut invalid);
                open_turn = None;
            }
            SessionEvent::StepStart { turn, step } => {
                if Some(*turn) != open_turn || step_attempt.is_some() {
                    invalid = true;
                    break;
                }
                step_attempt = Some((*turn, *step));
            }
            SessionEvent::StepEnd { turn, step } => {
                if Some((*turn, *step)) != step_attempt {
                    invalid = true;
                    break;
                }
                close_attempt(&mut usage, &mut step_usage_sample, &mut step_attempt, &mut invalid);
            }
            SessionEvent::AssistantMessage {
                turn,
                step,
                usage: Some(sample),
                ..
            } => {
                if Some((*turn, *step)) != step_attempt {
                    invalid = true;
                    break;
                }
                step_usage_sample = Some(*sample);
            }
            _ => {}
        }
    }
    if invalid || !found_turn || open_turn.is_some() {
        return None;
    }
    Some(usage)
}

trait TokenUsageExt {
    fn cache_write_tokens_opt(&self) -> Option<u64>;
}

impl TokenUsageExt for TokenUsage {
    fn cache_write_tokens_opt(&self) -> Option<u64> {
        // denia 当前 TokenUsage 不含 cache_write;扩字段会破坏日志格式,
        // 因此总是 None,以保持字段名稳定。
        let _ = self;
        None
    }
}

/// 增量 fold:维护 `ContextBreakdown`(纯估算)+ `TurnTokenUsage`(精确)+ `ContextPressure`(锚点 + 表面增量)。
///
/// 口径完全对齐 dsh `contextPressure` / `contextBreakdown` 投影:
/// - 表面总量(`surface_tokens`)对所有模型可见消息持续累加,不因锚点重置;
/// - 每个 usage 样本(AssistantMessage.usage)都重设锚点,且在**该消息加入
///   表面之前**盖章 —— 样本对应的请求不包含它自己的回复,锚点必须对着
///   请求所见的表面;
/// - 路由容量来自最新 `request/context` 记录;
/// - 没有 usage 样本就没有 `pressureTokens`,不做启发式兜底。
pub struct ContextMeter {
    system_tokens: u64,
    tools_tokens: u64,
    /// 模型可见消息的启发式运行总量(对齐 dsh `surfaceTokens`)。
    surface_tokens: u64,
    /// 每个 surface 节点(事件 seq)的启发式 token 数;替换剪枝时据此扣减
    /// 旧节点(对齐 dsh shadow-price 的净效果;内存版不需要落 shadow 事件)。
    surface_nodes: HashMap<u64, u64>,
    /// session 内累计的精确 usage(每个完成的 Turn 累加一次)。
    turn_usage: TurnTokenUsage,
    /// 最近一次 provider usage 的 prompt 侧总量(锚点;None = 无样本)。
    pressure_tokens: Option<u64>,
    /// 取锚点时的表面总量;锚点后的表面增量 = surface - sampled。
    sampled_surface_tokens: Option<u64>,
    /// 最新 `request/context` 的路由容量(上下文窗口)。
    context_window: Option<u64>,
}

impl ContextMeter {
    pub fn new() -> Self {
        Self {
            system_tokens: 0,
            tools_tokens: 0,
            surface_tokens: 0,
            surface_nodes: HashMap::new(),
            turn_usage: TurnTokenUsage::default(),
            pressure_tokens: None,
            sampled_surface_tokens: None,
            context_window: None,
        }
    }

    /// 单事件增量回放。
    pub fn apply_one(&mut self, envelope: &SessionEnvelope) {
        match &envelope.event {
            SessionEvent::RequestContext { context_window, .. } => {
                // 路由容量 last-wins;未通告时移除(dsh 同语义)。
                self.context_window = *context_window;
            }
            SessionEvent::SystemPrompt { .. } => {
                // 不参与 fold;由 driver 调 `set_system_tokens` 喂 framed 版本。
            }
            SessionEvent::UserMessage { text, .. } => {
                let message = ChatMessage::user(text);
                self.fold_message_add(envelope.seq, estimate_message(&message));
            }
            SessionEvent::AssistantMessage { blocks, usage, .. } => {
                // usage 样本先于本消息入表:消息本身不在产生它的请求里。
                if let Some(sample) = usage {
                    self.pressure_tokens = Some(prompt_tokens(sample));
                    self.sampled_surface_tokens = Some(self.surface_tokens);
                }
                if let Some(message) = extract_assistant_message(blocks) {
                    self.fold_message_add(envelope.seq, estimate_message(&message));
                }
            }
            SessionEvent::ToolResult {
                content,
                replaces,
                ..
            } => {
                let message = ChatMessage::tool_result("__unused__", content);
                let tokens = estimate_message(&message);
                if let Some(replaced_seq) = replaces {
                    // 剪枝替换:从表面总量里扣掉旧节点,再按当前 seq 落新节点。
                    if let Some(old_tokens) = self.surface_nodes.remove(replaced_seq) {
                        self.surface_tokens = self.surface_tokens.saturating_sub(old_tokens);
                    } else {
                        // 旧节点不在表面(异常/旧日志):降级为普通追加,不破坏不变量。
                    }
                }
                self.fold_message_add(envelope.seq, tokens);
            }
            _ => {}
        }
    }

    /// 一批事件按顺序增量回放。
    pub fn fold(&mut self, events: &[SessionEnvelope]) {
        for envelope in events {
            self.apply_one(envelope);
        }
    }

    /// 把 Turn 闭包内的事件喂进来,fold 出该 turn 的精确 usage 并并入会话累计。
    /// 必须在 turn 闭合时(`turn-end` 之后)调用;返回的 Option 表示该 turn
    /// 是不是 fold 成功。锚点不在这里设:每个 usage 样本由 `apply_one` 处理。
    pub fn fold_turn(&mut self, events: &[SessionEnvelope]) -> bool {
        if let Some(turn) = derive_turn_token_usage(events) {
            self.turn_usage.uncached_input_tokens = self
                .turn_usage
                .uncached_input_tokens
                .saturating_add(turn.uncached_input_tokens);
            self.turn_usage.output_tokens = self
                .turn_usage
                .output_tokens
                .saturating_add(turn.output_tokens);
            self.turn_usage.cache_read_tokens = self
                .turn_usage
                .cache_read_tokens
                .saturating_add(turn.cache_read_tokens);
            self.turn_usage.cache_write_tokens = self
                .turn_usage
                .cache_write_tokens
                .saturating_add(turn.cache_write_tokens);
            self.turn_usage.reasoning_tokens = self
                .turn_usage
                .reasoning_tokens
                .saturating_add(turn.reasoning_tokens);
            true
        } else {
            false
        }
    }

    fn fold_message_add(&mut self, seq: u64, tokens: u64) {
        self.surface_tokens = self.surface_tokens.saturating_add(tokens);
        self.surface_nodes.insert(seq, tokens);
    }

    /// 读当前快照(纯启发式拆分;message = 表面运行总量,不因锚点重置)。
    pub fn breakdown(&self) -> ContextBreakdown {
        ContextBreakdown {
            system_tokens: self.system_tokens,
            tools_tokens: self.tools_tokens,
            message_tokens: self.surface_tokens,
        }
    }

    /// 读当前精确 usage 累计。
    pub fn turn_usage(&self) -> TurnTokenUsage {
        self.turn_usage.clone()
    }

    /// 读当前压力投影(锚点 + 锚点后表面增量)。
    pub fn context_pressure(&self) -> ContextPressure {
        let projected = self.pressure_tokens.map(|pressure| {
            let sampled = self.sampled_surface_tokens.unwrap_or(0);
            let drift = self.surface_tokens.saturating_sub(sampled);
            pressure.saturating_add(drift)
        });
        ContextPressure {
            context_window: self.context_window,
            pressure_tokens: self.pressure_tokens,
            projected_tokens: projected,
        }
    }

    /// 更新系统提示词 token(framed 版本)。
    pub fn set_system_tokens(&mut self, tokens: u64) {
        self.system_tokens = tokens;
    }

    /// 更新工具声明 token。
    pub fn set_tools_tokens(&mut self, tokens: u64) {
        self.tools_tokens = tokens;
    }
}

impl Default for ContextMeter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::session::SessionEnvelope;

    fn envelope(seq: u64, event: SessionEvent) -> SessionEnvelope {
        SessionEnvelope {
            seq,
            time: 1_700_000_000_000 + seq,
            event,
        }
    }

    #[test]
    fn estimate_simple_text() {
        let m = ChatMessage::user("hello world");
        assert_eq!(estimate_message(&m), ROLE_OVERHEAD + (11_u64.div_ceil(4)));
    }

    #[test]
    fn system_prompt_event_ignored_in_breakdown() {
        let events = vec![
            envelope(1, SessionEvent::UserMessage { text: "a".into(), injected: false, images: Vec::new() }),
            envelope(
                2,
                SessionEvent::SystemPrompt {
                    turn: 1,
                    step: 1,
                    text: "任一".into(),
                },
            ),
        ];
        let mut meter = ContextMeter::new();
        meter.fold(&events);
        assert_eq!(meter.breakdown().system_tokens, 0);
        assert!(meter.breakdown().message_tokens > 0);
    }

    #[test]
    fn derive_turn_token_usage_with_two_attempts() {
        let events = vec![
            envelope(1, SessionEvent::TurnStart { turn: 1 }),
            envelope(2, SessionEvent::StepStart { turn: 1, step: 1 }),
            envelope(
                3,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 1,
                    blocks: vec![ContentBlock::Text { text: "tool".into() }],
                    usage: Some(TokenUsage {
                        input_tokens: 100,
                        output_tokens: 5,
                        cache_read_tokens: Some(20),
                        reasoning_tokens: None,
                    }),
                    interrupted: false,
                    source_event_seqs: Vec::new(),
                },
            ),
            envelope(4, SessionEvent::StepEnd { turn: 1, step: 1 }),
            envelope(5, SessionEvent::StepStart { turn: 1, step: 2 }),
            envelope(
                6,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 2,
                    blocks: vec![ContentBlock::Text { text: "ok".into() }],
                    usage: Some(TokenUsage {
                        input_tokens: 80,
                        output_tokens: 3,
                        cache_read_tokens: Some(0),
                        reasoning_tokens: Some(2),
                    }),
                    interrupted: false,
                    source_event_seqs: Vec::new(),
                },
            ),
            envelope(7, SessionEvent::StepEnd { turn: 1, step: 2 }),
            envelope(8, SessionEvent::TurnEnd { turn: 1, reason: denia_core::session::TurnEndReason::Completed }),
        ];
        let u = derive_turn_token_usage(&events).expect("turn should fold");
        assert_eq!(u.uncached_input_tokens, 180);
        assert_eq!(u.output_tokens, 8);
        assert_eq!(u.cache_read_tokens, 20);
        assert_eq!(u.reasoning_tokens, 2);
    }

    #[test]
    fn derive_turn_token_usage_rejects_missing_usage() {
        // 缺 step 的 usage,整个 turn 应被拒绝。
        let events = vec![
            envelope(1, SessionEvent::TurnStart { turn: 1 }),
            envelope(2, SessionEvent::StepStart { turn: 1, step: 1 }),
            envelope(
                3,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 1,
                    blocks: vec![ContentBlock::Text { text: "x".into() }],
                    usage: None,
                    interrupted: false,
                    source_event_seqs: Vec::new(),
                },
            ),
            envelope(4, SessionEvent::StepEnd { turn: 1, step: 1 }),
            envelope(5, SessionEvent::TurnEnd { turn: 1, reason: denia_core::session::TurnEndReason::Completed }),
        ];
        assert!(derive_turn_token_usage(&events).is_none());
    }

    #[test]
    fn tool_result_replacement_shrinks_surface_tokens() {
        let mut meter = ContextMeter::new();
        let long = "x".repeat(4_000);
        let short = "pruned";
        let user_tokens = estimate_message(&ChatMessage::user("hi"));
        let long_tokens = estimate_message(&ChatMessage::tool_result("c", &long));
        let short_tokens = estimate_message(&ChatMessage::tool_result("c", short));

        meter.apply_one(&envelope(
            1,
            SessionEvent::UserMessage {
                text: "hi".into(),
                injected: false,
                images: Vec::new(),
            },
        ));
        meter.apply_one(&envelope(
            2,
            SessionEvent::ToolResult {
                turn: 1,
                step: 1,
                call_id: "c".into(),
                content: long,
                is_error: false,
                error: None,
                error_identity: None,
                meta: None,
                replaces: None,
            },
        ));
        assert_eq!(
            meter.breakdown().message_tokens,
            user_tokens + long_tokens
        );

        meter.apply_one(&envelope(
            3,
            SessionEvent::ToolResult {
                turn: 1,
                step: 1,
                call_id: "c".into(),
                content: short.into(),
                is_error: false,
                error: None,
                error_identity: None,
                meta: None,
                replaces: Some(2),
            },
        ));
        let after = meter.breakdown().message_tokens;
        assert_eq!(after, user_tokens + short_tokens);
        assert!(after < user_tokens + long_tokens);
    }

    #[test]
    fn pressure_anchor_uses_last_usage() {
        let mut meter = ContextMeter::new();
        // 模拟 turn 1 完整回放(锚点由 apply_one 在 usage 样本处设置)。
        let events = vec![
            envelope(1, SessionEvent::TurnStart { turn: 1 }),
            envelope(2, SessionEvent::StepStart { turn: 1, step: 1 }),
            envelope(
                3,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 1,
                    blocks: vec![ContentBlock::Text { text: "ok".into() }],
                    usage: Some(TokenUsage {
                        input_tokens: 100,
                        output_tokens: 5,
                        cache_read_tokens: Some(20),
                        reasoning_tokens: None,
                    }),
                    interrupted: false,
                    source_event_seqs: Vec::new(),
                },
            ),
            envelope(4, SessionEvent::StepEnd { turn: 1, step: 1 }),
            envelope(5, SessionEvent::TurnEnd { turn: 1, reason: denia_core::session::TurnEndReason::Completed }),
        ];
        meter.fold(&events);
        // 锚点 = input + cache_read = 120;锚点在本消息入表前盖章,所以
        // projected = 120 + 本消息启发式(dsh 语义:回答下一次请求规模)。
        let p = meter.context_pressure();
        assert_eq!(p.pressure_tokens, Some(120));
        let assistant_tokens = meter.breakdown().message_tokens;
        assert_eq!(p.projected_tokens, Some(120 + assistant_tokens));
        // 锚点之后多发一条 user 消息:projected 继续增长,锚点不动。
        meter.apply_one(&envelope(
            6,
            SessionEvent::UserMessage {
                text: "再来".into(),
                injected: false,
                images: Vec::new(),
            },
        ));
        let p2 = meter.context_pressure();
        assert_eq!(p2.pressure_tokens, Some(120));
        assert!(p2.projected_tokens.unwrap() > p.projected_tokens.unwrap());
        // breakdown 的 message_tokens 是表面运行总量,与 projected 的表面
        // 部分一致。
        assert!(meter.breakdown().message_tokens > assistant_tokens);
    }

    #[test]
    fn pressure_without_anchor_has_no_fallback() {
        // 无 usage 样本:没有 pressureTokens,不做启发式兜底(dsh 语义,
        // provider 没报数就不显示占用)。
        let mut meter = ContextMeter::new();
        meter.set_system_tokens(100);
        meter.set_tools_tokens(50);
        meter.apply_one(&envelope(
            1,
            SessionEvent::UserMessage {
                text: "hi".into(),
                injected: false,
                images: Vec::new(),
            },
        ));
        let p = meter.context_pressure();
        assert_eq!(p.pressure_tokens, None);
        assert_eq!(p.projected_tokens, None);
        assert_eq!(p.context_window, None);
        // 纯启发式拆分照常可读;message = 表面运行总量(不含 system/tools)。
        let b = meter.breakdown();
        assert_eq!(b.system_tokens, 100);
        assert_eq!(b.tools_tokens, 50);
        // role 4 + text ceil(2/4) 1 = 5。
        assert_eq!(b.message_tokens, 5);
    }

    #[test]
    fn request_context_supplies_window_last_wins() {
        let mut meter = ContextMeter::new();
        meter.apply_one(&envelope(
            1,
            SessionEvent::RequestContext {
                turn: 1,
                step: 1,
                provider: "deepseek".into(),
                model: "deepseek-chat".into(),
                context_window: Some(128_000),
            },
        ));
        assert_eq!(meter.context_pressure().context_window, Some(128_000));
        // 未通告的新记录移除容量(dsh 同语义)。
        meter.apply_one(&envelope(
            2,
            SessionEvent::RequestContext {
                turn: 1,
                step: 2,
                provider: "deepseek".into(),
                model: "deepseek-chat".into(),
                context_window: None,
            },
        ));
        assert_eq!(meter.context_pressure().context_window, None);
    }

    #[test]
    fn projected_tokens_answers_next_request() {
        // 锚点后发消息,projected = 锚点 + 锚点后表面增量;换模型重锚后,
        // 以新锚点为基准。
        let mut meter = ContextMeter::new();
        meter.apply_one(&envelope(
            1,
            SessionEvent::UserMessage {
                text: "hi".into(),
                injected: false,
                images: Vec::new(),
            },
        ));
        meter.apply_one(&envelope(
            2,
            SessionEvent::AssistantMessage {
                turn: 1,
                step: 1,
                blocks: vec![ContentBlock::Text { text: "ok".into() }],
                usage: Some(TokenUsage {
                    input_tokens: 1_000,
                    output_tokens: 5,
                    cache_read_tokens: Some(0),
                    reasoning_tokens: None,
                }),
                interrupted: false,
                source_event_seqs: Vec::new(),
            },
        ));
        let p = meter.context_pressure();
        assert_eq!(p.pressure_tokens, Some(1_000));
        // sampled = 锚点前表面(只有 user "hi"),projected = 1000 + assistant。
        let user_tokens = estimate_message(&ChatMessage::user("hi"));
        let assistant_tokens = estimate_message(&ChatMessage::assistant("ok", None, Vec::new()));
        assert_eq!(p.projected_tokens, Some(1_000 + assistant_tokens));
        assert_eq!(meter.breakdown().message_tokens, user_tokens + assistant_tokens);
    }
}
