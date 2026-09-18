//! 工作区指令（AGENTS.md）的发现与预算渲染；对齐 dsh agent-instructions 语义：
//! 注入式 user 消息、字节预算内 UTF-8 边界截断、整块基线替换（不重写冻结历史）。
//!
//! 作用域（从宽到窄）：用户全局（`$DENIA_HOME/AGENTS.md`）→ 项目根到会话 cwd
//! 的祖先链每层 → 工具触碰文件落进 cwd 之下的后代目录链。候选文件只认
//! `AGENTS.md`；同目录单候选，无需 dsh 的多候选内容去重。
use std::path::{Path, PathBuf};

/// 一份已读取的指令文件；`display_path` 是模型可见的来源标签。
pub struct InstructionFile {
    pub display_path: String,
    pub content: String,
}

/// 渲染预算之外的单文件读取上限；超过即整份跳过（对齐 dsh readBounded）。
pub fn discover(
    home: &Path,
    cwd: &Path,
    touched: &[PathBuf],
    max_source_bytes: u64,
) -> Vec<InstructionFile> {
    let cwd = cwd.to_path_buf();
    let mut dirs: Vec<PathBuf> = Vec::new();
    let push_dir = |dirs: &mut Vec<PathBuf>, dir: PathBuf| {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    };
    // 用户全局最宽，排最前。
    push_dir(&mut dirs, home.to_path_buf());
    // 项目根：cwd 向上找 .git（skills.rs 同款）；祖先链从宽到窄。
    let project_root = cwd
        .ancestors()
        .find(|p| p.join(".git").exists())
        .unwrap_or(&cwd)
        .to_path_buf();
    for dir in ancestor_chain(&project_root, &cwd) {
        push_dir(&mut dirs, dir);
    }
    // 触碰文件的后代目录链：cwd 之下、从浅到深（dsh descendantDirsBetween）。
    for path in touched {
        for dir in descendant_dirs(&cwd, path) {
            push_dir(&mut dirs, dir);
        }
    }
    dirs.into_iter()
        .filter_map(|dir| load_file(&dir, &project_root, home, max_source_bytes))
        .collect()
}

/// 根到 cwd 的目录链（含两端），从宽到窄。
fn ancestor_chain(root: &Path, cwd: &Path) -> Vec<PathBuf> {
    cwd.ancestors()
        .take_while(|p| p.starts_with(root))
        .map(Path::to_path_buf)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// cwd 与触碰文件父目录之间的后代目录链（不含 cwd）；越出 cwd 的触碰忽略。
fn descendant_dirs(cwd: &Path, touched: &Path) -> Vec<PathBuf> {
    let absolute = if touched.is_absolute() {
        touched.to_path_buf()
    } else {
        cwd.join(touched)
    };
    let Some(parent) = absolute.parent() else {
        return Vec::new();
    };
    if !parent.starts_with(cwd) || parent == cwd {
        return Vec::new();
    }
    parent
        .ancestors()
        .take_while(|p| p.starts_with(cwd))
        .filter(|p| *p != cwd)
        .map(Path::to_path_buf)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn load_file(
    dir: &Path,
    project_root: &Path,
    home: &Path,
    max_source_bytes: u64,
) -> Option<InstructionFile> {
    let path = dir.join("AGENTS.md");
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() || metadata.len() > max_source_bytes {
        return None;
    }
    let raw = std::fs::read_to_string(&path).ok()?;
    // BOM 与 CRLF 归一：Windows 编辑器常态，归一后字节预算与幂等比较才稳定。
    let content = raw.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let display_path = if dir == home {
        "~/AGENTS.md".to_string()
    } else {
        path.strip_prefix(project_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/")
    };
    Some(InstructionFile {
        display_path,
        content,
    })
}

const REMINDER_OPEN: &str = "<system-reminder>";
const REMINDER_OPEN_CLOSE: &str = "</system-reminder>";
const INTRO_BASELINE: &str = "工作区指令:以下工作区指令可能与当前工作相关，适用时作为指导。更具体的指令优先于更宽泛的指令；它们不覆盖系统提示词或用户直接下达的指令。";
const INTRO_REPLACEMENT: &str = "工作区指令:此完整工作区指令基线取代之前所有工作区指令基线。以下指令可能与当前工作相关，适用时作为指导。更具体的指令优先于更宽泛的指令；它们不覆盖系统提示词或用户直接下达的指令。";
const INTRO_EMPTY_REPLACEMENT: &str = "工作区指令:此完整工作区指令基线取代之前所有工作区指令基线。当前没有激活的工作区指令。";
const TRUNCATED_NOTICE: &str = "\n\n（工作区指令超出字节预算，已截断。）";

/// 提取一条本通道注入文本的正文（去掉框架与引导语行）；供新旧内容比较。
fn extract_body(message: &str) -> &str {
    let inner = message
        .strip_prefix(REMINDER_OPEN)
        .and_then(|s| s.strip_prefix('\n'))
        .and_then(|s| s.strip_suffix(REMINDER_OPEN_CLOSE))
        .and_then(|s| s.strip_suffix('\n'))
        .unwrap_or("");
    match inner.split_once("\n\n") {
        Some((_, body)) => body,
        None => "",
    }
}

/// 计算本步应注入的完整文本；返回 None 表示无需注入。
///
/// 引导语的选择由**正文内容是否变化**决定，而不是"日志里有没有旧基线"：
/// 首次（无旧文本）用基线引导语；旧文本存在但正文相同 → 不注入（幂等）；
/// 正文变化才切换"取代"引导语。否则注入基线后 `has_baseline` 翻转会
/// 让同一段正文以"取代"版自我扰动地再注入一次。
pub fn render(
    files: &[InstructionFile],
    max_bytes: usize,
    previous: Option<&str>,
) -> Option<String> {
    let mut body = String::new();
    for file in files {
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str(&format!(
            "来自 {} 的指令:\n\n{}",
            file.display_path, file.content
        ));
    }
    if body.len() > max_bytes {
        body = truncate_utf8(&body, max_bytes);
        body.push_str(TRUNCATED_NOTICE);
    }
    let frame = |intro: &str, body: &str| {
        if body.is_empty() {
            format!("{REMINDER_OPEN}\n{intro}\n{REMINDER_OPEN_CLOSE}")
        } else {
            format!("{REMINDER_OPEN}\n{intro}\n\n{body}\n{REMINDER_OPEN_CLOSE}")
        }
    };
    match previous {
        None => (!body.is_empty()).then(|| frame(INTRO_BASELINE, &body)),
        Some(prev) => {
            if extract_body(prev) == body {
                return None;
            }
            let intro = if body.is_empty() {
                INTRO_EMPTY_REPLACEMENT
            } else {
                INTRO_REPLACEMENT
            };
            Some(frame(intro, &body))
        }
    }
}

/// UTF-8 边界截断：预算切进多字节序列时回退到首个被排除字节的 lead byte（dsh truncateUtf8）。
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

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("denia-wsi-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn discovers_user_global_project_chain_and_touched_descendants() {
        let root = setup("chain");
        let home = root.join("home");
        let project = root.join("project");
        let cwd = project.join("a/b");
        std::fs::create_dir_all(cwd.join("c")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::write(home.join("AGENTS.md"), "global rules").unwrap();
        std::fs::write(project.join("AGENTS.md"), "root rules").unwrap();
        std::fs::write(project.join("a/AGENTS.md"), "mid rules").unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "cwd rules").unwrap();
        std::fs::write(project.join("a/b/c/AGENTS.md"), "deep rules").unwrap();

        // 基线：祖先链 root→cwd，不含 cwd 之下。
        let files = discover(&home, &cwd, &[], 1_048_576);
        let displays: Vec<&str> = files.iter().map(|f| f.display_path.as_str()).collect();
        assert_eq!(
            displays,
            vec!["~/AGENTS.md", "AGENTS.md", "a/AGENTS.md", "a/b/AGENTS.md"]
        );

        // 触碰 cwd 之下的文件：补上后代目录链（浅→深），祖先链不重复。
        let files = discover(&home, &cwd, &[cwd.join("c/x.txt")], 1_048_576);
        let displays: Vec<&str> = files.iter().map(|f| f.display_path.as_str()).collect();
        assert_eq!(
            displays,
            vec![
                "~/AGENTS.md",
                "AGENTS.md",
                "a/AGENTS.md",
                "a/b/AGENTS.md",
                "a/b/c/AGENTS.md"
            ]
        );
        // 越出 cwd 的触碰忽略。
        let files = discover(&home, &cwd, &[project.join("outside.txt")], 1_048_576);
        assert_eq!(files.len(), 4);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn skips_oversized_and_missing_files() {
        let root = setup("budget");
        let home = root.join("home");
        let cwd = root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "x".repeat(4096)).unwrap();
        assert!(discover(&home, &cwd, &[], 1024).is_empty());
        let files = discover(&home, &cwd, &[], 1_048_576);
        assert_eq!(files.len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_discovery_only_replaces_when_baseline_visible() {
        assert_eq!(render(&[], 65536, None), None);
        let files = vec![InstructionFile {
            display_path: "AGENTS.md".into(),
            content: "rules".into(),
        }];
        let baseline = render(&files, 65536, None).unwrap();
        // 文件全部消失 → 空替换声明;再算一步幂等。
        let emptied = render(&[], 65536, Some(&baseline)).unwrap();
        assert!(emptied.starts_with("<system-reminder>\n工作区指令:"));
        assert!(emptied.contains("当前没有激活的工作区指令"));
        assert_eq!(render(&[], 65536, Some(&emptied)), None);
    }

    #[test]
    fn render_intro_depends_on_body_change_not_baseline_presence() {
        let files = vec![InstructionFile {
            display_path: "AGENTS.md".into(),
            content: "rules".into(),
        }];
        let first = render(&files, 65536, None).unwrap();
        assert!(first.contains(INTRO_BASELINE));
        // 回归:注入基线后(Previous 存在)正文未变 → 不得再注入(旧实现会用
        // "取代"引导语把同一段正文重发一次)。
        assert_eq!(render(&files, 65536, Some(&first)), None);
        // 正文变化 → "取代"引导语。
        let changed = vec![InstructionFile {
            display_path: "AGENTS.md".into(),
            content: "new rules".into(),
        }];
        let replaced = render(&changed, 65536, Some(&first)).unwrap();
        assert!(replaced.contains(INTRO_REPLACEMENT));
        assert_ne!(first, replaced);
    }

    #[test]
    fn render_truncates_long_body_and_reports_it() {
        let files = vec![InstructionFile {
            display_path: "AGENTS.md".into(),
            content: "x".repeat(100),
        }];
        let text = render(&files, 30, None).unwrap();
        assert!(text.contains(TRUNCATED_NOTICE));
        assert!(text.ends_with("</system-reminder>"));
        assert!(!text.contains(&"x".repeat(100)));
    }

    #[test]
    fn truncate_utf8_never_splits_codepoints() {
        // “好”占 3 字节；预算切在多字节序列中间必须回退到 lead byte。
        assert_eq!(truncate_utf8("好好好好好", 8), "好好");
        assert_eq!(truncate_utf8("好好好好好", 9), "好好好");
        assert_eq!(truncate_utf8("abc", 10), "abc");
        assert_eq!(truncate_utf8("abc", 0), "");
    }

    #[test]
    fn crlf_and_bom_are_normalized() {
        let root = setup("crlf");
        let home = root.join("home");
        let cwd = root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "\u{feff}line1\r\nline2\r\n").unwrap();
        let files = discover(&home, &cwd, &[], 1_048_576);
        assert_eq!(files[0].content, "line1\nline2\n");
        std::fs::remove_dir_all(root).unwrap();
    }
}
