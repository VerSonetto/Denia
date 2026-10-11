//! 用量记账事件的编解码:压缩请求用量、缺 usage 时的保守估算标注。
//!
//! 为什么不新增 `SessionEvent` 变体:用量记账必须落在**日志事件**上(会话
//! 重载后账本要能从日志重建,内存计数不算数),而本次改动边界不含
//! `crates/core/src/session.rs`。于是复用既有形态里唯一"仅日志、不进模型面、
//! 带 code + message 轨迹槽"的 `RetryAttempt`:
//! `attempt: 0` 把它与真实重试轨迹(attempt ≥ 1)区分开,`code` 标明记账
//! 类型,`message` 是机器可读的 JSON 载荷(导出与面板按 code 归类)。
//!
//! 写入方(agent-loop 的 request/compact)与读取方
//! ([`crate::derive_turn_token_usage`])都只经这一层,core 侧将来有专用事件
//! 时这里是唯一替换点。

use denia_core::session::SessionEvent;
use denia_core::stream::TokenUsage;

/// 压缩摘要请求的用量记账 code。
pub const COMPACTION_USAGE_CODE: &str = "COMPACTION_USAGE";
/// 缺 provider usage 时的保守估算标注 code。
pub const USAGE_ESTIMATED_CODE: &str = "USAGE_ESTIMATED";
/// 记账标注统一使用的 attempt 值(0 = 不是重试轨迹)。
pub const ACCOUNTING_ATTEMPT: u32 = 0;

/// 一次"不产生 step 终稿"的请求用量(压缩摘要),或一次保守估算。
///
/// 计费口径与 [`crate::TurnTokenUsage`] 完全一致(五分量之和),因此能直接
/// 并进 turn 账本,不会出现两套口径相减。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountedUsage {
    pub uncached_input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    /// 是否来自保守估算(provider 没报 usage)。
    #[serde(default)]
    pub estimated: bool,
}

impl AccountedUsage {
    /// 计费口径的全量 token(与 `TurnTokenUsage::total` 同口径)。
    pub fn total(&self) -> u64 {
        self.uncached_input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
            .saturating_add(self.reasoning_tokens)
    }

    /// 精确用量(provider 上报)。
    pub fn from_usage(usage: &TokenUsage) -> Self {
        Self {
            uncached_input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_tokens.unwrap_or(0),
            cache_write_tokens: 0,
            reasoning_tokens: usage.reasoning_tokens.unwrap_or(0),
            estimated: false,
        }
    }

    /// 保守估算(provider 没报 usage 的那一步)。
    pub fn estimated_only(uncached_input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            uncached_input_tokens,
            output_tokens,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            estimated: true,
        }
    }

    /// 编成一条只记用量、不进模型面的事件。
    ///
    /// `estimated` 决定 code(而非只靠载荷字段):UI/导出的分组读 code。
    pub fn event(&self, turn: u32, step: u32) -> SessionEvent {
        let code = if self.estimated {
            USAGE_ESTIMATED_CODE
        } else {
            COMPACTION_USAGE_CODE
        };
        SessionEvent::RetryAttempt {
            turn,
            step,
            attempt: ACCOUNTING_ATTEMPT,
            code: code.to_string(),
            message: serde_json::to_string(self).unwrap_or_default(),
            delay_ms: 0,
        }
    }

    /// 从事件还原;不是记账标注(真实重试轨迹、别的 code、坏载荷)返回 None。
    pub fn parse(event: &SessionEvent) -> Option<Self> {
        let SessionEvent::RetryAttempt {
            attempt,
            code,
            message,
            ..
        } = event
        else {
            return None;
        };
        if *attempt != ACCOUNTING_ATTEMPT {
            return None;
        }
        if code != COMPACTION_USAGE_CODE && code != USAGE_ESTIMATED_CODE {
            return None;
        }
        let mut usage: Self = serde_json::from_str(message).ok()?;
        // code 是权威标注:载荷里的 estimated 只做自描述,不覆盖 code。
        usage.estimated = code == USAGE_ESTIMATED_CODE;
        Some(usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact() -> AccountedUsage {
        AccountedUsage {
            uncached_input_tokens: 120,
            output_tokens: 30,
            cache_read_tokens: 40,
            cache_write_tokens: 0,
            reasoning_tokens: 5,
            estimated: false,
        }
    }

    #[test]
    fn compaction_usage_round_trips_through_the_event() {
        let usage = exact();
        let event = usage.event(2, 3);
        assert_eq!(AccountedUsage::parse(&event), Some(usage));
        assert_eq!(usage.total(), 195);
    }

    #[test]
    fn estimated_usage_round_trips_and_keeps_its_marker() {
        let usage = AccountedUsage::estimated_only(1_000, 200);
        let event = usage.event(1, 4);
        let SessionEvent::RetryAttempt { code, attempt, .. } = &event else {
            panic!("记账标注必须是日志事件");
        };
        assert_eq!(code, USAGE_ESTIMATED_CODE);
        assert_eq!(*attempt, ACCOUNTING_ATTEMPT);
        let parsed = AccountedUsage::parse(&event).expect("估算标注可还原");
        assert!(parsed.estimated);
        assert_eq!(parsed.total(), 1_200);
    }

    #[test]
    fn real_retry_traces_are_not_accounting() {
        let retry = SessionEvent::RetryAttempt {
            turn: 1,
            step: 1,
            attempt: 1,
            code: COMPACTION_USAGE_CODE.into(),
            message: serde_json::to_string(&exact()).unwrap(),
            delay_ms: 250,
        };
        assert_eq!(AccountedUsage::parse(&retry), None);
        let other_code = SessionEvent::RetryAttempt {
            turn: 1,
            step: 1,
            attempt: ACCOUNTING_ATTEMPT,
            code: "MAX_TOKENS_CONTINUATION".into(),
            message: "x".into(),
            delay_ms: 0,
        };
        assert_eq!(AccountedUsage::parse(&other_code), None);
    }

    #[test]
    fn token_usage_conversion_uses_the_billing_buckets() {
        let usage = TokenUsage {
            input_tokens: 10,
            output_tokens: 2,
            cache_read_tokens: Some(3),
            reasoning_tokens: Some(1),
        };
        let accounted = AccountedUsage::from_usage(&usage);
        assert!(!accounted.estimated);
        assert_eq!(accounted.total(), 16);
    }
}
