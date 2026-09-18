//! Agent preset 名册:随部署提供的组装 + 用户目录里的自定义组装。
//!
//! 与 dsh 的 `agent-presets` 同构,但 denia 的 preset 是**声明式**的:一份
//! `preset.yml` 说明该组装给会话哪些工具、用什么 persona(denia 没有 Cordis
//! 插件子树可挂)。名册合并两个根:
//!
//! - 随附集合(内置代码,`trust: shipped`,不可写、不可删);
//! - 用户根 `<denia home>/agent-presets/<id>/preset.yml`(可复制创作、可删)。
//!
//! 创作即复制:调用方从不提交组装文本,复制某个既有 preset 的整个目录,
//! 于是"一次复制不会授予名册尚未携带的任何能力"。文件就是编辑器——用户
//! 用自己的编辑器改 `preset.yml`,名册监听文件变化后热刷新。
//!
//! 损坏的 preset 不会被隐藏:它以带原因的 broken 行出现,让人看得见该修
//! 什么;会话端解析不到时回退到部署默认组装,不会因为一个坏文件发不出请求。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use denia_core::preset::{
    builtin_presets, is_valid_preset_id, AgentPreset, PresetFeatures, PresetSpec, PresetTrust,
    DEFAULT_PRESET_ID,
};
use denia_settings::SettingsStore;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};

use crate::state::ServerEvent;

/// 用户 preset 根目录名(位于 denia 数据目录下)。
pub const DIR_NAME: &str = "agent-presets";
/// 每个 preset 目录里的组装文件名。
pub const FILE_NAME: &str = "preset.yml";
/// 承载"默认 preset"的用户设置命名空间。
pub const SETTINGS_NS: &str = "agent-presets";

/// 名册里的一行:随附/用户 preset 的展示元数据,或一条 broken 记录。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresetRow {
    /// 健康行的完整定义(persona 正文按需读盘,名册不外发全文)。
    #[serde(skip)]
    def: Option<AgentPreset>,
    pub id: String,
    pub trust: &'static str,
    pub name: String,
    pub description: String,
    /// 工具白名单;`None` = 出厂全量工具集。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// 是否带 persona 覆盖(正文走详情接口,名册只报有无)。
    pub has_persona: bool,
    /// 可写行 = 用户根下的 preset,可删除、可被覆盖对照。
    pub writable: bool,
    /// 功能开关快照(前端徽章展示);broken 行按默认全开展示。
    pub features: PresetFeatures,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// 损坏原因;有值时该行无法组装,也不会被会话选中。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broken: Option<String>,
}

impl PresetRow {
    fn healthy(preset: &AgentPreset) -> Self {
        Self {
            def: Some(preset.clone()),
            id: preset.id.clone(),
            trust: preset.trust.as_str(),
            name: preset.name.clone(),
            description: preset.description.clone(),
            tools: preset.tools.clone(),
            has_persona: preset.persona.is_some(),
            writable: preset.trust == PresetTrust::User,
            features: preset.features,
            path: preset.path.clone(),
            broken: None,
        }
    }

    /// 一条无法组装的记录:保留 id 与原因,让人看得见该修什么。
    fn broken(id: &str, trust: PresetTrust, path: Option<&Path>, reason: impl Into<String>) -> Self {
        Self {
            def: None,
            id: id.to_string(),
            trust: trust.as_str(),
            name: id.to_string(),
            description: String::new(),
            tools: None,
            has_persona: false,
            writable: trust == PresetTrust::User,
            features: PresetFeatures::default(),
            path: path.map(|path| path.display().to_string()),
            broken: Some(reason.into()),
        }
    }
}

/// `preset.yml` 的磁盘格式。缺省字段都按"沿用更外层"解释:
/// 没有 `tools` 就是全量工具集,没有 `persona` 就沿用部署 persona,
/// 没有 `features` 就是全功能开启。未知键直接拒绝(fail loud)——一个
/// 拼写错的键默默不生效,比解析失败更难排查。
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PresetFile {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    tools: Option<Vec<String>>,
    #[serde(default)]
    persona: Option<String>,
    #[serde(default)]
    persona_complete: bool,
    #[serde(default)]
    features: PresetFeatures,
}

/// Agent preset 名册:随附集合 + 用户根目录,文件变化热刷新。
pub struct PresetStore {
    root: PathBuf,
    rows: ArcSwap<Vec<PresetRow>>,
    settings: Arc<SettingsStore>,
    /// 部署已注册的工具名(server 组装完注册表后注入;`None` = 尚未注入,
    /// 工具名校验跳过)。名册用它把"引用了不存在工具"的 preset 标成
    /// broken——白名单收窄是取交集,不校验的话拼错的工具名会被静默丢弃,
    /// 名册显示健康行而实际工具面悄悄缺工具。
    known_tools: ArcSwap<Option<Arc<BTreeSet<String>>>>,
}

impl PresetStore {
    /// 打开名册:随附集合立即可用,用户根目录按当前磁盘内容读一次。
    pub fn load(home: &Path, settings: Arc<SettingsStore>) -> Arc<Self> {
        let store = Arc::new(Self {
            root: home.join(DIR_NAME),
            rows: ArcSwap::from_pointee(Vec::new()),
            settings,
            known_tools: ArcSwap::from_pointee(None),
        });
        store.refresh();
        store
    }

    /// 注入部署已注册的工具名(MCP 动态工具不在此列,`mcp__` 前缀的引用
    /// 不校验)。注入后名册立即按它重读磁盘。
    pub fn set_known_tools(&self, names: impl IntoIterator<Item = String>) {
        self.known_tools
            .store(Arc::new(Some(Arc::new(names.into_iter().collect()))));
        self.refresh();
    }

    /// 用户 preset 根目录;首次复制创作时按需创建。
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn rows(&self) -> Arc<Vec<PresetRow>> {
        self.rows.load_full()
    }

    /// 解析一个 preset id;未知名与损坏行都返回 `None`。
    pub fn resolve(&self, id: &str) -> Option<AgentPreset> {
        let rows = self.rows.load();
        rows.iter().find(|row| row.id == id)?.def.clone()
    }

    /// 「允许切换 agent 模式」开关:设置里未写时默认开启。
    pub fn mode_selection_enabled(&self) -> bool {
        self.settings
            .resolved(SETTINGS_NS)
            .ok()
            .and_then(|value| {
                value
                    .get("modeSelectionEnabled")
                    .and_then(|value| value.as_bool())
            })
            .unwrap_or(true)
    }

    /// 部署默认 preset id:读用户设置,校验它仍是一个健康行;否则回退
    /// 随附默认值。默认值在每次解析时读取,绝不缓存成快照——否则用户在
    /// 设置里改完默认,已存在的空白会话会与新会话各说各话。
    /// 模式选择关闭时忽略用户保存的 default、回落部署默认(对齐 dsh
    /// selectionPolicy:选择面被隐藏时,不该有用户无法看见的默认在治下)。
    pub fn default_id(&self) -> String {
        if !self.mode_selection_enabled() {
            return DEFAULT_PRESET_ID.to_string();
        }
        let configured = self
            .settings
            .resolved(SETTINGS_NS)
            .ok()
            .and_then(|value| {
                value
                    .get("default")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
            });
        match configured {
            Some(id) if self.resolve(&id).is_some() => id,
            _ => DEFAULT_PRESET_ID.to_string(),
        }
    }

    /// 重读磁盘:随附集合在前(同名 id 由随附集合赢得),用户行按 id 排序。
    pub fn refresh(&self) {
        let known_tools = self.known_tools.load_full();
        let mut rows: Vec<PresetRow> = builtin_presets()
            .iter()
            .map(PresetRow::healthy)
            .collect();
        let mut user_rows: Vec<PresetRow> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let Some(id) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                if !is_valid_preset_id(id) {
                    user_rows.push(PresetRow::broken(
                        id,
                        PresetTrust::User,
                        Some(&path),
                        format!("目录名不是合法的 preset id(小写字母数字与连字符):{id}"),
                    ));
                    continue;
                }
                if rows.iter().any(|row| row.id == id) {
                    user_rows.push(PresetRow::broken(
                        id,
                        PresetTrust::User,
                        Some(&path),
                        "与随附 preset 同名,该目录不会被加载".to_string(),
                    ));
                    continue;
                }
                user_rows.push(read_user_row(id, &path, known_tools.as_deref()));
            }
        }
        user_rows.sort_by(|a, b| a.id.cmp(&b.id));
        rows.extend(user_rows);
        self.rows.store(Arc::new(rows));
    }

    /// 用户 preset 的 `preset.yml` 全文(只读查看器与"用编辑器打开"用)。
    pub fn describe_text(&self, id: &str) -> Result<String, String> {
        if !is_valid_preset_id(id) {
            return Err(format!("非法的 preset id:{id}"));
        }
        match builtin_presets().into_iter().find(|preset| preset.id == id) {
            Some(preset) => Ok(render_preset_file(&preset)),
            None => std::fs::read_to_string(self.root.join(id).join(FILE_NAME))
                .map_err(|error| format!("读取 preset 失败:{error}")),
        }
    }

    /// 复制创作:把 `from` 的整个目录复制成 `id`,重写显示元数据。
    ///
    /// 两道拒绝是刻意的:名册检查挡住任一提供者已占用的 id(与随附 preset
    /// 同名的用户目录会被遮蔽,复制只会落下一个永远不被列出的文件);目录
    /// 存在性检查挡住"占着名字却不是 preset"的目录,那是 discovery 看不见的。
    pub fn copy(&self, from: &str, id: &str, name: Option<&str>) -> Result<PresetRow, String> {
        if !is_valid_preset_id(id) {
            return Err(format!(
                "非法的 preset id:{id}(只允许小写字母、数字与连字符,且以字母数字开头)"
            ));
        }
        let source = self
            .resolve(from)
            .ok_or_else(|| format!("来源 preset 不存在或无法加载:{from}"))?;
        if self.rows().iter().any(|row| row.id == id) {
            return Err(format!("preset id 已被占用:{id}"));
        }
        let target = self.root.join(id);
        if target.exists() {
            return Err(format!(
                "目录已存在:{}(占着名字却不是名册里的 preset)",
                target.display()
            ));
        }
        let name = name
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{} 副本", source.name));
        if let Err(error) = std::fs::create_dir_all(&target) {
            return Err(format!("创建 preset 目录失败:{error}"));
        }
        // 来源目录存在时复制其内容(随附 preset 没有目录,只写一份组装文件)。
        if let Some(source_path) = source.path.as_deref().map(PathBuf::from)
            && source_path.is_dir()
            && let Err(error) = copy_tree(&source_path, &target)
        {
            let _ = std::fs::remove_dir_all(&target);
            return Err(error);
        }
        let mut preset = source.clone();
        preset.id = id.to_string();
        preset.name = name;
        preset.trust = PresetTrust::User;
        preset.path = Some(target.display().to_string());
        let rendered = render_preset_file(&preset);
        if let Err(error) = std::fs::write(target.join(FILE_NAME), rendered) {
            let _ = std::fs::remove_dir_all(&target);
            return Err(format!("写入 preset.yml 失败:{error}"));
        }
        self.refresh();
        self.rows()
            .iter()
            .find(|row| row.id == id)
            .cloned()
            .ok_or_else(|| "复制后名册里找不到新 preset".to_string())
    }

    /// 创作落盘:按 `create_preset` 工具提交的规范直接写一份新 preset。
    ///
    /// 与复制创作的两道拒绝同构:id 非法或已被占用都拒绝;写盘失败清理
    /// 半成品目录。features/tools 的校验口径与解析手写文件一致(工具名
    /// 对照部署名册,`mcp__` 前缀豁免)。
    pub fn author(&self, spec: &PresetSpec) -> Result<PresetRow, String> {
        let id = spec.id.trim();
        if !is_valid_preset_id(id) {
            return Err(format!(
                "非法的 preset id:{id}(只允许小写字母、数字与连字符,且以字母数字开头)"
            ));
        }
        let name = spec.name.trim();
        if name.is_empty() {
            return Err("缺少显示名 name".to_string());
        }
        if let Some(tools) = &spec.tools {
            if tools.is_empty() {
                return Err("tools 不能是空列表(要全量工具集就省略该字段)".to_string());
            }
            if let Some(blank) = tools.iter().find(|name| name.trim().is_empty()) {
                return Err(format!("tools 里有空工具名:{blank:?}"));
            }
            validate_tool_names(tools, self.known_tools.load_full().as_deref())?;
        }
        if self.rows().iter().any(|row| row.id == id) {
            return Err(format!("preset id 已被占用:{id}"));
        }
        let target = self.root.join(id);
        if target.exists() {
            return Err(format!(
                "目录已存在:{}(占着名字却不是名册里的 preset)",
                target.display()
            ));
        }
        if let Err(error) = std::fs::create_dir_all(&target) {
            return Err(format!("创建 preset 目录失败:{error}"));
        }
        let preset = AgentPreset {
            id: id.to_string(),
            name: name.to_string(),
            description: spec.description.trim().to_string(),
            trust: PresetTrust::User,
            tools: spec.tools.clone(),
            persona: spec
                .persona
                .as_deref()
                .map(str::trim_end)
                .filter(|text| !text.trim().is_empty())
                .map(str::to_string),
            persona_complete: spec.persona_complete,
            features: spec.features,
            path: Some(target.display().to_string()),
        };
        if let Err(error) = std::fs::write(target.join(FILE_NAME), render_preset_file(&preset)) {
            let _ = std::fs::remove_dir_all(&target);
            return Err(format!("写入 preset.yml 失败:{error}"));
        }
        self.refresh();
        self.rows()
            .iter()
            .find(|row| row.id == id)
            .cloned()
            .ok_or_else(|| "创建后名册里找不到新 preset".to_string())
    }

    /// 删除本地创作的 preset;随附 preset 拒绝删除。
    pub fn remove(&self, id: &str) -> Result<(), String> {
        if !is_valid_preset_id(id) {
            return Err(format!("非法的 preset id:{id}"));
        }
        let Some(row) = self.rows().iter().find(|row| row.id == id).cloned() else {
            return Err(format!("preset 不存在:{id}"));
        };
        if !row.writable {
            return Err(format!("随附 preset 不可删除:{id}"));
        }
        let dir = self.root.join(id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|error| format!("删除 preset 目录失败:{error}"))?;
        }
        self.refresh();
        Ok(())
    }

    /// 监听用户根目录:preset 文件增删改后热刷新名册并广播。
    ///
    /// 会话每个 step 装配时通过 `resolve()` 读名册快照,因此刷新后新 step
    /// 即生效;已在飞的请求不受影响。
    pub fn spawn_watcher(self: &Arc<Self>, events: tokio::sync::broadcast::Sender<ServerEvent>) {
        let root = self.root.clone();
        let store = Arc::clone(self);
        std::thread::spawn(move || {
            let (tx, rx) = std::sync::mpsc::channel();
            let mut watcher = match RecommendedWatcher::new(
                move |result| {
                    let _ = tx.send(result);
                },
                notify::Config::default(),
            ) {
                Ok(watcher) => watcher,
                Err(error) => {
                    tracing::warn!(%error, "agent preset watcher failed to start");
                    return;
                }
            };
            // 根目录可能尚不存在(还没创作过 preset):先创建再监听,
            // 否则第一次复制创作要等重启才会被名册看见。
            if let Err(error) = std::fs::create_dir_all(&root) {
                tracing::warn!(%error, "agent preset root could not be created");
                return;
            }
            if let Err(error) = watcher.watch(&root, RecursiveMode::Recursive) {
                tracing::warn!(%error, "agent preset watcher could not watch root");
                return;
            }
            let mut last_refresh = std::time::Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(std::time::Instant::now);
            while let Ok(result) = rx.recv() {
                let Ok(event) = result else {
                    continue;
                };
                if !matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                ) {
                    continue;
                }
                if last_refresh.elapsed() < Duration::from_millis(200) {
                    continue;
                }
                last_refresh = std::time::Instant::now();
                store.refresh();
                let _ = events.send(ServerEvent::AgentPresetsUpdated);
            }
        });
    }
}

impl denia_agent_loop::AgentPresetSource for PresetStore {
    fn resolve(&self, id: &str) -> Option<AgentPreset> {
        PresetStore::resolve(self, id)
    }

    fn default_id(&self) -> String {
        PresetStore::default_id(self)
    }
}

/// 读一个用户 preset 目录:合成定义或一条带原因的 broken 行。
fn read_user_row(id: &str, dir: &Path, known_tools: Option<&BTreeSet<String>>) -> PresetRow {
    let file = dir.join(FILE_NAME);
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(error) => {
            return PresetRow::broken(
                id,
                PresetTrust::User,
                Some(dir),
                format!("缺少可读的 {FILE_NAME}:{error}"),
            )
        }
    };
    match parse_preset(id, &text, known_tools) {
        Ok(mut preset) => {
            // 用户 preset 带上自己的目录:复制创作要连目录里的附带文件
            // (资产/参考材料)一起复制,只抄 preset.yml 会丢内容。
            preset.path = Some(dir.display().to_string());
            PresetRow::healthy(&preset)
        }
        Err(reason) => PresetRow::broken(id, PresetTrust::User, Some(dir), reason),
    }
}

/// 校验 tools 白名单引用的工具名都真实存在(部署注册名册注入后生效)。
/// `mcp__` 前缀豁免:MCP 工具随服务器配置动态进出,不属于部署静态名册。
fn validate_tool_names(
    tools: &[String],
    known_tools: Option<&BTreeSet<String>>,
) -> Result<(), String> {
    let Some(known) = known_tools else {
        return Ok(());
    };
    for name in tools {
        let name = name.trim();
        if name.starts_with("mcp__") || known.contains(name) {
            continue;
        }
        return Err(format!("tools 引用了部署不存在的工具:{name}"));
    }
    Ok(())
}

/// 解析 `preset.yml`;`id` 来自目录名,文件里不写 id。
fn parse_preset(
    id: &str,
    text: &str,
    known_tools: Option<&BTreeSet<String>>,
) -> Result<AgentPreset, String> {
    let file: PresetFile =
        serde_yaml::from_str(text).map_err(|error| format!("{FILE_NAME} 解析失败:{error}"))?;
    if let Some(tools) = &file.tools {
        if tools.is_empty() {
            return Err("tools 不能是空列表(要全量工具集就省略该字段)".to_string());
        }
        if let Some(blank) = tools.iter().find(|name| name.trim().is_empty()) {
            return Err(format!("tools 里有空工具名:{blank:?}"));
        }
        validate_tool_names(tools, known_tools)?;
    }
    Ok(AgentPreset {
        id: id.to_string(),
        name: file
            .name
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| id.to_string()),
        description: file
            .description
            .map(|text| text.trim().to_string())
            .unwrap_or_default(),
        trust: PresetTrust::User,
        tools: file.tools,
        persona: file
            .persona
            .map(|text| text.trim_end().to_string())
            .filter(|text| !text.trim().is_empty()),
        persona_complete: file.persona_complete,
        features: file.features,
        path: None,
    })
}

/// 随附 preset 的等价组装文本(只读查看器渲染用)。
///
/// features 只渲染关闭的键:省略的键 = 开启,与解析语义一致,文件保持最小。
fn render_preset_file(preset: &AgentPreset) -> String {
    let mut out = String::new();
    out.push_str(&format!("name: {}\n", yaml_scalar(&preset.name)));
    if !preset.description.is_empty() {
        out.push_str(&format!(
            "description: {}\n",
            yaml_scalar(&preset.description)
        ));
    }
    if let Some(tools) = &preset.tools {
        out.push_str("tools:\n");
        for tool in tools {
            out.push_str(&format!("  - {tool}\n"));
        }
    }
    if let Some(persona) = &preset.persona {
        out.push_str("persona: |\n");
        for line in persona.lines() {
            out.push_str(&format!("  {line}\n"));
        }
    }
    if preset.persona_complete {
        out.push_str("personaComplete: true\n");
    }
    if !preset.features.is_default() {
        out.push_str("features:\n");
        let features = &preset.features;
        let flags = [
            ("agentsMd", features.agents_md),
            ("memory", features.memory),
            ("compaction", features.compaction),
            ("goal", features.goal),
            ("skills", features.skills),
            ("subagents", features.subagents),
            ("jobs", features.jobs),
            ("browser", features.browser),
            ("ask", features.ask),
            ("planMode", features.plan_mode),
        ];
        for (key, enabled) in flags {
            if !enabled {
                out.push_str(&format!("  {key}: false\n"));
            }
        }
    }
    out
}

/// YAML 标量:含特殊字符时用双引号包裹,避免写出解析不回来的文件。
fn yaml_scalar(text: &str) -> String {
    let needs_quotes = text.is_empty()
        || text.starts_with(['"', '\'', '[', '{', '-', '?', ':', '#', '&', '*', '!', '|', '>', '@', '%', '`'])
        || text.contains([':', '#', '\n'])
        || text.trim() != text;
    if !needs_quotes {
        return text.to_string();
    }
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// 递归复制目录内容(符号链接一律拒绝:preset 必须自包含,而跟随链接
/// 会把工作区之外的东西拖进用户目录)。
fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    for entry in std::fs::read_dir(from).map_err(|error| format!("读取来源目录失败:{error}"))? {
        let entry = entry.map_err(|error| format!("读取来源目录项失败:{error}"))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("读取来源目录项类型失败:{error}"))?;
        let target = to.join(entry.file_name());
        if file_type.is_symlink() {
            return Err(format!("来源 preset 含符号链接,拒绝复制:{}", path.display()));
        }
        if file_type.is_dir() {
            std::fs::create_dir_all(&target).map_err(|error| format!("创建目录失败:{error}"))?;
            copy_tree(&path, &target)?;
        } else if file_type.is_file() {
            std::fs::copy(&path, &target).map_err(|error| format!("复制文件失败:{error}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("denia-presets-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open_store(home: &Path) -> Arc<PresetStore> {
        let settings = Arc::new(SettingsStore::open(home).unwrap());
        settings
            .register(
                SETTINGS_NS,
                denia_settings::NamespaceSpec {
                    defaults: serde_json::json!({ "default": DEFAULT_PRESET_ID }),
                    validate: |value| {
                        if value.get("default").is_some_and(|v| !v.is_string()) {
                            return Err("default 必须是 preset id 字符串".to_string());
                        }
                        Ok(value)
                    },
                    secrets: &[],
                    applies: denia_settings::Applies::Live,
                },
                serde_json::json!({}),
            )
            .unwrap();
        PresetStore::load(home, settings)
    }

    fn write_user_preset(home: &Path, id: &str, body: &str) {
        let dir = home.join(DIR_NAME).join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(FILE_NAME), body).unwrap();
    }

    #[test]
    fn roster_starts_with_shipped_presets() {
        let home = temp_home();
        let store = open_store(&home);
        let rows = store.rows();
        assert_eq!(rows[0].id, DEFAULT_PRESET_ID);
        assert_eq!(rows[0].trust, "shipped");
        assert!(!rows[0].writable);
        assert!(store.resolve("minimal").is_some());
        assert!(store.resolve("nope").is_none());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn user_preset_is_discovered_and_sorted_after_shipped() {
        let home = temp_home();
        write_user_preset(
            &home,
            "explore",
            "name: 探索模式\ndescription: 只读检索\ntools:\n  - read_file\n  - grep\npersona: |\n  你是探索者。\n",
        );
        let store = open_store(&home);
        let rows = store.rows();
        let row = rows.iter().find(|row| row.id == "explore").expect("发现用户 preset");
        assert!(row.writable);
        assert!(row.has_persona);
        assert_eq!(row.tools.as_ref().unwrap().len(), 2);
        let preset = store.resolve("explore").unwrap();
        assert_eq!(preset.persona.as_deref(), Some("你是探索者。"));
        // 随附集合仍然在前。
        assert_eq!(rows.last().unwrap().id, "explore");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn broken_preset_is_listed_with_a_reason() {
        let home = temp_home();
        write_user_preset(&home, "broken", "name: [未闭合\n");
        let store = open_store(&home);
        let row = store
            .rows()
            .iter()
            .find(|row| row.id == "broken")
            .cloned()
            .expect("损坏的 preset 也要出现在名册里");
        assert!(row.broken.is_some());
        assert!(store.resolve("broken").is_none());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn preset_without_file_is_broken() {
        let home = temp_home();
        std::fs::create_dir_all(home.join(DIR_NAME).join("empty")).unwrap();
        let store = open_store(&home);
        let row = store.rows().iter().find(|row| row.id == "empty").cloned();
        assert!(row.is_some_and(|row| row.broken.is_some()));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn user_directory_shadowed_by_shipped_id_is_reported() {
        let home = temp_home();
        write_user_preset(&home, "standard", "name: 假标准\n");
        let store = open_store(&home);
        let rows = store.rows();
        let standard_rows: Vec<&PresetRow> = rows
            .iter()
            .filter(|row| row.id == "standard")
            .collect();
        assert_eq!(standard_rows.len(), 2, "遮蔽事实必须可见,而不是静默丢弃");
        assert!(standard_rows[0].broken.is_none());
        assert!(standard_rows[1].broken.is_some());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn copy_creates_an_editable_preset_and_rejects_bad_targets() {
        let home = temp_home();
        let store = open_store(&home);
        let row = store.copy("minimal", "my-minimal", Some("我的极简")).unwrap();
        assert_eq!(row.name, "我的极简");
        assert!(row.writable);
        assert_eq!(row.tools.as_ref().unwrap().len(), 1);
        let preset = store.resolve("my-minimal").unwrap();
        assert_eq!(preset.trust, PresetTrust::User);
        // 极简的形态随复制携带:persona 独占 + 功能全关。
        assert!(preset.persona_complete);
        assert_eq!(preset.features, PresetFeatures::all_off());
        // 已有 id 拒绝(不覆盖)。
        assert!(store.copy("minimal", "my-minimal", None).is_err());
        // 非法 id 拒绝。
        assert!(store.copy("minimal", "../escape", None).is_err());
        assert!(store.copy("minimal", "Bad", None).is_err());
        // 未知来源拒绝。
        assert!(store.copy("nope", "fresh", None).is_err());
        // 占着名字却不是 preset 的目录同样拒绝。
        std::fs::create_dir_all(home.join(DIR_NAME).join("occupied")).unwrap();
        assert!(store.copy("minimal", "occupied", None).is_err());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn copy_rejects_symlinked_sources() {
        let home = temp_home();
        write_user_preset(&home, "linked", "name: 链接\n");
        let link = home.join(DIR_NAME).join("linked").join("asset");
        let target = home.join("outside.txt");
        std::fs::write(&target, "outside").unwrap();
        if !symlink_file(&target, &link) {
            // 平台不支持符号链接(权限):跳过这一条,其余断言仍覆盖复制路径。
            std::fs::remove_dir_all(&home).unwrap();
            return;
        }
        let store = open_store(&home);
        let error = store.copy("linked", "copy-of-linked", None).unwrap_err();
        assert!(error.contains("符号链接"), "错误信息应指名原因:{error}");
        std::fs::remove_dir_all(&home).unwrap();
    }

    fn symlink_file(from: &Path, to: &Path) -> bool {
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(from, to).is_ok()
        }
        #[cfg(not(windows))]
        {
            std::os::unix::fs::symlink(from, to).is_ok()
        }
    }

    #[test]
    fn remove_only_touches_user_presets() {
        let home = temp_home();
        let store = open_store(&home);
        assert!(store.remove(DEFAULT_PRESET_ID).is_err(), "随附 preset 不可删除");
        store.copy("minimal", "temp-one", None).unwrap();
        assert!(store.resolve("temp-one").is_some());
        store.remove("temp-one").unwrap();
        assert!(store.resolve("temp-one").is_none());
        assert!(store.remove("temp-one").is_err(), "重复删除要报错而不是静默");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn default_id_prefers_settings_and_falls_back_when_stale() {
        let home = temp_home();
        let store = open_store(&home);
        assert_eq!(PresetStore::default_id(&store), DEFAULT_PRESET_ID);
        std::fs::write(
            home.join("settings.yaml"),
            "agent-presets:\n  default: gone\n",
        )
        .unwrap();
        let reloaded = open_store(&home);
        assert_eq!(
            PresetStore::default_id(&reloaded),
            DEFAULT_PRESET_ID,
            "指向不存在的 preset 时必须回退到随附默认值"
        );
        std::fs::write(
            home.join("settings.yaml"),
            "agent-presets:\n  default: minimal\n",
        )
        .unwrap();
        let reloaded = open_store(&home);
        assert_eq!(PresetStore::default_id(&reloaded), "minimal");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn describe_text_renders_shipped_and_reads_user_files() {
        let home = temp_home();
        write_user_preset(&home, "custom", "name: 自定义\n");
        let store = open_store(&home);
        let shipped_text = store.describe_text("minimal").unwrap();
        assert!(shipped_text.contains("tools:"));
        // 内置极简形态完整渲染:persona 独占 + 关闭的功能逐键列出。
        assert!(shipped_text.contains("personaComplete: true"));
        assert!(shipped_text.contains("agentsMd: false"));
        assert!(shipped_text.contains("goal: false"));
        let user_text = store.describe_text("custom").unwrap();
        assert!(user_text.contains("自定义"));
        assert!(store.describe_text("nope").is_err());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn features_and_persona_complete_parse_from_disk() {
        let home = temp_home();
        write_user_preset(
            &home,
            "lean",
            "name: 精简\ndescription: 关掉一部分\nfeatures:\n  agentsMd: false\n  goal: false\npersonaComplete: true\n",
        );
        let store = open_store(&home);
        let preset = store.resolve("lean").expect("features 合法的行可解析");
        assert!(preset.persona_complete);
        assert!(!preset.features.agents_md);
        assert!(!preset.features.goal);
        assert!(preset.features.memory, "未声明的键保持默认开启");
        let row = store
            .rows()
            .iter()
            .find(|row| row.id == "lean")
            .cloned()
            .unwrap();
        assert!(!row.features.agents_md);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn unknown_feature_key_or_tool_name_marks_the_row_broken() {
        let home = temp_home();
        write_user_preset(
            &home,
            "typo",
            "features:\n  agentMd: false\n",
        );
        write_user_preset(
            &home,
            "ghost-tool",
            "tools:\n  - bash\n  - not_a_real_tool\n",
        );
        write_user_preset(
            &home,
            "mcp-ok",
            "tools:\n  - mcp__some__tool\n",
        );
        let store = open_store(&home);
        // 未知 features 键:fail loud 成 broken 行,不静默忽略。
        let typo = store
            .rows()
            .iter()
            .find(|row| row.id == "typo")
            .cloned()
            .expect("未知键的行仍要出现");
        assert!(typo.broken.is_some());
        // 工具名册注入后,引用不存在工具的行 broken;mcp__ 前缀豁免。
        store.set_known_tools(["bash".to_string(), "read_file".to_string()]);
        let ghost = store
            .rows()
            .iter()
            .find(|row| row.id == "ghost-tool")
            .cloned()
            .unwrap();
        assert!(
            ghost.broken.as_deref().is_some_and(|reason| reason.contains("not_a_real_tool")),
            "broken 原因要点名工具:{:?}",
            ghost.broken
        );
        let mcp_row = store
            .rows()
            .iter()
            .find(|row| row.id == "mcp-ok")
            .cloned()
            .unwrap();
        assert!(mcp_row.broken.is_none(), "mcp__ 工具不校验");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn mode_selection_off_falls_back_to_shipped_default() {
        let home = temp_home();
        write_user_preset(&home, "custom-default", "name: 自定义默认\n");
        std::fs::write(
            home.join("settings.yaml"),
            "agent-presets:\n  default: custom-default\n  modeSelectionEnabled: false\n",
        )
        .unwrap();
        let reloaded = open_store(&home);
        assert!(
            !reloaded.mode_selection_enabled(),
            "设置写 false 时开关必须为 false"
        );
        assert_eq!(
            reloaded.default_id(),
            DEFAULT_PRESET_ID,
            "模式选择关闭时忽略用户 default,部署默认治下"
        );
        // 开关重新打开后,保存的 default 恢复生效。
        std::fs::write(
            home.join("settings.yaml"),
            "agent-presets:\n  default: custom-default\n  modeSelectionEnabled: true\n",
        )
        .unwrap();
        let reloaded = open_store(&home);
        assert_eq!(reloaded.default_id(), "custom-default");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn copy_source_with_a_directory_copies_its_contents() {
        let home = temp_home();
        write_user_preset(&home, "rich", "name: 丰富\n");
        std::fs::write(
            home.join(DIR_NAME).join("rich").join("notes.md"),
            "附带资料",
        )
        .unwrap();
        let store = open_store(&home);
        store.copy("rich", "rich-copy", None).unwrap();
        let copied = std::fs::read_to_string(
            home.join(DIR_NAME).join("rich-copy").join("notes.md"),
        )
        .unwrap();
        assert_eq!(copied, "附带资料");
        std::fs::remove_dir_all(&home).unwrap();
    }
}
