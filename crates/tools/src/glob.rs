//! The `glob` tool: discover files by glob pattern.
//!
//! 走 `ignore` 的 gitignore 语法 overrides(dsh 同款语义):
//! - `path` 接受绝对路径与相对路径(相对路径锚定会话工作区),统一走
//!   [`resolve_within`],沙箱开启时越界即拒绝;
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

use crate::support::parse_tool_args;
use crate::{Tool, ToolContext, ToolOutput, resolve_within};

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
    /// 搜索目录;绝对路径原样使用,相对路径锚定会话工作区。
    #[serde(default)]
    path: Option<String>,
    /// 最多返回的路径数(默认 100,上限 5000)。
    #[serde(default)]
    max_results: Option<usize>,
    /// 跳过排序后(最新在前)的前 N 条路径,与 max_results 组成分页。
    #[serde(default)]
    offset: usize,
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
                description: "按 glob 模式查找文件,返回匹配的文件路径(不含目录),按修改时间从新到旧排序。模式里没有 \"/\" 时匹配任意深度的文件名,所以 \"*.rs\" 会搜索整棵树。path 可传绝对路径或相对工作区的相对路径,缺省为会话工作区。默认包含隐藏文件,VCS 元数据目录(.git 等)排除。结果路径以会话工作区为基准显示(工作区外的结果给绝对路径)。结果多时用 offset/max_results 分页。".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "glob 模式(如 \"**/*.ts\"、\"src/**/*.test.js\"、\"*.{rs,toml}\");没有 \"/\" 时匹配任意深度。"
                        },
                        "path": {
                            "type": "string",
                            "description": "搜索目录,默认会话工作区。绝对路径与相对路径都可用:相对路径锚定会话工作区(如 \"src\" 或 \"./src\")。"
                        },
                        "max_results": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": HARD_MAX_RESULTS as i64,
                            "description": "最多返回的路径数,默认 100。"
                        },
                        "offset": {
                            "type": "integer",
                            "minimum": 0,
                            "default": 0,
                            "description": "跳过排序后前 N 条路径(从 0 开始);与 max_results 配合分页浏览完整结果。"
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
        let args: GlobArgs = match parse_tool_args(arguments) {
            Ok(args) => args,
            Err(error) => {
                return ToolOutput::error(format!(
                    "[工具错误] 参数解析失败:{error}\n建议:参数必须是 JSON 对象,必填字段为 pattern(字符串)"
                ));
            }
        };
        if args.pattern.trim().is_empty() {
            return ToolOutput::error(
                "[工具错误] pattern 不能为空\n建议:pattern 是 glob 模式,如 \"**/*.ts\";没有 \"/\" 时匹配任意深度的文件名",
            );
        }
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
                "[工具错误] '{}' 不是目录\n建议:path 必须指向存在的目录(绝对或相对工作区均可);找文件的内容请用 grep 或 read_file",
                root.display()
            ));
        }
        let max = args
            .max_results
            .unwrap_or(DEFAULT_MAX_RESULTS)
            .clamp(1, HARD_MAX_RESULTS);
        let offset = args.offset;
        // 每线程 batch 收集前 offset+max 条,归并排序后切窗口 [offset, offset+max)。
        let collect_cap = offset.saturating_add(max);

        let cancel = ctx.cancel.clone();
        let display_root = ctx.cwd.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut builder = WalkBuilder::new(&root);
            builder
                // 注意极性:`hidden` 是"过滤器开关",true = 把隐藏条目滤掉。
                // 这里要包含隐藏文件(dsh glob 契约:/--hidden/),所以传 false;
                // VCS 元数据目录不走这条,在 visitor 里按 basename 单独剪枝。
                .hidden(false)
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
                let mut batch =
                    TopNBatch::new(collect_cap, Arc::clone(&batches), Arc::clone(&total_hits));
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
                    return ToolOutput::text("未找到匹配的文件");
                }
                let skipped = offset.min(ranked.len());
                let window: Vec<_> = ranked.iter().skip(skipped).take(max).collect();
                if window.is_empty() {
                    return ToolOutput::text(format!(
                        "offset={offset} 之后再无匹配路径(总共 {total} 条);减小 offset 或缩小 pattern"
                    ));
                }
                let mut output = String::new();
                for (modified, path) in &window {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    let _ = modified;
                    let rel = path
                        .strip_prefix(&display_root)
                        .unwrap_or(path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    output.push_str(&rel);
                }
                let remaining_total = total.saturating_sub(skipped);
                if remaining_total > window.len() {
                    output.push_str(&format!(
                        "\n\n(显示第 {}-{} 条,共 {total} 条;增大 offset 看下一页,或缩小 pattern)",
                        skipped + 1,
                        skipped + window.len()
                    ));
                }
                if overflow {
                    output.push_str("\n\n(收集已达上限,结果可能不完整)");
                }
                ToolOutput::text(output)
            }
            Ok(Err(message)) => ToolOutput::error(format!(
                "[工具错误] {message}\n建议:核对 glob 模式语法;没有 \"/\" 的模式匹配任意深度的文件名"
            )),
            Err(join_error) => ToolOutput::error(format!(
                "[工具错误] glob 任务失败:{join_error}\n建议:请重试一次"
            )),
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
        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(b"x").unwrap();
    }

    /// 绝对路径塞进 JSON:统一写成正斜杠(Windows 上同样合法),免去转义。
    fn json_path(path: &Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }

    #[tokio::test]
    async fn relative_path_is_anchored_at_workspace_and_absolute_path_is_accepted() {
        let root = temp_root();
        touch(&root.join("src/a.rs"));
        touch(&root.join("src/deep/b.rs"));
        let ctx = context(root.clone());
        let tool = GlobTool::new();

        // 相对路径锚定工作区。
        let out = tool
            .execute(r#"{"pattern":"*.rs","path":"src"}"#, &ctx)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("src/a.rs"), "{}", out.content);

        // 同一个目录写成绝对路径,必须得到同样的结果(输出仍以工作区为基准)。
        let raw = format!(
            r#"{{"pattern":"a.rs","path":"{}"}}"#,
            json_path(&root.join("src"))
        );
        let out = tool.execute(&raw, &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.content.trim(), "src/a.rs", "{}", out.content);

        // 工作区根本身写成绝对路径也可用(等于缺省 path)。
        let raw = format!(r#"{{"pattern":"*.rs","path":"{}"}}"#, json_path(&root));
        let out = tool.execute(&raw, &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("src/a.rs"), "{}", out.content);
        assert!(out.content.contains("src/deep/b.rs"), "{}", out.content);

        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn absolute_path_outside_workspace_is_rejected() {
        let root = temp_root();
        let outside = temp_root();
        touch(&root.join("a.rs"));
        touch(&outside.join("b.rs"));
        let ctx = context(root.clone());
        let tool = GlobTool::new();
        let raw = format!(r#"{{"pattern":"*.rs","path":"{}"}}"#, json_path(&outside));
        let out = tool.execute(&raw, &ctx).await;
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("越出了会话工作区"), "{}", out.content);
        assert!(out.content.contains("建议:"), "{}", out.content);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
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

    /// 回归保护:描述承诺"包含隐藏文件",实现就必须真的包含。
    ///
    /// `ignore::WalkBuilder::hidden` 是过滤器开关——传 true 是把隐藏条目**滤掉**,
    /// 不是"打开隐藏"。这里曾写成 `.hidden(true)` 并配注释"包含隐藏文件",
    /// 于是模型按描述去找 `.github/workflows/*.yml` 会颗粒无收,还以为文件不存在。
    #[tokio::test]
    async fn hidden_files_are_included_except_under_vcs_directories() {
        let root = temp_root();
        touch(&root.join(".github/workflows/ci.yml"));
        touch(&root.join(".gitignore"));
        touch(&root.join("src/keep.rs"));
        let ctx = context(root.clone());
        let tool = GlobTool::new();
        let out = tool.execute(r#"{"pattern":"*"}"#, &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains(".github/workflows/ci.yml"),
            "隐藏目录下的文件必须出现在结果里:{}",
            out.content
        );
        assert!(out.content.contains(".gitignore"), "{}", out.content);
        assert!(out.content.contains("src/keep.rs"), "{}", out.content);
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
        assert!(out.content.contains("共 20 条"), "{}", out.content);
        assert!(out.content.contains("增大 offset"), "{}", out.content);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn offset_pages_through_results() {
        let root = temp_root();
        for i in 0..8 {
            touch(&root.join(format!("f{i}.txt")));
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        let ctx = context(root);
        let tool = GlobTool::new();
        // 第一页:最新 3 条(f7..f5)。
        let page1 = tool
            .execute(r#"{"pattern":"*.txt","max_results":3}"#, &ctx)
            .await;
        assert!(!page1.is_error, "{}", page1.content);
        let lines1: Vec<&str> = page1.content.lines().collect();
        assert_eq!(lines1[0], "f7.txt", "{}", page1.content);
        // 第二页:跳过 3 条,拿下一批(f4..f2)。
        let page2 = tool
            .execute(r#"{"pattern":"*.txt","max_results":3,"offset":3}"#, &ctx)
            .await;
        assert!(!page2.is_error, "{}", page2.content);
        let lines2: Vec<&str> = page2.content.lines().collect();
        assert_eq!(lines2[0], "f4.txt", "{}", page2.content);
        assert!(lines2.contains(&"f2.txt"), "{}", page2.content);
        assert!(!page2.content.contains("f7.txt"), "{}", page2.content);
        // 越界 offset:明确提示。
        let past = tool
            .execute(r#"{"pattern":"*.txt","offset":100}"#, &ctx)
            .await;
        assert!(!past.is_error, "{}", past.content);
        assert!(
            past.content.contains("offset=100 之后再无匹配路径"),
            "{}",
            past.content
        );
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
