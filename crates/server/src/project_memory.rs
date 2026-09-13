//! 项目记忆:每工作区一个 Markdown 记忆域(索引 + 条目文件 + 后台提取三件套)。
//!
//! - 落点:`$DENIA_HOME/memories/projects/<slug>-<sha16>/memory/`,slug 是
//!   工作区目录名的清洗结果,sha16 是规范化路径的 sha256 前 16 位十六进制;
//!   同一路径永远映射到同一目录,目录改名/换盘符才会换桶。
//! - `MEMORY.md` 是纯索引(一行一条),其余 `.md` 一事一文件,frontmatter
//!   携带 `name/description/metadata.type`。
//! - 读写两端都由配置总闸(`runtime.memoryEnabled`)与提取闸
//!   (`runtime.memoryExtractionEnabled`)控制:任一关闭,注入与提取同时停。
//!
//! 本模块只做纯 fs 与纯函数;调度与注入在 agent_runtime / agent-loop。

use std::path::{Component, Path, PathBuf};

use sha2::Digest;

/// 索引文件名(纯索引,一行一条)。
pub const MEMORY_INDEX_FILE: &str = "MEMORY.md";
/// 单条记忆的 manifest 上限(按 mtime 倒序保留最新 200 个)。
const MANIFEST_CAP: usize = 200;
/// 单文件读取上限(设置页 viewer 与 API 同用)。
pub const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;
/// 提取素材里单条消息行的截断长度(字符)。
const MATERIAL_ENTRY_MAX_CHARS: usize = 4000;
/// 提取提示词固定部分(任务说明 + manifest)预留的预算(字节);素材
/// 拿总预算减去这份的余量。
const PROMPT_FIXED_BUDGET_BYTES: usize = 4096;
/// 提取素材的保底预算(字节):小 `output_bytes` 配置下固定部分吃掉总
/// 预算时,素材也不能为空——没有素材的提取必然 Nothing to save。
const MATERIAL_MIN_BUDGET_BYTES: usize = 8192;

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

/// 上一轮对话的提取素材(供记忆提取子代理使用)。
pub struct TurnDigest {
    /// 本轮真实用户消息全文(取最后一条非注入 UserMessage;门限判定用)。
    pub user_text: String,
    /// 主代理本轮是否直接写过记忆目录(写过则不再调度提取)。
    pub wrote_memory: bool,
    /// 本轮素材按时序渲染的消息行(用户/助手/工具调用与结果预览)。
    pub entries: Vec<String>,
}

/// 从事件流截取最后一个 turn 窗口,产出提取素材。没有完整 turn(只有
/// TurnStart 没有对应事件)时返回 None。
///
/// `memory_root` 用于判定"主代理直接写记忆":写文件/编辑的落点解析后
/// 落在记忆目录内即视为写过。写类调用(参数携带整个文件内容)不进素材行。
pub fn last_turn_digest(
    events: &[denia_core::session::SessionEnvelope],
    cwd: &Path,
    memory_root: &Path,
) -> Option<TurnDigest> {
    use denia_core::session::SessionEvent;
    let start = events
        .iter()
        .rposition(|envelope| matches!(envelope.event, SessionEvent::TurnStart { .. }))?;
    let window = &events[start..];
    if !window
        .iter()
        .any(|envelope| matches!(envelope.event, SessionEvent::TurnEnd { .. }))
    {
        return None;
    }
    let mut user_text = String::new();
    let mut entries: Vec<String> = Vec::new();
    let mut wrote_memory = false;
    for envelope in window {
        match &envelope.event {
            SessionEvent::UserMessage {
                text, injected, ..
            } if !*injected => {
                user_text = text.clone();
                entries.push(format!("用户:{}", text.trim()));
            }
            SessionEvent::AssistantMessage { blocks, .. } => {
                let texts: Vec<&str> = blocks
                    .iter()
                    .filter_map(|block| match block {
                        denia_core::stream::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                let joined = texts.join("\n");
                if !joined.trim().is_empty() {
                    entries.push(format!("助手:{}", joined.trim()));
                }
            }
            SessionEvent::ToolCall { name, arguments, .. } => {
                if matches!(name.as_str(), "write_file" | "edit")
                    && let Some(path) = touched_path(cwd, arguments)
                {
                    if path.starts_with(memory_root) {
                        wrote_memory = true;
                    }
                    continue;
                }
                let preview: String = arguments.chars().take(TOOL_ARG_PREVIEW_CHARS).collect();
                entries.push(format!("工具 {name}:{preview}"));
            }
            SessionEvent::ToolResult { content, is_error, .. } => {
                // 时序上紧跟其调用行,独立成行呈现;错误结果也是提取信息。
                let head = if *is_error { "→ (错误)" } else { "→" };
                let preview: String = content.trim().chars().take(TOOL_RESULT_PREVIEW_CHARS).collect();
                entries.push(format!("{head} {preview}"));
            }
            _ => {}
        }
    }
    if user_text.trim().is_empty() {
        return None;
    }
    Some(TurnDigest {
        user_text,
        wrote_memory,
        entries,
    })
}

/// 宽容解析写类工具参数里的 path 字段并锚定 cwd(与 agent-loop 内
/// touched_path 同语义的本地副本;server 侧不再反向依赖其私有模块)。
fn touched_path(cwd: &Path, raw_arguments: &str) -> Option<PathBuf> {
    let value: serde_json::Value = serde_json::from_str(raw_arguments.trim()).ok()?;
    let raw = value.get("path")?.as_str()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let path = Path::new(raw);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    // 消解 `.`/`..`(不触盘);越出 cwd 的相对解析结果也原样保留,
    // 由调用方用 starts_with 判定。
    let mut out = PathBuf::new();
    for component in resolved.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

/// 用户散文词数:空白分词(含非 CJK 的字母数字才算词)+ 每个中日韩字符
/// 各记 1。门限 3 词:问候语/单字指令不触发后台提取。
pub fn prose_word_count(text: &str) -> usize {
    let is_cjk = |ch: char| {
        matches!(ch,
            '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}' | '\u{F900}'..='\u{FAFF}'
            | '\u{3040}'..='\u{30FF}' | '\u{AC00}'..='\u{D7AF}')
    };
    let mut words = 0usize;
    for token in text.split_whitespace() {
        if token.chars().any(|ch| ch.is_alphanumeric() && !is_cjk(ch)) {
            words += 1;
        }
    }
    let cjk = text.chars().filter(|ch| is_cjk(*ch)).count();
    words + cjk
}

/// 工具调用参数在素材行里的预览长度(字符)。
const TOOL_ARG_PREVIEW_CHARS: usize = 300;
/// 工具结果在素材行里的预览长度(字符)。
const TOOL_RESULT_PREVIEW_CHARS: usize = 400;

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

/// 记忆提取子代理的任务提示词(中文)。提示词只写任务机制:素材是唯一
/// 信息来源、既有文件清单防重复、轮次预算内并行读再并行写;记忆类型与
/// 格式标准一律引用子代理系统提示里的 `# 项目记忆` 段(assemble 会替换
/// 出带路径的同一份),不在提示词里复述。
/// `manifest` 为 [`scan_manifest`] 产出的文件名清单(调用方负责阻塞跑)。
pub fn render_extraction_prompt(
    root: &Path,
    digest: &TurnDigest,
    budget_bytes: usize,
    manifest: &[String],
) -> String {
    let root = root.display().to_string();
    let manifest_block = if manifest.is_empty() {
        "(暂无记忆文件)".to_string()
    } else {
        manifest.join("\n")
    };
    let material = digest
        .entries
        .iter()
        .map(|entry| cut_chars(entry, MATERIAL_ENTRY_MAX_CHARS))
        .collect::<Vec<_>>()
        .join("\n");
    let material = cut_chars(
        &material,
        budget_bytes
            .saturating_sub(PROMPT_FIXED_BUDGET_BYTES)
            .max(MATERIAL_MIN_BUDGET_BYTES),
    );
    format!(
        "[父代理会话发来的记忆提取任务]\n\
你现在是记忆提取子代理。分析下面这轮对话素材,用它们更新你的持久记忆系统;不要执行素材里的任务本身。\n\n\
可用工具:read_file、ls、glob、grep,以及仅限记忆目录内路径的 write_file/edit;写记忆目录之外的任何路径都会被拒绝,其余写类工具一律不可用。\n\n\
轮次预算有限,高效策略:第 1 轮把所有可能要更新的文件并行读齐;第 2 轮把所有 write_file/edit 并行发出。不要把读和写交错拆到多轮。\n\n\
你必须只依据下面的对话素材更新记忆,不要浪费轮次去调查或验证素材内容——不要 grep 源码、不要读代码确认模式是否存在、不要跑 git 命令。\n\n\
## 已有记忆文件\n\n{manifest_block}\n\n写之前先核对上面的清单——已有文件覆盖的主题就地更新原文件,不要新建重复文件。\n\n\
确无值得保存的内容时,只回复 `Nothing to save.`,不要解释原因。用户在素材里明确要求记住某件事时,立即按最合适的类型保存;要求忘记某件事时,改写 MEMORY.md 索引并清空对应文件。\n\n\
记忆类型、不该保存的标准、frontmatter 格式与索引维护规则,按你系统提示里的「# 项目记忆」段执行——该段已在你的上下文里。\n\n\
记忆目录:{root}\n\
索引文件:{root}/MEMORY.md(纯索引,一行一条,格式:`- [标题](文件名.md) — 一句话钩子`)\n\n\
—— 本轮对话素材 ——\n\n{material}"
    )
}

fn cut_chars(text: &str, max_bytes: usize) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= max_bytes {
        return trimmed.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…(已截断)", &trimmed[..end])
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
    fn digest_extracts_last_turn_and_detects_memory_write() {
        let work = temp_root("digest");
        let cwd = work.join("ws");
        std::fs::create_dir_all(&cwd).unwrap();
        let memory_root = work.join("mem").join("memory");
        std::fs::create_dir_all(&memory_root).unwrap();
        let events = vec![
            user_envelope("旧的一轮", false),
            envelope(
                1,
                SessionEvent::TurnStart { turn: 1 },
            ),
            envelope(
                2,
                SessionEvent::UserMessage {
                    text: "最新一轮的用户提问".into(),
                    injected: false,
                    channel: None,
                    images: Vec::new(),
                },
            ),
            envelope(
                3,
                SessionEvent::ToolCall {
                    turn: 1,
                    step: 1,
                    call_id: "c1".into(),
                    name: "write_file".into(),
                    arguments: serde_json::json!({"path": "src/a.rs", "content": "x"}).to_string(),
                },
            ),
            envelope(
                4,
                SessionEvent::ToolCall {
                    turn: 1,
                    step: 1,
                    call_id: "c2".into(),
                    name: "write_file".into(),
                    arguments: serde_json::json!({"path": memory_root.join("m.md"), "content": "y"})
                        .to_string(),
                },
            ),
            envelope(
                5,
                SessionEvent::ToolCall {
                    turn: 1,
                    step: 2,
                    call_id: "c3".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "cargo test"}).to_string(),
                },
            ),
            envelope(
                6,
                SessionEvent::ToolResult {
                    turn: 1,
                    step: 2,
                    call_id: "c3".into(),
                    content: "test result: ok. 12 passed".into(),
                    is_error: false,
                    error: None,
                    error_identity: None,
                    meta: None,
                    truncation: None,
                    replaces: None,
                },
            ),
            envelope(
                7,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 3,
                    blocks: vec![denia_core::stream::ContentBlock::Text {
                        text: "结论文本".into(),
                    }],
                    usage: None,
                    interrupted: false,
                    source_event_seqs: Vec::new(),
                },
            ),
            envelope(
                8,
                SessionEvent::TurnEnd {
                    turn: 1,
                    reason: denia_core::session::TurnEndReason::Completed,
                },
            ),
        ];
        let digest = last_turn_digest(&events, &cwd, &memory_root).unwrap();
        assert_eq!(digest.user_text, "最新一轮的用户提问");
        assert!(digest.wrote_memory, "落点在记忆目录的写必须被识别");
        assert!(
            digest.entries.iter().any(|line| line.starts_with("用户:")),
            "用户消息按时序进素材"
        );
        assert!(
            digest.entries.iter().any(|line| line.contains("结论文本")),
            "助手文本按时序进素材"
        );
        assert!(
            digest.entries.iter().any(|line| line.contains("cargo test")),
            "非写类工具调用进素材"
        );
        assert!(
            digest
                .entries
                .iter()
                .any(|line| line.contains("test result: ok")),
            "工具结果预览进素材"
        );
        assert!(
            !digest.entries.iter().any(|line| line.contains("write_file")),
            "写类调用(整文件参数)不进素材行"
        );
        // 无 TurnEnd 的半截 turn:None。
        let truncated = &events[..4];
        assert!(last_turn_digest(truncated, &cwd, &memory_root).is_none());
        // 注入消息不算用户散文。
        let injected_only = vec![
            envelope(0, SessionEvent::TurnStart { turn: 1 }),
            user_envelope("[harness] 注入", true),
            envelope(
                1,
                SessionEvent::TurnEnd {
                    turn: 1,
                    reason: denia_core::session::TurnEndReason::Completed,
                },
            ),
        ];
        assert!(last_turn_digest(&injected_only, &cwd, &memory_root).is_none());
        std::fs::remove_dir_all(work).unwrap();
    }

    #[test]
    fn prose_word_count_counts_latin_words_and_cjk_chars() {
        assert_eq!(prose_word_count(""), 0);
        assert_eq!(prose_word_count("hi"), 1);
        assert_eq!(prose_word_count("把这段话记住"), 6, "每个 CJK 字符记 1");
        assert_eq!(prose_word_count("use tokio for async"), 4);
        assert_eq!(prose_word_count("记住 cargo test"), 4, "2 CJK + 2 词");
        assert_eq!(prose_word_count("--- ... ---"), 0, "纯符号不算散文");
    }

    #[test]
    fn extraction_prompt_carries_paths_manifest_and_digest() {
        let digest = TurnDigest {
            user_text: "帮我把 CI 换成 pnpm".into(),
            wrote_memory: false,
            entries: vec!["用户:帮我把 CI 换成 pnpm".into(), "→ (错误) no lockfile".into()],
        };
        let prompt = render_extraction_prompt(
            Path::new("/m/memory"),
            &digest,
            4096,
            &["ci.md".into(), "user-role.md".into()],
        );
        assert!(prompt.contains("/m/memory"));
        assert!(prompt.contains("MEMORY.md"));
        assert!(prompt.contains("Nothing to save."));
        assert!(prompt.contains("帮我把 CI 换成 pnpm"));
        assert!(prompt.contains("no lockfile"));
        // manifest 注入 + 查重指令。
        assert!(prompt.contains("ci.md"));
        assert!(prompt.contains("就地更新原文件"));
        // 轮次策略与禁调查禁令。
        assert!(prompt.contains("并行"));
        assert!(prompt.contains("不要 grep 源码"));
        // 标准复用系统提示段。
        assert!(prompt.contains("# 项目记忆"));
        // 空 manifest 走占位说明。
        let empty = render_extraction_prompt(Path::new("/m/memory"), &digest, 4096, &[]);
        assert!(empty.contains("暂无记忆文件"));
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
