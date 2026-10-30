//! 项目记忆:每工作区一个 Markdown 记忆域(索引 + 条目文件)。
//!
//! - 落点:`$DENIA_HOME/memories/projects/<slug>-<sha16>/memory/`,slug 是
//!   工作区目录名的清洗结果,sha16 是规范化路径的 sha256 前 16 位十六进制;
//!   同一路径永远映射到同一目录,目录改名/换盘符才会换桶。
//! - `MEMORY.md` 是纯索引(一行一条,按「经验 / 事实 / 历史」分区),其余
//!   `.md` 一事一文件,frontmatter 携带
//!   `name/description/metadata.kind/metadata.status`。
//! - 写入完全由**主代理主动完成**(见系统提示的 `# 项目记忆` 段纪律):
//!   轮次结束时由模型自己判断该不该记,没有后台提取子代理。
//!   总闸 `runtime.memoryEnabled` 关闭时,注入与写入同时停。
//!
//! ## 为什么分「经验 / 事实」
//!
//! 这两类东西的失效方式完全不同,混在一起就只能靠模型自己记着哪条还算数:
//!
//! - **经验(experience)**回答"该怎么做":纠正、被验证有效的做法、踩过的坑。
//!   它不会因为时间过去而变错,只会在被更好的做法取代时失效,所以正文要求
//!   写清 **Why** 与 **How to apply**。
//! - **事实(fact)**回答"现在是什么状态":某个开关的真实语义、外部依赖、
//!   进行中的目标。它有版本——状态一变旧值就是错的,所以**只追加不原地改**,
//!   新条目用 `supersedes` 指回被取代者,旧条目转 `status: superseded`。
//!
//! 原地改写的代价是丢掉历史:后来者看不到"为什么曾经这么以为",同一个坑会
//! 被重新踩一遍。因此 supersede 是唯一的状态迁移,删除只用于记错的内容。
//!
//! 本模块只做纯 fs 与纯函数;注入在 agent-loop,检索在 [`retrieval`]。

use std::path::{Component, Path, PathBuf};

use sha2::Digest;

pub mod retrieval;

/// 索引文件名(纯索引,一行一条,按「经验 / 事实 / 历史」分区)。
pub const MEMORY_INDEX_FILE: &str = "MEMORY.md";
/// 单条记忆的 manifest 上限(按 mtime 倒序保留最新 200 个)。
const MANIFEST_CAP: usize = 200;
/// 单文件读取上限(设置页 viewer 与 API 同用)。
pub const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;

/// 索引分区标题:经验区。
pub const SECTION_EXPERIENCE: &str = "## 经验（how）";
/// 索引分区标题:事实区。
pub const SECTION_FACT: &str = "## 事实（what）";
/// 索引分区标题:历史区(取代后的条目,不参与注入)。
pub const SECTION_HISTORY: &str = "## 历史（superseded，不注入）";

/// 记忆类型。两类东西的失效方式不同,见模块头注释。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryKind {
    /// 该怎么做:纠正、被验证有效的做法、踩过的坑。
    Experience,
    /// 现在是什么状态:语义、依赖、目标、外部资源。
    Fact,
}

impl MemoryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryKind::Experience => "experience",
            MemoryKind::Fact => "fact",
        }
    }

    /// 解析 frontmatter `metadata.kind`,兼容旧字段 `metadata.type`。
    ///
    /// 旧四类(user/feedback/project/reference)按"失效方式"归并:只有
    /// feedback 讲的是工作方式,其余三类都是状态。
    fn parse(raw: &str) -> Option<Self> {
        match raw
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_ascii_lowercase()
            .as_str()
        {
            "experience" | "feedback" => Some(MemoryKind::Experience),
            "fact" | "user" | "project" | "reference" => Some(MemoryKind::Fact),
            _ => None,
        }
    }
}

/// 记忆状态。`Superseded` 只表示"被更新的事实取代",内容仍然可追溯。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryStatus {
    Active,
    Superseded,
}

impl MemoryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryStatus::Active => "active",
            MemoryStatus::Superseded => "superseded",
        }
    }
}

/// 一条记忆的 frontmatter。字段缺失一律降级为空/默认,不报错——
/// 记忆文件是用户可以直接手改的,解析必须容错。
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryFrontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
    pub kind: Option<MemoryKind>,
    pub status: Option<MemoryStatus>,
    /// 本条取代的旧条目 name(事实区专用)。
    pub supersedes: Vec<String>,
    /// 取代本条的条目 name(写在旧条目上)。
    pub superseded_by: Option<String>,
}

/// 读文件头部(最多 8 KiB)并解析 frontmatter。文件不可读或没有
/// frontmatter 时返回空结构(调用方按"未标注"处理)。
pub fn read_frontmatter(path: &Path) -> MemoryFrontmatter {
    use std::io::Read;
    let Ok(file) = std::fs::File::open(path) else {
        return MemoryFrontmatter::default();
    };
    let mut bytes = Vec::new();
    let _ = file.take(8 * 1024).read_to_end(&mut bytes);
    parse_frontmatter(&String::from_utf8_lossy(&bytes))
}

/// 解析 frontmatter 文本。只认文件头到第二个 `---` 之间的行,
/// 逐行读 `key: value` 与 `key: [a, b]`,不引入 YAML 依赖。
fn parse_frontmatter(buffer: &str) -> MemoryFrontmatter {
    let mut out = MemoryFrontmatter::default();
    let mut lines = buffer.lines();
    if lines.next().map(str::trim) != Some("---") {
        return out;
    }
    let mut in_metadata = false;
    for line in lines.take(40) {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // 顶层 `metadata:` 开关缩进块。
        if trimmed == "metadata:" {
            in_metadata = true;
            continue;
        }
        // 回到顶层键(无缩进)时关闭 metadata 块。
        if !line.starts_with([' ', '\t']) {
            in_metadata = false;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        // `name` / `description` 只认顶层;类型与状态两种写法都收——
        // 旧文件嵌在 `metadata:` 下,新写的可以直接放顶层,手改不会失效。
        match key {
            "name" if !in_metadata => out.name = non_empty(value),
            "description" if !in_metadata => out.description = non_empty(value),
            "kind" | "type" => {
                // 显式 kind 优先于旧 type 的推断。
                if key == "kind" || out.kind.is_none() {
                    out.kind = MemoryKind::parse(value);
                }
            }
            "status" => {
                out.status = match value.to_ascii_lowercase().as_str() {
                    "active" => Some(MemoryStatus::Active),
                    "superseded" => Some(MemoryStatus::Superseded),
                    _ => None,
                }
            }
            "supersedes" => out.supersedes = parse_list(value),
            "superseded_by" | "superseded-by" => out.superseded_by = non_empty(value),
            _ => {}
        }
    }
    out
}

/// 去掉引号后的非空值。
fn non_empty(value: &str) -> Option<String> {
    let value = value.trim().trim_matches('"').trim_matches('\'').trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// 解析 `[a, b]` / `a` 两种写法。
fn parse_list(value: &str) -> Vec<String> {
    let inner = value.trim().trim_start_matches('[').trim_end_matches(']');
    inner.split(',').filter_map(non_empty).collect()
}

/// 一个工作区的记忆目录根。
pub fn memory_root(home: &Path, workspace_path: &Path) -> PathBuf {
    home.join("memories")
        .join("projects")
        .join(project_slug(workspace_path))
        .join("memory")
}

/// `<slug>-<sha16>`:目录名小写清洗(非字母数字一律 `-`,截 48 字符)+
/// 规范化路径哈希前 16 位,保证不同项目不撞桶、同项目跨会话稳定。
fn project_slug(workspace_path: &Path) -> String {
    let normalized = crate::workspace::normalize_path(workspace_path);
    let digest = sha2::Sha256::digest(normalized.to_string_lossy().as_bytes());
    let hash: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let base = normalized
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut cleaned: String = base
        .to_lowercase()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    while cleaned.ends_with('-') {
        cleaned.pop();
    }
    cleaned.truncate(48);
    while cleaned.ends_with('-') || cleaned.ends_with('_') {
        cleaned.pop();
    }
    if cleaned.is_empty() {
        cleaned.push_str("project");
    }
    format!("{cleaned}-{hash}")
}

/// 索引正文的行硬上限:超出才截断,截断告警带总行数/总条目文件数与全文指针。
const INDEX_MAX_LINES: usize = 200;

/// 读 `MEMORY.md` 原文(BOM 与 CRLF 归一,不做任何截断);缺失或纯空白
/// 返回 None。
pub fn read_index_text(root: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(root.join(MEMORY_INDEX_FILE)).ok()?;
    let content = raw.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    if content.trim().is_empty() {
        return None;
    }
    Some(content)
}

/// 注入通道入口:读索引原文 → 丢掉历史区 → 行/字节预算。
///
/// 正文只由索引文本与字节预算决定,不读任何会话状态:注入通道靠正文比较
/// 判幂等,正文一旦随“本轮读过哪些文件/最新用户消息”变化,每个新输入都要
/// 重发整块索引,而旧块留在模型面上不退场。索引一行一条、体量很小,全量
/// 注入一次比反复增量便宜;条目正文仍按需 `read_file`。
///
/// `## 历史（superseded…）` 整段不注入:被取代的旧事实留在磁盘上是为了
/// 追溯,把它喂给模型只会让过期状态和当前状态一起出现在上下文里,反而
/// 增加判断成本。要读历史条目就 `read_file` 索引原文。
/// 索引缺失或纯空白返回 None(空桶不注入)。
pub fn read_index_for_injection(root: &Path, max_bytes: usize) -> Option<String> {
    let content = read_index_text(root)?;
    let total_lines = content.lines().count();
    let body = drop_history_section(&content);
    let (mut body, cut) = truncate_lines(&body, INDEX_MAX_LINES);
    if cut {
        // 只有病态索引(结构行自己就超限)才走到这里,到这一步才扫目录取文件数。
        let files = scan_manifest(root).files.len();
        body.push_str(&format!(
            "\n（项目记忆索引超过 {INDEX_MAX_LINES} 行，已截断：共 {total_lines} 行、{files} 个条目文件，用 read_file 读 {}/MEMORY.md 获取全文。）",
            root.display()
        ));
    }
    if body.len() > max_bytes {
        body = truncate_utf8(&body, max_bytes);
        body.push_str(&format!(
            "\n\n（项目记忆索引超出字节预算，已截断：用 read_file 读 {}/MEMORY.md 获取全文。）",
            root.display()
        ));
    }
    Some(body)
}

/// 去掉历史区:从 `## 历史…` 标题到下一个同级/更高级标题(或文末)。
fn drop_history_section(content: &str) -> String {
    let mut out = String::new();
    let mut in_history = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if !in_history && known_section(trimmed) == Some(SECTION_HISTORY) {
            in_history = true;
            continue;
        }
        if in_history {
            // 遇到下一个一/二级标题就退出历史区。
            if trimmed.starts_with('#') && !trimmed.starts_with("###") {
                in_history = false;
            } else {
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// 保留前 `max_lines` 行;返回截断后的正文与"是否发生截断"(告警文案由
/// 调用方补,那里才有总行数与总条目文件数)。
fn truncate_lines(content: &str, max_lines: usize) -> (String, bool) {
    let mut ends: Vec<usize> = content
        .match_indices('\n')
        .map(|(index, _)| index)
        .take(max_lines)
        .collect();
    let total_lines = content.lines().count();
    if total_lines <= max_lines {
        return (content.to_string(), false);
    }
    // 第 max_lines 行之后的第一个换行处截断。
    let cut = match ends.len() {
        n if n == max_lines => ends.pop().unwrap_or(content.len()),
        _ => content.len(),
    };
    let mut body = content[..cut].to_string();
    if !body.ends_with('\n') {
        body.push('\n');
    }
    (body, true)
}

/// UTF-8 边界截断(与 workspace_instructions 同款算法)。
fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

/// 记忆目录里一个文件的清单条目(设置页 viewer 用)。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryFileInfo {
    /// 相对记忆目录的路径(`/` 分隔)。
    pub name: String,
    pub size: u64,
    /// epoch 毫秒。
    pub modified_ms: u64,
    /// 一句话摘要(frontmatter `description`)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 经验 / 事实;未标注为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<MemoryKind>,
    /// active / superseded;未标注为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<MemoryStatus>,
}

/// 一个工作区记忆目录的清单。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectMemoryManifest {
    pub root: String,
    pub index: Option<MemoryFileInfo>,
    pub files: Vec<MemoryFileInfo>,
    /// 全目录最新 mtime(epoch 毫秒);空目录为 0。
    pub updated_at: u64,
}

/// 递归列出记忆目录里的 `.md` 文件(不含 MEMORY.md 索引本身),
/// 跳过符号链接,按 mtime 倒序保留最新 [`MANIFEST_CAP`] 个。
/// 目录不存在返回空清单(不报错:未启用过记忆的工作区是常态)。
pub fn scan_manifest(root: &Path) -> ProjectMemoryManifest {
    let mut files: Vec<MemoryFileInfo> = Vec::new();
    if root.is_dir() {
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                    continue;
                };
                if metadata.is_symlink() {
                    continue;
                }
                if metadata.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !metadata.is_file() || path.extension().is_none_or(|ext| ext != "md") {
                    continue;
                }
                if dir == root
                    && path
                        .file_name()
                        .is_some_and(|name| name == MEMORY_INDEX_FILE)
                {
                    continue;
                }
                let name = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                // 只读文件头 8 KiB 拿 frontmatter,清单要带类型与状态。
                let front = read_frontmatter(&path);
                files.push(MemoryFileInfo {
                    name,
                    size: metadata.len(),
                    modified_ms: mtime_millis(&metadata),
                    description: front.description,
                    kind: front.kind,
                    status: front.status,
                });
            }
        }
        files.sort_by(|left, right| {
            right
                .modified_ms
                .cmp(&left.modified_ms)
                .then_with(|| left.name.cmp(&right.name))
        });
        files.truncate(MANIFEST_CAP);
    }
    let index_path = root.join(MEMORY_INDEX_FILE);
    let index = std::fs::symlink_metadata(&index_path)
        .ok()
        .filter(|meta| meta.is_file())
        .map(|meta| MemoryFileInfo {
            name: MEMORY_INDEX_FILE.to_string(),
            size: meta.len(),
            modified_ms: mtime_millis(&meta),
            description: None,
            kind: None,
            status: None,
        });
    let updated_at = files
        .iter()
        .map(|file| file.modified_ms)
        .chain(index.iter().map(|file| file.modified_ms))
        .max()
        .unwrap_or(0);
    ProjectMemoryManifest {
        root: root.to_string_lossy().to_string(),
        index,
        files,
        updated_at,
    }
}

fn mtime_millis(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// 校验 viewer/API 传来的相对文件名:只接受 `/` 分隔、落在记忆目录内、
/// 不含 `..` 与符号链接的 `.md` 文件路径。返回解析后的绝对路径。
pub fn resolve_manifest_file(root: &Path, raw: &str) -> Result<PathBuf, String> {
    let raw = raw.trim().trim_start_matches('/');
    if raw.is_empty() {
        return Err("文件名不能为空".into());
    }
    let relative = Path::new(raw);
    let mut out = root.to_path_buf();
    for component in relative.components() {
        match component {
            Component::Normal(part) => out.push(part),
            _ => return Err(format!("非法路径:{raw}")),
        }
    }
    if out.extension().is_none_or(|ext| ext != "md") {
        return Err("只允许读取 .md 记忆文件".into());
    }
    let Ok(metadata) = std::fs::symlink_metadata(&out) else {
        return Err(format!("文件不存在:{raw}"));
    };
    if metadata.is_symlink() || !metadata.is_file() {
        return Err(format!("非法文件:{raw}"));
    }
    Ok(out)
}

/// 渲染「项目记忆」注入块(通道幂等的正文由调用方比较)。
/// `index` 为 [`read_index_for_injection`] 的正文(整份索引,无省略指针)。
/// None 由调用方处理(不注入)。注入块只负责把索引内容与目录位置交给模型,
/// 使用与验证规则在 `# 项目记忆` 段,不重复纪律。
pub fn render_index_block(root: &Path, index: &str) -> String {
    format!(
        "<system-reminder>\n{}/MEMORY.md 的内容(用户的自动记忆,跨会话持久):\n\n{index}\n</system-reminder>",
        root.display()
    )
}

/// 重建 `MEMORY.md`:扫描目录里所有条目文件,按 kind/status 分成三个区,
/// 每条一行 `- [标题](文件名) — 一句话钩子`。
///
/// 只做机械归类,不改写任何条目文件。返回值是重建后的索引正文与实际分到
/// 各区的条数——调用方(设置页的"整理")用它给用户看结果。
///
/// 旧索引里手写的、没有对应文件的自由行会被丢弃:索引的唯一职责是指向
/// 条目文件,留不住的自由行会让断链检查长期报警。
pub fn rebuild_index(root: &Path) -> Result<IndexRebuild, String> {
    if !root.is_dir() {
        return Err(format!("记忆目录不存在:{}", root.display()));
    }
    let manifest = scan_manifest(root);
    let mut experience: Vec<String> = Vec::new();
    let mut fact: Vec<String> = Vec::new();
    let mut history: Vec<String> = Vec::new();
    // 未标注 kind 的条目落在事实区末尾并标注,交给模型或用户补。
    let mut untyped = 0usize;

    for file in &manifest.files {
        let front = read_frontmatter(&root.join(&file.name));
        let title = front
            .name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| file.name.trim_end_matches(".md").to_string());
        let hook = front
            .description
            .as_deref()
            .map(str::trim)
            .filter(|description| !description.is_empty())
            .unwrap_or("（缺少 description）");
        let line = format!("- [{title}]({}) — {hook}", file.name);
        if front.status == Some(MemoryStatus::Superseded) {
            history.push(line);
            continue;
        }
        match front.kind {
            Some(MemoryKind::Experience) => experience.push(line),
            Some(MemoryKind::Fact) => fact.push(line),
            None => {
                untyped += 1;
                fact.push(line);
            }
        }
    }

    let mut body = String::new();
    body.push_str("# 项目记忆索引\n\n");
    body.push_str("> 由设置页「整理记忆」按各条 frontmatter 重建；改条目请改文件本身。\n\n");
    body.push_str(SECTION_EXPERIENCE);
    body.push('\n');
    body.push_str(&render_section(&mut experience, "（还没有经验条目）"));
    body.push('\n');
    body.push_str(SECTION_FACT);
    body.push('\n');
    body.push_str(&render_section(&mut fact, "（还没有事实条目）"));
    body.push('\n');
    body.push_str(SECTION_HISTORY);
    body.push('\n');
    body.push_str(&render_section(&mut history, "（没有被取代的历史条目）"));

    // 原子替换:先写临时文件再 rename,避免重建中途崩溃留下半份索引。
    let target = root.join(MEMORY_INDEX_FILE);
    let staging = root.join(format!("{MEMORY_INDEX_FILE}.tmp"));
    std::fs::write(&staging, &body).map_err(|error| format!("写入索引失败:{error}"))?;
    std::fs::rename(&staging, &target).map_err(|error| {
        let _ = std::fs::remove_file(&staging);
        format!("替换索引失败:{error}")
    })?;

    Ok(IndexRebuild {
        experience: experience.len(),
        fact: fact.len(),
        history: history.len(),
        untyped,
    })
}

/// 分区正文:空区给一行占位,免得读者以为重建失败。
fn render_section(lines: &mut [String], empty_hint: &str) -> String {
    if lines.is_empty() {
        return format!("{empty_hint}\n");
    }
    lines.sort();
    let mut out = String::new();
    for line in lines.iter() {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// [`rebuild_index`] 的结果计数(设置页展示用)。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexRebuild {
    pub experience: usize,
    pub fact: usize,
    pub history: usize,
    /// 未标注 kind、被临时归入事实区的条数。
    pub untyped: usize,
}

/// 记忆体检报告:确定性检查,不做任何模型调用、不修改任何文件。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryHealthReport {
    /// 索引条目指向但目录里不存在的文件(相对记忆目录的路径)。
    pub broken_links: Vec<String>,
    /// 目录里存在、但索引里没有登记的文件。
    pub orphans: Vec<String>,
    /// 描述完全相同的文件组(每组 ≥2 个文件)。
    pub duplicate_groups: Vec<Vec<String>>,
    /// 索引行数;无索引时为 None。
    pub index_lines: Option<usize>,
    /// 索引里的条目行数(不含标题与空行)。
    pub index_entries: Option<usize>,
    /// 索引字节数。
    pub index_bytes: Option<usize>,
    /// 索引是否越过注入预算口径(行或字节)。
    pub index_over_limit: bool,
    /// 索引缺少哪个分区标题(空表示三个区都在)。旧索引会缺"历史"区。
    pub missing_sections: Vec<String>,
    /// 已 `superseded` 但仍登记在经验/事实区的条目(注入时会误导)。
    pub superseded_still_listed: Vec<String>,
    /// 状态仍是 `active` 却登记在历史区的条目(会被白藏起来)。
    pub active_in_history: Vec<String>,
    /// `superseded_by` 指向的文件不存在。
    pub dangling_supersede_targets: Vec<String>,
    /// 未标注 kind 的条目(新旧格式混用时会出现在这里)。
    pub untyped: Vec<String>,
}

/// 索引字节的参考上限与注入默认预算同量级;仅用于体检提示,不参与裁剪。
const HEALTH_INDEX_BYTES_HINT: usize = 25 * 1024;

/// 记忆体检:断链 / 孤儿 / 重复描述 / 索引规模 / 分区与取代链一致性。
///
/// 只做机械检查(文件名与 frontmatter 的字面比较),为设置页的"整理"
/// 提供确定性依据;语义层面的合并与改写由主代理在会话里完成——
/// 这里不发起模型调用,也不写任何文件。
pub fn memory_health(root: &Path) -> MemoryHealthReport {
    let index = read_index_text(root);
    let mut missing_sections: Vec<String> = Vec::new();
    let mut superseded_still_listed: Vec<String> = Vec::new();
    let mut active_in_history: Vec<String> = Vec::new();
    let (index_lines, index_entries, index_bytes, broken_links, linked) = match index.as_deref() {
        Some(content) => {
            let mut broken = Vec::new();
            let mut linked: Vec<String> = Vec::new();
            let mut entries = 0usize;
            // 当前所处分区:None 表示还没进任何已知分区。
            let mut section: Option<&str> = None;
            let mut seen_sections: Vec<&str> = Vec::new();
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Some(heading) = known_section(trimmed) {
                    section = Some(heading);
                    if !seen_sections.contains(&heading) {
                        seen_sections.push(heading);
                    }
                    continue;
                }
                // 其他标题视为分区外(说明文字),不算条目。
                if trimmed.starts_with('#') {
                    section = None;
                    continue;
                }
                entries += 1;
                let Some(target) = entry_target(trimmed) else {
                    continue;
                };
                if linked.iter().any(|name| name == &target) {
                    continue;
                }
                let path = root.join(&target);
                if !path.is_file() {
                    broken.push(target.clone());
                    linked.push(target);
                    continue;
                }
                let front = read_frontmatter(&path);
                let is_superseded = front.status == Some(MemoryStatus::Superseded);
                match section {
                    Some(SECTION_HISTORY) => {
                        if !is_superseded {
                            active_in_history.push(target.clone());
                        }
                    }
                    Some(SECTION_EXPERIENCE) | Some(SECTION_FACT) if is_superseded => {
                        superseded_still_listed.push(target.clone());
                    }
                    // 没有分区标题的旧索引:位置说明不了状态,不报。
                    _ => {}
                }
                linked.push(target);
            }
            for required in [SECTION_EXPERIENCE, SECTION_FACT, SECTION_HISTORY] {
                if !seen_sections.contains(&required) {
                    missing_sections.push(required.to_string());
                }
            }
            (
                Some(content.lines().count()),
                Some(entries),
                Some(content.len()),
                broken,
                linked,
            )
        }
        None => (None, None, None, Vec::new(), Vec::new()),
    };

    let manifest = scan_manifest(root);
    let mut orphans: Vec<String> = manifest
        .files
        .iter()
        .filter(|file| !linked.iter().any(|name| name == &file.name))
        .map(|file| file.name.clone())
        .collect();
    orphans.sort();

    // 重复描述 / 未标注 kind / 取代链断头:一次遍历文件头收齐。
    let mut by_description: Vec<(String, String)> = Vec::new();
    let mut untyped: Vec<String> = Vec::new();
    let mut dangling_supersede_targets: Vec<String> = Vec::new();
    for file in &manifest.files {
        let front = read_frontmatter(&root.join(&file.name));
        if front.kind.is_none() {
            untyped.push(file.name.clone());
        }
        if let Some(target) = &front.superseded_by {
            // 允许带或不带 `.md`。
            let candidate = if target.ends_with(".md") {
                target.clone()
            } else {
                format!("{target}.md")
            };
            if !root.join(&candidate).is_file() && !root.join(target).is_file() {
                dangling_supersede_targets.push(format!("{} -> {target}", file.name));
            }
        }
        if let Some(description) = front.description {
            by_description.push((description, file.name.clone()));
        }
    }
    untyped.sort();
    dangling_supersede_targets.sort();

    let mut duplicate_groups: Vec<Vec<String>> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for (description, _) in &by_description {
        if seen.iter().any(|item| item == description) {
            continue;
        }
        seen.push(description.clone());
        let mut group: Vec<String> = by_description
            .iter()
            .filter(|(other, _)| other == description)
            .map(|(_, name)| name.clone())
            .collect();
        if group.len() > 1 {
            group.sort();
            duplicate_groups.push(group);
        }
    }
    duplicate_groups.sort();

    let index_over_limit = index_lines.is_some_and(|lines| lines > INDEX_MAX_LINES)
        || index_bytes.is_some_and(|bytes| bytes > HEALTH_INDEX_BYTES_HINT);

    MemoryHealthReport {
        broken_links,
        orphans,
        duplicate_groups,
        index_lines,
        index_entries,
        index_bytes,
        index_over_limit,
        missing_sections,
        superseded_still_listed,
        active_in_history,
        dangling_supersede_targets,
        untyped,
    }
}

/// 索引里已知的分区标题 → 规范化常量;其他标题返回 None。
fn known_section(trimmed: &str) -> Option<&'static str> {
    // 只用「经验 / 事实 / 历史」三个前缀匹配,括号里的说明文字允许用户改写。
    let rest = trimmed.strip_prefix("##")?.trim();
    if rest.starts_with("经验") {
        Some(SECTION_EXPERIENCE)
    } else if rest.starts_with("事实") {
        Some(SECTION_FACT)
    } else if rest.starts_with("历史") {
        Some(SECTION_HISTORY)
    } else {
        None
    }
}

/// 从索引条目行里取目标文件名:Markdown 链接 `](…)` 的内容;取不到
/// (手写的自由行)按"未登记目标"处理,不参与断链检查。
fn entry_target(line: &str) -> Option<String> {
    let start = line.find("](")? + 2;
    let rest = &line[start..];
    let end = rest.find(')')?;
    let target = rest[..end].trim();
    if target.is_empty() || target.contains("://") {
        return None;
    }
    Some(target.trim_start_matches("./").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("denia-mem-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 体检能报出断链 / 孤儿 / 重复描述,且不改动任何文件。
    #[test]
    fn health_reports_broken_orphans_and_duplicates() {
        let work = temp_root("health");
        let root = work.join("memory");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(MEMORY_INDEX_FILE),
            "# 索引\n- [A](a.md) — 钩子\n- [缺失](missing.md) — 钩子\n",
        )
        .unwrap();
        std::fs::write(
            root.join("a.md"),
            "---\nname: a\ndescription: 同一个描述\n---\n\n正文\n",
        )
        .unwrap();
        std::fs::write(
            root.join("b.md"),
            "---\nname: b\ndescription: 同一个描述\n---\n\n正文\n",
        )
        .unwrap();
        let report = memory_health(&root);
        assert_eq!(report.broken_links, vec!["missing.md".to_string()]);
        assert_eq!(
            report.orphans,
            vec!["b.md".to_string()],
            "b.md 未登记进索引"
        );
        assert_eq!(
            report.duplicate_groups,
            vec![vec!["a.md".to_string(), "b.md".to_string()]]
        );
        assert_eq!(report.index_entries, Some(2));
        assert!(report.index_lines.is_some());
        assert!(!report.index_over_limit);
        std::fs::remove_dir_all(work).unwrap();
    }

    /// 目录为空(未启用过记忆的工作区)时体检不报错,返回空报告。
    #[test]
    fn health_is_empty_for_missing_root() {
        let work = temp_root("health-empty");
        let report = memory_health(&work.join("missing"));
        assert!(report.broken_links.is_empty());
        assert!(report.orphans.is_empty());
        assert!(report.duplicate_groups.is_empty());
        assert_eq!(report.index_lines, None);
        assert!(!report.index_over_limit);
        std::fs::remove_dir_all(work).unwrap();
    }

    #[test]
    fn slug_is_stable_and_collision_free() {
        let work = temp_root("slug");
        let project = work.join("My Project");
        std::fs::create_dir_all(&project).unwrap();
        let a = project_slug(&project);
        let b = project_slug(&project);
        assert_eq!(a, b, "同一路径 slug 必须稳定");
        let other = work.join("My Project2");
        std::fs::create_dir_all(&other).unwrap();
        assert_ne!(a, project_slug(&other), "不同路径不得撞桶");
        // slug 形如 <清洗目录名>-<16 位十六进制>。
        let (base, hash) = a.rsplit_once('-').unwrap();
        assert!(base.starts_with("my-project"), "{base}");
        assert_eq!(hash.len(), 16);
        assert!(hash.chars().all(|ch| ch.is_ascii_hexdigit()));
        std::fs::remove_dir_all(work).unwrap();
    }

    #[test]
    fn read_index_truncates_lines_and_bytes_with_notice() {
        let work = temp_root("index");
        let root = work.join("memory");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(MEMORY_INDEX_FILE), "- a\n- b\n- c\n").unwrap();
        assert_eq!(
            read_index_for_injection(&root, 1024).unwrap(),
            "- a\n- b\n- c\n",
            "未超限时原样返回"
        );
        // 病态索引:结构行(标题)自己就超过 200 行,截行 + 告警带总行数/文件数。
        let many: String = (0..300).map(|i| format!("# h{i}\n")).collect();
        std::fs::write(root.join(MEMORY_INDEX_FILE), &many).unwrap();
        std::fs::write(root.join("one.md"), "1").unwrap();
        std::fs::write(root.join("two.md"), "2").unwrap();
        let cut = read_index_for_injection(&root, 1_048_576).unwrap();
        assert!(cut.contains("已截断"));
        assert!(cut.contains("# h199"));
        assert!(!cut.contains("# h200"));
        assert!(cut.contains("共 300 行"), "{cut}");
        assert!(cut.contains("2 个条目文件"), "{cut}");
        // 超字节预算:UTF-8 边界截断 + 告警。
        std::fs::write(root.join(MEMORY_INDEX_FILE), "好".repeat(100)).unwrap();
        let cut = read_index_for_injection(&root, 8).unwrap();
        assert!(
            cut.starts_with("好好"),
            "预算 8 字节只能装下 2 个汉字:{cut}"
        );
        assert!(!cut.contains("好好好"));
        assert!(cut.contains("字节预算"));
        // 空文件与缺失都返回 None(空桶不注入)。
        std::fs::write(root.join(MEMORY_INDEX_FILE), "   \n").unwrap();
        assert!(read_index_for_injection(&root, 1024).is_none());
        assert!(read_index_text(&root).is_none());
        std::fs::remove_file(root.join(MEMORY_INDEX_FILE)).unwrap();
        assert!(read_index_for_injection(&root, 1024).is_none());
        std::fs::remove_dir_all(work).unwrap();
    }

    /// 端到端(读 → 渲染):注入的是整份索引,不按会话状态挑选,也不留省略指针。
    #[test]
    fn injection_returns_the_whole_index() {
        let work = temp_root("inject");
        let root = work.join("memory");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(MEMORY_INDEX_FILE),
            "- [注入通道](injections.md) — 注入与幂等基准\n- [面板配色](frontend-css.md) — 设置页样式\n",
        )
        .unwrap();
        let body = read_index_for_injection(&root, 1_048_576).unwrap();
        assert!(body.contains("injections.md"));
        assert!(
            body.contains("frontend-css.md"),
            "未命中的条目同样在正文里:{body}"
        );
        // 同一索引文本永远给出同一正文:通道的幂等比较全靠这一条。
        assert_eq!(body, read_index_for_injection(&root, 1_048_576).unwrap());
        let block = render_index_block(&root, &body);
        assert!(!block.contains("另有"), "整份注入不留省略指针:{block}");
        std::fs::remove_dir_all(work).unwrap();
    }

    #[test]
    fn scan_manifest_orders_and_excludes_index() {
        let work = temp_root("manifest");
        let root = work.join("memory");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join(MEMORY_INDEX_FILE), "index").unwrap();
        std::fs::write(root.join("b.md"), "b").unwrap();
        std::fs::write(root.join("sub/nested.md"), "n").unwrap();
        std::fs::write(root.join("notes.txt"), "忽略非 md").unwrap();
        // b.md 设为最新。
        let new_time = std::time::SystemTime::now() + std::time::Duration::from_secs(10);
        let file = std::fs::File::options()
            .append(true)
            .open(root.join("b.md"))
            .unwrap();
        file.set_modified(new_time).unwrap();
        let manifest = scan_manifest(&root);
        assert_eq!(manifest.files.len(), 2);
        assert_eq!(manifest.files[0].name, "b.md", "按 mtime 倒序");
        assert_eq!(manifest.files[1].name, "sub/nested.md");
        assert!(manifest.index.is_some());
        assert_eq!(manifest.updated_at, manifest.files[0].modified_ms);
        // 目录不存在:空清单不报错。
        let empty = scan_manifest(&work.join("missing/memory"));
        assert!(empty.files.is_empty());
        assert!(empty.index.is_none());
        std::fs::remove_dir_all(work).unwrap();
    }

    #[test]
    fn manifest_file_resolution_rejects_escape_and_non_md() {
        let work = temp_root("resolve");
        let root = work.join("memory");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.md"), "ok").unwrap();
        assert!(resolve_manifest_file(&root, "a.md").is_ok());
        assert!(
            resolve_manifest_file(&root, "/a.md").is_ok(),
            "前导斜杠被剥掉"
        );
        assert!(resolve_manifest_file(&root, "../escape.md").is_err());
        assert!(resolve_manifest_file(&root, "sub/../../x.md").is_err());
        assert!(resolve_manifest_file(&root, "a.txt").is_err());
        assert!(resolve_manifest_file(&root, "").is_err());
        assert!(resolve_manifest_file(&root, "missing.md").is_err());
        std::fs::remove_dir_all(work).unwrap();
    }

    /// 重建索引:按 kind/status 分三区,带钩子,原子替换,不碰条目文件。
    #[test]
    fn rebuild_index_splits_sections_and_keeps_bodies() {
        let work = temp_root("rebuild");
        let root = work.join("memory");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("a.md"),
            "---\nname: a\ndescription: 经验A\nmetadata:\n  kind: experience\n---\n正文A\n",
        )
        .unwrap();
        std::fs::write(
            root.join("b.md"),
            "---\nname: b\ndescription: 事实B\nmetadata:\n  kind: fact\n  status: superseded\n  superseded_by: c.md\n---\n正文B\n",
        )
        .unwrap();
        std::fs::write(
            root.join("c.md"),
            "---\nname: c\ndescription: 事实C\nmetadata:\n  kind: fact\n  status: active\n---\n正文C\n",
        )
        .unwrap();
        // 旧索引里有一条指向不存在文件的自由行:重建后应消失。
        std::fs::write(root.join(MEMORY_INDEX_FILE), "- [旧](gone.md) — 断链\n").unwrap();

        let rebuilt = rebuild_index(&root).unwrap();
        assert_eq!(rebuilt.experience, 1);
        assert_eq!(rebuilt.fact, 1, "superseded 进历史区,不计入事实");
        assert_eq!(rebuilt.history, 1);
        assert_eq!(rebuilt.untyped, 0);

        let index = read_index_text(&root).unwrap();
        assert!(index.contains(SECTION_EXPERIENCE));
        assert!(index.contains(SECTION_FACT));
        assert!(index.contains(SECTION_HISTORY));
        assert!(index.contains("- [a](a.md) — 经验A"), "{index}");
        assert!(index.contains("- [b](b.md) — 事实B"), "{index}");
        assert!(!index.contains("gone.md"), "自由行被丢掉:{index}");
        // 分区归属正确:经验区在事实区之前,历史条目落在历史区之后。
        let exp_at = index.find(SECTION_EXPERIENCE).unwrap();
        let fact_at = index.find(SECTION_FACT).unwrap();
        let hist_at = index.find(SECTION_HISTORY).unwrap();
        assert!(exp_at < fact_at && fact_at < hist_at);
        assert!(index[exp_at..fact_at].contains("a.md"));
        assert!(index[exp_at..fact_at].contains("经验A"));
        assert!(index[hist_at..].contains("b.md"));
        // 条目文件一个字节都没动。
        assert!(
            std::fs::read_to_string(root.join("a.md"))
                .unwrap()
                .contains("正文A")
        );
        // 重建后的索引体检不应报缺区/错位。
        let report = memory_health(&root);
        assert!(report.missing_sections.is_empty(), "{report:?}");
        assert!(report.superseded_still_listed.is_empty(), "{report:?}");
        assert!(report.active_in_history.is_empty(), "{report:?}");
        assert!(report.broken_links.is_empty(), "{report:?}");
        assert!(report.orphans.is_empty(), "{report:?}");
        std::fs::remove_dir_all(work).unwrap();
    }

    /// 体检能指出:缺分区、superseded 仍在活跃区、active 在历史区、
    /// 取代链断头、未标注 kind。
    #[test]
    fn health_reports_section_and_supersede_problems() {
        let work = temp_root("health-v2");
        let root = work.join("memory");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("old.md"),
            "---\nname: old\ndescription: 旧\nkind: fact\nstatus: superseded\nsuperseded_by: missing.md\n---\n正文\n",
        )
        .unwrap();
        std::fs::write(
            root.join("new.md"),
            "---\nname: new\ndescription: 新\nkind: fact\nstatus: active\n---\n正文\n",
        )
        .unwrap();
        std::fs::write(root.join("loose.md"), "没有 frontmatter\n").unwrap();
        // 只写经验区,缺事实与历史;old.md 被错放在经验区。
        std::fs::write(
            root.join(MEMORY_INDEX_FILE),
            "## 经验（how）\n- [old](old.md) — 旧\n- [new](new.md) — 新\n",
        )
        .unwrap();

        let report = memory_health(&root);
        assert_eq!(
            report.missing_sections,
            vec![SECTION_FACT.to_string(), SECTION_HISTORY.to_string()]
        );
        assert_eq!(report.superseded_still_listed, vec!["old.md".to_string()]);
        assert!(
            report
                .dangling_supersede_targets
                .iter()
                .any(|item| item.contains("old.md") && item.contains("missing.md")),
            "{report:?}"
        );
        assert!(
            report.untyped.contains(&"loose.md".to_string()),
            "{report:?}"
        );
        std::fs::remove_dir_all(work).unwrap();
    }

    /// 旧四类 type 按失效方式归并:feedback→经验,其余→事实。
    #[test]
    fn legacy_type_maps_to_kind() {
        let front = parse_frontmatter(
            "---\nname: a\ndescription: d\nmetadata:\n  type: feedback\n---\n正文\n",
        );
        assert_eq!(front.kind, Some(MemoryKind::Experience));
        let front = parse_frontmatter(
            "---\nname: a\ndescription: d\nmetadata:\n  type: project\n---\n正文\n",
        );
        assert_eq!(front.kind, Some(MemoryKind::Fact));
        // 显式 kind 覆盖旧 type。
        let front = parse_frontmatter(
            "---\nname: a\nmetadata:\n  type: project\n  kind: experience\n---\n",
        );
        assert_eq!(front.kind, Some(MemoryKind::Experience));
    }

    #[test]
    fn injection_drops_history_section() {
        let work = temp_root("inject-history");
        let root = work.join("memory");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(MEMORY_INDEX_FILE),
            "# 项目记忆索引\n\n## 经验（how）\n- [e](e.md) — 经验钩子\n\n## 事实（what）\n- [f](f.md) — 事实钩子\n\n## 历史（superseded，不注入）\n- [old](old.md) — 已过期的旧状态\n- [older](older.md) — 更旧的\n",
        )
        .unwrap();
        let body = read_index_for_injection(&root, 1_048_576).unwrap();
        assert!(body.contains("经验钩子"), "{body}");
        assert!(body.contains("事实钩子"), "{body}");
        assert!(!body.contains("old.md"), "历史区不得注入:{body}");
        assert!(!body.contains("已过期的旧状态"), "{body}");
        // 历史区之后的同级标题要恢复收录。
        std::fs::write(
            root.join(MEMORY_INDEX_FILE),
            "## 事实（what）\n- [f](f.md) — 事实\n\n## 历史（x）\n- [o](o.md) — 旧\n\n## 其他\n- [k](k.md) — 保留\n",
        )
        .unwrap();
        let body = read_index_for_injection(&root, 1_048_576).unwrap();
        assert!(!body.contains("o.md"), "{body}");
        assert!(body.contains("k.md"), "历史区之后的标题必须收回来:{body}");
        std::fs::remove_dir_all(work).unwrap();
    }

    #[test]
    fn index_block_frames_auto_memory_index() {
        let root = Path::new("/m/memory");

        let block = render_index_block(root, "- [a](a.md) — x");
        assert!(block.starts_with("<system-reminder>"));
        assert!(block.ends_with("</system-reminder>"));
        assert!(block.contains("/m/memory/MEMORY.md"));
        assert!(block.contains("跨会话持久"));
        assert!(!block.contains("另有"), "整份注入不得出现省略指针:{block}");
    }
}
