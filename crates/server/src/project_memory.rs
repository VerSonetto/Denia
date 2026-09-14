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

/// 读 `MEMORY.md` 全文;缺失/空白返回 None。先按 200 行截行,再按字节
/// 预算做 UTF-8 边界截断,两处截断都附告警(200 行 / 25KB 口径)。
pub fn read_index(root: &Path, max_bytes: usize) -> Option<String> {
    let raw = std::fs::read_to_string(root.join(MEMORY_INDEX_FILE)).ok()?;
    let content = raw.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    if content.trim().is_empty() {
        return None;
    }
    let mut body = truncate_lines(&content, 200);
    if body.len() > max_bytes {
        body = truncate_utf8(&body, max_bytes);
        body.push_str("\n\n（项目记忆索引超出字节预算，已截断。）");
    }
    Some(body)
}

/// 保留前 `max_lines` 行并附告警;总行数不超限时原样返回。
fn truncate_lines(content: &str, max_lines: usize) -> String {
    let mut ends: Vec<usize> = content
        .match_indices('\n')
        .map(|(index, _)| index)
        .take(max_lines)
        .collect();
    let total_lines = content.lines().count();
    if total_lines <= max_lines {
        return content.to_string();
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
    body.push_str("\n（项目记忆索引超过 200 行，已截断。）");
    body
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
                if dir == root && path.file_name().is_some_and(|name| name == MEMORY_INDEX_FILE) {
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

/// 渲染每 step 的「项目记忆」注入块(通道幂等的正文由调用方比较)。
/// `index` 为 [`read_index`] 的输出;None 由调用方处理(不注入)。
/// 注入块只负责把索引内容与目录位置交给模型,使用与验证规则在
/// `# 项目记忆` 段,不重复纪律。
pub fn render_index_block(root: &Path, index: &str) -> String {
    format!(
        "<system-reminder>\n{}/MEMORY.md 的内容(用户的自动记忆,跨会话持久):\n\n{index}\n</system-reminder>",
        root.display()
    )
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
            read_index(&root, 1024).unwrap(),
            "- a\n- b\n- c\n",
            "未超限时原样返回"
        );
        // 超 200 行:截行 + 告警。
        let many: String = (0..300).map(|i| format!("- line{i}\n")).collect();
        std::fs::write(root.join(MEMORY_INDEX_FILE), &many).unwrap();
        let cut = read_index(&root, 1_048_576).unwrap();
        assert!(cut.contains("已截断"));
        assert!(cut.contains("- line199"));
        assert!(!cut.contains("- line200"));
        // 超字节预算:UTF-8 边界截断 + 告警。
        std::fs::write(root.join(MEMORY_INDEX_FILE), "好".repeat(100)).unwrap();
        let cut = read_index(&root, 8).unwrap();
        assert!(cut.starts_with("好好"), "预算 8 字节只能装下 2 个汉字:{cut}");
        assert!(!cut.contains("好好好"));
        assert!(cut.contains("字节预算"));
        // 空文件与缺失都返回 None。
        std::fs::write(root.join(MEMORY_INDEX_FILE), "   \n").unwrap();
        assert!(read_index(&root, 1024).is_none());
        std::fs::remove_file(root.join(MEMORY_INDEX_FILE)).unwrap();
        assert!(read_index(&root, 1024).is_none());
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
        let file = std::fs::File::options().append(true).open(root.join("b.md")).unwrap();
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
        assert!(resolve_manifest_file(&root, "/a.md").is_ok(), "前导斜杠被剥掉");
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
        denia_core::session::SessionEnvelope { seq, time: 0, event }
    }

    #[test]
    fn index_block_frames_auto_memory_index() {
        let block = render_index_block(Path::new("/m/memory"), "- [a](a.md) — x");
        assert!(block.starts_with("<system-reminder>"));
        assert!(block.ends_with("</system-reminder>"));
        assert!(block.contains("/m/memory/MEMORY.md"));
        assert!(block.contains("跨会话持久"));
    }
}
