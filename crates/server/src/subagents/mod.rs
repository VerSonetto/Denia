//! 子代理子系统：定义仓库、派遣解析、统一授权与旧配置迁移。
//!
//! 所有权边界（与执行计划 §4 一致）：
//!
//! - [`profiles::SubagentProfileStore`]：定义文件与 revision 的唯一所有者；
//!   UI 只持草稿，不能直接落盘。
//! - [`policy`]：纯计算——把父代理可授予集合、定义请求与硬禁项合成一份
//!   `EffectiveToolGrant`，schema／目录／执行器消费同一结果。
//! - `agent_runtime::Runtime`：准入槽位、父子身份、生命周期；不重复定义
//!   工具授权口径。
//!
//! 运行事实（快照、事件、终态）归会话日志所有，绝不写回定义文件。

pub mod migration;
pub mod policy;
pub mod profiles;
pub mod resolver;

pub use policy::{EffectiveToolGrant, GrantRequest, grant_for};
pub use profiles::{ProfileRow, SubagentError, SubagentProfileStore};
pub use resolver::{DispatchDefinition, policy_grant, resolve_definition};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 新调度设置的命名空间：唯一持有并发上限，不与旧 `runtime.maxAgents` 双写。
pub const SETTINGS_NS: &str = "subagent-policy";

/// 子代理调度设置。
///
/// 只有并发限制，**没有**深度配置——禁止嵌套是运行时硬规则，不是可调参数。
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct SubagentPolicyConfig {
    /// 当前服务进程全部活动子代理（含等待审批/输入）的上限。
    pub max_concurrent_runs: usize,
}

impl Default for SubagentPolicyConfig {
    fn default() -> Self {
        Self {
            max_concurrent_runs: 8,
        }
    }
}

pub fn validate_subagent_policy(value: Value) -> Result<Value, String> {
    let config: SubagentPolicyConfig =
        serde_json::from_value(value).map_err(|e| format!("子代理调度配置无效：{e}"))?;
    if !(1..=64).contains(&config.max_concurrent_runs) {
        return Err("subagent-policy.maxConcurrentRuns 必须在 1–64 之间".into());
    }
    serde_json::to_value(config).map_err(|e| e.to_string())
}

/// 从会话 cwd 推出项目根（向上找 `.git`，与工作区指令/技能同一套发现规则）。
///
/// 项目级定义必须有明确的根作用域：不同项目里的同名定义不是同一个身份，
/// 因此派遣入口从会话推导，绝不接受模型或 API 调用方指定路径。
pub fn project_root_of(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(std::path::Path::to_path_buf)
}

/// 把冻结快照写进会话目录（同目录临时文件 + 原子替换）。
pub fn write_snapshot_file(
    dir: &std::path::Path,
    file: &denia_core::subagent::SubagentSnapshotFile,
) -> Result<(), String> {
    use denia_core::subagent::SNAPSHOT_FILE;
    let bytes = serde_json::to_vec_pretty(file).map_err(|e| format!("快照序列化失败：{e}"))?;
    let target = dir.join(SNAPSHOT_FILE);
    let temp = dir.join(format!("{SNAPSHOT_FILE}.tmp"));
    std::fs::write(&temp, &bytes).map_err(|e| format!("写入子代理快照失败：{e}"))?;
    if let Err(error) = std::fs::rename(&temp, &target) {
        let _ = std::fs::remove_file(&temp);
        return Err(format!("提交子代理快照失败：{error}"));
    }
    Ok(())
}
