//! 子代理调度策略：唯一持有并发上限的命名空间与统一准入原语。
//!
//! 旧实现把"全局并发"藏在 `runtime.maxAgents` 里，与 jobs、消息预算混在
//! 同一命名空间。本计划把并发上限迁到 `subagent-policy.maxConcurrentRuns`，
//! 旧字段一次性迁移后彻底退出正式 schema（见 [`super::migration`]）。
//!
//! 深度配置**不存在**：子代理禁止派遣子代理是运行时硬规则，不是可调参数。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Mutex;

/// 新调度设置的命名空间名。
pub const SETTINGS_NS: &str = "subagent-policy";

/// 当前服务进程全部活动子代理（含等待审批/输入）的上限。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct SubagentPolicyConfig {
    pub max_concurrent_runs: usize,
}

impl Default for SubagentPolicyConfig {
    fn default() -> Self {
        Self {
            max_concurrent_runs: 8,
        }
    }
}

pub fn validate_policy(value: Value) -> Result<Value, String> {
    let config: SubagentPolicyConfig =
        serde_json::from_value(value).map_err(|error| format!("子代理调度配置无效:{error}"))?;
    if !(1..=64).contains(&config.max_concurrent_runs) {
        return Err("maxConcurrentRuns 必须在 1–64 之间".to_string());
    }
    serde_json::to_value(config).map_err(|error| error.to_string())
}

/// 统一准入：创建、终态 child 恢复、消息唤醒共用同一份槽位账。
///
/// - `gate` 串行化"检查 + 预留"这段临界区（`tokio::sync::Mutex`，可跨 await）；
/// - `reserved` 记录当前占槽的会话 id（std 锁，只在同步段内使用）。
///
/// 释放必须幂等：取消/失败/启动回滚只释放一次，多释放一次不会挤掉别人的槽。
pub struct Admission {
    gate: tokio::sync::Mutex<()>,
    reserved: Mutex<HashSet<String>>,
}

impl Default for Admission {
    fn default() -> Self {
        Self::new()
    }
}

impl Admission {
    pub fn new() -> Self {
        Self {
            gate: tokio::sync::Mutex::new(()),
            reserved: Mutex::new(HashSet::new()),
        }
    }

    /// 进入准入临界区。调用方在临界区内完成检查与预留，再 drop guard。
    pub async fn enter(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.gate.lock().await
    }

    pub fn reserved_count(&self) -> usize {
        self.reserved
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }

    pub fn is_reserved(&self, id: &str) -> bool {
        self.reserved
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(id)
    }

    /// 预留一个槽位。已在槽内的 id 视为成功（幂等），不重复计数。
    pub fn reserve(&self, id: &str, limit: usize) -> bool {
        let mut reserved = self.reserved.lock().unwrap_or_else(|p| p.into_inner());
        if reserved.contains(id) {
            return true;
        }
        if reserved.len() >= limit {
            return false;
        }
        reserved.insert(id.to_string());
        true
    }

    /// 释放槽位；重复释放是无害的。
    pub fn release(&self, id: &str) {
        self.reserved
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn policy_defaults_and_validates_range() {
        assert_eq!(SubagentPolicyConfig::default().max_concurrent_runs, 8);
        assert!(validate_policy(json!({"maxConcurrentRuns": 1})).is_ok());
        assert!(validate_policy(json!({"maxConcurrentRuns": 64})).is_ok());
        assert!(validate_policy(json!({"maxConcurrentRuns": 0})).is_err());
        assert!(validate_policy(json!({"maxConcurrentRuns": 65})).is_err());
        // 深度配置不是本命名空间的字段：出现即失败，而不是被静默忽略。
        assert!(validate_policy(json!({"maxConcurrentRuns": 8, "maxDepth": 3})).is_err());
    }

    #[tokio::test]
    async fn admission_reserves_releases_and_is_idempotent() {
        let admission = Admission::new();
        let guard = admission.enter().await;
        assert!(admission.reserve("a", 2));
        assert!(admission.reserve("a", 2), "重复预留同一个 id 必须幂等");
        assert!(admission.reserve("b", 2));
        assert!(!admission.reserve("c", 2), "超过上限必须立即拒绝");
        assert_eq!(admission.reserved_count(), 2);
        drop(guard);
        admission.release("a");
        assert_eq!(admission.reserved_count(), 1);
        // 重复释放无害。
        admission.release("a");
        assert_eq!(admission.reserved_count(), 1);
        assert!(admission.reserve("c", 2));
        // 下调上限不杀已有任务：只阻止新增。
        assert!(!admission.reserve("d", 2));
    }
}
