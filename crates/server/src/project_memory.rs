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

/// 索引正文的行硬上限:选择后的正文(结构行 + 选中条目)仍超限才截断,
/// 截断告警带总行数/总条目文件数与全文指针。
const INDEX_MAX_LINES: usize = 200;
/// 选择层最多列出的条目行数:命中再多也只列这么多,其余走全文指针。
const INDEX_SELECT_MAX_ENTRIES: usize = 60;
/// 没有任何信号(无关键词、无触碰路径)时保底列出的条目行数:给模型一个
/// 可用起点,而不是让整块索引静默消失。
const INDEX_SELECT_FALLBACK_ENTRIES: usize = 20;
/// 触碰路径命中条目的加分:路径相关比词面命中更可信,超上限时优先留。
const TOUCHED_HIT_SCORE: usize = 3;
/// 参与匹配的 ASCII 标识符最短长度(滤掉 `id`/`ok` 一类噪声)。
const MIN_ASCII_TOKEN: usize = 3;
/// 参与匹配的触碰路径组件最短长度。
const MIN_PATH_NEEDLE: usize = 3;

/// 一次索引注入的选择结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSelection {
    /// 选中的索引正文(保持索引原顺序,标题与空行原样保留)。
    pub body: String,
    /// 被省略的条目行数(渲染层据此附全文指针)。
    pub omitted: usize,
    /// 索引里的条目行总数(不含标题与空行)。
    pub total: usize,
}

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

/// 注入通道入口:读索引 → 按触碰路径与最近用户消息选择条目 → 行/字节预算。
///
/// 选择结果**只由输入决定**:同一 `(cwd, touched, recent_user, 索引文本)`
/// 永远给出同一正文(无时间戳、无随机序、去重与排序都走确定序),通道的
/// 幂等比较因此不会因为无关变化抖动。索引缺失或纯空白返回 None(空桶不注入)。
pub fn read_index_for_injection(
    root: &Path,
    cwd: &Path,
    touched: &[PathBuf],
    recent_user: Option<&str>,
    max_bytes: usize,
) -> Option<IndexSelection> {
    let content = read_index_text(root)?;
    let total_lines = content.lines().count();
    let mut selection = select_index_lines(&content, cwd, touched, recent_user);
    let (body, cut) = truncate_lines(&selection.body, INDEX_MAX_LINES);
    selection.body = body;
    if cut {
        // 只有病态索引(结构行自己就超限)才走到这里,到这一步才扫目录取文件数。
        let files = scan_manifest(root).files.len();
        selection.body.push_str(&format!(
            "\n（项目记忆索引超过 {INDEX_MAX_LINES} 行，已截断：共 {total_lines} 行、{files} 个条目文件，用 read_file 读 {}/MEMORY.md 获取全文。）",
            root.display()
        ));
    }
    if selection.body.len() > max_bytes {
        selection.body = truncate_utf8(&selection.body, max_bytes);
        selection.body.push_str(&format!(
            "\n\n（项目记忆索引超出字节预算，已截断：用 read_file 读 {}/MEMORY.md 获取全文。）",
            root.display()
        ));
    }
    Some(selection)
}

/// 按触碰路径与最近用户消息选择索引行(纯函数,不碰 io)。
///
/// 口径:
/// - 标题(`#` 开头)与空行是结构,恒保留,不计入条目数;
/// - 其余每行算一个条目,得分 = 触碰路径命中 × [`TOUCHED_HIT_SCORE`] +
///   关键词命中 × 1;
/// - 有命中的条目按得分优先保留(同分保持索引原顺序),最多
///   [`INDEX_SELECT_MAX_ENTRIES`] 条;
/// - 一个都没命中时退回索引最前 [`INDEX_SELECT_FALLBACK_ENTRIES`] 条;
/// - 被省略的条目数交给渲染层写全文指针,全文按需 `read_file`。
pub fn select_index_lines(
    content: &str,
    cwd: &Path,
    touched: &[PathBuf],
    recent_user: Option<&str>,
) -> IndexSelection {
    let lines: Vec<&str> = content.lines().collect();
    let tokens = recent_user.map(query_tokens).unwrap_or_default();
    let needles = touched_needles(cwd, touched);
    let mut entries: Vec<usize> = Vec::new();
    let mut matched: Vec<(usize, usize)> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        entries.push(index);
        let score = entry_score(&line.to_lowercase(), &tokens, &needles);
        if score > 0 {
            matched.push((index, score));
        }
    }
    let mut keep = vec![false; lines.len()];
    let kept = if matched.is_empty() {
        for index in entries.iter().take(INDEX_SELECT_FALLBACK_ENTRIES) {
            keep[*index] = true;
        }
        entries.len().min(INDEX_SELECT_FALLBACK_ENTRIES)
    } else {
        // 得分降序、同分按索引原顺序:确定性排序,与 HashMap 迭代序无关。
        matched.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        matched.truncate(INDEX_SELECT_MAX_ENTRIES);
        for (index, _) in &matched {
            keep[*index] = true;
        }
        matched.len()
    };
    let mut body = lines
        .iter()
        .enumerate()
        .filter(|(index, line)| {
            if keep[*index] {
                return true;
            }
            let trimmed = line.trim();
            trimmed.is_empty() || trimmed.starts_with('#')
        })
        .map(|(_, line)| *line)
        .collect::<Vec<_>>()
        .join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    IndexSelection {
        body,
        omitted: entries.len() - kept,
        total: entries.len(),
    }
}

/// 条目行得分:触碰路径命中比词面命中更可信。
fn entry_score(lower_line: &str, tokens: &[String], needles: &[String]) -> usize {
    let mut score = 0;
    for needle in needles {
        if lower_line.contains(needle.as_str()) {
            score += TOUCHED_HIT_SCORE;
        }
    }
    for token in tokens {
        if lower_line.contains(token.as_str()) {
            score += 1;
        }
    }
    score
}

/// 查询文本的匹配单元:ASCII 标识符(≥ [`MIN_ASCII_TOKEN`] 字符,小写)
/// 与 CJK 双字组。同一文本永远给出同一序列,重复单元只留第一次。
fn query_tokens(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut ascii = String::new();
    let mut cjk: Vec<char> = Vec::new();
    for ch in text.chars() {
        if is_cjk(ch) {
            flush_ascii(&mut tokens, &mut ascii);
            cjk.push(ch);
        } else if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/') {
            flush_cjk(&mut tokens, &mut cjk);
            ascii.push(ch.to_ascii_lowercase());
        } else {
            flush_ascii(&mut tokens, &mut ascii);
            flush_cjk(&mut tokens, &mut cjk);
        }
    }
    flush_ascii(&mut tokens, &mut ascii);
    flush_cjk(&mut tokens, &mut cjk);
    tokens
}

fn flush_ascii(tokens: &mut Vec<String>, ascii: &mut String) {
    if ascii.chars().count() >= MIN_ASCII_TOKEN
        && !tokens.iter().any(|token| token.as_str() == ascii.as_str())
    {
        tokens.push(ascii.clone());
    }
    ascii.clear();
}

fn flush_cjk(tokens: &mut Vec<String>, cjk: &mut Vec<char>) {
    for pair in cjk.windows(2) {
        let token: String = pair.iter().collect();
        if !tokens.contains(&token) {
            tokens.push(token);
        }
    }
    cjk.clear();
}

/// 中日韩文字(匹配目标是记忆索引的自由文本,够用即可)。
fn is_cjk(ch: char) -> bool {
    matches!(ch as u32, 0x3040..=0x30ff | 0x3400..=0x9fff | 0xac00..=0xd7af | 0xf900..=0xfaff)
}

/// 触碰路径派生的匹配面:文件名、去扩展名的 stem、所在目录名、相对 cwd 的
/// 整条路径(均 ≥ [`MIN_PATH_NEEDLE`] 字符,小写去重,相对路径按 cwd 落位)。
fn touched_needles(cwd: &Path, touched: &[PathBuf]) -> Vec<String> {
    let mut needles: Vec<String> = Vec::new();
    for path in touched {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(path)
        };
        let mut parts: Vec<String> = Vec::new();
        if let Some(name) = absolute.file_name() {
            parts.push(name.to_string_lossy().to_string());
        }
        if let Some(stem) = absolute.file_stem() {
            parts.push(stem.to_string_lossy().to_string());
        }
        if let Some(dir) = absolute.parent().and_then(Path::file_name) {
            parts.push(dir.to_string_lossy().to_string());
        }
        if let Ok(relative) = absolute.strip_prefix(cwd) {
            parts.push(relative.to_string_lossy().replace('\\', "/"));
        }
        for part in parts {
            let needle = part.trim().to_lowercase();
            if needle.chars().count() >= MIN_PATH_NEEDLE && !needles.contains(&needle) {
                needles.push(needle);
            }
        }
    }
    needles
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

/// 渲染每 step 的「项目记忆」注入块(通道幂等的正文由调用方比较)。
/// `index` 为 [`read_index_for_injection`] 的正文;`omitted` 是被省略的
/// 条目行数,>0 时附全文指针——被选择过的索引必须让模型知道还有东西没
/// 看到、以及去哪里看(条目正文按需 `read_file`)。None 由调用方处理(不注入)。
/// 注入块只负责把索引内容与目录位置交给模型,使用与验证规则在
/// `# 项目记忆` 段,不重复纪律。
pub fn render_index_block(root: &Path, index: &str, omitted: usize) -> String {
    let pointer = if omitted > 0 {
        format!(
            "\n\n（另有 {omitted} 条未列出，用 read_file 读 {}/MEMORY.md（或直接搜该目录）查看。）",
            root.display()
        )
    } else {
        String::new()
    };
    format!(
        "<system-reminder>\n{}/MEMORY.md 的内容(用户的自动记忆,跨会话持久):\n\n{index}{pointer}\n</system-reminder>",
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
        let cwd = Path::new("/work");
        std::fs::write(root.join(MEMORY_INDEX_FILE), "- a\n- b\n- c\n").unwrap();
        assert_eq!(
            read_index_for_injection(&root, cwd, &[], None, 1024)
                .unwrap()
                .body,
            "- a\n- b\n- c\n",
            "未超限时原样返回"
        );
        // 病态索引:结构行(标题)自己就超过 200 行,截行 + 告警带总行数/文件数。
        let many: String = (0..300).map(|i| format!("# h{i}\n")).collect();
        std::fs::write(root.join(MEMORY_INDEX_FILE), &many).unwrap();
        std::fs::write(root.join("one.md"), "1").unwrap();
        std::fs::write(root.join("two.md"), "2").unwrap();
        let cut = read_index_for_injection(&root, cwd, &[], None, 1_048_576)
            .unwrap()
            .body;
        assert!(cut.contains("已截断"));
        assert!(cut.contains("# h199"));
        assert!(!cut.contains("# h200"));
        assert!(cut.contains("共 300 行"), "{cut}");
        assert!(cut.contains("2 个条目文件"), "{cut}");
        // 超字节预算:UTF-8 边界截断 + 告警。
        std::fs::write(root.join(MEMORY_INDEX_FILE), "好".repeat(100)).unwrap();
        let cut = read_index_for_injection(&root, cwd, &[], None, 8)
            .unwrap()
            .body;
        assert!(
            cut.starts_with("好好"),
            "预算 8 字节只能装下 2 个汉字:{cut}"
        );
        assert!(!cut.contains("好好好"));
        assert!(cut.contains("字节预算"));
        // 空文件与缺失都返回 None(空桶不注入)。
        std::fs::write(root.join(MEMORY_INDEX_FILE), "   \n").unwrap();
        assert!(read_index_for_injection(&root, cwd, &[], None, 1024).is_none());
        assert!(read_index_text(&root).is_none());
        std::fs::remove_file(root.join(MEMORY_INDEX_FILE)).unwrap();
        assert!(read_index_for_injection(&root, cwd, &[], None, 1024).is_none());
        std::fs::remove_dir_all(work).unwrap();
    }

    /// 选择口径:触碰路径命中、关键词命中、两者都不命中(退回索引开头)。
    /// 同一输入必须给出同一输出——注入通道的幂等比较全靠这一条。
    #[test]
    fn select_index_lines_matches_touched_paths_and_keywords() {
        let cwd = Path::new("/work");
        let index = "# 项目记忆\n\n- [注入通道](injections.md) — 每步注入与幂等基准\n- [面板配色](frontend-css.md) — 设置页样式\n- [记忆选择](memory-select.md) — 索引选择层\n";

        // 触碰路径命中:`.../src/injections.rs` 的 stem 与文件名命中条目。
        let touched = [PathBuf::from("crates/agent-loop/src/injections.rs")];
        let by_path = select_index_lines(index, cwd, &touched, None);
        assert!(by_path.body.contains("injections.md"), "{}", by_path.body);
        assert!(!by_path.body.contains("frontend-css.md"));
        assert!(!by_path.body.contains("memory-select.md"));
        assert!(by_path.body.contains("# 项目记忆"), "标题恒保留");
        assert_eq!(by_path.total, 3);
        assert_eq!(by_path.omitted, 2);
        assert_eq!(
            by_path,
            select_index_lines(index, cwd, &touched, None),
            "同一输入必须给出同一输出"
        );

        // 关键词命中(中文按双字组匹配,不引入分词依赖)。
        let by_keyword = select_index_lines(index, cwd, &[], Some("把设置页的样式调一下"));
        assert!(
            by_keyword.body.contains("frontend-css.md"),
            "{}",
            by_keyword.body
        );
        assert_eq!(by_keyword.omitted, 2);

        // 两个信号都没有:退回索引最前若干条,而不是整块消失。
        let none = select_index_lines(index, cwd, &[], None);
        assert_eq!(none.omitted, 0, "条目数在保底上限内应全列出");
        assert!(none.body.contains("memory-select.md"));
        assert_eq!(none.total, 3);
    }

    /// 命中数超上限时按得分裁剪(触碰路径优先级更高),省略数如实统计;
    /// 无信号时退回保底条数而不是全量注入。
    #[test]
    fn select_index_lines_caps_entries_and_reports_omitted() {
        let cwd = Path::new("/work");
        let index: String = (0..30)
            .map(|i| format!("- [条目{i}](file{i}.md) — 说明{i}\n"))
            .collect();
        let fallback = select_index_lines(&index, cwd, &[], None);
        assert_eq!(fallback.total, 30);
        assert_eq!(fallback.omitted, 30 - INDEX_SELECT_FALLBACK_ENTRIES);
        assert!(fallback.body.contains("条目0"));
        assert!(!fallback.body.contains("条目20"));

        // 全部命中的大索引:条目行上限生效。
        let big: String = (0..80)
            .map(|i| format!("- [条目{i}](file{i}.md) — 说明\n"))
            .collect();
        let capped = select_index_lines(&big, cwd, &[], Some("说明"));
        assert_eq!(capped.total, 80);
        assert_eq!(capped.omitted, 80 - INDEX_SELECT_MAX_ENTRIES);
        assert!(capped.body.contains("条目0"));
        assert!(
            capped
                .body
                .contains(&format!("条目{}", INDEX_SELECT_MAX_ENTRIES - 1))
        );
        assert!(
            !capped
                .body
                .contains(&format!("条目{INDEX_SELECT_MAX_ENTRIES}")),
            "超出上限的条目不得列出"
        );

        // 触碰路径命中(得分更高)的条目即便排在很后面也必须保留。
        let with_path: String = (0..70)
            .map(|i| {
                if i == 65 {
                    "- [触碰项](special.md) — 说明65\n".to_string()
                } else {
                    format!("- [条目{i}](file{i}.md) — 说明{i}\n")
                }
            })
            .collect();
        let picked = select_index_lines(
            &with_path,
            cwd,
            &[PathBuf::from("special.md")],
            Some("说明"),
        );
        assert_eq!(picked.omitted, 70 - INDEX_SELECT_MAX_ENTRIES);
        assert!(picked.body.contains("触碰项"), "{}", picked.body);
    }

    /// 端到端(读 → 选 → 渲染):触碰路径命中的条目进正文,其余走省略指针。
    #[test]
    fn injection_reads_selects_and_points_to_the_full_index() {
        let work = temp_root("inject");
        let root = work.join("memory");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(MEMORY_INDEX_FILE),
            "- [注入通道](injections.md) — 注入与幂等基准\n- [面板配色](frontend-css.md) — 设置页样式\n",
        )
        .unwrap();
        let selection = read_index_for_injection(
            &root,
            Path::new("/work"),
            &[PathBuf::from("crates/agent-loop/src/injections.rs")],
            None,
            1_048_576,
        )
        .unwrap();
        assert_eq!(selection.omitted, 1);
        let block = render_index_block(&root, &selection.body, selection.omitted);
        assert!(block.contains("injections.md"));
        assert!(!block.contains("frontend-css.md"));
        assert!(block.contains("另有 1 条未列出"), "{block}");
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
        let block = render_index_block(root, "- [a](a.md) — x", 0);
        assert!(block.starts_with("<system-reminder>"));
        assert!(block.ends_with("</system-reminder>"));
        assert!(block.contains("/m/memory/MEMORY.md"));
        assert!(block.contains("跨会话持久"));
        assert!(!block.contains("另有"), "没有省略时不得出现指针:{block}");
    }

    /// 省略指针:被选择过的索引必须让模型知道还有条目没看到、去哪里看。
    #[test]
    fn index_block_points_to_the_full_index_when_entries_are_omitted() {
        let root = Path::new("/m/memory");
        let block = render_index_block(root, "- [a](a.md) — x", 7);
        assert!(block.contains("另有 7 条未列出"), "{block}");
        assert!(
            block.contains("用 read_file 读 /m/memory/MEMORY.md"),
            "{block}"
        );
        assert!(block.ends_with("</system-reminder>"), "{block}");
    }
}
