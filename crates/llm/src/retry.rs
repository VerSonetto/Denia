//! Retry policy and executor for streaming attempts.

use std::future::Future;
use std::time::Duration;

use denia_core::error::{LlmError, codes};
use rand::Rng;

/// Per-route retry behavior. Setup failures with retryable codes re-run the
/// attempt; mid-stream chunk errors never retry here.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub initial_delay_ms: u64,
    pub max_delay_ms: u64,
    pub jitter_ratio: f64,
    pub retryable_codes: Vec<&'static str>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay_ms: 500,
            max_delay_ms: 10_000,
            jitter_ratio: 0.1,
            retryable_codes: vec![
                codes::EMPTY_RESPONSE,
                codes::RATE_LIMIT,
                codes::SERVER,
                codes::TIMEOUT,
                codes::TRANSPORT,
            ],
        }
    }
}

impl RetryPolicy {
    fn is_retryable(&self, code: &str) -> bool {
        self.retryable_codes.iter().any(|c| *c == code)
    }

    /// Bounded exponential backoff with symmetric jitter, honoring a
    /// provider `Retry-After` hint under the cap.
    fn delay_ms(&self, attempt: u32, provider_hint_ms: Option<u64>) -> u64 {
        if let Some(hint) = provider_hint_ms {
            if hint <= self.max_delay_ms {
                return hint;
            }
        }
        let shift = (attempt.saturating_sub(1)).min(10);
        let base = self.initial_delay_ms.saturating_mul(1u64 << shift);
        let capped = base.min(self.max_delay_ms);
        let jitter = capped as f64 * self.jitter_ratio * (rand::thread_rng().gen_range(-1.0..1.0));
        ((capped as f64) + jitter).max(0.0) as u64
    }
}

/// Runs `make` until it succeeds, exhausts `policy.max_retries`, or fails
/// with a non-retryable code.
pub async fn with_retry<F, Fut, T>(policy: &RetryPolicy, mut make: F) -> Result<T, LlmError>
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
                let delay = policy.delay_ms(attempt, error.failure.provider_retry_after_ms);
                tracing::warn!(
                    attempt,
                    delay_ms = delay,
                    code = %error.code,
                    "retrying model request"
                );
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
        }
    }
}
