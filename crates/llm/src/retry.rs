//! Retry policy and executor for streaming attempts.

use std::future::Future;
use std::time::Duration;

use denia_core::error::{LlmError, codes};
use rand::Rng;

/// 一次重试尝试的轨迹(对齐 dsh llm-retry 的事件化重试:每次退避重试都
/// 可观测)。供会话日志落盘,不进入模型历史。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RetryAttempt {
    /// 第几次尝试失败(1 起;到达 `max_retries` 后不再有后续尝试)。
    pub attempt: u32,
    /// 失败的错误码(canonical codes)。
    pub code: String,
    /// 失败消息。
    pub message: String,
    /// 本次失败后的退避延迟(毫秒;下一次尝试前等待)。
    pub delay_ms: u64,
}

/// Per-route retry behavior. Setup failures with retryable codes re-run the
/// attempt; mid-stream chunk errors never retry here.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub initial_delay_ms: u64,
    pub max_delay_ms: u64,
    pub jitter_ratio: f64,
    pub retryable_codes: Vec<&'static str>,
    /// 连接类错误(网络层:连接建立失败/超时)独立退避曲线,学 codex
    /// `responses_retry.rs`:连接抖动用更长的指数退避(5s→60s),与业务错误
    /// (4xx/5xx)的短退避分离,且不烧普通重试预算。
    pub connection_codes: Vec<&'static str>,
    pub connection_initial_delay_ms: u64,
    pub connection_max_delay_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            // 对齐 dsh llm-retry 默认值:首次请求之后的额外尝试上限 5。
            max_retries: 5,
            initial_delay_ms: 500,
            max_delay_ms: 10_000,
            jitter_ratio: 0.1,
            // dsh 默认可重试集:EMPTY_RESPONSE/RATE_LIMIT/SERVER/TIMEOUT/TRANSPORT。
            // denia 优化(实测依据):INVALID_REQUEST 与 STREAM_CLOSED 被 cat 等
            // 网关间歇性抛出(404 openai_error、[DONE] 前断流),同一步此前
            // 成功过即证明形态合法,重试收益明确,故加入。
            retryable_codes: vec![
                codes::EMPTY_RESPONSE,
                codes::RATE_LIMIT,
                codes::SERVER,
                codes::TIMEOUT,
                codes::TRANSPORT,
                codes::INVALID_REQUEST,
                codes::STREAM_CLOSED,
            ],
            // 连接类错误独立退避(对齐 codex INITIAL_CONNECTION_RETRY_DELAY=5s、
            // MAX_CONNECTION_RETRY_DELAY=60s)。
            connection_codes: vec![codes::TRANSPORT, codes::TIMEOUT],
            connection_initial_delay_ms: 5_000,
            connection_max_delay_ms: 60_000,
        }
    }
}

impl RetryPolicy {
    /// 该错误码是否走退避重试(agent-loop 的 finish/流错误重试环复用)。
    pub fn is_retryable(&self, code: &str) -> bool {
        self.retryable_codes.iter().any(|c| *c == code)
    }

    /// 该错误码是否为连接类错误(网络层抖动,独立退避曲线)。
    pub fn is_connection_error(&self, code: &str) -> bool {
        self.connection_codes.iter().any(|c| *c == code)
    }

    /// dsh 对齐退避:`min(initial · 2^min(retry-1, 1024), max) · jitter` 再夹 max,
    /// jitter 为 `[1-ratio, 1+ratio]` 对称均匀;`providerRetryAfterMs` 有效时
    /// 优先直接采用(≤ max 时),超过 max 视为"provider 要求等太久"返回 None,
    /// 调用方按 dsh 语义放弃重试。
    pub fn delay_ms(&self, attempt: u32, provider_hint_ms: Option<u64>) -> Option<u64> {
        self.delay_ms_for(attempt, provider_hint_ms, false)
    }

    /// 带错误类别的退避:连接类错误走独立(更长)曲线,学 codex。
    pub fn delay_ms_for(
        &self,
        attempt: u32,
        provider_hint_ms: Option<u64>,
        connection: bool,
    ) -> Option<u64> {
        let (initial, max) = if connection {
            (self.connection_initial_delay_ms, self.connection_max_delay_ms)
        } else {
            (self.initial_delay_ms, self.max_delay_ms)
        };
        if let Some(hint) = provider_hint_ms {
            if hint > max {
                return None;
            }
            return Some(hint);
        }
        let shift = (attempt.saturating_sub(1)).min(1024);
        let exponential = (initial as u128)
            .saturating_mul(1u128 << shift)
            .min(max as u128) as u64;
        let jitter = 1.0 - self.jitter_ratio
            + 2.0 * self.jitter_ratio * rand::thread_rng().gen_range(0.0..1.0);
        Some(((exponential as f64) * jitter).min(max as f64) as u64)
    }
}

/// Runs `make` until it succeeds, exhausts `policy.max_retries`, or fails
/// with a non-retryable code. Every failed attempt is reported through
/// `on_retry` before the backoff sleep (attempts are observable, 对齐 dsh
/// llm-retry 的事件化重试)。
pub async fn with_retry<F, Fut, T>(
    policy: &RetryPolicy,
    mut on_retry: impl FnMut(&RetryAttempt),
    mut make: F,
) -> Result<T, LlmError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, LlmError>>,
{
    let mut attempt: u32 = 0;
    loop {
        match make().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                attempt += 1;
                if attempt > policy.max_retries || !policy.is_retryable(&error.code) {
                    return Err(error);
                }
                let connection = policy.is_connection_error(&error.code);
                let delay =
                    policy.delay_ms_for(attempt, error.failure.provider_retry_after_ms, connection);
                let Some(delay) = delay else {
                    // dsh 语义:provider 要求的等待超过本地上限 → 放弃重试。
                    tracing::warn!(
                        attempt,
                        code = %error.code,
                        "provider retry-after exceeds max delay; giving up retry"
                    );
                    return Err(error);
                };
                on_retry(&RetryAttempt {
                    attempt,
                    code: error.code.clone(),
                    message: error.message.clone(),
                    delay_ms: delay,
                });
                tracing::warn!(
                    attempt,
                    delay_ms = delay,
                    code = %error.code,
                    connection,
                    "retrying model request"
                );
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
        }
    }
}
