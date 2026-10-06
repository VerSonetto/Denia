//! 子代理定义仓库：内置集合 + 用户目录 + 项目目录，同 id 后者覆盖前者。
//!
//! 磁盘格式是严格 YAML frontmatter + Markdown 正文（正文即 `instructions`）。
//! 三个作用域各自只有一个写入口，没有第二条绕开 revision 的写路径：
//!
//! - 内置（程序资源）：不可删、可禁用、可复制；"编辑内置"写的是用户覆盖，
//!   "恢复默认"删掉覆盖；
//! - 用户级 `$DENIA_HOME/subagents/<id>.md`；
//! - 项目级 `<projectRoot>/.denia/subagents/<id>.md`。
//!
//! 合并后每个逻辑 id 只有一行**有效定义**。高优先级定义损坏或被禁用时，
//! 该 id 整体不可派遣——绝不回落到低优先级同名项偷偷启用一份被遮蔽的配置。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use denia_core::subagent::{
    DEFAULT_SUBAGENT_ID, PermissionCeiling, SubagentDiagnostic, SubagentProfile,
    SubagentProfileSource, SUBAGENT_SCHEMA_VERSION, builtin_profiles, is_valid_subagent_id,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 用户级子代理目录名（位于 denia 数据目录下）。
pub const DIR_NAME: &str = "subagents";
/// 项目级子代理目录（位于项目根之下）。
pub const PROJECT_DIR: &str = ".denia/subagents";
/// 定义文件后缀。
pub const FILE_SUFFIX: &str = ".md";

/// 派遣解析失败：稳定错误码 + 字段 + 可执行原因 + 可用候选。
#[derive(Debug, Clone, PartialEq)]
pub struct SubagentError {
    pub code: String,
    pub field: Option<String>,
    pub reason: String,
    pub candidates: Vec<String>,
}

impl SubagentError {
    pub fn new(code: &str, reason: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            field: None,
            reason: reason.into(),
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
}

impl std::fmt::Display for SubagentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.reason)?;
        if let Some(field) = &self.field {
            write!(f, "（字段：{field}）")?;
        }
        if !self.candidates.is_empty() {
            write!(f, "；可用候选：{}", self.candidates.join("、"))?;
        }
        Ok(())
    }
}

impl std::error::Error for SubagentError {}

/// 名册里的一行：某个作用域下的一份定义（不论是否有效）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileRow {
    /// `builtin:explore` / `user:api-reviewer` / `project:api-reviewer`。
    pub qualified_id: String,
    pub id: String,
    pub source: SubagentProfileSource,
    /// 内容版本；外部编辑文件同样会让旧 revision 失效。
    pub revision: String,
    /// 该作用域的定义是否可写（内置定义可写用户覆盖，见 `overridesBuiltin`）。
    pub editable: bool,
    /// 内置 id 在用户/项目作用域存在覆盖。
    pub overrides_builtin: bool,
    /// 项目覆盖了同名用户定义。
    pub overrides_user: bool,
    /// 是否是本逻辑 id 的有效定义。
    pub effective: bool,
    /// 被同名高优先级定义遮蔽（可在管理页查看/复制，不可直接派遣）。
    pub shadowed: bool,
    /// 健康行才有定义；损坏行为 `None`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<SubagentProfile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broken: Option<String>,
    pub diagnostics: Vec<SubagentDiagnostic>,
}

impl ProfileRow {
    pub fn enabled(&self) -> bool {
        self.profile.as_ref().is_some_and(|profile| profile.enabled)
    }
}

/// 磁盘上的 frontmatter；未知键直接拒绝（拼错的键默默不生效比解析失败更难查）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProfileFile {
    schema_version: u32,
    id: String,
    name: String,
    description: String,
    #[serde(default = "default_true")]
    enabled: bool,
    tools: denia_core::subagent::ToolChoice,
    model: denia_core::subagent::ModelChoice,
    #[serde(default)]
    permission_ceiling: PermissionCeiling,
    #[serde(default)]
    color: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProfileFileOut<'a> {
    schema_version: u32,
    id: &'a str,
    name: &'a str,
    description: &'a str,
    enabled: bool,
    tools: &'a denia_core::subagent::ToolChoice,
    model: &'a denia_core::subagent::ModelChoice,
    permission_ceiling: PermissionCeiling,
    #[serde(skip_serializing_if = "Option::is_none")]
    color: Option<&'a str>,
}

fn default_true() -> bool {
    true
}

/// 子代理定义仓库；按项目根分别解析，用户级定义对所有项目可见。
pub struct SubagentProfileStore {
    home: PathBuf,
    /// 部署已注册的工具名（server 组装完注册表后注入）；`None` = 未注入，
    /// 工具名校验跳过（`mcp__*` 前缀的动态工具始终豁免）。
    known_tools: ArcSwap<Option<Arc<BTreeSet<String>>>>,
    /// 根 → 名册；`refresh()` 清空（外部编辑与 CRUD 都走它）。
    cache: std::sync::Mutex<BTreeMap<PathBuf, Arc<Vec<ProfileRow>>>>,
}

impl SubagentProfileStore {
    pub fn load(home: &Path) -> Arc<Self> {
        Arc::new(Self {
            home: home.to_path_buf(),
            known_tools: ArcSwap::from_pointee(None),
            cache: std::sync::Mutex::new(BTreeMap::new()),
        })
    }

    /// 注入部署已注册的工具名并清缓存。
    pub fn set_known_tools(&self, names: impl IntoIterator<Item = String>) {
        self.known_tools
            .store(Arc::new(Some(Arc::new(names.into_iter().collect()))));
        self.refresh();
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    /// 用户级定义目录。
    pub fn user_root(&self) -> PathBuf {
        self.home.join(DIR_NAME)
    }

    /// 项目级定义目录；`None` = 未解析出项目根。
    pub fn project_root(&self, project_root: Option<&Path>) -> Option<PathBuf> {
        project_root.map(|root| root.join(PROJECT_DIR))
    }

    pub fn refresh(&self) {
        self.cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }

    /// 某个项目根下的完整名册（含被遮蔽行）。结果按 (有效优先, source, id) 排序。
    pub fn rows_for(&self, project_root: Option<&Path>) -> Arc<Vec<ProfileRow>> {
        let key = project_root.map(Path::to_path_buf).unwrap_or_default();
        if let Some(hit) = self
            .cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
        {
            return hit.clone();
        }
        let rows = Arc::new(self.build(project_root));
        self.cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key, rows.clone());
        rows
    }

    fn build(&self, project_root: Option<&Path>) -> Vec<ProfileRow> {
        let known = self.known_tools.load_full();
        let known = known.as_deref();
        let mut rows: Vec<ProfileRow> = Vec::new();
        for profile in builtin_profiles() {
            let diagnostics = profile.validate();
            rows.push(row_from_profile(
                profile,
                SubagentProfileSource::Builtin,
                None,
                diagnostics,
            ));
        }
        let user_root = self.user_root();
        let user_rows = read_scope(&user_root, SubagentProfileSource::User, known);
        rows.extend(user_rows);
        if let Some(project_root) = project_root {
            rows.extend(read_scope(
                &project_root.join(PROJECT_DIR),
                SubagentProfileSource::Project,
                known,
            ));
        }
        // 有效定义：同 id 取合并顺序里最后出现的那一行（builtin→user→project）。
        let mut effective: BTreeMap<String, usize> = BTreeMap::new();
        for (index, row) in rows.iter().enumerate() {
            effective.insert(row.id.clone(), index);
        }
        let overridden_builtin: BTreeSet<String> = rows
            .iter()
            .filter(|row| row.source != SubagentProfileSource::Builtin)
            .map(|row| row.id.clone())
            .collect();
        let overridden_user: BTreeSet<String> = rows
            .iter()
            .filter(|row| row.source == SubagentProfileSource::Project)
            .map(|row| row.id.clone())
            .collect();
        for (index, row) in rows.iter_mut().enumerate() {
            let is_effective = effective.get(&row.id) == Some(&index);
            row.effective = is_effective;
            row.shadowed = !is_effective;
            row.overrides_builtin = overridden_builtin.contains(&row.id)
                && row.source == SubagentProfileSource::User;
            row.overrides_user = overridden_user.contains(&row.id)
                && row.source == SubagentProfileSource::User;
            row.editable = true;
            if row.source == SubagentProfileSource::Builtin && overridden_builtin.contains(&row.id)
            {
                // 内置行被覆盖时，编辑动作落用户覆盖、删除动作针对覆盖。
                row.editable = true;
                row.overrides_builtin = true;
            }
        }
        // 有效行在前，其余按 source→id 稳定排序。
        rows.sort_by(|a, b| {
            b.effective
                .cmp(&a.effective)
                .then(a.source.cmp(&b.source))
                .then(a.id.cmp(&b.id))
        });
        rows
    }

    /// 模型可见的定义目录：只含每个逻辑 id 的有效定义，且必须健康、启用。
    pub fn catalog(&self, project_root: Option<&Path>) -> Vec<CatalogEntry> {
        self.rows_for(project_root)
            .iter()
            .filter(|row| row.effective && row.enabled())
            .filter_map(|row| {
                let profile = row.profile.as_ref()?;
                Some(CatalogEntry {
                    qualified_id: row.qualified_id.clone(),
                    name: profile.name.clone(),
                    description: profile.description.clone(),
                    tools: tools_summary(&profile.tools),
                    model: model_summary(profile),
                })
            })
            .collect()
    }

    /// 按 qualifiedId 或逻辑 id 解析一份可派遣定义。
    ///
    /// 遮蔽行（`user:x` 被 `project:x` 覆盖）不提供给派遣，返回
    /// `subagent/profile-shadowed` 并指出有效项；禁用与损坏同样拒绝，
    /// 不回落到下层同名定义。
    pub fn resolve(
        &self,
        target: &str,
        project_root: Option<&Path>,
    ) -> Result<ResolvedProfile, SubagentError> {
        let rows = self.rows_for(project_root);
        let (source, id) = match target.split_once(':') {
            Some((prefix, id))
                if matches!(prefix, "builtin" | "user" | "project") && !id.is_empty() =>
            {
                let source = match prefix {
                    "builtin" => SubagentProfileSource::Builtin,
                    "user" => SubagentProfileSource::User,
                    _ => SubagentProfileSource::Project,
                };
                (Some(source), id.to_string())
            }
            _ => (None, target.to_string()),
        };
        if !is_valid_subagent_id(&id) {
            return Err(SubagentError::new(
                "subagent/invalid-profile-id",
                format!("非法的子代理定义 id：{id}"),
            )
            .field("profileId")
            .candidates(self.available_ids(&rows)));
        }
        if source == Some(SubagentProfileSource::Project) && project_root.is_none() {
            return Err(SubagentError::new(
                "subagent/project-scope-unavailable",
                "当前会话解析不出项目根，无法使用项目级定义",
            )
            .field("profileId"));
        }
        let candidates = self.available_ids(&rows);
        let row = match source {
            Some(source) => rows
                .iter()
                .find(|row| row.id == id && row.source == source)
                .ok_or_else(|| {
                    SubagentError::new(
                        "subagent/profile-not-found",
                        format!("未找到定义：{}:{id}", source.as_str()),
                    )
                    .field("profileId")
                    .candidates(candidates.clone())
                })?,
            None => rows
                .iter()
                .find(|row| row.id == id && row.effective)
                .ok_or_else(|| {
                    SubagentError::new(
                        "subagent/profile-not-found",
                        format!("未找到子代理定义：{id}"),
                    )
                    .field("profileId")
                    .candidates(candidates.clone())
                })?,
        };
        if row.shadowed {
            let effective = rows.iter().find(|row| row.id == id && row.effective);
            return Err(SubagentError::new(
                "subagent/profile-shadowed",
                format!(
                    "定义 {} 已被更高优先级定义覆盖，不能直接派遣",
                    row.qualified_id
                ),
            )
            .field("profileId")
            .candidates(
                effective
                    .map(|row| vec![row.qualified_id.clone()])
                    .unwrap_or(candidates),
            ));
        }
        let Some(profile) = row.profile.clone() else {
            return Err(SubagentError::new(
                "subagent/profile-broken",
                format!(
                    "定义 {} 无法解析，先修好它再派遣：{}",
                    row.qualified_id,
                    row.broken.as_deref().unwrap_or("未知原因")
                ),
            )
            .field("profileId"));
        };
        if !profile.enabled {
            return Err(SubagentError::new(
                "subagent/profile-disabled",
                format!("定义 {} 已被禁用，请选择其他类型或先启用它", row.qualified_id),
            )
            .field("profileId")
            .candidates(candidates));
        }
        let diagnostics = profile.validate();
        if !diagnostics.is_empty() {
            return Err(SubagentError::new(
                "subagent/profile-invalid",
                format!(
                    "定义 {} 校验不通过：{}",
                    row.qualified_id, diagnostics[0].reason
                ),
            )
            .field("profileId"));
        }
        Ok(ResolvedProfile {
            profile,
            qualified_id: row.qualified_id.clone(),
            source: row.source,
            revision: row.revision.clone(),
        })
    }

    /// 未指定定义时的默认预设（有效 develop）。
    pub fn resolve_default(
        &self,
        project_root: Option<&Path>,
    ) -> Result<ResolvedProfile, SubagentError> {
        self.resolve(DEFAULT_SUBAGENT_ID, project_root).map_err(|error| {
            SubagentError::new(&error.code, format!(
                "默认子代理定义 `{DEFAULT_SUBAGENT_ID}` 当前不可用（{}）；请显式选择 profileId 或提供 inline 定义",
                error.reason
            ))
            .field("profileId")
            .candidates(error.candidates)
        })
    }

    fn available_ids(&self, rows: &[ProfileRow]) -> Vec<String> {
        rows.iter()
            .filter(|row| row.effective && row.enabled())
            .map(|row| row.qualified_id.clone())
            .collect()
    }

    /// 管理页详情：定义 + 磁盘原文（用户/项目定义）。
    pub fn describe_text(
        &self,
        qualified_id: &str,
        project_root: Option<&Path>,
    ) -> Result<String, SubagentError> {
        let rows = self.rows_for(project_root);
        let row = rows
            .iter()
            .find(|row| row.qualified_id == qualified_id)
            .ok_or_else(|| {
                SubagentError::new(
                    "subagent/profile-not-found",
                    format!("未找到定义：{qualified_id}"),
                )
            })?;
        match &row.path {
            Some(path) => std::fs::read_to_string(path).map_err(|error| {
                SubagentError::new(
                    "subagent/profile-unreadable",
                    format!("读取定义文件失败：{error}"),
                )
            }),
            None => row
                .profile
                .as_ref()
                .map(render_file)
                .ok_or_else(|| {
                    SubagentError::new(
                        "subagent/profile-broken",
                        row.broken.clone().unwrap_or_else(|| "定义无法解析".into()),
                    )
                }),
        }
    }

    /// 创建或覆盖一份定义（写入口唯一）。
    ///
    /// `scope` 只允许 user/project；内置定义另走 [`Self::update`] 的覆盖语义。
    pub fn write(
        &self,
        scope: SubagentProfileSource,
        profile: &SubagentProfile,
        project_root: Option<&Path>,
        expected_revision: Option<&str>,
    ) -> Result<ProfileRow, SubagentError> {
        if scope == SubagentProfileSource::Builtin {
            return Err(SubagentError::new(
                "subagent/builtin-read-only",
                "内置定义的程序资源不可写：请写用户覆盖或复制成自定义定义",
            ));
        }
        let diagnostics = profile.validate();
        if !diagnostics.is_empty() {
            return Err(SubagentError::new(
                "subagent/profile-invalid",
                diagnostics[0].reason.clone(),
            )
            .field(diagnostics[0].field.as_deref().unwrap_or("profile")));
        }
        let root = self.scope_root(scope, project_root)?;
        let existing = self
            .rows_for(project_root)
            .iter()
            .find(|row| row.id == profile.id && row.source == scope)
            .cloned();
        match (&existing, expected_revision) {
            (Some(row), Some(expected)) if row.revision != expected => {
                return Err(SubagentError::new(
                    "subagent/revision-conflict",
                    format!(
                        "定义 {} 已被其他窗口或外部编辑修改（期望 {}，实际 {}）",
                        row.qualified_id, expected, row.revision
                    ),
                )
                .field("expectedRevision"));
            }
            (Some(_), None) => {
                return Err(SubagentError::new(
                    "subagent/revision-required",
                    "覆盖已有定义必须带 expectedRevision",
                )
                .field("expectedRevision"));
            }
            _ => {}
        }
        let path = self.definition_path(&root, &profile.id)?;
        write_definition(&path, profile)?;
        self.refresh();
        self.rows_for(project_root)
            .iter()
            .find(|row| row.id == profile.id && row.source == scope)
            .cloned()
            .ok_or_else(|| {
                SubagentError::new(
                    "subagent/profile-write-unverified",
                    "定义已写入但名册里读不回来，请刷新后重试",
                )
            })
    }

    /// 删除某个作用域下的定义（等价于删除覆盖）；返回删除后生效的定义描述。
    pub fn remove(
        &self,
        scope: SubagentProfileSource,
        id: &str,
        project_root: Option<&Path>,
        expected_revision: Option<&str>,
    ) -> Result<Option<ProfileRow>, SubagentError> {
        if scope == SubagentProfileSource::Builtin {
            return Err(SubagentError::new(
                "subagent/builtin-read-only",
                "内置定义不可删除：可禁用它、复制成自定义定义，或删除用户/项目覆盖",
            ));
        }
        let root = self.scope_root(scope, project_root)?;
        let existing = self
            .rows_for(project_root)
            .iter()
            .find(|row| row.id == id && row.source == scope)
            .cloned()
            .ok_or_else(|| {
                SubagentError::new(
                    "subagent/profile-not-found",
                    format!("{} 作用域下没有定义：{id}", scope.as_str()),
                )
            })?;
        if let Some(expected) = expected_revision
            && existing.revision != expected
        {
            return Err(SubagentError::new(
                "subagent/revision-conflict",
                format!(
                    "定义 {} 已被其他窗口或外部编辑修改（期望 {}，实际 {}）",
                    existing.qualified_id, expected, existing.revision
                ),
            )
            .field("expectedRevision"));
        }
        let path = self.definition_path(&root, id)?;
        if path.exists() {
            std::fs::remove_file(&path).map_err(|error| {
                SubagentError::new(
                    "subagent/profile-remove-failed",
                    format!("删除定义文件失败：{error}"),
                )
            })?;
        }
        self.refresh();
        Ok(self
            .rows_for(project_root)
            .iter()
            .find(|row| row.id == id && row.effective)
            .cloned())
    }

    /// 复制一份定义到指定作用域：整个定义的显式副本，不继承来源的写权限状态。
    pub fn copy(
        &self,
        qualified_id: &str,
        scope: SubagentProfileSource,
        new_id: &str,
        new_name: Option<&str>,
        project_root: Option<&Path>,
    ) -> Result<ProfileRow, SubagentError> {
        if !is_valid_subagent_id(new_id) {
            return Err(SubagentError::new(
                "subagent/invalid-id",
                format!("非法的定义 id：{new_id}（只允许小写字母、数字与连字符）"),
            )
            .field("id"));
        }
        let rows = self.rows_for(project_root);
        if rows.iter().any(|row| row.id == new_id && row.source == scope) {
            return Err(SubagentError::new(
                "subagent/profile-exists",
                format!("{} 作用域下已存在同名定义：{new_id}", scope.as_str()),
            )
            .field("id"));
        }
        let source = rows
            .iter()
            .find(|row| row.qualified_id == qualified_id)
            .ok_or_else(|| {
                SubagentError::new(
                    "subagent/profile-not-found",
                    format!("未找到定义：{qualified_id}"),
                )
            })?;
        let mut profile = source.profile.clone().ok_or_else(|| {
            SubagentError::new(
                "subagent/profile-broken",
                format!("来源定义无法解析：{qualified_id}"),
            )
        })?;
        profile.id = new_id.to_string();
        if let Some(name) = new_name.map(str::trim).filter(|name| !name.is_empty()) {
            profile.name = name.to_string();
        }
        profile.enabled = true;
        self.write(scope, &profile, project_root, None)
    }

    fn scope_root(
        &self,
        scope: SubagentProfileSource,
        project_root: Option<&Path>,
    ) -> Result<PathBuf, SubagentError> {
        match scope {
            SubagentProfileSource::User => Ok(self.user_root()),
            SubagentProfileSource::Project => {
                let root = project_root.ok_or_else(|| {
                    SubagentError::new(
                        "subagent/project-scope-unavailable",
                        "当前会话解析不出项目根，无法写项目级定义",
                    )
                })?;
                Ok(root.join(PROJECT_DIR))
            }
            SubagentProfileSource::Builtin => Err(SubagentError::new(
                "subagent/builtin-read-only",
                "内置定义的资源目录不可写",
            )),
        }
    }

    /// 定义文件路径：id 已过 slug 语法白名单，再做目录逃逸与符号链接落点校验。
    fn definition_path(&self, root: &Path, id: &str) -> Result<PathBuf, SubagentError> {
        if !is_valid_subagent_id(id) {
            return Err(SubagentError::new(
                "subagent/invalid-id",
                format!("非法的定义 id：{id}"),
            )
            .field("id"));
        }
        std::fs::create_dir_all(root).map_err(|error| {
            SubagentError::new(
                "subagent/profile-write-failed",
                format!("创建定义目录失败：{error}"),
            )
        })?;
        let canonical_root = root.canonicalize().map_err(|error| {
            SubagentError::new(
                "subagent/profile-write-failed",
                format!("解析定义目录失败：{error}"),
            )
        })?;
        let path = canonical_root.join(format!("{id}{FILE_SUFFIX}"));
        if path.exists() {
            let resolved = path.canonicalize().map_err(|error| {
                SubagentError::new(
                    "subagent/profile-write-failed",
                    format!("解析定义文件失败：{error}"),
                )
            })?;
            if !resolved.starts_with(&canonical_root) {
                return Err(SubagentError::new(
                    "subagent/path-escape",
                    format!("定义文件落在定义目录之外：{}", resolved.display()),
                )
                .field("id"));
            }
        }
        Ok(path)
    }
}

/// 模型可见目录里的一行：只给挑选所需的信息，不含 instructions 全文。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    pub qualified_id: String,
    pub name: String,
    pub description: String,
    pub tools: String,
    pub model: String,
}

/// 解析结果：可执行的定义 + 来源身份。
#[derive(Debug, Clone)]
pub struct ResolvedProfile {
    pub profile: SubagentProfile,
    pub qualified_id: String,
    pub source: SubagentProfileSource,
    pub revision: String,
}

/// 目录里的工具摘要：只说模式与数量，不铺开全部名字。
fn tools_summary(tools: &denia_core::subagent::ToolChoice) -> String {
    match tools {
        denia_core::subagent::ToolChoice::Inherit => "继承父代理可授予工具".to_string(),
        denia_core::subagent::ToolChoice::Allowlist { names } => {
            if names.is_empty() {
                "无工具".to_string()
            } else {
                names.join("、")
            }
        }
    }
}

fn model_summary(profile: &SubagentProfile) -> String {
    match &profile.model {
        denia_core::subagent::ModelChoice::Inherit => "继承父代理模型".to_string(),
        denia_core::subagent::ModelChoice::Explicit { selection } => {
            match &selection.reasoning_effort {
                Some(effort) => format!("{}/{} ({effort})", selection.provider, selection.model),
                None => format!("{}/{}", selection.provider, selection.model),
            }
        }
    }
}

fn row_from_profile(
    profile: SubagentProfile,
    source: SubagentProfileSource,
    path: Option<&Path>,
    diagnostics: Vec<SubagentDiagnostic>,
) -> ProfileRow {
    let revision = revision_of(&profile);
    ProfileRow {
        qualified_id: format!("{}:{}", source.prefix(), profile.id),
        id: profile.id.clone(),
        source,
        revision,
        editable: true,
        overrides_builtin: false,
        overrides_user: false,
        effective: false,
        shadowed: false,
        profile: Some(profile),
        path: path.map(|path| path.display().to_string()),
        broken: None,
        diagnostics,
    }
}

fn broken_row(
    id: &str,
    source: SubagentProfileSource,
    path: Option<&Path>,
    reason: impl Into<String>,
) -> ProfileRow {
    let reason = reason.into();
    ProfileRow {
        qualified_id: format!("{}:{id}", source.prefix()),
        id: id.to_string(),
        source,
        revision: String::new(),
        editable: true,
        overrides_builtin: false,
        overrides_user: false,
        effective: false,
        shadowed: false,
        profile: None,
        path: path.map(|path| path.display().to_string()),
        broken: Some(reason.clone()),
        diagnostics: vec![SubagentDiagnostic::new(
            "subagent/profile-broken",
            None,
            reason,
        )],
    }
}

/// 读一个作用域目录；文件损坏保留诊断而不是隐去。
fn read_scope(
    root: &Path,
    source: SubagentProfileSource,
    known_tools: Option<&BTreeSet<String>>,
) -> Vec<ProfileRow> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut rows: Vec<ProfileRow> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(id) = file_name.strip_suffix(FILE_SUFFIX) else {
            continue;
        };
        if !is_valid_subagent_id(id) {
            rows.push(broken_row(
                id,
                source,
                Some(&path),
                format!("文件名不是合法的定义 id（小写字母、数字与连字符）：{file_name}"),
            ));
            continue;
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) => {
                rows.push(broken_row(
                    id,
                    source,
                    Some(&path),
                    format!("读取定义文件失败：{error}"),
                ));
                continue;
            }
        };
        match parse_file(&raw) {
            Ok(mut profile) => {
                if profile.id != id {
                    rows.push(broken_row(
                        id,
                        source,
                        Some(&path),
                        format!("定义里的 id（{}）与文件名不一致", profile.id),
                    ));
                    continue;
                }
                let mut diagnostics = profile.validate();
                if let Some(unknown) = unknown_tools(&profile, known_tools) {
                    diagnostics.push(SubagentDiagnostic::new(
                        "subagent/unknown-tool",
                        Some("tools"),
                        format!("引用了当前部署不存在的工具：{}", unknown.join("、")),
                    ));
                }
                // 文件名即身份，正文里的 id 与文件名一致才继续。
                profile.id = id.to_string();
                rows.push(row_from_profile(profile, source, Some(&path), diagnostics));
            }
            Err(reason) => rows.push(broken_row(id, source, Some(&path), reason)),
        }
    }
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    rows
}

/// 白名单里引用了部署不存在的工具；`mcp__*` 前缀是动态工具，不校验。
fn unknown_tools(
    profile: &SubagentProfile,
    known_tools: Option<&BTreeSet<String>>,
) -> Option<Vec<String>> {
    let known = known_tools?;
    let names = profile.tools.allowlist()?;
    let unknown: Vec<String> = names
        .iter()
        .filter(|name| !name.starts_with("mcp__") && !known.contains(name.as_str()))
        .cloned()
        .collect();
    (!unknown.is_empty()).then_some(unknown)
}

/// 解析 frontmatter + 正文；任何结构问题都返回可执行原因。
pub fn parse_file(raw: &str) -> Result<SubagentProfile, String> {
    let text = raw.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let rest = text
        .strip_prefix("---\n")
        .ok_or("定义文件必须以 `---` 起始的 YAML frontmatter 开头")?;
    let (front, body) = rest
        .split_once("\n---")
        .ok_or("frontmatter 没有闭合的 `---` 行")?;
    let body = body.strip_prefix('\n').unwrap_or(body);
    let file: ProfileFile = serde_yaml::from_str(front)
        .map_err(|error| format!("frontmatter 解析失败（未知字段会被拒绝）：{error}"))?;
    Ok(SubagentProfile {
        schema_version: file.schema_version,
        id: file.id,
        name: file.name,
        description: file.description,
        instructions: body.trim_end_matches('\n').to_string(),
        enabled: file.enabled,
        tools: file.tools,
        model: file.model,
        permission_ceiling: file.permission_ceiling,
        color: file.color,
    })
}

/// 渲染定义文件全文（内置定义的"查看原文"与复制落盘共用一份口径）。
pub fn render_file(profile: &SubagentProfile) -> String {
    let front = ProfileFileOut {
        schema_version: if profile.schema_version == 0 {
            SUBAGENT_SCHEMA_VERSION
        } else {
            profile.schema_version
        },
        id: &profile.id,
        name: &profile.name,
        description: &profile.description,
        enabled: profile.enabled,
        tools: &profile.tools,
        model: &profile.model,
        permission_ceiling: profile.permission_ceiling,
        color: profile.color.as_deref(),
    };
    let yaml = serde_yaml::to_string(&front).unwrap_or_default();
    format!("---\n{}---\n{}\n", yaml, profile.instructions)
}

/// 同目录临时文件 + 原子替换；写的是 UTF-8 明文，外部编辑可随时接管。
fn write_definition(path: &Path, profile: &SubagentProfile) -> Result<(), SubagentError> {
    let parent = path.parent().ok_or_else(|| {
        SubagentError::new("subagent/profile-write-failed", "定义文件路径没有父目录")
    })?;
    std::fs::create_dir_all(parent).map_err(|error| {
        SubagentError::new(
            "subagent/profile-write-failed",
            format!("创建定义目录失败：{error}"),
        )
    })?;
    let temp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("subagent"),
        uuid::Uuid::new_v4()
    ));
    let rendered = render_file(profile);
    std::fs::write(&temp, rendered.as_bytes()).map_err(|error| {
        SubagentError::new(
            "subagent/profile-write-failed",
            format!("写入定义临时文件失败：{error}"),
        )
    })?;
    if let Err(error) = replace_file(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(SubagentError::new(
            "subagent/profile-write-failed",
            format!("原子替换定义文件失败：{error}"),
        ));
    }
    Ok(())
}

/// 平台适配的原子替换：Windows 上 `rename` 不能覆盖既有文件。
fn replace_file(temp: &Path, target: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        if target.exists() {
            // 先删后改名不是原子操作，但保留旧文件的"改名覆盖"在 Windows
            // 上根本不存在；同目录改名保证内容完整，宁可短暂缺文件也不写半截。
            std::fs::remove_file(target)?;
        }
    }
    std::fs::rename(temp, target)
}

/// 内容版本：定义规范化 JSON 的 SHA-256 前 16 位十六进制。
///
/// 只覆盖"会改变执行语义"的字段——空格与注释不参与，外部编辑器改内容
/// 也会换出一个新版本，旧 revision 的写入随之失效。
pub fn revision_of(profile: &SubagentProfile) -> String {
    let canonical = serde_json::json!({
        "schemaVersion": profile.schema_version,
        "id": profile.id,
        "name": profile.name,
        "description": profile.description,
        "instructions": profile.instructions,
        "enabled": profile.enabled,
        "tools": profile.tools,
        "model": profile.model,
        "permissionCeiling": profile.permission_ceiling,
        "color": profile.color,
    });
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string().as_bytes());
    let digest = hasher.finalize();
    digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::subagent::{ModelChoice, ToolChoice};

    fn temp_home(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "denia-subagent-profiles-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn custom(id: &str, tools: Vec<&str>) -> SubagentProfile {
        SubagentProfile {
            schema_version: SUBAGENT_SCHEMA_VERSION,
            id: id.to_string(),
            name: format!("定义 {id}"),
            description: "测试用定义".to_string(),
            instructions: "只做测试。".to_string(),
            enabled: true,
            tools: ToolChoice::Allowlist {
                names: tools.into_iter().map(str::to_string).collect(),
            },
            model: ModelChoice::Inherit,
            permission_ceiling: PermissionCeiling::Inherit,
            color: None,
        }
    }

    #[test]
    fn builtins_are_listed_and_resolvable() {
        let home = temp_home("builtins");
        let store = SubagentProfileStore::load(&home);
        let rows = store.rows_for(None);
        for id in ["explore", "develop", "verify"] {
            let row = rows
                .iter()
                .find(|row| row.qualified_id == format!("builtin:{id}"))
                .expect("内置定义必须存在");
            assert!(row.effective && !row.shadowed);
            assert!(store.resolve(id, None).is_ok());
            assert!(store.resolve(&format!("builtin:{id}"), None).is_ok());
        }
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn project_overrides_user_and_shadowed_qualified_id_is_rejected() {
        let home = temp_home("shadow");
        let project = temp_home("project");
        let store = SubagentProfileStore::load(&home);
        store
            .write(
                SubagentProfileSource::User,
                &custom("api-reviewer", vec!["read_file"]),
                Some(&project),
                None,
            )
            .unwrap();
        store
            .write(
                SubagentProfileSource::Project,
                &custom("api-reviewer", vec!["read_file", "bash"]),
                Some(&project),
                None,
            )
            .unwrap();
        let effective = store.resolve("api-reviewer", Some(&project)).unwrap();
        assert_eq!(effective.source, SubagentProfileSource::Project);
        let error = store
            .resolve("user:api-reviewer", Some(&project))
            .unwrap_err();
        assert_eq!(error.code, "subagent/profile-shadowed");
        assert_eq!(error.candidates, vec!["project:api-reviewer".to_string()]);
        // 不带项目根时用户定义仍是有效项。
        let user_only = store.resolve("api-reviewer", None).unwrap();
        assert_eq!(user_only.source, SubagentProfileSource::User);
        std::fs::remove_dir_all(home).unwrap();
        std::fs::remove_dir_all(project).unwrap();
    }

    #[test]
    fn disabled_high_priority_definition_does_not_fall_back() {
        let home = temp_home("disabled");
        let store = SubagentProfileStore::load(&home);
        let mut profile = custom("explore", vec!["read_file"]);
        profile.enabled = false;
        store
            .write(SubagentProfileSource::User, &profile, None, None)
            .unwrap();
        let error = store.resolve("explore", None).unwrap_err();
        assert_eq!(error.code, "subagent/profile-disabled");
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn broken_file_keeps_diagnostics_and_blocks_dispatch() {
        let home = temp_home("broken");
        let store = SubagentProfileStore::load(&home);
        let root = store.user_root();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("bad.md"), "---\nid: bad\nnope: 1\n---\n正文\n").unwrap();
        let row = store
            .rows_for(None)
            .iter()
            .find(|row| row.qualified_id == "user:bad")
            .cloned()
            .expect("损坏的定义必须仍然列出来");
        assert!(row.profile.is_none());
        assert!(row.broken.is_some());
        assert_eq!(
            store.resolve("bad", None).unwrap_err().code,
            "subagent/profile-broken"
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn revision_conflict_is_reported_and_unknown_fields_rejected() {
        let home = temp_home("revision");
        let store = SubagentProfileStore::load(&home);
        let profile = custom("reviewer", vec!["read_file"]);
        let row = store
            .write(SubagentProfileSource::User, &profile, None, None)
            .unwrap();
        let error = store
            .write(
                SubagentProfileSource::User,
                &profile,
                None,
                Some("deadbeef"),
            )
            .unwrap_err();
        assert_eq!(error.code, "subagent/revision-conflict");
        // 带正确 revision 的更新成功，且 revision 随内容变化。
        let mut updated = profile.clone();
        updated.description = "改过的描述".to_string();
        let next = store
            .write(
                SubagentProfileSource::User,
                &updated,
                None,
                Some(&row.revision),
            )
            .unwrap();
        assert_ne!(next.revision, row.revision);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn file_round_trip_preserves_definition() {
        let profile = custom("round-trip", vec!["read_file", "bash"]);
        let text = render_file(&profile);
        let parsed = parse_file(&text).unwrap();
        assert_eq!(parsed, profile);
    }

    #[test]
    fn missing_authorization_list_in_legacy_log_is_not_inherit() {
        use denia_core::session::SubagentDescriptor;
        let descriptor = SubagentDescriptor {
            label: "旧子代理".into(),
            depth: 2,
            mode: "spawn".into(),
            selection: denia_core::config::ModelSelection {
                provider: "p".into(),
                model: "m".into(),
                reasoning_effort: None,
            },
            persona: None,
            allowed_tools: None,
            snapshot: None,
        };
        let tools = descriptor.effective_tools();
        assert!(tools.contains(&"read_file".to_string()));
        assert!(!tools.contains(&"bash".to_string()));
        assert!(!tools.contains(&"write_file".to_string()));
        assert_eq!(
            descriptor.permission_ceiling(),
            PermissionCeiling::ReadOnly
        );
    }

    #[test]
    fn explicit_legacy_tools_are_intersected_with_the_historical_ceiling() {
        use denia_core::session::SubagentDescriptor;
        let descriptor = SubagentDescriptor {
            label: "旧子代理".into(),
            depth: 1,
            mode: "spawn".into(),
            selection: denia_core::config::ModelSelection {
                provider: "p".into(),
                model: "m".into(),
                reasoning_effort: None,
            },
            persona: None,
            allowed_tools: Some(vec![
                "read_file".into(),
                "write_file".into(),
                "bash".into(),
                "spawn_agent".into(),
            ]),
            snapshot: None,
        };
        let tools = descriptor.effective_tools();
        assert!(tools.contains(&"read_file".to_string()));
        assert!(tools.contains(&"write_file".to_string()));
        assert!(!tools.contains(&"bash".to_string()), "bash 超出历史上限");
        assert!(!tools.contains(&"spawn_agent".to_string()), "派遣工具必须被扣掉");
    }
}
