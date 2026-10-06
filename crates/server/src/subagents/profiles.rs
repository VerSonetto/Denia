//! 子代理定义仓库：文件、覆盖层、revision 与解析诊断的**唯一所有者**。
//!
//! 存储格式是严格 YAML frontmatter + Markdown 正文（正文即 `instructions`）。
//! 目录：
//! - 内置：程序提供（[`denia_core::subagent::builtin_subagent_profiles`]）；
//! - 用户级：`$DENIA_HOME/subagents/<id>.md`；
//! - 项目级：`<projectRoot>/.denia/subagents/<id>.md`，projectRoot 复用 Denia
//!   当前的项目根发现规则（向父目录找 `.git`）。
//!
//! 合并顺序 builtin → user → project，同 id 后者覆盖前者。**被覆盖**的定义
//! 不会被删除，只是不再作为该逻辑 id 的有效项；请求被覆盖的限定 id 会被明确
//! 拒绝（`subagent/profile-shadowed`）而不是绕过覆盖生效。
//!
//! 本模块不做任何模型调用，也不创建进程——它只回答"定义是什么"。

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use denia_core::subagent::{
    PermissionCeiling, ProfileColor, ProfileDiagnostic, ProfileSource, ProfileWriteScope,
    QualifiedProfileId, SUBAGENT_SCHEMA_VERSION, SubagentProfile, SubagentProfileView,
    ToolSelection, codes, is_valid_subagent_id,
};
use serde_json::Value as JsonValue;
use serde_yaml::Value as YamlValue;

/// 用户级定义目录名（相对 `$DENIA_HOME`）。
pub const USER_DIR: &str = "subagents";
/// 项目级定义目录（相对项目根）。
pub const PROJECT_DIR: &str = ".denia/subagents";
/// frontmatter 中不允许出现、只属于正文的字段。
const BODY_FIELDS: &[&str] = &["instructions", "body"];

/// 定义仓库的错误：带稳定 code、字段与候选，供 API 与模型侧错误统一消费。
#[derive(Debug, Clone)]
pub struct ProfileError {
    pub code: String,
    pub message: String,
    pub field: Option<String>,
    pub candidates: Vec<String>,
}

impl ProfileError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            field: None,
            candidates: Vec::new(),
        }
    }

    pub fn field(mut self, field: &str) -> Self {
        self.field = Some(field.to_string());
        self
    }

    pub fn candidates(mut self, candidates: Vec<String>) -> Self {
        self.candidates = candidates;
        self
    }

    /// 模型侧错误文本：稳定 code + 原因 + 候选，便于模型自我纠正。
    pub fn render(&self) -> String {
        let mut text = format!("{}（{}）", self.message, self.code);
        if let Some(field) = &self.field {
            text.push_str(&format!("；字段：{field}"));
        }
        if !self.candidates.is_empty() {
            text.push_str(&format!("；可用候选：{}", self.candidates.join("、")));
        }
        text
    }
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.render())
    }
}

/// 一个逻辑 id 的有效定义（供派遣解析）。
#[derive(Debug, Clone)]
pub struct ResolvedProfile {
    pub qualified_id: String,
    pub source: ProfileSource,
    pub revision: u64,
    pub profile: SubagentProfile,
}

/// 目录快照：有效项 + 被覆盖项 + 诊断。
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    /// 每个逻辑 id 的有效项（派遣与父代理目录用这一份）。
    pub effective: Vec<SubagentProfileView>,
    /// 被覆盖的底层定义：仅管理页可见/可复制，不可直接派遣。
    pub shadowed: Vec<SubagentProfileView>,
    /// 解析诊断（损坏文件、非法字段、目录不可写等）。
    pub diagnostics: Vec<ProfileDiagnostic>,
    /// 目录内容版本（内容 hash），供 UI 判断是否需要重载。
    pub revision: u64,
    pub user_dir: String,
    pub project_dir: Option<String>,
    pub project_root: Option<String>,
}

/// `$DENIA_HOME/subagents` 等目录的解析与读写的唯一入口。
pub struct ProfileStore {
    home: PathBuf,
    builtin: Vec<SubagentProfile>,
    /// 写路径互斥：创建/覆盖只有一条写路径，且同目录临时文件 + 原子替换。
    write_lock: Mutex<()>,
}

impl ProfileStore {
    pub fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            builtin: denia_core::subagent::builtin_subagent_profiles(),
            write_lock: Mutex::new(()),
        }
    }

    pub fn user_dir(&self) -> PathBuf {
        self.home.join(USER_DIR)
    }

    /// 项目根：cwd 向上找 `.git`（与 skills/AGENTS.md 同款规则），找不到就用 cwd。
    pub fn project_root(cwd: &Path) -> PathBuf {
        cwd.ancestors()
            .find(|path| path.join(".git").exists())
            .unwrap_or(cwd)
            .to_path_buf()
    }

    pub fn project_dir_for(&self, cwd: &Path) -> PathBuf {
        Self::project_root(cwd).join(PROJECT_DIR)
    }

    /// 读取完整目录（含被覆盖项）。project_root 为 None 时只有内置 + 用户级。
    pub fn catalog(&self, project_root: Option<&Path>) -> Catalog {
        let user_dir = self.user_dir();
        let project_dir = project_root.map(|root| root.join(PROJECT_DIR));
        let user_files = self.scan_dir(&user_dir, ProfileSource::User);
        let project_files = match &project_dir {
            Some(dir) => self.scan_dir(dir, ProfileSource::Project),
            None => ScanResult::default(),
        };
        let mut diagnostics: Vec<ProfileDiagnostic> = Vec::new();
        diagnostics.extend(user_files.diagnostics.iter().cloned());
        diagnostics.extend(project_files.diagnostics.iter().cloned());
        // builtin → user → project：后者覆盖前者，BTreeMap 保证稳定排序。
        let mut by_id: BTreeMap<String, Layer> = BTreeMap::new();
        let mut layers: Vec<SubagentProfileView> = Vec::new();
        let builtin_ids: HashSet<String> = self.builtin.iter().map(|p| p.id.clone()).collect();
        for profile in &self.builtin {
            let revision = revision_of_builtin(profile);
            let view = view_of(
                profile.clone(),
                ProfileSource::Builtin,
                revision,
                false,
                project_dir.is_some(),
                Vec::new(),
            );
            layers.push(view.clone());
            by_id.insert(
                profile.id.clone(),
                Layer {
                    view,
                    shadowed_by: None,
                },
            );
        }
        for layer in [
            (ProfileSource::User, &user_files.definitions),
            (ProfileSource::Project, &project_files.definitions),
        ] {
            for (profile, revision, extra) in layer.1 {
                let overrides = builtin_ids.contains(&profile.id);
                let view = view_of(
                    profile.clone(),
                    layer.0,
                    *revision,
                    overrides,
                    project_dir.is_some(),
                    extra.clone(),
                );
                layers.push(view.clone());
                by_id.insert(
                    profile.id.clone(),
                    Layer {
                        view,
                        shadowed_by: None,
                    },
                );
            }
        }
        // 被覆盖项：同一逻辑 id 有多个 layer 时，除最后一个外都记录为 shadowed。
        let mut by_logical: BTreeMap<String, Vec<SubagentProfileView>> = BTreeMap::new();
        for view in &layers {
            by_logical
                .entry(view.profile.id.clone())
                .or_default()
                .push(view.clone());
        }
        let mut effective: Vec<SubagentProfileView> = Vec::new();
        let mut shadowed: Vec<SubagentProfileView> = Vec::new();
        for (_id, mut group) in by_logical {
            // 目录扫描顺序即合并顺序；group 内最后一个为有效项。
            let winner = group.pop().expect("group is non-empty");
            shadowed.extend(group);
            effective.push(winner);
        }
        let revision = catalog_revision(&effective, &shadowed);
        Catalog {
            effective,
            shadowed,
            diagnostics,
            revision,
            user_dir: user_dir.display().to_string(),
            project_dir: project_dir.map(|dir| dir.display().to_string()),
            project_root: project_root.map(|root| root.display().to_string()),
        }
    }

    /// 派遣目录条目：每个逻辑 id 的有效项，只含选型所需摘要。
    pub fn catalog_entries(
        &self,
        project_root: Option<&Path>,
    ) -> Vec<denia_core::subagent::SubagentCatalogEntry> {
        let catalog = self.catalog(project_root);
        let mut out: Vec<denia_core::subagent::SubagentCatalogEntry> = catalog
            .effective
            .iter()
            .filter(|view| view.profile.enabled && view.diagnostics.is_empty())
            .map(|view| {
                denia_core::subagent::SubagentCatalogEntry::new(
                    view.qualified_id.clone(),
                    &view.profile,
                )
            })
            .collect();
        out.sort_by(|left, right| left.qualified_id.cmp(&right.qualified_id));
        out
    }

    /// 全部可派遣的限定 id（按逻辑 id 排序）。
    pub fn dispatchable(&self, project_root: Option<&Path>) -> Vec<ResolvedProfile> {
        let catalog = self.catalog(project_root);
        let mut out = Vec::new();
        for view in catalog.effective {
            if !view.profile.enabled || !view.diagnostics.is_empty() {
                // 禁用或损坏的有效定义不可派遣（不回落低优先级同名项）。
                continue;
            }
            out.push(ResolvedProfile {
                qualified_id: view.qualified_id.clone(),
                source: view.source,
                revision: view.revision,
                profile: view.profile,
            });
        }
        out
    }

    /// 解析一个限定 id。
    ///
    /// - 请求的是**有效**限定 id → 返回它；
    /// - 请求的是被覆盖的底层限定 id → `subagent/profile-shadowed` 并指出有效项；
    /// - 逻辑 id 不存在 → `subagent/profile-not-found`；
    /// - 有效定义被禁用或损坏 → `subagent/profile-disabled` / `profile-invalid`
    ///   （**不**回落同名低优先级定义）。
    pub fn resolve(
        &self,
        project_root: Option<&Path>,
        raw: &str,
    ) -> Result<ResolvedProfile, ProfileError> {
        let requested = QualifiedProfileId::parse(raw).map_err(|message| {
            ProfileError::new(codes::PROFILE_INVALID, message)
                .field("profile_id")
                .candidates(self.effective_ids(project_root))
        })?;
        let catalog = self.catalog(project_root);
        let effective = catalog
            .effective
            .iter()
            .find(|view| view.profile.id == requested.id)
            .ok_or_else(|| {
                ProfileError::new(codes::PROFILE_NOT_FOUND, format!("未知的子代理定义：{raw}"))
                    .field("profile_id")
                    .candidates(self.effective_ids(project_root))
            })?;
        if effective.source != requested.source {
            return Err(ProfileError::new(
                codes::PROFILE_SHADOWED,
                format!(
                    "{raw} 已被更高优先级的定义覆盖，不能绕过覆盖直接派遣（有效项：{}）",
                    effective.qualified_id
                ),
            )
            .field("profile_id")
            .candidates(vec![effective.qualified_id.clone()]));
        }
        if !effective.diagnostics.is_empty() {
            return Err(ProfileError::new(
                codes::PROFILE_INVALID,
                format!(
                    "{} 的定义文件无法解析，已拒绝使用：{}",
                    effective.qualified_id,
                    effective
                        .diagnostics
                        .iter()
                        .map(|issue| issue.message.as_str())
                        .collect::<Vec<_>>()
                        .join(";")
                ),
            )
            .field("profile_id"));
        }
        if !effective.profile.enabled {
            return Err(ProfileError::new(
                codes::PROFILE_DISABLED,
                format!(
                    "{} 已被禁用，不能派遣；请选择其他类型或先在设置里启用",
                    effective.qualified_id
                ),
            )
            .field("profile_id")
            .candidates(self.effective_ids(project_root)));
        }
        Ok(ResolvedProfile {
            qualified_id: effective.qualified_id.clone(),
            source: effective.source,
            revision: effective.revision,
            profile: effective.profile.clone(),
        })
    }

    /// 省略 `profile_id` 时的默认定义：逻辑 id `develop` 的有效项。
    pub fn resolve_default(
        &self,
        project_root: Option<&Path>,
    ) -> Result<ResolvedProfile, ProfileError> {
        let catalog = self.catalog(project_root);
        let effective = catalog
            .effective
            .iter()
            .find(|view| view.profile.id == "develop")
            .ok_or_else(|| {
                ProfileError::new(
                    codes::PROFILE_NOT_FOUND,
                    "默认的 develop 定义不存在；请显式指定 profile_id 或 inline 规格",
                )
            })?;
        let qualified = effective.qualified_id.clone();
        self.resolve(project_root, &qualified).map_err(|error| {
            ProfileError::new(
                codes::PROFILE_DISABLED,
                format!(
                    "默认的 develop 定义当前不可用（{}）；请选择其他类型",
                    error.message
                ),
            )
            .field("profile_id")
            .candidates(self.effective_ids(project_root))
        })
    }

    pub fn effective_ids(&self, project_root: Option<&Path>) -> Vec<String> {
        self.catalog(project_root)
            .effective
            .iter()
            .filter(|view| view.profile.enabled && view.diagnostics.is_empty())
            .map(|view| view.qualified_id.clone())
            .collect()
    }

    /// 创建定义。`scope` 只允许 user/project（builtin 不可写程序资源）。
    pub fn create(
        &self,
        scope: ProfileWriteScope,
        project_root: Option<&Path>,
        mut profile: SubagentProfile,
        body: String,
    ) -> Result<SubagentProfileView, ProfileError> {
        profile.instructions = body;
        profile.schema_version = SUBAGENT_SCHEMA_VERSION;
        self.validate_for_write(&profile)?;
        let target_dir = self.dir_for_write(scope, project_root)?;
        let _guard = self.write_lock.lock().unwrap_or_else(|p| p.into_inner());
        let path = definition_path(&target_dir, &profile.id)?;
        if path.exists() {
            return Err(ProfileError::new(
                codes::PROFILE_EXISTS,
                format!(
                    "{} 已存在：{}",
                    QualifiedProfileId::new(source_for_scope(scope), profile.id.clone()),
                    path.display()
                ),
            )
            .field("id"));
        }
        write_definition(&target_dir, &path, &profile)?;
        let catalog = self.catalog(project_root);
        self.view_for(&catalog, &profile.id).ok_or_else(|| {
            ProfileError::new(
                codes::PROFILE_INVALID,
                "定义已写入，但重新读取失败；请检查文件内容",
            )
        })
    }

    /// 全量更新（必须带 expectedRevision）。内置把内容写进用户覆盖层，
    /// 不修改程序资源。
    pub fn update(
        &self,
        project_root: Option<&Path>,
        raw: &str,
        mut profile: SubagentProfile,
        body: String,
        expected_revision: u64,
    ) -> Result<SubagentProfileView, ProfileError> {
        let catalog = self.catalog(project_root);
        let qualified = QualifiedProfileId::parse(raw).map_err(|message| {
            ProfileError::new(codes::PROFILE_INVALID, message).field("qualifiedId")
        })?;
        let layer = catalog
            .effective
            .iter()
            .find(|view| view.profile.id == qualified.id)
            .ok_or_else(|| {
                ProfileError::new(codes::PROFILE_NOT_FOUND, format!("未知的子代理定义：{raw}"))
                    .candidates(
                        catalog
                            .effective
                            .iter()
                            .map(|view| view.qualified_id.clone())
                            .collect(),
                    )
            })?;
        // 有效项不是调用方指的那个（被覆盖）时拒绝，避免写错层。
        if layer.source != qualified.source {
            return Err(ProfileError::new(
                codes::PROFILE_SHADOWED,
                format!(
                    "{raw} 已被 {} 覆盖；请改为编辑有效项，或把 scope 指到覆盖它的作用域",
                    layer.qualified_id
                ),
            )
            .field("qualifiedId")
            .candidates(vec![layer.qualified_id.clone()]));
        }
        if layer.revision != expected_revision {
            return Err(ProfileError::new(
                codes::REVISION_CONFLICT,
                format!(
                    "定义已被其他改动更新（当前 revision {}，提交 {expected_revision}）；请重新加载后再保存",
                    layer.revision
                ),
            )
            .field("expectedRevision"));
        }
        // 写入作用域：内置写用户覆盖；其余写自身层。
        let scope = scope_for_source(layer.source);
        profile.id = qualified.id.clone();
        profile.instructions = body;
        profile.schema_version = SUBAGENT_SCHEMA_VERSION;
        self.validate_for_write(&profile)?;
        let target_dir = self.dir_for_write(scope, project_root)?;
        let _guard = self.write_lock.lock().unwrap_or_else(|p| p.into_inner());
        let path = definition_path(&target_dir, &profile.id)?;
        // 覆盖写路径同样做 revision 复核：两个窗口并发保存，后到者拿到冲突。
        if let Some(current) = read_definition(&path, &profile.id) {
            let current_revision = current.revision;
            if current_revision != layer.revision && layer.source == source_for_scope(scope) {
                return Err(ProfileError::new(
                    codes::REVISION_CONFLICT,
                    format!(
                        "文件已在别处被修改（当前 revision {current_revision}）；请重新加载后再保存"
                    ),
                )
                .field("expectedRevision"));
            }
        }
        write_definition(&target_dir, &path, &profile)?;
        let catalog = self.catalog(project_root);
        self.view_for(&catalog, &profile.id).ok_or_else(|| {
            ProfileError::new(
                codes::PROFILE_INVALID,
                "定义已写入，但重新读取失败；请检查文件内容",
            )
        })
    }

    /// 删除某个作用域的覆盖/定义，返回恢复后的有效项。
    pub fn delete(
        &self,
        project_root: Option<&Path>,
        raw: &str,
        scope: ProfileWriteScope,
        expected_revision: Option<u64>,
    ) -> Result<Option<SubagentProfileView>, ProfileError> {
        let qualified = QualifiedProfileId::parse(raw).map_err(|message| {
            ProfileError::new(codes::PROFILE_INVALID, message).field("qualifiedId")
        })?;
        let catalog = self.catalog(project_root);
        let Some(layer) = catalog
            .effective
            .iter()
            .find(|view| view.profile.id == qualified.id)
        else {
            return Err(ProfileError::new(
                codes::PROFILE_NOT_FOUND,
                format!("未知的子代理定义：{raw}"),
            )
            .candidates(
                catalog
                    .effective
                    .iter()
                    .map(|view| view.qualified_id.clone())
                    .collect(),
            ));
        };
        let revision = layer.revision;
        if let Some(expected) = expected_revision
            && expected != revision
        {
            return Err(ProfileError::new(
                codes::REVISION_CONFLICT,
                format!(
                    "定义已被其他改动更新（当前 revision {revision}，提交 {expected}）；请重新加载后再删除"
                ),
            )
            .field("expectedRevision"));
        }
        let target_dir = self.dir_for_write(scope, project_root)?;
        let _guard = self.write_lock.lock().unwrap_or_else(|p| p.into_inner());
        let path = definition_path(&target_dir, &qualified.id)?;
        if !path.exists() {
            return Err(ProfileError::new(
                codes::PROFILE_NOT_FOUND,
                format!(
                    "{} 在 {} 作用域下没有可删除的定义",
                    qualified,
                    source_for_scope(scope).as_str()
                ),
            )
            .field("scope"));
        }
        remove_definition(&path)?;
        let catalog = self.catalog(project_root);
        Ok(self.view_for(&catalog, &qualified.id))
    }

    /// 内置恢复默认：删除指定作用域的覆盖。
    pub fn reset(
        &self,
        project_root: Option<&Path>,
        raw: &str,
        scope: ProfileWriteScope,
    ) -> Result<Option<SubagentProfileView>, ProfileError> {
        self.delete(project_root, raw, scope, None)
    }

    fn view_for(&self, catalog: &Catalog, logical_id: &str) -> Option<SubagentProfileView> {
        catalog
            .effective
            .iter()
            .find(|view| view.profile.id == logical_id)
            .cloned()
    }

    fn validate_for_write(&self, profile: &SubagentProfile) -> Result<(), ProfileError> {
        if let Err(issues) = profile.validate() {
            let message = issues
                .iter()
                .map(|issue| issue.message.as_str())
                .collect::<Vec<_>>()
                .join(";");
            let field = issues.first().and_then(|issue| issue.field.clone());
            let mut error = ProfileError::new(codes::PROFILE_INVALID, message);
            error.field = field;
            return Err(error);
        }
        Ok(())
    }

    fn dir_for_write(
        &self,
        scope: ProfileWriteScope,
        project_root: Option<&Path>,
    ) -> Result<PathBuf, ProfileError> {
        match scope {
            ProfileWriteScope::User => Ok(self.user_dir()),
            ProfileWriteScope::Project => {
                let root = project_root.ok_or_else(|| {
                    ProfileError::new(
                        codes::SCOPE_FORBIDDEN,
                        "当前上下文没有项目根，不能写项目级定义；请指定会话或改用用户作用域",
                    )
                    .field("scope")
                })?;
                Ok(root.join(PROJECT_DIR))
            }
        }
    }

    fn scan_dir(&self, dir: &Path, source: ProfileSource) -> ScanResult {
        let mut result = ScanResult::default();
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return result,
            Err(error) => {
                result.diagnostics.push(ProfileDiagnostic::error(
                    codes::PROFILE_INVALID,
                    Some("dir"),
                    format!("无法读取定义目录 {}：{error}", dir.display()),
                ));
                return result;
            }
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
            .collect();
        paths.sort();
        for path in paths {
            match parse_definition_file(&path) {
                Ok((profile, revision)) => {
                    if profile.id != file_stem(&path) {
                        result.diagnostics.push(ProfileDiagnostic::error(
                            codes::PROFILE_INVALID,
                            Some("id"),
                            format!(
                                "{} 里的 id `{}` 与文件名不一致；文件名即 id",
                                path.display(),
                                profile.id
                            ),
                        ));
                        continue;
                    }
                    result.definitions.push((profile, revision, Vec::new()));
                }
                Err(issues) => {
                    let label = path
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_default();
                    result
                        .diagnostics
                        .extend(issues.into_iter().map(|mut issue| {
                            issue.message = format!("{label}：{}", issue.message);
                            issue
                        }));
                }
            }
        }
        let _ = source;
        result
    }
}

/// 管理页看到的写入目标作用域。
pub fn write_scope_for(view: &SubagentProfileView) -> ProfileWriteScope {
    scope_for_source(view.source)
}

fn scope_for_source(source: ProfileSource) -> ProfileWriteScope {
    match source {
        // 内置不可写程序资源：编辑写用户覆盖，删除/恢复默认作用域由调用方指定。
        ProfileSource::Builtin | ProfileSource::User => ProfileWriteScope::User,
        ProfileSource::Project => ProfileWriteScope::Project,
    }
}

fn source_for_scope(scope: ProfileWriteScope) -> ProfileSource {
    match scope {
        ProfileWriteScope::User => ProfileSource::User,
        ProfileWriteScope::Project => ProfileSource::Project,
    }
}

#[derive(Default)]
struct ScanResult {
    definitions: Vec<(SubagentProfile, u64, Vec<ProfileDiagnostic>)>,
    diagnostics: Vec<ProfileDiagnostic>,
}

struct Layer {
    view: SubagentProfileView,
    #[allow(dead_code)]
    shadowed_by: Option<String>,
}

fn view_of(
    profile: SubagentProfile,
    source: ProfileSource,
    revision: u64,
    overrides_builtin: bool,
    project_writable: bool,
    diagnostics: Vec<ProfileDiagnostic>,
) -> SubagentProfileView {
    SubagentProfileView {
        qualified_id: QualifiedProfileId::new(source, profile.id.clone()).to_string(),
        profile,
        source,
        revision,
        editable: true,
        overrides_builtin,
        project_writable,
        diagnostics,
    }
}

/// 定义文件路径。id 已通过语法校验，因此不可能含分隔符或 `..`；
/// 这里再做一次防御性复核，并拒绝把定义写到符号链接上。
fn definition_path(dir: &Path, id: &str) -> Result<PathBuf, ProfileError> {
    if !is_valid_subagent_id(id) {
        return Err(
            ProfileError::new(codes::PROFILE_INVALID, format!("非法的定义 id：{id}")).field("id"),
        );
    }
    Ok(dir.join(format!("{id}.md")))
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// 解析一份定义文件：严格 frontmatter + 正文。
fn parse_definition_file(path: &Path) -> Result<(SubagentProfile, u64), Vec<ProfileDiagnostic>> {
    let bytes = std::fs::read(path).map_err(|error| {
        vec![ProfileDiagnostic::error(
            codes::PROFILE_INVALID,
            None,
            format!("无法读取定义文件：{error}"),
        )]
    })?;
    let revision = hash_bytes(&bytes);
    let raw = String::from_utf8(bytes).map_err(|_| {
        vec![ProfileDiagnostic::error(
            codes::PROFILE_INVALID,
            None,
            "定义文件不是合法的 UTF-8".to_string(),
        )]
    })?;
    let normalized = raw.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let (frontmatter, body) = split_frontmatter(&normalized)?;
    let profile = parse_frontmatter(&frontmatter, &file_stem(path), &body)?;
    Ok((profile, revision))
}

/// frontmatter 与正文的分割：`---` 起、`---` 止；缺失 frontmatter 直接报错。
fn split_frontmatter(text: &str) -> Result<(String, String), Vec<ProfileDiagnostic>> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
        .ok_or_else(|| {
            vec![ProfileDiagnostic::error(
                codes::PROFILE_INVALID,
                Some("frontmatter"),
                "定义文件必须以 `---` 开头的 YAML frontmatter 起始".to_string(),
            )]
        })?;
    let end = rest.find("\n---").ok_or_else(|| {
        vec![ProfileDiagnostic::error(
            codes::PROFILE_INVALID,
            Some("frontmatter"),
            "定义文件缺少结束的 `---`".to_string(),
        )]
    })?;
    let frontmatter = rest[..end].to_string();
    let after = &rest[end + 4..];
    let body = after.strip_prefix('\n').unwrap_or(after);
    Ok((frontmatter, body.to_string()))
}

/// 严格解析 frontmatter：每一层的键集合都必须精确匹配，未知键失败。
fn parse_frontmatter(
    frontmatter: &str,
    file_id: &str,
    body: &str,
) -> Result<SubagentProfile, Vec<ProfileDiagnostic>> {
    let value: YamlValue = serde_yaml::from_str(frontmatter).map_err(|error| {
        vec![ProfileDiagnostic::error(
            codes::PROFILE_INVALID,
            Some("frontmatter"),
            format!("frontmatter 不是合法 YAML：{error}"),
        )]
    })?;
    let mut issues: Vec<ProfileDiagnostic> = Vec::new();
    let mapping = match &value {
        YamlValue::Mapping(mapping) => mapping,
        _ => {
            return Err(vec![ProfileDiagnostic::error(
                codes::PROFILE_INVALID,
                Some("frontmatter"),
                "frontmatter 必须是一个映射".to_string(),
            )]);
        }
    };
    let allowed = [
        "schemaVersion",
        "id",
        "name",
        "description",
        "enabled",
        "tools",
        "model",
        "permissionCeiling",
        "color",
    ];
    strict_keys(mapping, &allowed, "frontmatter", &mut issues);
    for field in BODY_FIELDS {
        if mapping.contains_key(YamlValue::String((*field).to_string())) {
            issues.push(ProfileDiagnostic::error(
                codes::PROFILE_INVALID,
                Some(field),
                format!("{field} 属于 Markdown 正文，不能写在 frontmatter 里"),
            ));
        }
    }
    if let Some(tools) = mapping.get(YamlValue::String("tools".into())) {
        match tools {
            YamlValue::Mapping(tools) => {
                strict_keys(tools, &["mode", "names"], "tools", &mut issues);
                check_mode(tools, &["inherit", "allowlist"], "tools", &mut issues);
                if tools.get(YamlValue::String("mode".into()))
                    == Some(&YamlValue::String("inherit".into()))
                    && tools.contains_key(YamlValue::String("names".into()))
                {
                    issues.push(ProfileDiagnostic::error(
                        codes::PROFILE_INVALID,
                        Some("tools.names"),
                        "tools.mode=inherit 时不能同时给出 names".to_string(),
                    ));
                }
            }
            _ => issues.push(ProfileDiagnostic::error(
                codes::PROFILE_INVALID,
                Some("tools"),
                "tools 必须是映射".to_string(),
            )),
        }
    }
    if let Some(model) = mapping.get(YamlValue::String("model".into())) {
        match model {
            YamlValue::Mapping(model) => {
                strict_keys(model, &["mode", "selection"], "model", &mut issues);
                check_mode(model, &["inherit", "explicit"], "model", &mut issues);
                if let Some(selection) = model.get(YamlValue::String("selection".into())) {
                    match selection {
                        YamlValue::Mapping(selection) => {
                            strict_keys(
                                selection,
                                &["provider", "model", "reasoningEffort"],
                                "model.selection",
                                &mut issues,
                            );
                        }
                        _ => issues.push(ProfileDiagnostic::error(
                            codes::PROFILE_INVALID,
                            Some("model.selection"),
                            "model.selection 必须是映射".to_string(),
                        )),
                    }
                }
            }
            _ => issues.push(ProfileDiagnostic::error(
                codes::PROFILE_INVALID,
                Some("model"),
                "model 必须是映射".to_string(),
            )),
        }
    }
    if let Some(color) = mapping.get(YamlValue::String("color".into())) {
        let known = ["blue", "green", "purple", "orange", "red", "gray"];
        match color.as_str() {
            Some(name) if known.contains(&name) => {}
            _ => issues.push(ProfileDiagnostic::error(
                codes::PROFILE_INVALID,
                Some("color"),
                format!("未知的颜色；可选：{}", known.join("、")),
            )),
        }
    }
    if !issues.is_empty() {
        return Err(issues);
    }
    let json = serde_json::to_value(&value).map_err(|error| {
        vec![ProfileDiagnostic::error(
            codes::PROFILE_INVALID,
            None,
            error.to_string(),
        )]
    })?;
    let mut profile: SubagentProfile = serde_json::from_value(json).map_err(|error| {
        vec![ProfileDiagnostic::error(
            codes::PROFILE_INVALID,
            None,
            format!("定义字段无法解析：{error}"),
        )]
    })?;
    let _ = file_id;
    profile.instructions = body.to_string();
    profile.validate().map_err(|issues| issues)?;
    Ok(profile)
}

fn strict_keys(
    mapping: &serde_yaml::Mapping,
    allowed: &[&str],
    where_: &str,
    issues: &mut Vec<ProfileDiagnostic>,
) {
    for key in mapping.keys() {
        let Some(name) = key.as_str() else {
            issues.push(ProfileDiagnostic::error(
                codes::PROFILE_INVALID,
                Some(where_),
                format!("{where_} 的键必须是字符串"),
            ));
            continue;
        };
        if !allowed.contains(&name) {
            issues.push(ProfileDiagnostic::error(
                codes::PROFILE_INVALID,
                Some(where_),
                format!(
                    "{where_} 不支持字段 `{name}`（允许：{}）",
                    allowed.join("、")
                ),
            ));
        }
    }
}

fn check_mode(
    mapping: &serde_yaml::Mapping,
    modes: &[&str],
    where_: &str,
    issues: &mut Vec<ProfileDiagnostic>,
) {
    match mapping
        .get(YamlValue::String("mode".into()))
        .and_then(YamlValue::as_str)
    {
        Some(mode) if modes.contains(&mode) => {}
        Some(mode) => issues.push(ProfileDiagnostic::error(
            codes::PROFILE_INVALID,
            Some(where_),
            format!(
                "{where_}.mode 不支持 `{mode}`（允许：{}）",
                modes.join("、")
            ),
        )),
        None => issues.push(ProfileDiagnostic::error(
            codes::PROFILE_INVALID,
            Some(where_),
            format!("{where_} 缺少必填的 mode"),
        )),
    }
}

/// 读取一份已存在的定义（用于写前 revision 复核）；解析失败返回 None。
fn read_definition(path: &Path, _id: &str) -> Option<ResolvedProfile> {
    if !path.exists() {
        return None;
    }
    let (profile, revision) = parse_definition_file(path).ok()?;
    Some(ResolvedProfile {
        qualified_id: profile.id.clone(),
        source: ProfileSource::User,
        revision,
        profile,
    })
}

/// 序列化一份定义：frontmatter 与正文。
pub fn serialize_definition(profile: &SubagentProfile) -> Result<String, ProfileError> {
    let mut front = serde_json::Map::new();
    front.insert(
        "schemaVersion".into(),
        JsonValue::from(profile.schema_version),
    );
    front.insert("id".into(), JsonValue::from(profile.id.clone()));
    front.insert("name".into(), JsonValue::from(profile.name.clone()));
    front.insert(
        "description".into(),
        JsonValue::from(profile.description.clone()),
    );
    front.insert("enabled".into(), JsonValue::from(profile.enabled));
    front.insert(
        "tools".into(),
        serde_json::to_value(&profile.tools)
            .map_err(|error| ProfileError::new(codes::PROFILE_INVALID, error.to_string()))?,
    );
    front.insert(
        "model".into(),
        serde_json::to_value(&profile.model)
            .map_err(|error| ProfileError::new(codes::PROFILE_INVALID, error.to_string()))?,
    );
    front.insert(
        "permissionCeiling".into(),
        JsonValue::from(profile.permission_ceiling.as_str()),
    );
    if let Some(color) = profile.color {
        front.insert(
            "color".into(),
            serde_json::to_value(color)
                .map_err(|error| ProfileError::new(codes::PROFILE_INVALID, error.to_string()))?,
        );
    }
    let front_yaml = serde_yaml::to_string(&front)
        .map_err(|error| ProfileError::new(codes::PROFILE_INVALID, error.to_string()))?;
    let mut out = String::from("---\n");
    out.push_str(&front_yaml);
    out.push_str("---\n");
    out.push_str(&profile.instructions);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// 唯一写路径：同目录临时文件 + 原子替换。
fn write_definition(
    dir: &Path,
    path: &Path,
    profile: &SubagentProfile,
) -> Result<(), ProfileError> {
    std::fs::create_dir_all(dir).map_err(|error| {
        ProfileError::new(
            codes::SCOPE_FORBIDDEN,
            format!("无法创建定义目录 {}：{error}", dir.display()),
        )
    })?;
    // 目录本身是符号链接时拒绝：否则"写定义"会落到目录外。
    let meta = std::fs::symlink_metadata(dir).map_err(|error| {
        ProfileError::new(
            codes::SCOPE_FORBIDDEN,
            format!("无法访问定义目录 {}：{error}", dir.display()),
        )
    })?;
    if meta.file_type().is_symlink() {
        return Err(ProfileError::new(
            codes::SCOPE_FORBIDDEN,
            format!("定义目录是符号链接，拒绝写入：{}", dir.display()),
        ));
    }
    if let Ok(existing) = std::fs::symlink_metadata(path)
        && existing.file_type().is_symlink()
    {
        return Err(ProfileError::new(
            codes::SCOPE_FORBIDDEN,
            format!("定义文件是符号链接，拒绝覆盖：{}", path.display()),
        ));
    }
    let text = serialize_definition(profile)?;
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, text.as_bytes()).map_err(|error| {
        ProfileError::new(codes::SCOPE_FORBIDDEN, format!("写入定义失败：{error}"))
    })?;
    std::fs::rename(&tmp, path).map_err(|error| {
        let _ = std::fs::remove_file(&tmp);
        ProfileError::new(codes::SCOPE_FORBIDDEN, format!("替换定义文件失败：{error}"))
    })?;
    Ok(())
}

fn remove_definition(path: &Path) -> Result<(), ProfileError> {
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && meta.file_type().is_symlink()
    {
        return Err(ProfileError::new(
            codes::SCOPE_FORBIDDEN,
            format!("定义文件是符号链接，拒绝删除：{}", path.display()),
        ));
    }
    std::fs::remove_file(path).map_err(|error| {
        ProfileError::new(codes::SCOPE_FORBIDDEN, format!("删除定义失败：{error}"))
    })
}

/// FNV-1a：内容版本（稳定、跨进程一致，外部编辑即刻使旧 revision 失效）。
fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn revision_of_builtin(profile: &SubagentProfile) -> u64 {
    let text = serialize_definition(profile).unwrap_or_else(|_| profile.id.clone());
    hash_bytes(text.as_bytes())
}

fn catalog_revision(effective: &[SubagentProfileView], shadowed: &[SubagentProfileView]) -> u64 {
    let mut text = String::new();
    for view in effective.iter().chain(shadowed.iter()) {
        text.push_str(&view.qualified_id);
        text.push(':');
        text.push_str(&view.revision.to_string());
        text.push(';');
    }
    hash_bytes(text.as_bytes())
}

/// UI 与预览共用的默认颜色列表（前端也读同一份枚举）。
pub fn color_options() -> Vec<ProfileColor> {
    ProfileColor::ALL.to_vec()
}

/// 供测试与诊断：内置定义数量。
pub fn builtin_count() -> usize {
    denia_core::subagent::builtin_subagent_profiles().len()
}

/// 纯函数：把 `ToolSelection` 归一化成稳定排序的显式列表（allowlist 时）；
/// `inherit` 原样返回 None。
pub fn explicit_tool_names(selection: &ToolSelection) -> Option<Vec<String>> {
    match selection {
        ToolSelection::Inherit => None,
        ToolSelection::Allowlist { names } => {
            let mut names = names.clone();
            names.sort();
            Some(names)
        }
    }
}

/// 把 `PermissionCeiling` 渲染成模型可见的一句话。
pub fn ceiling_label(ceiling: PermissionCeiling) -> &'static str {
    match ceiling {
        PermissionCeiling::Inherit => "跟随父权限",
        PermissionCeiling::ReadOnly => "强制只读",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::subagent::ModelChoice;

    fn home(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("denia-subagent-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn project(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "denia-subagent-proj-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        dir
    }

    fn profile(id: &str) -> SubagentProfile {
        let mut profile = denia_core::subagent::builtin_subagent_profiles()[0].clone();
        profile.id = id.to_string();
        profile.name = format!("自定义 {id}");
        profile.description = "测试用定义".to_string();
        profile.tools = ToolSelection::Allowlist {
            names: vec!["read_file".into()],
        };
        profile.permission_ceiling = PermissionCeiling::ReadOnly;
        profile
    }

    #[test]
    fn builtins_are_listed_and_resolvable() {
        let home = home("builtin");
        let store = ProfileStore::new(&home);
        let catalog = store.catalog(None);
        let ids: Vec<&str> = catalog
            .effective
            .iter()
            .map(|view| view.profile.id.as_str())
            .collect();
        assert_eq!(ids, vec!["develop", "explore", "verify"]);
        assert!(catalog.shadowed.is_empty());
        let resolved = store.resolve(None, "builtin:explore").unwrap();
        assert_eq!(resolved.profile.id, "explore");
        assert_eq!(
            resolved.profile.permission_ceiling,
            PermissionCeiling::ReadOnly
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn user_definition_overrides_builtin_and_shadowed_id_is_refused() {
        let home = home("override");
        let project = project("override");
        let store = ProfileStore::new(&home);
        let mut custom = profile("explore");
        custom.description = "用户级覆盖".to_string();
        store
            .create(
                ProfileWriteScope::User,
                Some(&project),
                custom,
                "用户覆盖正文".to_string(),
            )
            .unwrap();
        let catalog = store.catalog(Some(&project));
        let explore = catalog
            .effective
            .iter()
            .find(|view| view.profile.id == "explore")
            .unwrap();
        assert_eq!(explore.source, ProfileSource::User);
        assert!(explore.overrides_builtin);
        // 被覆盖的 builtin 仍在列表里（管理页可见），但派遣被明确拒绝。
        assert!(
            catalog
                .shadowed
                .iter()
                .any(|view| view.qualified_id == "builtin:explore")
        );
        let error = store
            .resolve(Some(&project), "builtin:explore")
            .unwrap_err();
        assert_eq!(error.code, codes::PROFILE_SHADOWED);
        assert_eq!(error.candidates, vec!["user:explore".to_string()]);
        // 有效项仍可派遣。
        assert!(store.resolve(Some(&project), "user:explore").is_ok());
        std::fs::remove_dir_all(home).unwrap();
        std::fs::remove_dir_all(project).unwrap();
    }

    #[test]
    fn project_overrides_user_and_remains_readable_when_removed() {
        let home = home("project");
        let project = project("project");
        let store = ProfileStore::new(&home);
        let mut user_layer = profile("api-reviewer");
        user_layer.description = "用户层".to_string();
        store
            .create(
                ProfileWriteScope::User,
                Some(&project),
                user_layer,
                "用户正文".to_string(),
            )
            .unwrap();
        let mut project_layer = profile("api-reviewer");
        project_layer.description = "项目层".to_string();
        store
            .create(
                ProfileWriteScope::Project,
                Some(&project),
                project_layer,
                "项目正文".to_string(),
            )
            .unwrap();
        let resolved = store
            .resolve(Some(&project), "project:api-reviewer")
            .unwrap();
        assert_eq!(resolved.profile.description, "项目层");
        assert!(store.resolve(Some(&project), "user:api-reviewer").is_err());
        // 删除项目覆盖后，用户层重新成为有效项——不是"永久消失"。
        let restored = store
            .reset(
                Some(&project),
                "project:api-reviewer",
                ProfileWriteScope::Project,
            )
            .unwrap()
            .expect("用户层仍然存在");
        assert_eq!(restored.source, ProfileSource::User);
        assert_eq!(restored.profile.description, "用户层");
        std::fs::remove_dir_all(home).unwrap();
        std::fs::remove_dir_all(project).unwrap();
    }

    #[test]
    fn disabled_effective_definition_does_not_fall_back_to_lower_layer() {
        let home = home("disabled");
        let project = project("disabled");
        let store = ProfileStore::new(&home);
        let mut custom = profile("explore");
        custom.enabled = false;
        store
            .create(
                ProfileWriteScope::User,
                Some(&project),
                custom,
                "禁用".to_string(),
            )
            .unwrap();
        let error = store.resolve(Some(&project), "user:explore").unwrap_err();
        assert_eq!(error.code, codes::PROFILE_DISABLED);
        // 内置同名项不会因为上层被禁用而重新变成可派遣项。
        assert!(store.resolve(Some(&project), "builtin:explore").is_err());
        assert!(
            !store
                .effective_ids(Some(&project))
                .contains(&"builtin:explore".to_string())
        );
        std::fs::remove_dir_all(home).unwrap();
        std::fs::remove_dir_all(project).unwrap();
    }

    #[test]
    fn unknown_fields_bad_yaml_and_traversal_fail_loudly() {
        let home = home("strict");
        let project = project("strict");
        let store = ProfileStore::new(&home);
        std::fs::create_dir_all(store.user_dir()).unwrap();
        std::fs::write(
            store.user_dir().join("bad.md"),
            "---\nschemaVersion: 1\nid: bad\nname: x\ndescription: y\nunknownField: 1\ntools:\n  mode: inherit\n---\nbody\n",
        )
        .unwrap();
        let catalog = store.catalog(Some(&project));
        assert!(
            catalog
                .diagnostics
                .iter()
                .any(|issue| issue.message.contains("不支持字段 `unknownField`")),
            "未知字段必须报错:{:?}",
            catalog.diagnostics
        );
        assert!(
            !catalog
                .effective
                .iter()
                .any(|view| view.profile.id == "bad")
        );

        std::fs::write(store.user_dir().join("broken.md"), "---\nid: [\n---\n").unwrap();
        let catalog = store.catalog(Some(&project));
        assert!(
            catalog
                .diagnostics
                .iter()
                .any(|issue| issue.message.contains("YAML"))
        );

        std::fs::write(
            store.user_dir().join("escape.md"),
            "---\nschemaVersion: 1\nid: escape\nname: x\ndescription: y\ntools:\n  mode: allowlist\n  names: ['../../x']\n---\n",
        )
        .unwrap();
        // 非法工具名在这一层不报（需要注册表），但 id 与文件名必须一致；
        // 路径穿越由 id 语法挡住。
        assert!(!store.user_dir().join("../escape.md").exists());
        assert!(
            store
                .create(
                    ProfileWriteScope::User,
                    Some(&project),
                    profile("../escape"),
                    String::new(),
                )
                .is_err()
        );
        std::fs::remove_dir_all(home).unwrap();
        std::fs::remove_dir_all(project).unwrap();
    }

    #[test]
    fn revision_conflict_is_reported_and_draft_is_kept() {
        let home = home("revision");
        let project = project("revision");
        let store = ProfileStore::new(&home);
        let created = store
            .create(
                ProfileWriteScope::User,
                Some(&project),
                profile("api-reviewer"),
                "第一版".to_string(),
            )
            .unwrap();
        let stale = created.revision;
        // 另一个窗口先保存一次。
        let mut second = created.profile.clone();
        second.description = "第二版".to_string();
        store
            .update(
                Some(&project),
                "user:api-reviewer",
                second,
                "第二版正文".to_string(),
                stale,
            )
            .unwrap();
        // 拿旧 revision 再保存必须失败（旧 revision 保存失败且保留 UI 草稿）。
        let mut third = created.profile.clone();
        third.description = "第三版".to_string();
        let error = store
            .update(
                Some(&project),
                "user:api-reviewer",
                third,
                "第三版正文".to_string(),
                stale,
            )
            .unwrap_err();
        assert_eq!(error.code, codes::REVISION_CONFLICT);
        // 磁盘上仍是第二版。
        let current = store.resolve(Some(&project), "user:api-reviewer").unwrap();
        assert_eq!(current.profile.description, "第二版");
        std::fs::remove_dir_all(home).unwrap();
        std::fs::remove_dir_all(project).unwrap();
    }

    #[test]
    fn create_conflicts_on_same_scope_id_but_allows_builtin_override() {
        let home = home("conflict");
        let project = project("conflict");
        let store = ProfileStore::new(&home);
        store
            .create(
                ProfileWriteScope::User,
                Some(&project),
                profile("api-reviewer"),
                String::new(),
            )
            .unwrap();
        let error = store
            .create(
                ProfileWriteScope::User,
                Some(&project),
                profile("api-reviewer"),
                String::new(),
            )
            .unwrap_err();
        assert_eq!(error.code, codes::PROFILE_EXISTS);
        // builtin:explore 覆盖是允许的（内置可覆盖）。
        store
            .create(
                ProfileWriteScope::User,
                Some(&project),
                profile("explore"),
                String::new(),
            )
            .unwrap();
        std::fs::remove_dir_all(home).unwrap();
        std::fs::remove_dir_all(project).unwrap();
    }

    #[test]
    fn editing_a_builtin_writes_a_user_override_without_touching_program_assets() {
        let home = home("builtin-edit");
        let project = project("builtin-edit");
        let store = ProfileStore::new(&home);
        let builtin = store.resolve(Some(&project), "builtin:explore").unwrap();
        let mut edited = builtin.profile.clone();
        edited.instructions = "编辑后的角色".to_string();
        let view = store
            .update(
                Some(&project),
                "builtin:explore",
                edited,
                "编辑后的角色".to_string(),
                builtin.revision,
            )
            .unwrap();
        assert_eq!(view.source, ProfileSource::User);
        assert_eq!(view.qualified_id, "user:explore");
        assert!(store.user_dir().join("explore.md").exists());
        // 恢复默认后回到内置定义。
        let restored = store
            .reset(Some(&project), "user:explore", ProfileWriteScope::User)
            .unwrap()
            .unwrap();
        assert_eq!(restored.source, ProfileSource::Builtin);
        std::fs::remove_dir_all(home).unwrap();
        std::fs::remove_dir_all(project).unwrap();
    }

    #[test]
    fn definition_round_trips_through_frontmatter_and_body() {
        let home = home("roundtrip");
        let store = ProfileStore::new(&home);
        let mut custom = profile("writer");
        custom.instructions = "第一行\n第二行\n".to_string();
        custom.color = Some(ProfileColor::Green);
        custom.model = ModelChoice::Inherit;
        std::fs::create_dir_all(store.user_dir()).unwrap();
        let text = serialize_definition(&custom).unwrap();
        assert!(text.starts_with("---\n"));
        let path = store.user_dir().join("writer.md");
        std::fs::write(&path, text).unwrap();
        let (parsed, _revision) = parse_definition_file(&path).unwrap();
        assert_eq!(parsed.instructions, "第一行\n第二行\n");
        assert_eq!(parsed.color, Some(ProfileColor::Green));
        assert_eq!(parsed.id, "writer");
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn missing_project_root_cannot_write_project_scope() {
        let home = home("noproject");
        let store = ProfileStore::new(&home);
        let error = store
            .create(
                ProfileWriteScope::Project,
                None,
                profile("x1"),
                String::new(),
            )
            .unwrap_err();
        assert_eq!(error.code, codes::SCOPE_FORBIDDEN);
        std::fs::remove_dir_all(home).unwrap();
    }
}
