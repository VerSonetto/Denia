//! Model-facing tools: `bash`, `read_file`, `write_file`.
//!
//! Every failure mode (bad arguments, spawn errors, timeouts, path escapes)
//! normalizes into an `is_error` output — tools never panic and never return
//! a Rust error, so one tool call always yields exactly one model-facing
//! result.

mod bash;
mod edit;
mod files;
pub mod glob;
pub mod grep;
pub mod prompt;
pub mod shell;

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use denia_core::session::SessionEvent;
use denia_core::tool::ToolSchema;
use tokio_util::sync::CancellationToken;

pub use bash::BashTool;
pub use edit::EditTool;
pub use files::{ReadFileTool, WriteFileTool};
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use prompt::{default_shipped, register_shipped_prompt, shipped_with_persona};
pub use todo::TodoWriteTool;

mod todo;

/// Session-event sink handed to tools that emit log-only state (todo_write).
/// The agent loop wires it to the session append + broadcast; tools never
/// touch the session handle directly.
pub type SessionEventSink = Arc<dyn Fn(SessionEvent) + Send + Sync>;

/// 文件历史后端:写文件类工具在真正落盘前调用,由宿主实现备份原文件。
#[async_trait]
pub trait FileHistoryBackend: Send + Sync {
    /// 记录一次即将发生的写操作;实现应在文件被修改前备份原内容。
    async fn track_before_write(&self, path: &std::path::Path) -> Result<(), String>;
}

/// Execution context handed to every tool call.
pub struct ToolContext {
    /// The session's working directory; relative paths anchor here.
    pub cwd: PathBuf,
    /// Cancels the running call when the turn is aborted.
    pub cancel: CancellationToken,
    /// Sandbox: confine file access to `cwd`. Off allows absolute paths
    /// outside the workspace.
    pub confined: bool,
    /// Whether the current model is marked as vision-capable; `read_file`
    /// injects images only when true.
    pub vision_supported: bool,
    /// Log-only event sink; `None` for callers with no owning session.
    pub emit_event: Option<SessionEventSink>,
    /// 文件回退备份后端;`None` 表示当前调用不参与文件快照。
    pub file_history: Option<Arc<dyn FileHistoryBackend>>,
}

/// One model-facing tool outcome.
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

/// A model-facing capability.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The declaration sent to the model.
    fn schema(&self) -> &ToolSchema;

    /// Executes one call; `arguments` is the raw JSON string from the model.
    async fn execute(&self, arguments: &str, ctx: &ToolContext) -> ToolOutput;
}

/// The deployment's tool set, in registration order.
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.push(tool);
    }

    /// Model-facing schemas in registration order.
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.iter().map(|tool| tool.schema().clone()).collect()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .iter()
            .find(|tool| tool.schema().name == name)
            .cloned()
    }
}

/// The shipped tool set: bash + read_file + write_file + todo_write + glob + grep + edit.
pub fn default_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    registry.register(Arc::new(BashTool::default()));
    registry.register(Arc::new(ReadFileTool::default()));
    registry.register(Arc::new(WriteFileTool::default()));
    registry.register(Arc::new(TodoWriteTool::default()));
    registry.register(Arc::new(GlobTool::default()));
    registry.register(Arc::new(GrepTool::default()));
    registry.register(Arc::new(EditTool::default()));
    registry
}

/// Resolves one raw path for a tool call.
///
/// Relative paths anchor at `cwd`. Absolute paths are accepted as-is when
/// the session is not confined; with the sandbox on, every resolved path
/// must stay inside `cwd`.
pub(crate) fn resolve_within(cwd: &Path, raw: &str, confined: bool) -> Result<PathBuf, String> {
    if raw.trim().is_empty() {
        return Err("an empty path is not a path".to_string());
    }
    let mut out = if Path::new(raw).is_absolute() {
        PathBuf::new()
    } else {
        cwd.to_path_buf()
    };
    for component in Path::new(raw).components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                out = PathBuf::from(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return Err(format!("path '{raw}' escapes the session workspace"));
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    if confined && !out.starts_with(cwd) {
        return Err(format!("path '{raw}' escapes the session workspace (sandbox on)"));
    }
    Ok(out)
}

/// 宽容参数解析:只取第一个 JSON 值,忽略尾部垃圾。
/// 模型偶尔在参数后吐多余字符,硬失败会浪费一整步。
pub(crate) fn parse_args_lenient<T: serde::de::DeserializeOwned>(
    raw: &str,
) -> Result<T, String> {
    let mut iter = serde_json::Deserializer::from_str(raw.trim()).into_iter::<T>();
    match iter.next() {
        Some(Ok(value)) => Ok(value),
        Some(Err(error)) => Err(error.to_string()),
        None => Err("参数为空".to_string()),
    }
}

pub(crate) fn truncate(text: &str, cap: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(cap).collect();
    if chars.next().is_some() {
        format!("{head}\n…[truncated]")
    } else {
        head
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_within_anchors_and_rejects_escapes() {
        let cwd = if cfg!(windows) {
            PathBuf::from("C:\\work")
        } else {
            PathBuf::from("/work")
        };
        let inside = resolve_within(&cwd, "src/main.rs", true).unwrap();
        assert!(inside.starts_with(&cwd));
        assert!(resolve_within(&cwd, "../secret", true).is_err());
        assert!(resolve_within(&cwd, "a/../../secret", true).is_err());
        // Unconfined sessions may leave the workspace.
        assert!(resolve_within(&cwd, "../elsewhere", false).is_ok());
        assert!(resolve_within(&cwd, "", true).is_err());
    }

    #[test]
    fn lenient_parse_ignores_trailing_junk() {
        #[derive(serde::Deserialize)]
        struct Args {
            path: String,
        }
        let parsed: Args = parse_args_lenient(r#"{"path": "app/src"} 谢谢"#).unwrap();
        assert_eq!(parsed.path, "app/src");
        assert!(parse_args_lenient::<Args>("not json").is_err());
    }

    #[test]
    fn truncate_caps_and_marks() {
        let long = "x".repeat(10);
        let cut = truncate(&long, 4);
        assert!(cut.starts_with("xxxx"));
        assert!(cut.contains("truncated"));
        assert_eq!(truncate("short", 10), "short");
    }
}
