//! `settings.yaml` 的一次性迁移：把旧的子代理配置迁到新调度命名空间。
//!
//! 必须在 [`denia_settings::SettingsStore::open`] 与命名空间注册**之前**运行：
//! `RuntimeConfig` 是 `deny_unknown_fields` 的，旧 `maxAgents` / `maxDepth`
//! 留在文件里会让启动直接失败。
//!
//! 迁移规则（与执行计划 §12.2 一致）：
//!
//! 1. 保留所有无关字段，写带时间戳的备份；
//! 2. `subagent-policy.maxConcurrentRuns` 未设置时从合法的旧 `runtime.maxAgents`
//!    迁移；新值已存在则新值优先；
//! 3. 移除旧 `maxAgents` / `maxDepth`；`maxDepth` **不**映射成任何开关
//!    （深度配置是被删除的产品能力，不是被改名的设置）；
//! 4. 旧值非法时保留原文件与诊断，不静默回默认；
//! 5. 原子提交，重复启动幂等。

use std::path::{Path, PathBuf};

use serde_yaml::{Mapping, Value};

/// 迁移标记所在的位置（未注册的顶层小节，只用于幂等与审计）。
const MARKER_SECTION: &str = "migrations";
const MARKER_KEY: &str = "subagentPolicy";
const MARKER_VERSION: i64 = 1;

/// 迁移结果：启动日志与设置接口的"已废弃诊断"都读它。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MigrationOutcome {
    /// 文件是否被改写。
    pub changed: bool,
    /// 从 `runtime.maxAgents` 迁移过来的并发上限（仅在实际迁移时有值）。
    pub migrated_max_agents: Option<usize>,
    /// 被移除的旧字段名。
    pub removed: Vec<String>,
    /// 无法自动处理时的说明（此时 `changed` 为 false，启动会带着它失败）。
    pub diagnostic: Option<String>,
    pub backup: Option<PathBuf>,
}

impl MigrationOutcome {
    pub fn is_noop(&self) -> bool {
        !self.changed && self.diagnostic.is_none()
    }

    /// 已废弃字段的对外诊断文本；无迁移历史时为 `None`。
    pub fn deprecated_note(&self) -> Option<String> {
        if self.removed.is_empty() {
            return None;
        }
        Some(format!(
            "旧字段 {} 已从运行时配置移除；子代理并发上限改由 subagent-policy.maxConcurrentRuns 承担，派遣深度不再是可配置项。",
            self.removed.join("、")
        ))
    }
}

/// 执行迁移；`path` 是 `settings.yaml`。
pub fn migrate_settings_file(path: &Path) -> Result<MigrationOutcome, String> {
    if !path.exists() {
        return Ok(MigrationOutcome::default());
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("读取设置文件失败：{e}"))?;
    if text.trim().is_empty() {
        return Ok(MigrationOutcome::default());
    }
    let mut root: Value =
        serde_yaml::from_str(&text).map_err(|e| format!("设置文件不是合法 YAML：{e}"))?;
    let Value::Mapping(root_map) = &mut root else {
        return Err("设置文件根节点必须是映射".to_string());
    };
    if already_migrated(root_map) {
        return Ok(MigrationOutcome {
            changed: false,
            ..Default::default()
        });
    }
    let Some(runtime_key) = find_key(root_map, "runtime") else {
        return Ok(MigrationOutcome::default());
    };
    let Some(Value::Mapping(runtime)) = root_map.get_mut(&runtime_key) else {
        return Ok(MigrationOutcome::default());
    };
    let max_agents_key = find_key(runtime, "maxAgents");
    let max_depth_key = find_key(runtime, "maxDepth");
    if max_agents_key.is_none() && max_depth_key.is_none() {
        return Ok(MigrationOutcome::default());
    }

    // 旧并发值先校验：非法时保留原文件，让用户修完再启动。
    let legacy_max_agents = match max_agents_key.as_ref().and_then(|key| runtime.get(key)) {
        Some(Value::Number(number)) => match number.as_u64() {
            Some(value) if (1..=64).contains(&value) => Some(value as usize),
            _ => {
                return Ok(MigrationOutcome {
                    diagnostic: Some(
                        "runtime.maxAgents 不是 1–64 的整数：无法自动迁移，请手工改为 subagent-policy.maxConcurrentRuns 后删除旧字段。"
                            .to_string(),
                    ),
                    ..Default::default()
                });
            }
        },
        Some(_) => {
            return Ok(MigrationOutcome {
                diagnostic: Some(
                    "runtime.maxAgents 不是数字：无法自动迁移，请手工改为 subagent-policy.maxConcurrentRuns 后删除旧字段。"
                        .to_string(),
                ),
                ..Default::default()
            });
        }
        None => None,
    };

    let backup = write_backup(path, &text)?;

    let mut removed: Vec<String> = Vec::new();
    for (key, name) in [
        (max_agents_key.as_ref(), "maxAgents"),
        (max_depth_key.as_ref(), "maxDepth"),
    ] {
        if let Some(key) = key {
            runtime.remove(key);
            removed.push(format!("runtime.{name}"));
        }
    }
    if runtime.is_empty()
        && let Some(key) = find_key(root_map, "runtime")
    {
        root_map.remove(&key);
    }

    let mut migrated_max_agents = None;
    if let Some(value) = legacy_max_agents
        && ensure_policy_concurrency(root_map, value)
    {
        migrated_max_agents = Some(value);
    }
    set_marker(root_map);

    let rendered = serde_yaml::to_string(&root).map_err(|e| format!("序列化设置失败：{e}"))?;
    atomic_write(path, rendered.as_bytes())?;
    Ok(MigrationOutcome {
        changed: true,
        migrated_max_agents,
        removed,
        diagnostic: None,
        backup: Some(backup),
    })
}

/// 新值已存在时不覆盖（"新值优先"）。
fn ensure_policy_concurrency(root: &mut Mapping, value: usize) -> bool {
    let key = Value::String(super::SETTINGS_NS.to_string());
    match root.get_mut(&key) {
        Some(Value::Mapping(policy)) => {
            let existing = find_key(policy, "maxConcurrentRuns");
            match existing {
                Some(existing) => match policy.get(&existing) {
                    // 已有显式值：保留它，不写迁移值。
                    Some(Value::Number(_)) => false,
                    _ => {
                        policy.insert(existing, Value::Number(value.into()));
                        true
                    }
                },
                None => {
                    policy.insert(
                        Value::String("maxConcurrentRuns".to_string()),
                        Value::Number(value.into()),
                    );
                    true
                }
            }
        }
        _ => {
            let mut policy = Mapping::new();
            policy.insert(
                Value::String("maxConcurrentRuns".to_string()),
                Value::Number(value.into()),
            );
            root.insert(key, Value::Mapping(policy));
            true
        }
    }
}

fn set_marker(root: &mut Mapping) {
    let key = Value::String(MARKER_SECTION.to_string());
    let marker_key = Value::String(MARKER_KEY.to_string());
    match root.get_mut(&key) {
        Some(Value::Mapping(section)) => {
            section.insert(marker_key, Value::Number(MARKER_VERSION.into()));
        }
        _ => {
            let mut section = Mapping::new();
            section.insert(marker_key, Value::Number(MARKER_VERSION.into()));
            root.insert(key, Value::Mapping(section));
        }
    }
}

fn already_migrated(root: &Mapping) -> bool {
    root.get(Value::String(MARKER_SECTION.to_string()))
        .and_then(|section| section.get(Value::String(MARKER_KEY.to_string())))
        .and_then(Value::as_i64)
        .is_some_and(|version| version >= MARKER_VERSION)
}

fn find_key(map: &Mapping, name: &str) -> Option<Value> {
    map.keys().find(|key| key.as_str() == Some(name)).cloned()
}

fn write_backup(path: &Path, text: &str) -> Result<PathBuf, String> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let backup = path.with_extension(format!("yaml.bak-{stamp}"));
    std::fs::write(&backup, text).map_err(|e| format!("写设置备份失败：{e}"))?;
    Ok(backup)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temp = path.with_extension("yaml.migrating");
    std::fs::write(&temp, bytes).map_err(|e| format!("写迁移后设置失败：{e}"))?;
    std::fs::rename(&temp, path).map_err(|e| format!("提交迁移后设置失败：{e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("denia-migrate-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn migrates_legacy_agents_to_policy_and_keeps_unrelated_fields() {
        let dir = temp_dir("basic");
        let path = dir.join("settings.yaml");
        write(
            &path,
            "runtime:\n  maxAgents: 12\n  maxDepth: 3\n  maxJobs: 6\nconsole:\n  locale: zh\n",
        );
        let outcome = migrate_settings_file(&path).unwrap();
        assert!(outcome.changed);
        assert_eq!(outcome.migrated_max_agents, Some(12));
        assert_eq!(
            outcome.removed,
            vec!["runtime.maxAgents".to_string(), "runtime.maxDepth".to_string()]
        );
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains("maxAgents"), "{after}");
        assert!(!after.contains("maxDepth"), "{after}");
        assert!(after.contains("maxJobs"), "{after}");
        assert!(after.contains("locale"), "{after}");
        assert!(after.contains("maxConcurrentRuns"), "{after}");
        assert!(outcome.backup.as_ref().is_some_and(|p| p.exists()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn second_run_is_idempotent() {
        let dir = temp_dir("idempotent");
        let path = dir.join("settings.yaml");
        write(&path, "runtime:\n  maxAgents: 4\n");
        assert!(migrate_settings_file(&path).unwrap().changed);
        let second = migrate_settings_file(&path).unwrap();
        assert!(!second.changed);
        assert!(second.is_noop());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn existing_new_value_wins() {
        let dir = temp_dir("new-wins");
        let path = dir.join("settings.yaml");
        write(
            &path,
            "runtime:\n  maxAgents: 4\nsubagent-policy:\n  maxConcurrentRuns: 20\n",
        );
        let outcome = migrate_settings_file(&path).unwrap();
        assert!(outcome.changed);
        assert_eq!(outcome.migrated_max_agents, None);
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("20"), "{after}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_legacy_value_keeps_file_and_reports() {
        let dir = temp_dir("invalid");
        let path = dir.join("settings.yaml");
        let original = "runtime:\n  maxAgents: 999\n";
        write(&path, original);
        let outcome = migrate_settings_file(&path).unwrap();
        assert!(!outcome.changed);
        assert!(outcome.diagnostic.is_some());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn file_without_legacy_fields_is_untouched() {
        let dir = temp_dir("clean");
        let path = dir.join("settings.yaml");
        let original = "runtime:\n  maxJobs: 6\n";
        write(&path, original);
        let outcome = migrate_settings_file(&path).unwrap();
        assert!(outcome.is_noop());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
