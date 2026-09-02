//! dsh `@deepseek-ai/dsh-token-meter` 三层投影的 Rust 镜像。
//!
//! 1. `ContextBreakdown` —— **纯启发式**:固定密度 4 char/token + 块/角色框,
//!    拆成 system / tools / message 三行。同 dsh:不汇总,也不指望与
//!    `contextPressure` 相加。
//! 2. `TurnTokenUsage` —— **精确**,provider 上报的 usage 累加。`usage`
//!    缺失或 lifecycle 缺段的轮次不进总和(同 dsh `deriveTurnTokenUsage`:
//!    任何不完整则整个 Turn 跳过)。
//! 3. `ContextPressure` —— **锚点 + 启发式**。最近一次 provider usage 提供
//!    一个精确的"现在 prompt 多大"值;锚点之后的请求会引入新的输入消息,
//!    这些用启发式叠加;锚点未设时全部启发式。

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

/// 上下文压力(用于圆环面板:百分比基于 pressure / window)。
///
/// `pressure_tokens` 是 "下一次请求的 prompt 期望"——
/// - 若有 provider 锚点(最近 AssistantMessage.usage):锚点的
///   `input + cache` 视作当前 prompt 大小;锚点之后已折的新消息按启发式
///   累加。
/// - 无锚点:全部用启发式累加 message_tokens + system + tools。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextPressure {
    pub pressure_tokens: u64,
    /// 是否使用了 provider 精确锚点(同 dsh `contextPressure` 的语义)。
    pub anchored: bool,
    /// 最近一次 provider usage 报告的 prompt 总量(锚点本身)。
    pub anchor_tokens: u64,
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
    let reasoning: String = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Reasoning { text } => Some(text.as_str()),
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
        Some(ChatMessage::assistant(
            text,
            (!reasoning.is_empty()).then_some(reasoning),
            calls,
        ))
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

/// 增量 fold:维护 `ContextBreakdown`(纯估算)+ `TurnTokenUsage`(精确)+ `ContextPressure`(锚点 + 启发式)。
pub struct ContextMeter {
    system_tokens: u64,
    tools_tokens: u64,
    /// 启发式累计的 message_tokens;锚点之后从 anchor 重新计。
    message_tokens: u64,
    /// session 内累计的精确 usage(每个完成的 Turn 累加一次)。
    turn_usage: TurnTokenUsage,
    /// 最近一次 provider usage 的 prompt 总量(锚点)。
    last_anchor: u64,
    /// 锚点建立后,新 fold 出的 message 启发式增量(挂载在锚点之后)。
    post_anchor_message: u64,
    /// 是否已经有 provider 锚点。
    has_anchor: bool,
}

impl ContextMeter {
    pub fn new() -> Self {
        Self {
            system_tokens: 0,
            tools_tokens: 0,
            message_tokens: 0,
            turn_usage: TurnTokenUsage::default(),
            last_anchor: 0,
            post_anchor_message: 0,
            has_anchor: false,
        }
    }

    /// 单事件增量回放。
    pub fn apply_one(&mut self, envelope: &SessionEnvelope) {
        match &envelope.event {
            SessionEvent::SystemPrompt { .. } => {
                // 不参与 fold;由 driver 调 `set_system_tokens` 喂 framed 版本。
            }
            SessionEvent::UserMessage { text, .. } => {
                let message = ChatMessage::user(text);
                self.fold_message_delta(estimate_message(&message));
            }
            SessionEvent::AssistantMessage { blocks, .. } => {
                if let Some(message) = extract_assistant_message(blocks) {
                    self.fold_message_delta(estimate_message(&message));
                }
            }
            SessionEvent::ToolResult { content, .. } => {
                let message = ChatMessage::tool_result("__unused__", content);
                self.fold_message_delta(estimate_message(&message));
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
    /// 是不是 fold 成功。
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

            // 锚点:用最后一次 AssistantMessage.usage 的 input + cache 作 anchor。
            if let Some(last_usage) = events.iter().rev().find_map(|e| match &e.event {
                SessionEvent::AssistantMessage { usage: Some(u), .. } => Some(*u),
                _ => None,
            }) {
                self.last_anchor = prompt_tokens(&last_usage);
                self.has_anchor = true;
                // 锚点之后的新消息会继续 fold_message_delta,挂到 post_anchor_message。
            }
            true
        } else {
            false
        }
    }

    fn fold_message_delta(&mut self, tokens: u64) {
        if self.has_anchor {
            // 锚点已建立,新增消息只挂在锚点之后,前面的 message_tokens 失效。
            self.post_anchor_message = self.post_anchor_message.saturating_add(tokens);
        } else {
            self.message_tokens = self.message_tokens.saturating_add(tokens);
        }
    }

    /// 读当前快照(纯启发式拆分,不含锚点)。
    pub fn breakdown(&self) -> ContextBreakdown {
        ContextBreakdown {
            system_tokens: self.system_tokens,
            tools_tokens: self.tools_tokens,
            message_tokens: self.message_tokens,
        }
    }

    /// 读当前精确 usage 累计。
    pub fn turn_usage(&self) -> TurnTokenUsage {
        self.turn_usage.clone()
    }

    /// 读当前压力值(锚点 + 启发式)。
    pub fn context_pressure(&self) -> ContextPressure {
        if self.has_anchor {
            ContextPressure {
                pressure_tokens: self
                    .last_anchor
                    .saturating_add(self.post_anchor_message),
                anchored: true,
                anchor_tokens: self.last_anchor,
            }
        } else {
            ContextPressure {
                pressure_tokens: self
                    .system_tokens
                    .saturating_add(self.tools_tokens)
                    .saturating_add(self.message_tokens),
                anchored: false,
                anchor_tokens: 0,
            }
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
                },
            ),
            envelope(4, SessionEvent::StepEnd { turn: 1, step: 1 }),
            envelope(5, SessionEvent::TurnEnd { turn: 1, reason: denia_core::session::TurnEndReason::Completed }),
        ];
        assert!(derive_turn_token_usage(&events).is_none());
    }

    #[test]
    fn pressure_anchor_uses_last_usage() {
        let mut meter = ContextMeter::new();
        // 模拟 turn 1 闭合。
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
                },
            ),
            envelope(4, SessionEvent::StepEnd { turn: 1, step: 1 }),
            envelope(5, SessionEvent::TurnEnd { turn: 1, reason: denia_core::session::TurnEndReason::Completed }),
        ];
        meter.fold_turn(&events);
        let p = meter.context_pressure();
        assert!(p.anchored);
        assert_eq!(p.anchor_tokens, 120);
        // 锚点之后多发一条 user 消息。
        meter.apply_one(&envelope(
            6,
            SessionEvent::UserMessage {
                text: "再来".into(),
                injected: false,
                images: Vec::new(),
            },
        ));
        let p2 = meter.context_pressure();
        assert!(p2.anchored);
        assert!(p2.pressure_tokens > 120);
    }

    #[test]
    fn pressure_without_anchor_is_heuristic() {
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
        assert!(!p.anchored);
        // 100 + 50 + (role 4 + text ceil(2/4) 1) = 155。
        assert_eq!(p.pressure_tokens, 155);
    }
}
