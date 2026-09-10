//! The `ls` tool: list the entries of one directory.
//!
//! 存在的唯一理由:把"这个目录里有什么"从 bash 手里拿回来。之前工具集里
//! `glob` 只返文件、`grep` 只返命中行,列目录没有任何专用入口,模型只好写
//! `Get-ChildItem` / `dir /s` / `find`,在大目录树上慢得离谱且不读
//! `.gitignore`,一条命令能把整轮对话卡到超时。
//!
//! 契约(与 glob 对齐,便于模型在两者间迁移):
//! - 默认只列**一层**(`depth=1`),要更深必须显式给 depth——列目录是"看结构",
//!   不是"扫全树",扫树是 glob 的活;
//! - 包含隐藏文件;VCS 元数据目录(`.git` 等)整棵剪掉;
//! - 目录名尾部带 `/`,一眼分得清条目类型;
//! - 排序:目录在前、文件在后,各自按名称升序——列目录要的是稳定可读的清单,
//!   不是"最新改动优先"(那是 glob 的语义);
//! - `path` 接受绝对路径与相对路径,相对路径锚定会话工作区(走 `resolve_within`)。
//!
//! 性能:深度 1(热路径)走单线程 walker——一次目录枚举,不起线程池;
//! 深度 > 1 才开并行遍历。结果只保留路径字符串,不读文件内容、不 stat 文件
//! (目录判定用目录枚举时带回的 file type)。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use denia_core::tool::ToolSchema;
use ignore::WalkBuilder;
use serde::Deserialize;

use crate::support::parse_tool_args;
use crate::{Tool, ToolContext, ToolOutput, resolve_within};

const DEFAULT_MAX_RESULTS: usize = 200;
const HARD_MAX_RESULTS: usize = 2_000;
/// 默认只列直接子项。
const DEFAULT_DEPTH: usize = 1;
/// 深度上限:再深就该用 glob 按模式找,而不是把整棵子树摊平给模型。
const HARD_MAX_DEPTH: usize = 5;
/// 收集上限:条目数超过即提前收手(结果会标注不完整)。
const COLLECT_CAP: usize = 50_000;

/// VCS 元数据目录,整棵剪掉(与 glob 同一份名单)。
const VCS_EXCLUDES: &[&str] = &[".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];

#[derive(Deserialize)]
struct LsArgs {
    /// 要列出的目录;绝对路径原样使用,相对路径锚定会话工作区。
    #[serde(default)]
    path: Option<String>,
    /// 递归深度,默认 1(只列直接子项),上限 5。
    #[serde(default)]
    depth: Option<usize>,
    /// 最多返回的条目数(默认 200,上限 2000)。
    #[serde(default)]
    max_results: Option<usize>,
    /// 跳过排序后(目录在前、名称升序)的前 N 条,与 max_results 组成分页。
    #[serde(default)]
    offset: usize,
}

/// 一条待输出条目:`rel` 是相对搜索根的展示路径(已用 `/` 归一)。
struct Entry {
    is_dir: bool,
    rel: String,
}

/// Lists the entries of one directory.
pub struct LsTool {
    schema: ToolSchema,
}

impl LsTool {
    pub fn new() -> Self {
        Self {
            schema: ToolSchema {
                name: "ls".to_string(),
                description: "列出目录包含哪些条目(目录名尾部带 \"/\"),目录在前、文件在后,各自按名称升序。默认只列一层——要看更深请显式给 depth(上限 5),要按模式找文件请用 glob,要搜内容请用 grep。包含隐藏文件,VCS 元数据目录(.git 等)排除。path 支持绝对路径与相对路径,缺省为会话工作区。条目多时用 offset/max_results 分页。".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "要列出的目录,默认会话工作区。绝对路径与相对路径都可用:相对路径锚定会话工作区(如 \"src\" 或 \"./src\")。"
                        },
                        "depth": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": HARD_MAX_DEPTH as i64,
                            "default": 1,
                            "description": "递归深度,默认 1(只列直接子项),上限 5。只看目录结构就够了;按模式找文件用 glob,不要用深度换扫描范围。"
                        },
                        "max_results": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": HARD_MAX_RESULTS as i64,
                            "description": "最多返回的条目数,默认 200。"
                        },
                        "offset": {
                            "type": "integer",
                            "minimum": 0,
                            "default": 0,
                            "description": "跳过排序后前 N 条(从 0 开始);与 max_results 配合分页浏览完整结果。"
                        }
                    }
                }),
            },
        }
    }
}

impl Default for LsTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for LsTool {
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn execute(&self, arguments: &str, ctx: &ToolContext) -> ToolOutput {
        let args: LsArgs = match parse_tool_args(arguments) {
            Ok(args) => args,
            Err(error) => {
                return ToolOutput::error(format!(
                    "[工具错误] 参数解析失败:{error}\n建议:参数必须是 JSON 对象;path(可选)是要列出的目录,相对路径锚定会话工作区"
                ));
            }
        };
        let root = match resolve_within(&ctx.cwd, args.path.as_deref().unwrap_or("."), ctx.confined)
        {
            Ok(path) => path,
            Err(message) => {
                return ToolOutput::error(format!(
                    "[工具错误] {message}\n建议:path 可写绝对路径或相对工作区的相对路径;沙箱开启时路径必须在工作区内"
                ));
            }
        };
        if !root.is_dir() {
            return ToolOutput::error(format!(
                "[工具错误] '{}' 不是目录\n建议:path 必须指向存在的目录(绝对或相对工作区均可);要看文件内容请用 read_file,按模式找文件用 glob",
                root.display()
            ));
        }
        let depth = args
            .depth
            .unwrap_or(DEFAULT_DEPTH)
            .clamp(1, HARD_MAX_DEPTH);
        let max = args
            .max_results
            .unwrap_or(DEFAULT_MAX_RESULTS)
            .clamp(1, HARD_MAX_RESULTS);
        let offset = args.offset;
        let cancel = ctx.cancel.clone();
        // 提示文案要在 root 被移进 blocking 任务后仍然可用。
        let display_root = root.clone();

        // 目录遍历是阻塞 IO,整体丢进 blocking 池,不挡异步运行时。
        let result = tokio::task::spawn_blocking(move || {
            let mut builder = WalkBuilder::new(&root);
            builder
                .max_depth(Some(depth))
                // 注意极性:`hidden` 是"过滤器开关",true = 把隐藏条目滤掉。
                // 这里要包含隐藏文件,所以传 false。
                .hidden(false)
                .parents(false)
                .git_ignore(false)
                .git_exclude(false)
                .ignore(false)
                .git_global(false)
                .follow_links(false)
                // 深度 1 是一次目录枚举,起线程池纯属倒贴开销;
                // 只有真要下探时才值得并行。
                .threads(if depth > 1 { 32 } else { 1 });
            let walker = builder.build_parallel();

            // `run` 的工厂闭包每个工作线程调一次,状态必须共享而非 move:
            // 与 glob 同款 Arc/Mutex(深度 1 时只有 1 个线程,无竞争)。
            let root = Arc::new(root);
            let entries: Arc<Mutex<Vec<Entry>>> = Arc::new(Mutex::new(Vec::new()));
            let overflow = Arc::new(AtomicBool::new(false));
            let scan_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
            walker.run(|| {
                let cancel = cancel.clone();
                let root = Arc::clone(&root);
                let entries = Arc::clone(&entries);
                let overflow = Arc::clone(&overflow);
                let scan_error = Arc::clone(&scan_error);
                Box::new(move |entry| {
                    if cancel.is_cancelled() {
                        return ignore::WalkState::Quit;
                    }
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(error) => {
                            // 单个子目录读失败不该让整次列目录失败;但也不能
                            // 装作没发生——留一条痕迹在结果里。
                            let mut slot = scan_error.lock().unwrap();
                            if slot.is_none() {
                                *slot = Some(error.to_string());
                            }
                            return ignore::WalkState::Continue;
                        }
                    };
                    // 根自身不是"条目"。
                    if entry.depth() == 0 {
                        return ignore::WalkState::Continue;
                    }
                    let Some(file_type) = entry.file_type() else {
                        return ignore::WalkState::Continue;
                    };
                    if file_type.is_dir() {
                        let name = entry.file_name();
                        if VCS_EXCLUDES.iter().any(|vcs| name == *vcs) {
                            return ignore::WalkState::Skip;
                        }
                    } else if !file_type.is_file() {
                        // 符号链接等既非文件也非目录的条目,列出来只会让模型
                        // 误判类型,直接跳过。
                        return ignore::WalkState::Continue;
                    }
                    let mut sink = entries.lock().unwrap();
                    sink.push(Entry {
                        is_dir: file_type.is_dir(),
                        rel: relative_display(entry.path(), &root),
                    });
                    if sink.len() > COLLECT_CAP {
                        overflow.store(true, Ordering::Relaxed);
                        return ignore::WalkState::Quit;
                    }
                    ignore::WalkState::Continue
                })
            });

            let mut entries = std::mem::take(&mut *entries.lock().unwrap());
            // 目录在前、文件在后,各自按名称升序(大小写不敏感,同名时用原串兜底保证稳定)。
            entries.sort_by(|a, b| {
                a.is_dir.cmp(&b.is_dir).reverse().then_with(|| {
                    let (left, right) = (a.rel.to_lowercase(), b.rel.to_lowercase());
                    left.cmp(&right).then_with(|| a.rel.cmp(&b.rel))
                })
            });
            let overflow = overflow.load(Ordering::Relaxed);
            let scan_error = scan_error.lock().unwrap().take();
            Ok::<_, String>((entries, overflow, scan_error))
        })
        .await;

        match result {
            Ok(Ok((entries, overflow, scan_error))) => {
                if entries.is_empty() {
                    return ToolOutput::text(format!(
                        "目录 {} 为空(没有子项)",
                        display_dir(&display_root, &ctx.cwd)
                    ));
                }
                let total = entries.len();
                let skipped = offset.min(total);
                let window: Vec<&Entry> = entries.iter().skip(skipped).take(max).collect();
                if window.is_empty() {
                    return ToolOutput::text(format!(
                        "offset={offset} 之后已无条目(共 {total} 项);减小 offset"
                    ));
                }
                let dirs = entries.iter().filter(|entry| entry.is_dir).count();
                let mut output = String::new();
                for entry in &window {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str(&entry.rel);
                    if entry.is_dir {
                        output.push('/');
                    }
                }
                let remaining = total.saturating_sub(skipped);
                if remaining > window.len() {
                    output.push_str(&format!(
                        "\n\n(显示第 {}-{} 项,共 {total} 项:{} 个目录、{} 个文件;增大 offset 看下一页)",
                        skipped + 1,
                        skipped + window.len(),
                        dirs,
                        total - dirs
                    ));
                }
                if let Some(error) = scan_error {
                    output.push_str(&format!("\n\n(部分子目录读取失败,结果可能不完整:{error})"));
                }
                if overflow {
                    output.push_str("\n\n(条目数已达上限,结果不完整;缩小 depth 或用 glob 按模式找)");
                }
                ToolOutput::text(output)
            }
            Ok(Err(message)) => ToolOutput::error(format!(
                "[工具错误] {message}\n建议:核对 path;确认目录存在且当前权限允许读取"
            )),
            Err(join_error) => ToolOutput::error(format!(
                "[工具错误] 列目录任务失败:{join_error}\n建议:请重试一次"
            )),
        }
    }
}

/// 相对 `root` 的展示路径,统一用 `/`;越界(理论上不可达)时退回完整路径。
fn relative_display(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// 空目录提示里用的目录名:工作区内给相对路径,工作区外给绝对路径。
fn display_dir(root: &Path, cwd: &Path) -> String {
    match root.strip_prefix(cwd) {
        Ok(rel) if !rel.as_os_str().is_empty() => format!("./{}", rel.to_string_lossy().replace('\\', "/")),
        _ => root.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::session::PermissionMode;
    use std::path::PathBuf;
    use tokio_util::sync::CancellationToken;

    fn temp_root() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "denia-ls-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"x").unwrap();
    }

    /// 绝对路径塞进 JSON:统一写成正斜杠,免去转义。
    fn json_path(path: &Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }

    #[tokio::test]
    async fn lists_one_level_with_dirs_first_and_trailing_slash() {
        let root = temp_root();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("web")).unwrap();
        touch(&root.join("Cargo.toml"));
        touch(&root.join("README.md"));
        touch(&root.join("src/lib.rs"));
        touch(&root.join("src/deep/nested.rs"));
        let ctx = context(root.clone());
        let tool = LsTool::new();

        let out = tool.execute(r#"{}"#, &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        let lines: Vec<&str> = out.content.lines().collect();
        assert_eq!(
            lines,
            vec!["src/", "web/", "Cargo.toml", "README.md"],
            "默认只列一层、目录在前带斜杠:{}",
            out.content
        );
        // 一层不该看到孙辈。
        assert!(!out.content.contains("lib.rs"), "{}", out.content);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn depth_walks_deeper() {
        let root = temp_root();
        touch(&root.join("src/deep/nested.rs"));
        let ctx = context(root.clone());
        let tool = LsTool::new();

        let out = tool.execute(r#"{"depth":3}"#, &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("src/"), "{}", out.content);
        assert!(out.content.contains("src/deep/"), "{}", out.content);
        assert!(out.content.contains("src/deep/nested.rs"), "{}", out.content);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn depth_is_clamped_to_hard_max() {
        let root = temp_root();
        touch(&root.join("a.rs"));
        let ctx = context(root.clone());
        let tool = LsTool::new();
        // 超上限不报错,按上限执行(模型给大数字不该浪费一步)。
        let out = tool.execute(r#"{"depth":99}"#, &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("a.rs"));
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn hidden_files_included_vcs_directories_excluded() {
        let root = temp_root();
        touch(&root.join(".gitignore"));
        touch(&root.join(".git/objects/x"));
        touch(&root.join("visible.txt"));
        let ctx = context(root.clone());
        let tool = LsTool::new();
        let out = tool.execute(r#"{"depth":4}"#, &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains(".gitignore"), "{}", out.content);
        assert!(out.content.contains("visible.txt"), "{}", out.content);
        assert!(!out.content.contains(".git/"), "{}", out.content);
        assert!(!out.content.contains("objects"), "{}", out.content);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn absolute_path_is_accepted_and_output_is_workspace_relative() {
        let root = temp_root();
        std::fs::create_dir_all(root.join("src")).unwrap();
        touch(&root.join("src/a.rs"));
        let ctx = context(root.clone());
        let tool = LsTool::new();

        // 相对路径。
        let rel = tool.execute(r#"{"path":"src"}"#, &ctx).await;
        assert!(!rel.is_error, "{}", rel.content);
        assert_eq!(rel.content.trim(), "a.rs", "{}", rel.content);

        // 绝对路径:同一结果。
        let raw = format!(r#"{{"path":"{}"}}"#, json_path(&root.join("src")));
        let abs = tool.execute(&raw, &ctx).await;
        assert!(!abs.is_error, "{}", abs.content);
        assert_eq!(abs.content, rel.content, "绝对路径与相对路径结果不一致");

        // 工作区根本身写成绝对路径。
        let raw = format!(r#"{{"path":"{}"}}"#, json_path(&root));
        let out = tool.execute(&raw, &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("src/"), "{}", out.content);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn absolute_path_outside_workspace_is_rejected() {
        let root = temp_root();
        let outside = temp_root();
        touch(&outside.join("secret.txt"));
        let ctx = context(root.clone());
        let tool = LsTool::new();
        let raw = format!(r#"{{"path":"{}"}}"#, json_path(&outside));
        let out = tool.execute(&raw, &ctx).await;
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("越出了会话工作区"), "{}", out.content);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
    }

    #[tokio::test]
    async fn file_path_is_an_error_with_pointer_to_read_file() {
        let root = temp_root();
        touch(&root.join("a.txt"));
        let ctx = context(root.clone());
        let tool = LsTool::new();
        let out = tool.execute(r#"{"path":"a.txt"}"#, &ctx).await;
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("不是目录"), "{}", out.content);
        assert!(out.content.contains("read_file"), "{}", out.content);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn empty_directory_says_so_explicitly() {
        let root = temp_root();
        std::fs::create_dir_all(root.join("empty")).unwrap();
        let ctx = context(root.clone());
        let tool = LsTool::new();
        let out = tool.execute(r#"{"path":"empty"}"#, &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("为空"), "{}", out.content);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn offset_pages_through_entries() {
        let root = temp_root();
        for i in 0..10 {
            touch(&root.join(format!("f{i:02}.txt")));
        }
        let ctx = context(root.clone());
        let tool = LsTool::new();
        let page1 = tool
            .execute(r#"{"max_results":4}"#, &ctx)
            .await;
        assert!(!page1.is_error, "{}", page1.content);
        let lines1: Vec<&str> = page1.content.lines().collect();
        assert_eq!(lines1[0], "f00.txt", "{}", page1.content);
        assert!(page1.content.contains("共 10 项"), "{}", page1.content);
        // 第二页:紧接着的后 4 条。
        let page2 = tool
            .execute(r#"{"max_results":4,"offset":4}"#, &ctx)
            .await;
        let lines2: Vec<&str> = page2.content.lines().collect();
        assert_eq!(lines2[0], "f04.txt", "{}", page2.content);
        assert!(!page2.content.contains("f00.txt"), "{}", page2.content);
        // 越界 offset:明确提示。
        let past = tool.execute(r#"{"offset":100}"#, &ctx).await;
        assert!(!past.is_error, "{}", past.content);
        assert!(past.content.contains("已无条目"), "{}", past.content);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn missing_directory_is_an_error() {
        let root = temp_root();
        let ctx = context(root.clone());
        let tool = LsTool::new();
        let out = tool.execute(r#"{"path":"nope"}"#, &ctx).await;
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("不是目录"), "{}", out.content);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }
}
