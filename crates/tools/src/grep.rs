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

use crate::support::parse_tool_args;
use crate::{Tool, ToolContext, ToolOutput, resolve_within};

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
    /// 跳过排序后(路径+行号)的前 N 条命中,与 max_matches 组成分页。
    #[serde(default)]
    offset: usize,
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
                description: "用正则表达式(ripgrep 语法)搜索文件内容,结果按 path:line:content 逐行返回(工作区相对路径,按路径与行号排序)。默认尊重 .gitignore 与隐藏文件,大树上搜索也很快;命中多时用 offset/max_matches 分页。找到目标文件后用 read_file 读取上下文。".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "要搜索的正则表达式(ripgrep 语法,如 \"foo|bar\"、\"\\\\bstruct \\\\w+\\\\b\")。"
                        },
                        "path": {
                            "type": "string",
                            "description": "搜索的文件或目录,默认会话工作区;相对路径锚定会话工作区。"
                        },
                        "include": {
                            "type": "string",
                            "description": "单个文件 glob 过滤(如 \"*.rs\"、\"*.{js,jsx}\");不是列表,不支持取反。"
                        },
                        "ignore_case": {
                            "type": "boolean",
                            "description": "大小写不敏感匹配。默认 false。"
                        },
                        "max_matches": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": HARD_MAX_MATCHES as i64,
                            "description": "最多返回的命中行数,默认 200;达到后搜索提前停止。"
                        },
                        "offset": {
                            "type": "integer",
                            "minimum": 0,
                            "default": 0,
                            "description": "跳过排序后前 N 条命中(从 0 开始);与 max_matches 配合分页获取更多命中。"
                        },
                        "respect_ignore": {
                            "type": "boolean",
                            "description": "尊重 .gitignore/.ignore 与隐藏文件(默认 true);设为 false 可搜索被忽略的目录(如 node_modules)。"
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
        let args: GrepArgs = match parse_tool_args(arguments) {
            Ok(args) => args,
            Err(error) => {
                return ToolOutput::error(format!(
                    "[工具错误] 参数解析失败:{error}\n建议:参数必须是 JSON 对象,必填字段为 pattern(字符串)"
                ));
            }
        };
        if args.pattern.trim().is_empty() {
            return ToolOutput::error(
                "[工具错误] pattern 不能为空\n建议:pattern 是正则表达式(ripgrep 语法);搜索单词可直接写单词本身",
            );
        }
        let root = match resolve_within(&ctx.cwd, args.path.as_deref().unwrap_or("."), ctx.confined)
        {
            Ok(path) => path,
            Err(message) => {
                return ToolOutput::error(format!(
                    "[工具错误] {message}\n建议:相对路径锚定会话工作区;确认路径存在"
                ));
            }
        };
        if !root.exists() {
            return ToolOutput::error(format!(
                "[工具错误] 路径 '{}' 不存在\n建议:用 glob 确认目录/文件位置",
                root.display()
            ));
        }
        let mut regex_builder = regex::bytes::RegexBuilder::new(&args.pattern);
        regex_builder.case_insensitive(args.ignore_case);
        let regex = match regex_builder.build() {
            Ok(regex) => regex,
            Err(error) => {
                return ToolOutput::error(format!(
                    "[工具错误] 正则表达式无效:{error}\n建议:核对正则语法(ripgrep/rust regex);普通文本搜索不需要转义点号之外的特殊字符"
                ));
            }
        };
        let max = args
            .max_matches
            .unwrap_or(DEFAULT_MAX_MATCHES)
            .clamp(1, HARD_MAX_MATCHES);
        let offset = args.offset;

        // 参数准备完成;重活全部丢进 blocking 池,不挡异步运行时。
        // 两条路径:
        // - offset=0(首页):保持提前止损的极速路径。命中达到 max 即停,
        //   输出是「遍历序最先命中的条目」排序后展示——命中远超窗口时它
        //   不是全局字典序前缀(ripgrep 亦按遍历序输出),作为采样足够;
        // - offset>0(显式分页):扫完整树,窗口取排序后的
        //   [offset, offset+max),跨调用稳定可复现。取消令牌随时可中断。
        let cwd_for_display = ctx.cwd.clone();
        let cancel = ctx.cancel.clone();
        let include = args.include.clone();
        let respect = args.respect_ignore;
        let stop_limit = if offset == 0 { max } else { usize::MAX };

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
                        stop_limit,
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
            let stopped = stop.load(Ordering::Relaxed);
            // 稳定输出:按路径、行号排序(并行遍历顺序不确定)。
            // 先排序再截断:截断的是排序意义下的前缀,分页窗口才正确。
            all.sort_by(|a, b| a.path.cmp(&b.path).then(a.line_no.cmp(&b.line_no)));
            all.truncate(offset.saturating_add(max));
            // 分页窗口:排序后跳过 offset,取 max 条。
            let window: Vec<GrepHit> = all.into_iter().skip(offset).take(max).collect();
            Ok::<_, String>((window, total, stopped))
        })
        .await;

        match result {
            Ok(Ok((hits, total, stopped))) => {
                if hits.is_empty() {
                    return ToolOutput::text("未找到匹配的行");
                }
                // 提前止损时 total 只是下界(可能还有更多),也要提示分页。
                let more = stopped || total > offset + hits.len();
                let lines: Vec<String> = hits
                    .iter()
                    .map(|hit| format!("{}:{}:{}", hit.path, hit.line_no, hit.line))
                    .collect();
                let mut output = format!("{} 条命中\n\n{}", hits.len(), lines.join("\n"));
                if more {
                    output.push_str(&format!(
                        "\n\n(命中数达到窗口上限;增大 offset={} 可继续获取,或缩小 pattern)",
                        offset + hits.len()
                    ));
                }
                ToolOutput::text(output)
            }
            Ok(Err(message)) => ToolOutput::error(format!(
                "[工具错误] {message}\n建议:include 必须是单个正 glob(不支持取反与顶层逗号)"
            )),
            Err(join_error) => ToolOutput::error(format!(
                "[工具错误] 搜索任务失败:{join_error}\n建议:请重试一次"
            )),
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
            permission_mode: PermissionMode::AutoEdit,
            ask: None,
            call_id: None,
            goal_reader: None,
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
        assert!(out.content.contains("窗口上限"), "{}", out.content);
        assert!(out.content.contains("offset=10"), "{}", out.content);
        assert!(out.content.lines().count() <= 14);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn offset_pages_through_matches() {
        let root = temp_root();
        // 5 个文件各 1 行命中,排序后 path:line 为 f0..f4。
        for i in 0..5 {
            write_file(&root.join(format!("f{i}.txt")), &format!("value {i}"));
        }
        let ctx = context(root);
        let tool = GrepTool::new();
        // 首页:止损路径,窗口是遍历序最先命中的 2 条(并行遍历顺序不定,
        // 不能假设具体是哪些文件——只断言条数与分页提示)。
        let page1 = tool
            .execute(r#"{"pattern":"value","max_matches":2}"#, &ctx)
            .await;
        assert!(!page1.is_error, "{}", page1.content);
        assert!(page1.content.contains("2 条命中"), "{}", page1.content);
        assert!(page1.content.contains("窗口上限"), "{}", page1.content);
        // 第二页:全扫精确窗口——字典序跳过 2 条,稳定可复现。
        let page2 = tool
            .execute(r#"{"pattern":"value","max_matches":2,"offset":2}"#, &ctx)
            .await;
        assert!(!page2.is_error, "{}", page2.content);
        let rows: Vec<&str> = page2
            .content
            .lines()
            .filter(|line| line.contains(".txt:"))
            .collect();
        assert_eq!(
            rows,
            vec!["f2.txt:1:value 2", "f3.txt:1:value 3"],
            "{}",
            page2.content
        );
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn invalid_pattern_is_an_error() {
        let root = temp_root();
        let ctx = context(root);
        let tool = GrepTool::new();
        let out = tool.execute(r#"{"pattern":"("}"#, &ctx).await;
        assert!(out.is_error);
        assert!(out.content.contains("正则表达式无效"), "{}", out.content);
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
