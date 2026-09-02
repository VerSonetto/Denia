//! The `glob` tool: discover files by glob pattern.
//!
//! 走 `ignore` 的 gitignore 语法 overrides(dsh 同款语义):
//! - 无 "/" 的模式按 basename 匹配**任意深度**(`*.rs` ≈ `**/*.rs`);
//! - 结果只含文件、不含目录;按修改时间降序(新鲜优先);
//! - 默认包含隐藏与忽略文件(VCS 元数据目录除外),与 dsh glob 契约一致;
//! - 并行遍历,超大目录来去自如;路径回收上限防止把结果撑爆。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use denia_core::tool::ToolSchema;
use ignore::WalkBuilder;
use serde::Deserialize;

use crate::{Tool, ToolContext, ToolOutput, parse_args_lenient, resolve_within};

const DEFAULT_MAX_RESULTS: usize = 100;
const HARD_MAX_RESULTS: usize = 5_000;
/// 收集上限:超出的路径不再收集(结果本身仍会截断提示)。
const COLLECT_CAP: usize = 200_000;

/// gitignore 语义下不会出现在列表里的 VCS 元数据目录,显式排除。
const VCS_EXCLUDES: &[&str] = &[".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];

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
            Err(error) => return ToolOutput { content: format!("invalid arguments: {error}"), is_error: true },
        };
        if args.pattern.trim().is_empty() {
            return ToolOutput { content: "pattern must be a non-empty string".to_string(), is_error: true };
        }
        let root = match resolve_within(&ctx.cwd, args.path.as_deref().unwrap_or("."), ctx.confined) {
            Ok(path) => path,
            Err(message) => return ToolOutput { content: message, is_error: true },
        };
        if !root.is_dir() {
            return ToolOutput { content: format!("path '{}' is not a directory", root.display()), is_error: true };
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
                .follow_links(false);
            let mut overrides = ignore::overrides::OverrideBuilder::new(&root);
            overrides
                .add(&args.pattern)
                .map_err(|error| format!("invalid glob pattern: {error}"))?;
            for vcs in VCS_EXCLUDES {
                overrides.add(&format!("!{vcs}/")).ok();
                overrides.add(&format!("!**/{vcs}/**")).ok();
            }
            let overrides = overrides
                .build()
                .map_err(|error| format!("invalid glob pattern: {error}"))?;
            builder.overrides(overrides);
            let walker = builder.build_parallel();

            let found: Arc<std::sync::Mutex<Vec<(PathBuf, SystemTime)>>> =
                Arc::new(std::sync::Mutex::new(Vec::new()));
            let overflow = Arc::new(std::sync::atomic::AtomicBool::new(false));
            walker.run(|| {
                let cancel = cancel.clone();
                let found = found.clone();
                let overflow = overflow.clone();
                Box::new(move |entry| {
                    if cancel.is_cancelled() {
                        return ignore::WalkState::Quit;
                    }
                    let Ok(entry) = entry else { return ignore::WalkState::Continue };
                    let Some(file_type) = entry.file_type() else {
                        return ignore::WalkState::Continue;
                    };
                    if !file_type.is_file() {
                        return ignore::WalkState::Continue;
                    }
                    // overrides 命中即收集;收集满后仅置溢出标记。
                    let mut all = found.lock().unwrap();
                    if all.len() < COLLECT_CAP {
                        let modified = std::fs::metadata(entry.path())
                            .and_then(|meta| meta.modified())
                            .unwrap_or(SystemTime::UNIX_EPOCH);
                        all.push((entry.path().to_path_buf(), modified));
                    } else {
                        overflow.store(true, std::sync::atomic::Ordering::Relaxed);
                        return ignore::WalkState::Quit;
                    }
                    ignore::WalkState::Continue
                })
            });
            let mut found = std::mem::take(&mut *found.lock().unwrap());

            found.sort_by(|a, b| b.1.cmp(&a.1));
            let paths: Vec<String> = found
                .into_iter()
                .map(|(path, _)| {
                    path.strip_prefix(&display_root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/")
                })
                .collect();
            Ok::<_, String>((paths, overflow.load(std::sync::atomic::Ordering::Relaxed)))
        })
        .await;

        match result {
            Ok(Ok((paths, overflow))) => {
                if paths.is_empty() {
                    return ToolOutput { content: "No files found".to_string(), is_error: false };
                }
                let shown = &paths[..paths.len().min(max)];
                let mut output = shown.join("\n");
                if paths.len() > max {
                    output.push_str(&format!(
                        "\n\n(Showing {} of {} paths; narrow the pattern to see more)",
                        shown.len(),
                        paths.len()
                    ));
                }
                if overflow {
                    output.push_str("\n\n(collection capped; results may be incomplete)");
                }
                ToolOutput { content: output, is_error: false }
            }
            Ok(Err(message)) => ToolOutput { content: message, is_error: true },
            Err(join_error) => ToolOutput { content: format!("glob worker failed: {join_error}"), is_error: true },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::path::Path;
    use tokio_util::sync::CancellationToken;

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "denia-glob-{}",
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
            cwd,
            cancel: CancellationToken::new(),
            confined: true,
            emit_event: None,
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
}
