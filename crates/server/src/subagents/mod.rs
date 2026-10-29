//! 子代理定义管理与派遣解析。
//!
//! 模块划分（所有权见执行计划第 4 节）：
//! - [`profiles`] —— 定义文件、覆盖层、revision、诊断的唯一所有者；
//! - [`policy`] —— 调度设置命名空间与统一准入原语；
//! - [`resolver`] —— 纯解析/校验：定义 + 父能力 → 不可变派遣快照；
//! - [`migration`] —— `settings.yaml` 旧字段一次性迁移。

pub mod migration;
pub mod policy;
pub mod profiles;
pub mod resolver;

pub use policy::{SETTINGS_NS, SubagentPolicyConfig, validate_policy};
pub use profiles::{Catalog, ProfileError, ProfileStore, ResolvedProfile};

use std::path::Path;

/// 由会话工作目录推导项目根（与 `ProfileStore` 同一规则）。
pub fn project_root_for(cwd: &Path) -> std::path::PathBuf {
    ProfileStore::project_root(cwd)
}
