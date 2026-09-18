//! 防爆破:按来源 IP 的失败计数 + 指数退避 + 滑动窗口限流。
//!
//! 两道闸互补:
//! - **指数退避**治"快速猛攻":连续失败到阈值后,每一次失败都把下次允许的
//!   时间点往后推一倍(1s → 2s → … 封顶 5min);
//! - **滑动窗口**治"低速长跑":每 IP 每窗口内尝试次数有硬上限,不给攻击者
//!   用"每次慢一点点"绕开退避的机会。
//!
//! 任一成功即清零 —— 用户自己输对了 PIN 不该继续背着之前打错的惩罚。

use std::collections::HashMap;
use std::sync::Mutex;

use super::config::RateLimitSettings;

/// 一次准入判定的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    Allow,
    /// 被拒,附建议的重试等待秒数(响应里的 `Retry-After`)。
    Deny { retry_after_seconds: u64, reason: DenyReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// 连续失败后的指数退避窗口内。
    Backoff,
    /// 滑动窗口内尝试次数超限。
    Window,
}

impl DenyReason {
    pub fn code(self) -> &'static str {
        match self {
            DenyReason::Backoff => "remote/backoff",
            DenyReason::Window => "remote/rate-limited",
        }
    }
}

#[derive(Debug, Default)]
struct Entry {
    /// 连续失败次数。
    failures: u32,
    /// 退避解禁时刻(epoch ms)。
    blocked_until: u64,
    /// 窗口内尝试时刻(epoch ms,升序)。
    attempts: Vec<u64>,
}

/// 按 IP 的限流表。
pub struct RateLimiter {
    settings: Mutex<RateLimitSettings>,
    inner: Mutex<HashMap<String, Entry>>,
}

impl RateLimiter {
    pub fn new(settings: RateLimitSettings) -> Self {
        Self {
            settings: Mutex::new(settings),
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// 配置热更新(设置写入后调用)。计数与退避状态保留。
    pub fn update_settings(&self, settings: RateLimitSettings) {
        *self.settings.lock().unwrap_or_else(|p| p.into_inner()) = settings;
    }

    /// 尝试准入:检查退避与窗口,**并计入本次尝试**。
    ///
    /// 计入发生在准入之后而不是失败之后:否则攻击者可以用"发请求但不让
    /// 服务端判定失败"的方式绕开窗口计数。
    pub fn admit(&self, peer: &str, now_ms: u64) -> Admission {
        let settings = self.settings.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let entry = map.entry(peer.to_string()).or_default();

        if now_ms < entry.blocked_until {
            return Admission::Deny {
                retry_after_seconds: (entry.blocked_until - now_ms).div_ceil(1000).max(1),
                reason: DenyReason::Backoff,
            };
        }

        let window_ms = settings.window_seconds.saturating_mul(1000);
        entry
            .attempts
            .retain(|at| now_ms.saturating_sub(*at) < window_ms);
        if entry.attempts.len() as u32 >= settings.window_max_attempts {
            let oldest = entry.attempts.first().copied().unwrap_or(now_ms);
            let retry_after = (oldest + window_ms).saturating_sub(now_ms).div_ceil(1000).max(1);
            return Admission::Deny {
                retry_after_seconds: retry_after,
                reason: DenyReason::Window,
            };
        }

        entry.attempts.push(now_ms);
        Admission::Allow
    }

    /// 记一次失败,必要时安排退避。返回当前的连续失败次数。
    pub fn record_failure(&self, peer: &str, now_ms: u64) -> u32 {
        let settings = self.settings.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let entry = map.entry(peer.to_string()).or_default();
        entry.failures = entry.failures.saturating_add(1);
        if entry.failures >= settings.max_failures {
            // 第 max_failures 次失败起退避:延迟 = base * 2^(超出次数),封顶 max。
            let exponent = entry.failures.saturating_sub(settings.max_failures).min(16);
            let delay = settings
                .base_delay_ms
                .saturating_mul(1u64 << exponent)
                .min(settings.max_delay_ms);
            entry.blocked_until = now_ms.saturating_add(delay);
        }
        entry.failures
    }

    /// 记一次成功:清零该 IP 的失败与退避(窗口内的尝试记录保留,
    /// 防止"打对了就重置计数"被当成免费的爆破预算)。
    pub fn record_success(&self, peer: &str) {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(entry) = map.get_mut(peer) {
            entry.failures = 0;
            entry.blocked_until = 0;
        }
    }

    /// 断开/关闭时清空全部状态:不留上一次会话的退避影响下一次。
    pub fn reset(&self) {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }

    /// 当前仍在退避中的 IP 数(状态展示用)。
    pub fn blocked_count(&self, now_ms: u64) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .filter(|entry| now_ms < entry.blocked_until)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000_000;

    fn limiter() -> RateLimiter {
        RateLimiter::new(RateLimitSettings::default())
    }

    #[test]
    fn first_attempts_are_allowed() {
        let limiter = limiter();
        assert_eq!(limiter.admit("10.0.0.1", NOW), Admission::Allow);
        assert_eq!(limiter.admit("10.0.0.1", NOW + 1), Admission::Allow);
    }

    #[test]
    fn backoff_kicks_in_after_max_failures() {
        let limiter = limiter();
        // 默认 maxFailures = 5。
        for i in 0..5 {
            assert_eq!(limiter.admit("10.0.0.1", NOW + i), Admission::Allow);
            limiter.record_failure("10.0.0.1", NOW + i);
        }
        match limiter.admit("10.0.0.1", NOW + 5) {
            Admission::Deny { retry_after_seconds, reason } => {
                assert_eq!(reason, DenyReason::Backoff);
                assert!(retry_after_seconds >= 1, "必须给出重试等待秒数");
            }
            Admission::Allow => panic!("连续失败达阈值后必须进入退避"),
        }
    }

    #[test]
    fn backoff_delay_doubles_and_caps() {
        let limiter = limiter();
        for i in 0..5 {
            limiter.record_failure("10.0.0.1", NOW + i);
        }
        let first = match limiter.admit("10.0.0.1", NOW + 10) {
            Admission::Deny { retry_after_seconds, .. } => retry_after_seconds,
            Admission::Allow => panic!("应处于退避中"),
        };
        assert!(first <= 1, "首次退避是 1 秒,得到 {first}");

        limiter.record_failure("10.0.0.1", NOW + 10);
        let second = match limiter.admit("10.0.0.1", NOW + 11) {
            Admission::Deny { retry_after_seconds, .. } => retry_after_seconds,
            Admission::Allow => panic!("应处于退避中"),
        };
        assert!(second >= 2, "第二次退避应当翻倍,得到 {second}");

        // 一直失败:延迟必须封顶在 maxDelayMs(300s),不能无限增长。
        for i in 0..40 {
            limiter.record_failure("10.0.0.1", NOW + 100 + i);
        }
        let capped = match limiter.admit("10.0.0.1", NOW + 200) {
            Admission::Deny { retry_after_seconds, .. } => retry_after_seconds,
            Admission::Allow => panic!("应处于退避中"),
        };
        assert!(capped <= 300, "退避延迟必须封顶 300 秒,得到 {capped}");
    }

    #[test]
    fn success_clears_failures_and_backoff() {
        let limiter = limiter();
        for i in 0..6 {
            limiter.record_failure("10.0.0.1", NOW + i);
        }
        assert!(matches!(limiter.admit("10.0.0.1", NOW + 10), Admission::Deny { .. }));
        limiter.record_success("10.0.0.1");
        assert_eq!(
            limiter.admit("10.0.0.1", NOW + 11),
            Admission::Allow,
            "成功后必须解除退避"
        );
    }

    #[test]
    fn window_limits_low_and_slow_attempts() {
        let limiter = limiter();
        // 默认窗口 60 秒 20 次;每次成功判定,失败计数始终为 0。
        for i in 0..20 {
            assert_eq!(limiter.admit("10.0.0.2", NOW + i * 100), Admission::Allow);
        }
        match limiter.admit("10.0.0.2", NOW + 2_000) {
            Admission::Deny { reason, retry_after_seconds } => {
                assert_eq!(reason, DenyReason::Window);
                assert!(retry_after_seconds >= 1);
            }
            Admission::Allow => panic!("窗口内超限必须被拒"),
        }
        // 窗口滑过后恢复。
        assert_eq!(limiter.admit("10.0.0.2", NOW + 61_000), Admission::Allow);
    }

    #[test]
    fn peers_are_isolated() {
        let limiter = limiter();
        for i in 0..6 {
            limiter.record_failure("10.0.0.1", NOW + i);
        }
        assert_eq!(
            limiter.admit("10.0.0.9", NOW + 10),
            Admission::Allow,
            "一个 IP 的退避不得影响另一个 IP"
        );
        assert_eq!(limiter.blocked_count(NOW + 10), 1);
    }

    #[test]
    fn reset_clears_everything() {
        let limiter = limiter();
        for i in 0..6 {
            limiter.record_failure("10.0.0.1", NOW + i);
        }
        limiter.reset();
        assert_eq!(limiter.blocked_count(NOW + 10), 0);
        assert_eq!(limiter.admit("10.0.0.1", NOW + 10), Admission::Allow);
    }

    #[test]
    fn updated_settings_take_effect() {
        let limiter = limiter();
        limiter.update_settings(RateLimitSettings {
            max_failures: 2,
            ..RateLimitSettings::default()
        });
        limiter.record_failure("10.0.0.1", NOW);
        limiter.record_failure("10.0.0.1", NOW + 1);
        assert!(matches!(limiter.admit("10.0.0.1", NOW + 2), Admission::Deny { .. }));
    }
}
