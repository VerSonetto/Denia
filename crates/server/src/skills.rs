//! dsh 式分层技能发现：项目 > 自定义 > 用户；正文只在调用时读取。
use serde::{Deserialize, Serialize};

type CachedSkill = (u64, std::time::SystemTime, String, Skill);
static SUMMARY_CACHE: std::sync::OnceLock<std::sync::Mutex<BTreeMap<PathBuf, CachedSkill>>> =
    std::sync::OnceLock::new();

fn summary(path: &Path, source: &str) -> Result<Skill, String> {
    let metadata = std::fs::metadata(path).map_err(|e| e.to_string())?;
    let modified = metadata.modified().map_err(|e| e.to_string())?;
    let cache = SUMMARY_CACHE.get_or_init(Default::default);
    if let Some((size, time, origin, skill)) = cache.lock().unwrap().get(path) {
        if *size == metadata.len() && *time == modified && origin == source {
            return Ok(skill.clone());
        }
    }
    let (skill, _) = parse(path, source)?;
    let mut cache = cache.lock().unwrap();
    if cache.len() >= 1024 {
        cache.clear();
    }
    cache.insert(
        path.into(),
        (metadata.len(), modified, source.into(), skill.clone()),
    );
    Ok(skill)
}
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub source: String,
    pub path: PathBuf,
    pub resource_base: PathBuf,
    pub model_invocable: bool,
    pub user_invocable: bool,
    #[serde(skip)]
    pub builtin_body: Option<String>,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Metadata {
    name: Option<String>,
    description: Option<String>,
    #[serde(default)]
    disable_model_invocation: bool,
    user_invocable: Option<bool>,
}

fn parse(path: &Path, source: &str) -> Result<(Skill, String), String> {
    let size = std::fs::metadata(path)
        .map_err(|e| format!("读取技能属性失败：{e}"))?
        .len();
    if size > 1024 * 1024 {
        return Err(format!("技能文件过大：{}", path.display()));
    }
    let raw = std::fs::read_to_string(path).map_err(|e| format!("读取技能失败：{e}"))?;
    let normalized = raw.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let (metadata, body) = if let Some(rest) = normalized.strip_prefix("---\n") {
        let (header, body) = rest
            .split_once("\n---\n")
            .ok_or_else(|| format!("技能元数据缺少结束标记：{}", path.display()))?;
        (
            serde_yaml::from_str::<Metadata>(header)
                .map_err(|e| format!("技能元数据无效 {}：{e}", path.display()))?,
            body.to_string(),
        )
    } else {
        (Metadata::default(), normalized)
    };
    let fallback = if path.file_name().is_some_and(|n| n == "SKILL.md") {
        path.parent().and_then(Path::file_name)
    } else {
        path.file_stem()
    };
    let name = metadata
        .name
        .unwrap_or_else(|| fallback.unwrap_or_default().to_string_lossy().to_string());
    if name.is_empty()
        || !name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
    {
        return Err(format!("技能名称必须为小写 kebab-case：{name}"));
    }
    let description = metadata.description.unwrap_or_else(|| {
        body.lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim_start_matches('#')
            .trim()
            .to_string()
    });
    if description.is_empty() {
        return Err(format!("技能 {name} 缺少描述"));
    }
    Ok((
        Skill {
            name,
            description,
            source: source.into(),
            path: path.into(),
            resource_base: path.parent().unwrap().into(),
            model_invocable: !metadata.disable_model_invocation,
            user_invocable: metadata.user_invocable.unwrap_or(true),
            builtin_body: None,
        },
        body,
    ))
}

fn builtin_skills() -> Vec<Skill> {
    vec![Skill {
        name: "skill-creator".into(),
        description: "创建或更新符合要求的高质量 Codex skill，并按需组织参考资料、脚本和资源。"
            .into(),
        source: "bundled".into(),
        path: PathBuf::from("<denia-bundled>/skill-creator/SKILL.md"),
        resource_base: PathBuf::from("<denia-bundled>/skill-creator"),
        model_invocable: true,
        user_invocable: true,
        builtin_body: Some(include_str!("builtin_skills/skill-creator/SKILL.md").to_string()),
    }]
}

pub fn discover(home: &Path, cwd: &Path, custom: &[PathBuf]) -> Result<Vec<Skill>, String> {
    let project = cwd
        .ancestors()
        .find(|p| p.join(".git").exists())
        .unwrap_or(cwd);
    let mut roots = vec![
        (project.join(".denia/skills"), "project-denia"),
        (project.join(".dsh/skills"), "project-dsh"),
        (project.join(".agents/skills"), "project-agents"),
    ];
    roots.extend(custom.iter().cloned().map(|p| (p, "custom")));
    roots.push((home.join("skills"), "user-denia"));
    if let Some(user) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        roots.push((PathBuf::from(user).join(".agents/skills"), "user-agents"));
    }
    let mut found = BTreeMap::new();
    for (root, source) in roots {
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("发现技能失败 {}：{e}", root.display())),
        };
        let mut paths = entries
            .map(|e| e.map(|e| e.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        paths.sort();
        for entry in paths {
            if entry
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('.'))
            {
                continue;
            }
            let path = if entry.is_dir() {
                entry.join("SKILL.md")
            } else {
                entry
            };
            if !path.is_file() || path.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let skill = summary(&path, source)?;
            found.entry(skill.name.clone()).or_insert(skill);
        }
    }
    for skill in builtin_skills() {
        found.entry(skill.name.clone()).or_insert(skill);
    }
    Ok(found.into_values().collect())
}

pub fn load(skills: &[Skill], name: &str, user: bool) -> Result<serde_json::Value, String> {
    let skill = skills
        .iter()
        .find(|s| s.name == name)
        .ok_or_else(|| format!("找不到技能：{name}"))?;
    let (fresh, body) = match &skill.builtin_body {
        Some(body) => (skill.clone(), body.clone()),
        None => parse(&skill.path, &skill.source)?,
    };
    if fresh.name != name {
        return Err("技能已变更，请刷新目录".into());
    }
    if !(if user {
        fresh.user_invocable
    } else {
        fresh.model_invocable
    }) {
        return Err("该技能不允许此调用方式".into());
    }
    Ok(serde_json::json!({"skill":fresh,"body":body}))
}

pub fn resource(skills: &[Skill], name: &str, path: &str) -> Result<serde_json::Value, String> {
    // 重新加载策略，避免缓存目录在用户禁用模型调用后继续授予读取。
    load(skills, name, false)?;
    let skill = skills.iter().find(|s| s.name == name).ok_or("找不到技能")?;
    let root = skill
        .resource_base
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let resolved = root
        .join(path)
        .canonicalize()
        .map_err(|e| format!("技能资源不存在：{e}"))?;
    if !resolved.starts_with(&root) {
        return Err("技能资源路径超出技能目录".into());
    }
    if std::fs::metadata(&resolved)
        .map_err(|e| e.to_string())?
        .len()
        > 1024 * 1024
    {
        return Err("技能资源超过 1 MiB 读取上限".into());
    }
    let text = std::fs::read_to_string(&resolved)
        .map_err(|e| format!("技能资源不是可读的 UTF-8 文本：{e}"))?;
    Ok(serde_json::json!({"path":resolved,"text":text}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn precedence_and_policy() {
        let root = std::env::temp_dir().join(format!("denia-skills-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let cwd = root.join("project");
        std::fs::create_dir_all(cwd.join(".denia/skills/review")).unwrap();
        std::fs::create_dir_all(home.join("skills")).unwrap();
        std::fs::write(
            cwd.join(".denia/skills/review/SKILL.md"),
            "---\nname: review\ndescription: 审查\ndisable-model-invocation: true\n---\n执行审查",
        )
        .unwrap();
        std::fs::write(home.join("skills/review.md"), "用户版本").unwrap();
        let skills = discover(&home, &cwd, &[]).unwrap();
        let bundled = skills.iter().find(|s| s.name == "skill-creator").unwrap();
        assert_eq!(bundled.source, "bundled");
        assert!(
            load(&skills, "skill-creator", false).unwrap()["body"]
                .as_str()
                .unwrap()
                .contains("Denia 技能创建器")
        );
        let skill = skills.iter().find(|s| s.name == "review").unwrap();
        assert_eq!(skill.source, "project-denia");
        assert!(load(&skills, "review", false).is_err());
        assert!(load(&skills, "review", true).is_ok());
        assert!(resource(&skills, "review", "SKILL.md").is_err());
        std::fs::write(
            cwd.join(".denia/skills/review/SKILL.md"),
            "---\nname: review\ndescription: 资源检查\n---\n正文",
        )
        .unwrap();
        std::fs::write(cwd.join(".denia/skills/review/reference.md"), "技能参考").unwrap();
        std::fs::write(cwd.join(".denia/skills/outside.md"), "外部文件").unwrap();
        let fresh = discover(&home, &cwd, &[]).unwrap();
        assert_eq!(
            resource(&fresh, "review", "reference.md").unwrap()["text"],
            "技能参考"
        );
        assert!(resource(&fresh, "review", "../outside.md").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
