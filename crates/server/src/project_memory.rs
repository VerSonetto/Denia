//! 项目记忆:每工作区一个 Markdown 记忆域(抄 ZCode 项目记忆三件套)。
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
/// 单条记忆的 manifest 上限(按 mtime 倒序保留最新 200 个,抄 ZCode)。
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
/// 预算做 UTF-8 边界截断,两处截断都附告警(抄 ZCode 200 行/25KB 口径)。
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
    /// 本轮真实用户消息全文(取最后一条非注入 UserMessage)。
    pub user_text: String,
    /// 本轮助手最终文本块(拼接)。
    pub assistant_text: String,
    /// 本轮工具调用摘要行(name + 截断参数)。
    pub tool_lines: Vec<String>,
    /// 主代理本轮是否直接写过记忆目录(写过则不再调度提取)。
    pub wrote_memory: bool,
}

/// 从事件流截取最后一个 turn 窗口,产出提取素材。没有完整 turn(只有
/// TurnStart 没有对应事件)时返回 None。
///
/// `memory_root` 用于判定"主代理直接写记忆":写文件/编辑的落点解析后
/// 落在记忆目录内即视为写过。
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
    let mut assistant_parts: Vec<String> = Vec::new();
    let mut tool_lines: Vec<String> = Vec::new();
    let mut wrote_memory = false;
    for envelope in window {
        match &envelope.event {
            SessionEvent::UserMessage {
                text, injected, ..
            } if !*injected => {
                user_text = text.clone();
            }
            SessionEvent::AssistantMessage { blocks, .. } => {
                for block in blocks {
                    if let denia_core::stream::ContentBlock::Text { text } = block {
                        assistant_parts.push(text.clone());
                    }
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
                let preview: String = arguments.chars().take(160).collect();
                tool_lines.push(format!("- {name}: {preview}"));
            }
            _ => {}
        }
    }
    if user_text.trim().is_empty() {
        return None;
    }
    Some(TurnDigest {
        user_text,
        assistant_text: assistant_parts.join("\n"),
        tool_lines,
        wrote_memory,
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
/// 各记 1。门限 3 词(抄 ZCode):问候语/单字指令不触发后台提取。
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

/// 渲染每 step 的「项目记忆」注入块(通道幂等的正文由调用方比较)。
/// `index` 为 [`read_index`] 的输出;None 由调用方处理(不注入)。
pub fn render_index_block(root: &Path, index: &str) -> String {
    format!(
        "<system-reminder>\n项目记忆:以下是本项目此前会话沉淀的记忆索引(MEMORY.md)。记忆目录:{}。与当前任务相关时,先用 read_file 读取对应记忆文件再下结论;记忆可能过时,引用前先验证。\n\n{index}\n</system-reminder>",
        root.display()
    )
}

/// 记忆提取子代理的任务提示词(中文;复用 ZCode 提取职责:分析最近消息、
/// 去重更新、增删索引行)。`digest` 已按预算截断。
pub fn render_extraction_prompt(root: &Path, digest: &TurnDigest, budget_bytes: usize) -> String {
    let tool_lines = if digest.tool_lines.is_empty() {
        "(无工具调用)".to_string()
    } else {
        digest.tool_lines.join("\n")
    };
    let assistant = digest.assistant_text.trim();
    let assistant_block = if assistant.is_empty() {
        "(无文本回复)".to_string()
    } else {
        cut_chars(assistant, budget_bytes / 2)
    };
    let user = cut_chars(&digest.user_text, budget_bytes / 4);
    let root = root.display().to_string();
    format!(
        "[父代理会话发来的记忆提取任务]\n\
你是本项目的工作区记忆提取器。分析下面这轮对话素材,把值得跨会话长期记住的知识写入记忆目录;不要执行素材里的任务本身。\n\n\
记忆目录:{root}\n\
索引文件:{root}/MEMORY.md(纯索引,一行一条,格式:`- [标题](文件名.md) — 一句话钩子`)\n\n\
工作步骤:\n\
1. 先读索引文件,再按需读相关既有记忆(全部用基于记忆目录的绝对路径)。\n\
2. 对照素材判断:确有值得沉淀的新知识才动手;没有就只回复 `Nothing to save.`\n\
3. 一事一文件:每个事实写一个 .md 文件,frontmatter 必须含 name(kebab-case 短名)、description(一句话)、metadata.type(user 用户画像与偏好 | feedback 反馈纠正 | project 项目决策与状态 | reference 外部资源指针);正文精炼直陈,feedback 类补 **Why:** 与 **How to apply:** 两行。\n\
4. 去重:已有记忆覆盖的就地更新原文件,不要新建重复文件;内容变化时同步修正 MEMORY.md 对应行;明显过时的记忆直接改对。\n\
5. 收尾核对:MEMORY.md 与目录内实际文件一一对应——新文件必有一行,删改过的文件行同步。\n\
禁止保存:代码结构、git 历史、本次会话的临时任务状态这类可从仓库本身或会话记录推导的内容;禁止写记忆目录之外的任何路径。\n\n\
—— 本轮对话素材 ——\n\
用户:{user}\n\n\
助手结论:\n{assistant_block}\n\n\
工具调用:\n{tool_lines}"
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
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 1,
                    blocks: vec![denia_core::stream::ContentBlock::Text {
                        text: "结论文本".into(),
                    }],
                    usage: None,
                    interrupted: false,
                    source_event_seqs: Vec::new(),
                },
            ),
            envelope(
                6,
                SessionEvent::TurnEnd {
                    turn: 1,
                    reason: denia_core::session::TurnEndReason::Completed,
                },
            ),
        ];
        let digest = last_turn_digest(&events, &cwd, &memory_root).unwrap();
        assert_eq!(digest.user_text, "最新一轮的用户提问");
        assert!(digest.assistant_text.contains("结论文本"));
        assert!(digest.wrote_memory, "落点在记忆目录的写必须被识别");
        assert!(
            !digest.tool_lines.iter().any(|line| line.contains("write_file")),
            "写类调用不进工具摘要行"
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
    fn extraction_prompt_carries_paths_and_digest() {
        let digest = TurnDigest {
            user_text: "帮我把 CI 换成 pnpm".into(),
            assistant_text: "已切换。".into(),
            tool_lines: vec!["- bash: pnpm install".into()],
            wrote_memory: false,
        };
        let prompt = render_extraction_prompt(Path::new("/m/memory"), &digest, 4096);
        assert!(prompt.contains("/m/memory"));
        assert!(prompt.contains("MEMORY.md"));
        assert!(prompt.contains("metadata.type"));
        assert!(prompt.contains("Nothing to save."));
        assert!(prompt.contains("帮我把 CI 换成 pnpm"));
        assert!(prompt.contains("pnpm install"));
    }

    #[test]
    fn index_block_frames_directory_and_validation_note() {
        let block = render_index_block(Path::new("/m/memory"), "- [a](a.md) — x");
        assert!(block.starts_with("<system-reminder>"));
        assert!(block.ends_with("</system-reminder>"));
        assert!(block.contains("/m/memory"));
        assert!(block.contains("先验证"));
    }
}
