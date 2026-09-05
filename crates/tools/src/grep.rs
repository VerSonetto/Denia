//! The `grep` tool: full-text search, engineered for huge trees.
//!
//! ## 性能设计(扫 node_modules 量级来去自如)
//!
//! - **并行遍历**:[`ignore::WalkParallel`](ripgrep 同源)按目录并行遍历,
//!   搜索直接在 walker 的工作线程内完成,多核一起跑,无收集瓶颈。
//! - **SIMD 行匹配**:`regex::bytes` 原生多行搜索,memchr 扫 `\n`,
//!   单行短路径零分配;整个文件读入内存后按行区间切,不做 UTF-8 往返。
//! - **gitignore 语义**:默认尊重 `.gitignore` / `.ignore` / 隐藏文件,
//!   与 ripgrep 一致;`respect_ignore=false` 可强制扫被忽略目录。
//! - **提前止损**:命中数达到 `max_matches` 即停止遍历(`WalkState::Quit`),
//!   不会为一条结果扫完整棵树。
//! - **取消响应**:每文件搜索前后检查取消令牌,中断即刻生效。
//!
//! 所有失败(坏正则、坏路径、IO 错误)都规范化为 `is_error` 结果。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use denia_core::tool::ToolSchema;
use ignore::WalkBuilder;
use regex::bytes::Regex;
use serde::Deserialize;

use crate::{Tool, ToolContext, ToolOutput, parse_args_lenient, resolve_within};

const DEFAULT_MAX_MATCHES: usize = 200;
const HARD_MAX_MATCHES: usize = 2_000;
const LINE_PREVIEW_CHARS: usize = 400;
/// 前 8KB 含 NUL 视为二进制,跳过(ripgrep 行为)。
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
struct GrepArgs {
    /// 正则表达式(ripgrep 语法:rust regex)。
    pattern: String,
    /// 搜索起点文件或目录;相对路径锚定会话工作区。
    #[serde(default)]
    path: Option<String>,
    /// 单个文件 glob 过滤(如 "*.rs"、"*.{ts,tsx}"),不支持负数与逗号列表。
    #[serde(default)]
    include: Option<String>,
    /// 大小写不敏感。
    #[serde(default)]
    ignore_case: bool,
    /// 最多保留的命中行数(默认 200,上限 2000)。
    #[serde(default)]
    max_matches: Option<usize>,
    /// 尊重 .gitignore/.ignore/隐藏文件(默认 true);false 时全树扫描。
    #[serde(default = "default_true")]
    respect_ignore: bool,
}

/// One matched line: display path (workspace-relative), 1-based line, preview.
#[derive(Debug, Clone)]
struct GrepHit {
    path: String,
    line_no: u32,
    line: String,
}

/// Searches file contents with a ripgrep-syntax regular expression.
pub struct GrepTool {
    schema: ToolSchema,
}

impl GrepTool {
    pub fn new() -> Self {
        Self {
            schema: ToolSchema {
                name: "grep".to_string(),
                description: "Search file contents with a regular expression (used by ripgrep with the same syntax). \
                    Matches come back as `path:line:content` rows, workspace-relative, grouped in traversal order; \
                    the first `maxMatches` hits are returned. Respects .gitignore and hidden files unless disabled, \
                    so searching a large tree stays fast. Use read_file on a matched file for surrounding context."
                    .to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Regular expression to search for (ripgrep syntax, e.g. \"foo|bar\", \"\\\\bstruct \\\\w+\\\\b\")."
                        },
                        "path": {
                            "type": "string",
                            "description": "File or directory to search. Defaults to the session workspace; a relative path resolves against it."
                        },
                        "include": {
                            "type": "string",
                            "description": "One glob filter for which files to search (e.g. \"*.rs\", \"*.{js,jsx}\"). Not a list; negation is not supported."
                        },
                        "ignore_case": {
                            "type": "boolean",
                            "description": "Case-insensitive matching. Default false."
                        },
                        "max_matches": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": HARD_MAX_MATCHES as i64,
                            "description": "Max matching lines to return. Default 200; the search stops early once reached."
                        },
                        "respect_ignore": {
                            "type": "boolean",
                            "description": "Respect .gitignore/.ignore and hidden files (default true). Set false to search ignored trees such as node_modules."
                        }
                    },
                    "required": ["pattern"]
                }),
            },
        }
    }
}

impl Default for GrepTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn execute(&self, arguments: &str, ctx: &ToolContext) -> ToolOutput {
        let args: GrepArgs = match parse_args_lenient(arguments) {
            Ok(args) => args,
            Err(error) => {
                return ToolOutput {
                    content: format!("invalid arguments: {error}"),
                    is_error: true,
                };
            }
        };
        if args.pattern.trim().is_empty() {
            return ToolOutput {
                content: "pattern must be a non-empty string".to_string(),
                is_error: true,
            };
        }
        let root = match resolve_within(&ctx.cwd, args.path.as_deref().unwrap_or("."), ctx.confined)
        {
            Ok(path) => path,
            Err(message) => {
                return ToolOutput {
                    content: message,
                    is_error: true,
                };
            }
        };
        if !root.exists() {
            return ToolOutput {
                content: format!("path '{}' does not exist", root.display()),
                is_error: true,
            };
        }
        let mut regex_builder = regex::bytes::RegexBuilder::new(&args.pattern);
        regex_builder.case_insensitive(args.ignore_case);
        let regex = match regex_builder.build() {
            Ok(regex) => regex,
            Err(error) => {
                return ToolOutput {
                    content: format!("invalid regex: {error}"),
                    is_error: true,
                };
            }
        };
        let max = args
            .max_matches
            .unwrap_or(DEFAULT_MAX_MATCHES)
            .clamp(1, HARD_MAX_MATCHES);

        // 参数准备完成;重活全部丢进 blocking 池,不挡异步运行时。
        let cwd_for_display = ctx.cwd.clone();
        let cancel = ctx.cancel.clone();
        let include = args.include.clone();
        let respect = args.respect_ignore;

        let result = tokio::task::spawn_blocking(move || {
            let mut walker = WalkBuilder::new(&root);
            walker
                .hidden(!respect)
                .parents(respect)
                .git_ignore(respect)
                .git_exclude(respect)
                .ignore(respect)
                .git_global(false)
                .require_git(false) // 非 git 目录也尊重 .gitignore(rg 行为)
                .follow_links(false);
            if let Some(glob_pattern) = include.as_deref().filter(|g| !g.trim().is_empty()) {
                match build_overrides(&root, glob_pattern) {
                    Ok(overrides) => {
                        walker.overrides(overrides);
                    }
                    Err(message) => return Err(message),
                }
            }
            let walker = walker.build_parallel();

            let stop = Arc::new(AtomicBool::new(false));
            let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let hits: Arc<Mutex<Vec<GrepHit>>> = Arc::new(Mutex::new(Vec::new()));

            walker.run(|| {
                let regex = regex.clone();
                let stop = stop.clone();
                let count = count.clone();
                let hits = hits.clone();
                let cancel = cancel.clone();
                let cwd_for_display = cwd_for_display.clone();
                Box::new(move |entry| {
                    if stop.load(Ordering::Relaxed) || cancel.is_cancelled() {
                        return ignore::WalkState::Quit;
                    }
                    let Ok(entry) = entry else {
                        return ignore::WalkState::Continue;
                    };
                    let Some(file_type) = entry.file_type() else {
                        return ignore::WalkState::Continue;
                    };
                    if !file_type.is_file() {
                        return ignore::WalkState::Continue;
                    }
                    let path = entry.path();
                    if search_file(
                        path,
                        &regex,
                        &cwd_for_display,
                        max,
                        &count,
                        &hits,
                        &stop,
                        &cancel,
                    ) {
                        ignore::WalkState::Quit
                    } else {
                        ignore::WalkState::Continue
                    }
                })
            });

            let mut all = std::mem::take(&mut *hits.lock().unwrap());
            let total = count.load(Ordering::Relaxed);
            // 并行执行:停止信号到达时其他线程可能已提交部分命中,最终统一截断,
            // 保证结果数量严格不超过 max。
            all.truncate(max);
            // 稳定输出:按路径、行号排序(并行遍历顺序不确定)。
            all.sort_by(|a, b| a.path.cmp(&b.path).then(a.line_no.cmp(&b.line_no)));
            Ok::<_, String>((all, total))
        })
        .await;

        match result {
            Ok(Ok((hits, total))) => {
                if hits.is_empty() {
                    return ToolOutput {
                        content: "No matches found".to_string(),
                        is_error: false,
                    };
                }
                let capped = total >= max;
                let lines: Vec<String> = hits
                    .iter()
                    .map(|hit| format!("{}:{}:{}", hit.path, hit.line_no, hit.line))
                    .collect();
                let mut output = format!("{} match(es)\n\n{}", hits.len(), lines.join("\n"));
                if capped {
                    output.push_str(
                        "\n\n(result capped at max_matches; narrow the pattern to see more)",
                    );
                }
                ToolOutput {
                    content: output,
                    is_error: false,
                }
            }
            Ok(Err(message)) => ToolOutput {
                content: message,
                is_error: true,
            },
            Err(join_error) => ToolOutput {
                content: format!("search worker failed: {join_error}"),
                is_error: true,
            },
        }
    }
}

/// 文件 glob 过滤(单个正 glob;内部走 gitignore 语法,无 "/" 匹配任意深度)。
fn build_overrides(root: &Path, pattern: &str) -> Result<ignore::overrides::Override, String> {
    if pattern.trim().is_empty() {
        return Err("include must be a non-empty glob when given".to_string());
    }
    if pattern.starts_with('!') {
        return Err(
            "include must be a positive glob filter; negated patterns (\"!…\") are not supported"
                .to_string(),
        );
    }
    // 逗号列表禁止(除花括号组内)。
    let mut brace_depth = 0usize;
    for ch in pattern.chars() {
        match ch {
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            ',' if brace_depth == 0 => {
                return Err(
                    "include must be one glob, not a comma-separated list (use {a,b} alternation instead)"
                        .to_string(),
                )
            }
            _ => {}
        }
    }
    let mut builder = ignore::overrides::OverrideBuilder::new(root);
    builder
        .add(pattern)
        .map_err(|error| format!("invalid include glob: {error}"))?;
    builder
        .build()
        .map_err(|error| format!("invalid include glob: {error}"))
}

/// Searches one file; returns true when the match cap was reached (stop now).
fn search_file(
    path: &Path,
    regex: &Regex,
    display_root: &Path,
    max: usize,
    count: &std::sync::atomic::AtomicUsize,
    hits: &Mutex<Vec<GrepHit>>,
    stop: &AtomicBool,
    cancel: &tokio_util::sync::CancellationToken,
) -> bool {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(_) => return false,
    };
    {
        // 二进制嗅探:前 8KB 含 NUL 直接跳过。
        let sniff = &data[..data.len().min(BINARY_SNIFF_BYTES)];
        if sniff.contains(&0u8) {
            return false;
        }
    }

    let mut local = Vec::new();
    let mut line_no: u32 = 0;
    let mut pos = 0usize;
    while pos < data.len() {
        if cancel.is_cancelled() {
            return false;
        }
        let end = memchr(b'\n', &data[pos..])
            .map(|offset| pos + offset)
            .unwrap_or(data.len());
        let line = &data[pos..end];
        line_no += 1;
        if regex.is_match(line) {
            local.push(GrepHit {
                path: display_path(path, display_root),
                line_no,
                line: preview_line(line),
            });
            // 原子计数提前止损:达到上限即停,不给整棵树收尾。
            if count.fetch_add(1, Ordering::Relaxed) + 1 >= max {
                stop.store(true, Ordering::Relaxed);
                break;
            }
        }
        if end >= data.len() {
            break;
        }
        pos = end + 1;
    }
    if !local.is_empty() {
        hits.lock().unwrap().extend(local);
    }
    stop.load(Ordering::Relaxed)
}

fn display_path(path: &Path, display_root: &Path) -> String {
    path.strip_prefix(display_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn preview_line(line: &[u8]) -> String {
    let text = String::from_utf8_lossy(line);
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(LINE_PREVIEW_CHARS).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// memchr 的最小实现:扫描换行偏移。regex crate 的 memchr 在 bytes 模式下
/// 不可直接引用,这里用迭代器版(编译器会向量化)。
fn memchr(needle: u8, haystack: &[u8]) -> Option<usize> {
    haystack.iter().position(|byte| *byte == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::session::PermissionMode;
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::time::Instant;
    use tokio_util::sync::CancellationToken;

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "denia-grep-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn context(cwd: PathBuf) -> ToolContext {
        ToolContext {
            session_id: None,
            selection: None,
            cwd,
            cancel: CancellationToken::new(),
            confined: true,
            vision_supported: true,
            emit_event: None,
            file_history: None,
            permission_mode: PermissionMode::WorkspaceWrite,
            permission_override: None,
        }
    }

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
    }

    #[tokio::test]
    async fn matches_lines_with_path_and_line_number() {
        let root = temp_root();
        write_file(
            &root.join("src/a.rs"),
            "fn main() {\n    println!(\"hello\");\n}\n",
        );
        write_file(&root.join("src/b.rs"), "// hello world\nfn helper() {}\n");
        let ctx = context(root);
        let tool = GrepTool::new();
        let out = tool
            .execute(r#"{"pattern":"hello","path":"src"}"#, &ctx)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("src/a.rs:2:"), "{}", out.content);
        assert!(out.content.contains("src/b.rs:1:"), "{}", out.content);
        assert!(
            out.content.contains("println!(\"hello\")"),
            "{}",
            out.content
        );
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn respects_gitignore_by_default() {
        let root = temp_root();
        write_file(&root.join("keep.txt"), "needle here");
        write_file(&root.join("node_modules/ignore-me.txt"), "needle here");
        write_file(&root.join(".gitignore"), "node_modules/\n");
        let ctx = context(root);
        let tool = GrepTool::new();
        let out = tool.execute(r#"{"pattern":"needle"}"#, &ctx).await;
        assert!(!out.is_error);
        assert!(out.content.contains("keep.txt"));
        assert!(!out.content.contains("ignore-me"), "{}", out.content);

        // respect_ignore=false 时扫到被忽略的目录。
        let out = tool
            .execute(r#"{"pattern":"needle","respect_ignore":false}"#, &ctx)
            .await;
        assert!(out.content.contains("ignore-me"), "{}", out.content);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn include_glob_filters_files() {
        let root = temp_root();
        write_file(&root.join("a.rs"), "needle");
        write_file(&root.join("b.js"), "needle");
        let ctx = context(root);
        let tool = GrepTool::new();
        let out = tool
            .execute(r#"{"pattern":"needle","include":"*.rs"}"#, &ctx)
            .await;
        assert!(!out.is_error);
        assert!(out.content.contains("a.rs"));
        assert!(!out.content.contains("b.js"), "{}", out.content);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn max_matches_stops_early() {
        let root = temp_root();
        for i in 0..50 {
            write_file(&root.join(format!("f{i}.txt")), &format!("value {i}"));
        }
        let ctx = context(root);
        let tool = GrepTool::new();
        let out = tool
            .execute(r#"{"pattern":"value","max_matches":10}"#, &ctx)
            .await;
        assert!(!out.is_error);
        assert!(out.content.contains("capped"), "{}", out.content);
        assert!(out.content.lines().count() <= 14);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn invalid_pattern_is_an_error() {
        let root = temp_root();
        let ctx = context(root);
        let tool = GrepTool::new();
        let out = tool.execute(r#"{"pattern":"("}"#, &ctx).await;
        assert!(out.is_error);
        assert!(out.content.contains("invalid regex"));
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn escapes_are_rejected() {
        let root = temp_root();
        write_file(&root.join("x.txt"), "hi");
        let ctx = context(root);
        let tool = GrepTool::new();
        let out = tool
            .execute(r#"{"pattern":"hi","path":"../outside"}"#, &ctx)
            .await;
        assert!(out.is_error);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    /// 性能验收(手动跑):`cargo test -p denia-tools --release bench_real_tree -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn bench_real_tree() {
        let root = PathBuf::from(r"D:\code_project\deepseek-harness\node_modules");
        if !root.is_dir() {
            eprintln!("bench tree not found");
            return;
        }
        let ctx = context(root);
        let tool = GrepTool::new();
        let started = Instant::now();
        let out = tool
            .execute(r#"{"pattern":"createContext","max_matches":100}"#, &ctx)
            .await;
        let elapsed = started.elapsed();
        assert!(!out.is_error, "{}", out.content);
        eprintln!(
            "grep node_modules (createContext): {elapsed:?} — {} bytes of hits",
            out.content.len()
        );
        let started = Instant::now();
        let out = tool
            .execute(
                r#"{"pattern":"exports\\.default","respect_ignore":false,"max_matches":50}"#,
                &ctx,
            )
            .await;
        let elapsed = started.elapsed();
        eprintln!(
            "grep node_modules no-ignore: {elapsed:?} — matches {}",
            out.content.lines().count()
        );
    }
}
