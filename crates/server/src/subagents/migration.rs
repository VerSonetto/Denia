//! `settings.yaml` 的一次性迁移：把旧的子代理配置迁到新命名空间。
//!
//! 必须在 `SettingsStore::register`（含严格反序列化）**之前**运行——否则
//! `RuntimeConfig` 的 `deny_unknown_fields` 会让遗留的 `maxAgents`/`maxDepth`
//! 直接阻断启动。
//!
//! 规则：
//! 1. 读原始 YAML，保留所有无关字段；真正要改时先写带时间戳的备份。
//! 2. `subagent-policy.maxConcurrentRuns` 未设置时，从合法的旧 `runtime.maxAgents`
//!    迁移；新值已存在则新值优先。
//! 3. 移除旧 `maxAgents`/`maxDepth` 并写迁移标记；`maxDepth` **不**映射成
//!    任何其他开关（深度配置被删除，不是被改名）。
//! 4. 旧值非法且没有可用的新值时**不**静默回默认：保留原文件并返回诊断。
//! 5. 原子提交；重复启动幂等（无改动就不写盘、不留新备份）。

use std::path::Path;

use serde_json::Value;
use serde_yaml::Value as YamlValue;

/// 迁移标记所在的顶层键（不是 settings 命名空间，SettingsStore 会原样保留）。
pub const MARKER_SECTION: &str = "migrations";

/// 本次迁移的标记键与版本。
pub const MARKER_KEY: &str = "subagentRedesign";
pub const MARKER_VERSION: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationOutcome {
    /// 没有 settings.yaml —— 无需迁移。
    NoDocument,
    /// 已迁移或本来就没有旧字段，文件未被改写。
    Unchanged,
    /// 完成了迁移。
    Migrated {
        from_max_agents: Option<usize>,
        backup: Option<String>,
    },
}

/// 迁移结果 + 诊断（诊断不影响启动，供 UI/日志展示）。
#[derive(Debug, Clone)]
pub struct MigrationReport {
    pub outcome: MigrationOutcome,
    pub diagnostics: Vec<String>,
}

/// 执行迁移。返回 `Err` 表示**不能**继续启动（配置处于半迁移状态）。
pub fn migrate_settings(home: &Path) -> Result<MigrationReport, String> {
    let path = home.join("settings.yaml");
    if !path.exists() {
        return Ok(MigrationReport {
            outcome: MigrationOutcome::NoDocument,
            diagnostics: Vec::new(),
        });
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("无法读取 {}:{error}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(MigrationReport {
            outcome: MigrationOutcome::Unchanged,
            diagnostics: Vec::new(),
        });
    }
    let mut document: YamlValue = serde_yaml::from_str(&text)
        .map_err(|error| format!("{} 不是合法 YAML，无法迁移：{error}", path.display()))?;
    let Some(root) = document.as_mapping_mut() else {
        return Err(format!("{} 的根必须是映射", path.display()));
    };
    let mut diagnostics: Vec<String> = Vec::new();

    // ① 取出旧字段（同时从文档里摘掉）。
    let mut legacy_max_agents: Option<YamlValue> = None;
    let mut legacy_max_depth_present = false;
    if let Some(runtime) = root
        .get_mut(YamlValue::String("runtime".into()))
        .and_then(YamlValue::as_mapping_mut)
    {
        if let Some(value) = runtime.remove(YamlValue::String("maxAgents".into())) {
            legacy_max_agents = Some(value);
        }
        if runtime
            .remove(YamlValue::String("maxDepth".into()))
            .is_some()
        {
            legacy_max_depth_present = true;
        }
    }
    if legacy_max_depth_present {
        diagnostics.push(
            "已移除 runtime.maxDepth：子代理禁止派遣子代理是运行时硬规则，不存在可配置的委派深度。"
                .to_string(),
        );
    }

    // ② 读新值。
    let policy_key = YamlValue::String(crate::subagents::policy::SETTINGS_NS.into());
    let policy_present = root
        .get(&policy_key)
        .and_then(YamlValue::as_mapping)
        .and_then(|policy| policy.get(YamlValue::String("maxConcurrentRuns".into())))
        .is_some();

    let mut migrated_from: Option<usize> = None;
    match (&legacy_max_agents, policy_present) {
        (Some(raw), false) => {
            let parsed = raw.as_u64().filter(|value| (1..=64).contains(value));
            match parsed {
                Some(value) => {
                    let mut policy = serde_yaml::Mapping::new();
                    policy.insert(
                        YamlValue::String("maxConcurrentRuns".into()),
                        YamlValue::Number(serde_yaml::Number::from(value)),
                    );
                    root.insert(policy_key.clone(), YamlValue::Mapping(policy));
                    migrated_from = Some(value as usize);
                    diagnostics.push(format!(
                        "已将 runtime.maxAgents={value} 迁移为 subagent-policy.maxConcurrentRuns。"
                    ));
                }
                None => {
                    return Err(format!(
                        "{} 里的 runtime.maxAgents 不是 1–64 的整数；请手工改成合法值，\
                         或直接删除该字段（新配置在 subagent-policy.maxConcurrentRuns）。\
                         为不静默回退默认值，启动已中止。",
                        path.display()
                    ));
                }
            }
        }
        (Some(_raw), true) => {
            diagnostics.push(
                "subagent-policy.maxConcurrentRuns 已存在，遗留在 runtime 的旧并发字段被忽略并移除。"
                    .to_string(),
            );
        }
        (None, _) => {}
    }

    // ③ 迁移标记。
    let marker_present = root
        .get(YamlValue::String(MARKER_SECTION.into()))
        .and_then(YamlValue::as_mapping)
        .and_then(|section| section.get(YamlValue::String(MARKER_KEY.into())))
        .and_then(YamlValue::as_u64)
        .is_some_and(|version| version >= MARKER_VERSION);

    let touched = legacy_max_agents.is_some() || legacy_max_depth_present;
    if !touched && marker_present {
        return Ok(MigrationReport {
            outcome: MigrationOutcome::Unchanged,
            diagnostics,
        });
    }

    let mut section = root
        .get(YamlValue::String(MARKER_SECTION.into()))
        .and_then(YamlValue::as_mapping)
        .cloned()
        .unwrap_or_default();
    section.insert(
        YamlValue::String(MARKER_KEY.into()),
        YamlValue::Number(serde_yaml::Number::from(MARKER_VERSION)),
    );
    section.insert(
        YamlValue::String("updatedAt".into()),
        YamlValue::Number(serde_yaml::Number::from(now_millis())),
    );
    root.insert(
        YamlValue::String(MARKER_SECTION.into()),
        YamlValue::Mapping(section),
    );

    // ④ 先备份再原子提交。
    let backup = path.with_file_name(format!("settings.yaml.bak-{}", now_millis()));
    std::fs::copy(&path, &backup)
        .map_err(|error| format!("迁移前备份失败，已中止迁移：{error}"))?;
    let mut text = serde_yaml::to_string(&document)
        .map_err(|error| format!("序列化迁移后的配置失败：{error}"))?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    let tmp = path.with_extension("yaml.migration.tmp");
    std::fs::write(&tmp, text.as_bytes())
        .map_err(|error| format!("写入迁移后的配置失败，已中止迁移：{error}"))?;
    std::fs::rename(&tmp, &path).map_err(|error| {
        let _ = std::fs::remove_file(&tmp);
        format!("替换 {} 失败，已中止迁移：{error}", path.display())
    })?;

    Ok(MigrationReport {
        outcome: MigrationOutcome::Migrated {
            from_max_agents: migrated_from,
            backup: Some(backup.display().to_string()),
        },
        diagnostics,
    })
}

/// 迁移后，旧字段不应再出现在正式 schema 里——供启动自检与测试断言。
pub fn legacy_fields_present(path: &Path) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let value: Value = serde_yaml::from_str::<serde_yaml::Value>(&text)
        .map_err(|error| error.to_string())
        .and_then(|value| serde_json::to_value(value).map_err(|error| error.to_string()))?;
    let mut found = Vec::new();
    if let Some(runtime) = value.get("runtime").and_then(Value::as_object) {
        for key in ["maxAgents", "maxDepth"] {
            if runtime.contains_key(key) {
                found.push(format!("runtime.{key}"));
            }
        }
    }
    Ok(found)
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("denia-migration-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(home: &Path, text: &str) {
        std::fs::write(home.join("settings.yaml"), text).unwrap();
    }

    fn read(home: &Path) -> serde_json::Value {
        let text = std::fs::read_to_string(home.join("settings.yaml")).unwrap();
        let yaml: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
        serde_json::to_value(yaml).unwrap()
    }

    #[test]
    fn migrates_legacy_concurrency_and_drops_depth() {
        let home = home("basic");
        write(
            &home,
            "runtime:\n  maxAgents: 12\n  maxDepth: 3\n  maxJobs: 4\nconsole:\n  theme: dark\n",
        );
        let report = migrate_settings(&home).unwrap();
        assert!(matches!(
            report.outcome,
            MigrationOutcome::Migrated {
                from_max_agents: Some(12),
                ..
            }
        ));
        let value = read(&home);
        assert_eq!(value["subagent-policy"]["maxConcurrentRuns"], 12);
        // 无关字段保留。
        assert_eq!(value["runtime"]["maxJobs"], 4);
        assert_eq!(value["console"]["theme"], "dark");
        // 旧字段彻底退出。
        assert!(value["runtime"].get("maxAgents").is_none());
        assert!(value["runtime"].get("maxDepth").is_none());
        // 迁移标记。
        assert_eq!(value["migrations"]["subagentRedesign"], 1);
        assert!(
            legacy_fields_present(&home.join("settings.yaml"))
                .unwrap()
                .is_empty()
        );
        // 备份存在。
        let backups: Vec<_> = std::fs::read_dir(&home)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("settings.yaml.bak-")
            })
            .collect();
        assert_eq!(backups.len(), 1);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn second_start_produces_no_change_and_no_new_backup() {
        let home = home("idempotent");
        write(&home, "runtime:\n  maxAgents: 5\n  maxDepth: 2\n");
        migrate_settings(&home).unwrap();
        let first = std::fs::read_to_string(home.join("settings.yaml")).unwrap();
        let backups_after_first = count_backups(&home);
        let report = migrate_settings(&home).unwrap();
        assert_eq!(report.outcome, MigrationOutcome::Unchanged);
        assert_eq!(
            std::fs::read_to_string(home.join("settings.yaml")).unwrap(),
            first
        );
        assert_eq!(count_backups(&home), backups_after_first);
        std::fs::remove_dir_all(home).unwrap();
    }

    fn count_backups(home: &Path) -> usize {
        std::fs::read_dir(home)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("settings.yaml.bak-")
            })
            .count()
    }

    #[test]
    fn new_value_wins_over_legacy() {
        let home = home("newwins");
        write(
            &home,
            "runtime:\n  maxAgents: 3\n  maxDepth: 2\nsubagent-policy:\n  maxConcurrentRuns: 20\n",
        );
        migrate_settings(&home).unwrap();
        let value = read(&home);
        assert_eq!(value["subagent-policy"]["maxConcurrentRuns"], 20);
        assert!(value["runtime"].get("maxAgents").is_none());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn invalid_legacy_value_blocks_startup_without_silent_default() {
        let home = home("invalid");
        let original = "runtime:\n  maxAgents: 0\n  maxDepth: 2\n";
        write(&home, original);
        let error = migrate_settings(&home).unwrap_err();
        assert!(error.contains("maxAgents"), "{error}");
        // 原文件保持不变，不静默回默认。
        assert_eq!(
            std::fs::read_to_string(home.join("settings.yaml")).unwrap(),
            original
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn invalid_legacy_value_is_ignored_when_new_value_exists() {
        let home = home("invalid-but-new");
        write(
            &home,
            "runtime:\n  maxAgents: 0\nsubagent-policy:\n  maxConcurrentRuns: 4\n",
        );
        assert!(migrate_settings(&home).is_ok());
        let value = read(&home);
        assert_eq!(value["subagent-policy"]["maxConcurrentRuns"], 4);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn absent_document_is_a_no_op() {
        let home = home("absent");
        let report = migrate_settings(&home).unwrap();
        assert_eq!(report.outcome, MigrationOutcome::NoDocument);
        assert!(!home.join("settings.yaml").exists());
        std::fs::remove_dir_all(home).unwrap();
    }
}
