//! Model-facing tools: `bash`, `read_file`, `write_file`.
//!
//! Every failure mode (bad arguments, spawn errors, timeouts, path escapes)
//! normalizes into an `is_error` output — tools never panic and never return
//! a Rust error, so one tool call always yields exactly one model-facing
//! result.

mod bash;
mod files;

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use dshrs_core::tool::ToolSchema;
use tokio_util::sync::CancellationToken;

pub use bash::BashTool;
pub use files::{ReadFileTool, WriteFileTool};

/// Execution context handed to every tool call.
pub struct ToolContext {
    /// The session's working directory; relative paths anchor here.
    pub cwd: PathBuf,
    /// Cancels the running call when the turn is aborted.
    pub cancel: CancellationToken,
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

/// The shipped tool set: bash + read_file + write_file.
pub fn default_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    registry.register(Arc::new(BashTool::default()));
    registry.register(Arc::new(ReadFileTool::default()));
    registry.register(Arc::new(WriteFileTool::default()));
    registry
}

/// Resolves one raw path inside the session workspace.
///
/// Relative paths anchor at `cwd`; absolute paths are accepted only when they
/// stay inside it. Lexical normalization rejects `..` escapes.
pub(crate) fn resolve_within(cwd: &Path, raw: &str) -> Result<PathBuf, String> {
    if raw.trim().is_empty() {
        return Err("an empty path is not a path".to_string());
    }
    let mut out = cwd.to_path_buf();
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
    if !out.starts_with(cwd) {
        return Err(format!("path '{raw}' escapes the session workspace"));
    }
    Ok(out)
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
        let inside = resolve_within(&cwd, "src/main.rs").unwrap();
        assert!(inside.starts_with(&cwd));
        assert!(resolve_within(&cwd, "../secret").is_err());
        assert!(resolve_within(&cwd, "a/../../secret").is_err());
        assert!(resolve_within(&cwd, "").is_err());
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
