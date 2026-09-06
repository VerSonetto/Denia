//! The `glob` tool: discover files by glob pattern.
//!
//! 走 `ignore` 的 gitignore 语法 overrides(dsh 同款语义):
//! - 无 "/" 的模式按 basename 匹配**任意深度**(`*.rs` ≈ `**/*.rs`);
//! - 结果只含文件、不含目录;按修改时间降序(新鲜优先);
//! - 默认包含隐藏与忽略文件(VCS 元数据目录除外),与 dsh glob 契约一致;
//! - 并行遍历;每线程只保留 top-N(mtime 最新),归并出全局 top-N,
//!   命中总量只计数不收集路径——扫几十万文件的目录树内存恒定、零额外 syscall。

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use denia_core::tool::ToolSchema;
use ignore::WalkBuilder;
use serde::Deserialize;

use crate::{Tool, ToolContext, ToolOutput, parse_args_lenient, resolve_within};

const DEFAULT_MAX_RESULTS: usize = 100;
const HARD_MAX_RESULTS: usize = 5_000;
/// 收集上限:命中数超过即提前收手(结果仍会截断提示)。
const COLLECT_CAP: usize = 200_000;

/// gitignore 语义下不会出现在列表里的 VCS 元数据目录,显式排除。
const VCS_EXCLUDES: &[&str] = &[".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];

/// 堆内条目:`Reverse(mtime)` 让 `BinaryHeap` 成为 mtime 最小堆,
/// 堆顶即 top-N 里最旧的一条,新条目比它新才值得换入。
type Ranked = (Reverse<SystemTime>, PathBuf);

/// 一个 walker 线程的本地 top-N 收集器。
///
/// 只在堆顶被换入或堆未满时才把 `DirEntry` 转成 `PathBuf`,淘汰零分配;
/// Drop 时把本地堆交还归并池并汇入命中总数,visitor 闭包销毁即触发。
struct TopNBatch {
    cap: usize,
    top: BinaryHeap<Ranked>,
    hits: usize,
    pool: Arc<Mutex<Vec<BinaryHeap<Ranked>>>>,
    total_hits: Arc<AtomicUsize>,
}

impl TopNBatch {
    fn new(
        cap: usize,
        pool: Arc<Mutex<Vec<BinaryHeap<Ranked>>>>,
        total_hits: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            cap,
            top: BinaryHeap::new(),
            hits: 0,
            pool,
            total_hits,
        }
    }

    /// `path` 是惰性产出的 `DirEntry::into_path`,只有真正进堆才分配。
    fn offer(&mut self, modified: SystemTime, path: impl FnOnce() -> PathBuf) {
        self.hits += 1;
        if self.top.len() < self.cap {
            self.top.push((Reverse(modified), path()));
            return;
        }
        // 堆顶是 top-N 里最旧的一条,新条目更新才值得换入。
        if let Some(mut oldest) = self.top.peek_mut()
            && oldest.0.0 < modified
        {
            *oldest = (Reverse(modified), path());
        }
    }
}

impl Drop for TopNBatch {
    fn drop(&mut self) {
        if !self.top.is_empty() {
            self.pool
                .lock()
                .unwrap()
                .push(std::mem::take(&mut self.top));
        }
        self.total_hits.fetch_add(self.hits, Ordering::Relaxed);
    }
}

#[derive(Deserialize)]
struct GlobArgs {
    /// 匹配文件路径的 glob 模式(gitignore 语法;无 "/" 时匹配任意深度 basename)。
    pattern: String,
    /// 搜索目录;相对路径锚定会话工作区。
    #[serde(default)]
    path: Option<String>,
    /// 最多返回的路径数(默认 100,上限 5000)。
    #[serde(default)]
    max_results: Option<usize>,
}

/// Finds files whose paths match a glob pattern.
pub struct GlobTool {
    schema: ToolSchema,
}

impl GlobTool {
    pub fn new() -> Self {
        Self {
            schema: ToolSchema {
                name: "glob".to_string(),
                description: "Find files whose paths match a glob pattern. Returns matching file paths — never directories — \
                    in modification-time order, newest first. A pattern with no \"/\" matches the basename at any depth, so \
                    \"*.rs\" searches the whole tree. Hidden, ignored, and VCS-metadata files are excluded from discovery."
                    .to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Glob pattern (e.g. \"**/*.ts\", \"src/**/*.test.js\", \"*.{rs,toml}\"). A pattern with no \"/\" matches at any depth."
                        },
                        "path": {
                            "type": "string",
                            "description": "Directory to search in. Defaults to the session workspace; a relative path resolves against it."
                        },
                        "max_results": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": HARD_MAX_RESULTS as i64,
                            "description": "Max paths to return. Default 100."
                        }
                    },
                    "required": ["pattern"]
                }),
            },
        }
    }
}

impl Default for GlobTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn execute(&self, arguments: &str, ctx: &ToolContext) -> ToolOutput {
        let args: GlobArgs = match parse_args_lenient(arguments) {
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
        if !root.is_dir() {
            return ToolOutput {
                content: format!("path '{}' is not a directory", root.display()),
                is_error: true,
            };
        }
        let max = args
            .max_results
            .unwrap_or(DEFAULT_MAX_RESULTS)
            .clamp(1, HARD_MAX_RESULTS);

        let cancel = ctx.cancel.clone();
        let display_root = ctx.cwd.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut builder = WalkBuilder::new(&root);
            builder
                .hidden(true) // 包含隐藏文件(dsh glob 契约),VCS 目录单独排除
                .parents(false)
                .git_ignore(false)
                .git_exclude(false)
                .ignore(false)
                .git_global(false)
                .follow_links(false)
                // 目录枚举是阻塞 IO,多线程摊平等待;32 之后实测无增益。
                .threads(32);
            let mut overrides = ignore::overrides::OverrideBuilder::new(&root);
            overrides
                .add(&args.pattern)
                .map_err(|error| format!("invalid glob pattern: {error}"))?;
            // VCS 目录不走 overrides 规则(那要对每个 entry 多匹配 12 条
            // GlobSet 规则),在 visitor 里按 basename 面量剪枝,见下。
            let overrides = overrides
                .build()
                .map_err(|error| format!("invalid glob pattern: {error}"))?;
            builder.overrides(overrides);
            let walker = builder.build_parallel();

            let batches: Arc<Mutex<Vec<BinaryHeap<Ranked>>>> = Arc::new(Mutex::new(Vec::new()));
            let total_hits = Arc::new(AtomicUsize::new(0));
            let overflow = Arc::new(AtomicBool::new(false));
            walker.run(|| {
                let cancel = cancel.clone();
                let overflow = Arc::clone(&overflow);
                let mut batch = TopNBatch::new(max, Arc::clone(&batches), Arc::clone(&total_hits));
                Box::new(move |entry| {
                    if cancel.is_cancelled() {
                        return ignore::WalkState::Quit;
                    }
                    let Ok(entry) = entry else {
                        return ignore::WalkState::Continue;
                    };
                    let Some(file_type) = entry.file_type() else {
                        return ignore::WalkState::Continue;
                    };
                    if file_type.is_dir() {
                        // VCS 元数据目录按 basename 面量剪掉整棵子树;
                        // .git 文件(worktree 指针)不属于目录,照常走 pattern。
                        let name = entry.file_name();
                        if VCS_EXCLUDES.iter().any(|vcs| name == *vcs) {
                            return ignore::WalkState::Skip;
                        }
                        return ignore::WalkState::Continue;
                    }
                    if !file_type.is_file() {
                        return ignore::WalkState::Continue;
                    }
                    // DirEntry::metadata 复用目录枚举时已带回的时间戳,
                    // 比按完整路径再 stat 一次便宜一个量级(Windows 上零 syscall)。
                    let modified = match entry.metadata() {
                        Ok(meta) => meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                        Err(_) => SystemTime::UNIX_EPOCH,
                    };
                    batch.offer(modified, || entry.into_path());
                    if batch.hits > COLLECT_CAP {
                        overflow.store(true, std::sync::atomic::Ordering::Relaxed);
                        return ignore::WalkState::Quit;
                    }
                    ignore::WalkState::Continue
                })
            });

            let mut pools = std::mem::take(&mut *batches.lock().unwrap());
            let mut ranked: Vec<(SystemTime, PathBuf)> = pools
                .drain(..)
                .flat_map(|heap| {
                    heap.into_iter()
                        .map(|(Reverse(modified), path)| (modified, path))
                })
                .collect();
            ranked.sort_unstable_by_key(|(modified, _)| std::cmp::Reverse(*modified));
            let total = total_hits.load(std::sync::atomic::Ordering::Relaxed);
            Ok::<_, String>((
                ranked,
                total,
                overflow.load(std::sync::atomic::Ordering::Relaxed),
            ))
        })
        .await;

        match result {
            Ok(Ok((ranked, total, overflow))) => {
                if ranked.is_empty() {
                    return ToolOutput {
                        content: "No files found".to_string(),
                        is_error: false,
                    };
                }
                let shown_len = ranked.len().min(max);
                let mut output = String::new();
                for path in ranked.iter().take(max) {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    let rel = path
                        .1
                        .strip_prefix(&display_root)
                        .unwrap_or(&path.1)
                        .to_string_lossy()
                        .replace('\\', "/");
                    output.push_str(&rel);
                }
                if total > max {
                    output.push_str(&format!(
                        "\n\n(Showing {shown_len} of {total} paths; narrow the pattern to see more)"
                    ));
                }
                if overflow {
                    output.push_str("\n\n(collection capped; results may be incomplete)");
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
                content: format!("glob worker failed: {join_error}"),
                is_error: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::session::PermissionMode;
    use std::io::Write as _;
    use std::path::Path;
    use tokio_util::sync::CancellationToken;

    fn temp_root() -> PathBuf {
        // 并行测试在同一时钟 tick 里调用会撞名,目录共用会把对方的
        // 遍历/删除搅黄(见 remove_dir_all 的 PermissionDenied 抖动);
        // pid + 进程内原子序号保证唯一,不依赖时钟精度。
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "denia-glob-{}-{}",
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
            permission_mode: PermissionMode::WorkspaceWrite,
            permission_override: None,
        }
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(b"x").unwrap();
    }

    #[tokio::test]
    async fn matches_basename_at_any_depth() {
        let root = temp_root();
        touch(&root.join("a.rs"));
        touch(&root.join("src/b.rs"));
        touch(&root.join("src/deep/c.rs"));
        touch(&root.join("lib.txt"));
        let ctx = context(root);
        let tool = GlobTool::new();
        let out = tool.execute(r#"{"pattern":"*.rs"}"#, &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("a.rs"));
        assert!(out.content.contains("src/b.rs"));
        assert!(out.content.contains("src/deep/c.rs"));
        assert!(!out.content.contains("lib.txt"), "{}", out.content);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn anchored_pattern_limits_depth() {
        let root = temp_root();
        touch(&root.join("src/a.rs"));
        touch(&root.join("src/deep/b.rs"));
        let ctx = context(root);
        let tool = GlobTool::new();
        let out = tool.execute(r#"{"pattern":"src/*.rs"}"#, &ctx).await;
        assert!(!out.is_error);
        assert!(out.content.contains("src/a.rs"));
        assert!(!out.content.contains("deep"), "{}", out.content);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn vcs_directories_are_excluded() {
        let root = temp_root();
        touch(&root.join(".git/objects/x.txt"));
        touch(&root.join("src/keep.txt"));
        let ctx = context(root);
        let tool = GlobTool::new();
        let out = tool.execute(r#"{"pattern":"**/*.txt"}"#, &ctx).await;
        assert!(!out.is_error);
        assert!(out.content.contains("src/keep.txt"));
        assert!(!out.content.contains(".git"), "{}", out.content);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn max_results_truncates() {
        let root = temp_root();
        for i in 0..20 {
            touch(&root.join(format!("f{i}.txt")));
        }
        let ctx = context(root);
        let tool = GlobTool::new();
        let out = tool
            .execute(r#"{"pattern":"*.txt","max_results":5}"#, &ctx)
            .await;
        assert!(!out.is_error);
        assert!(out.content.contains("Showing 5 of 20"), "{}", out.content);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn newest_first_ordering() {
        let root = temp_root();
        for i in 0..4 {
            touch(&root.join(format!("f{i}.txt")));
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let ctx = context(root);
        let tool = GlobTool::new();
        let out = tool.execute(r#"{"pattern":"*.txt"}"#, &ctx).await;
        assert!(!out.is_error);
        let lines: Vec<&str> = out.content.lines().collect();
        assert_eq!(lines.len(), 4, "{}", out.content);
        assert_eq!(lines[0], "f3.txt", "{}", out.content);
        assert_eq!(lines[3], "f0.txt", "{}", out.content);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }
}
