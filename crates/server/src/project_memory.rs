//! 项目记忆:每工作区一个 Markdown 记忆域(索引 + 条目文件)。
//!
//! - 落点:`$DENIA_HOME/memories/projects/<slug>-<sha16>/memory/`,slug 是
//!   工作区目录名的清洗结果,sha16 是规范化路径的 sha256 前 16 位十六进制;
//!   同一路径永远映射到同一目录,目录改名/换盘符才会换桶。
//! - `MEMORY.md` 是纯索引(一行一条),其余 `.md` 一事一文件,frontmatter
//!   携带 `name/description/metadata.type`。
//! - 写入完全由**主代理主动完成**(见系统提示的 `# 项目记忆` 段纪律):
//!   轮次结束时由模型自己判断该不该记,没有后台提取子代理。
//!   总闸 `runtime.memoryEnabled` 关闭时,注入与写入同时停。
//!
//! 本模块只做纯 fs 与纯函数;注入在 agent-loop。

use std::path::{Component, Path, PathBuf};

use sha2::Digest;

/// 索引文件名(纯索引,一行一条)。
pub const MEMORY_INDEX_FILE: &str = "MEMORY.md";
/// 单条记忆的 manifest 上限(按 mtime 倒序保留最新 200 个)。
const MANIFEST_CAP: usize = 200;
/// 单文件读取上限(设置页 viewer 与 API 同用)。
pub const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;

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

/// 注入通道入口:读索引原文 → 行/字节预算。
///
/// 正文只由索引文本与字节预算决定,不读任何会话状态:注入通道靠正文比较
/// 判幂等,正文一旦随“本轮读过哪些文件/最新用户消息”变化,每个新输入都要
/// 重发整块索引,而旧块留在模型面上不退场。索引一行一条、体量很小,全量
/// 注入一次比反复增量便宜;条目正文仍按需 `read_file`。
/// 索引缺失或纯空白返回 None(空桶不注入)。
pub fn read_index_for_injection(root: &Path, max_bytes: usize) -> Option<String> {
    let content = read_index_text(root)?;
    let total_lines = content.lines().count();
    let (mut body, cut) = truncate_lines(&content, INDEX_MAX_LINES);
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
                files.push(MemoryFileInfo {
                    name,
                    size: metadata.len(),
                    modified_ms: mtime_millis(&metadata),
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
}

/// 索引字节的参考上限与注入默认预算同量级;仅用于体检提示,不参与裁剪。
const HEALTH_INDEX_BYTES_HINT: usize = 25 * 1024;

/// 记忆体检:断链 / 孤儿 / 重复描述 / 索引规模。
///
/// 只做机械检查(文件名与 frontmatter `description` 的字面比较),为设置页
/// 的"整理"提供确定性依据;语义层面的合并与改写由主代理在会话里完成——
/// 这里不发起模型调用,也不写任何文件。
pub fn memory_health(root: &Path) -> MemoryHealthReport {
    let index = read_index_text(root);
    let (index_lines, index_entries, index_bytes, broken_links, linked) =
        match index.as_deref() {
            Some(content) => {
                let mut broken = Vec::new();
                let mut linked: Vec<String> = Vec::new();
                let mut entries = 0usize;
                for line in content.lines() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() || trimmed.starts_with('#') {
                        continue;
                    }
                    entries += 1;
                    let Some(target) = entry_target(trimmed) else {
                        continue;
                    };
                    if linked.iter().any(|name| name == &target) {
                        continue;
                    }
                    if !root.join(&target).is_file() {
                        broken.push(target.clone());
                    }
                    linked.push(target);
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

    // 重复描述:description 字面相同的文件成组。只读每个文件头部 8 KiB
    // (frontmatter 在文件头,大文件不必全读)。
    let mut by_description: Vec<(String, String)> = Vec::new();
    for file in &manifest.files {
        let Some(description) = frontmatter_description(&root.join(&file.name)) else {
            continue;
        };
        by_description.push((description, file.name.clone()));
    }
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

/// 读文件头部(最多 8 KiB,UTF-8 边界不完整时 lossy)并提取 frontmatter
/// 的 `description` 值。
fn frontmatter_description(path: &Path) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    let _ = file.take(8 * 1024).read_to_end(&mut bytes);
    let buffer = String::from_utf8_lossy(&bytes);
    let mut lines = buffer.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines.take(40) {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("description:") {
            let value = value.trim().trim_matches('"').trim_matches('\'').trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::session::SessionEvent;

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
        assert_eq!(report.orphans, vec!["b.md".to_string()], "b.md 未登记进索引");
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

    fn user_envelope(text: &str, injected: bool) -> denia_core::session::SessionEnvelope {
        denia_core::session::SessionEnvelope {
            seq: 0,
            time: 0,
            event: SessionEvent::UserMessage {
                text: text.to_string(),
                injected,
                channel: None,
                images: Vec::new(),
            },
        }
    }

    fn envelope(seq: u64, event: SessionEvent) -> denia_core::session::SessionEnvelope {
        denia_core::session::SessionEnvelope {
            seq,
            time: 0,
            event,
        }
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
