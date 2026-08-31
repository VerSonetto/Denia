//! The `read_file` and `write_file` tools.

use async_trait::async_trait;
use dshrs_core::tool::ToolSchema;
use serde::Deserialize;

use crate::{Tool, ToolContext, ToolOutput, parse_args_lenient, resolve_within, truncate};

const READ_CAP: usize = 256_000;

#[derive(Deserialize)]
struct PathArgs {
    path: String,
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

/// Reads one UTF-8 text file from the session workspace.
pub struct ReadFileTool {
    schema: ToolSchema,
}

impl ReadFileTool {
    pub fn new() -> Self {
        Self {
            schema: ToolSchema {
                name: "read_file".to_string(),
                description: "Read one UTF-8 text file from the session workspace. Relative paths anchor at the workspace.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }),
            },
        }
    }
}

impl Default for ReadFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn execute(&self, arguments: &str, ctx: &ToolContext) -> ToolOutput {
        let args: PathArgs = match parse_args_lenient(arguments) {
            Ok(args) => args,
            Err(error) => {
                return ToolOutput {
                    content: format!("invalid arguments: {error}"),
                    is_error: true,
                };
            }
        };
        let path = match resolve_within(&ctx.cwd, &args.path, ctx.confined) {
            Ok(path) => path,
            Err(message) => return ToolOutput { content: message, is_error: true },
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => ToolOutput {
                content: truncate(&text, READ_CAP),
                is_error: false,
            },
            Err(error) => ToolOutput {
                content: format!("read failed: {error}"),
                is_error: true,
            },
        }
    }
}

/// Overwrites one UTF-8 text file in the session workspace.
pub struct WriteFileTool {
    schema: ToolSchema,
}

impl WriteFileTool {
    pub fn new() -> Self {
        Self {
            schema: ToolSchema {
                name: "write_file".to_string(),
                description: "Write one UTF-8 text file in the session workspace, creating parent directories and overwriting any existing file.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"]
                }),
            },
        }
    }
}

impl Default for WriteFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn execute(&self, arguments: &str, ctx: &ToolContext) -> ToolOutput {
        let args: WriteArgs = match parse_args_lenient(arguments) {
            Ok(args) => args,
            Err(error) => {
                return ToolOutput {
                    content: format!("invalid arguments: {error}"),
                    is_error: true,
                };
            }
        };
        let path = match resolve_within(&ctx.cwd, &args.path, ctx.confined) {
            Ok(path) => path,
            Err(message) => return ToolOutput { content: message, is_error: true },
        };
        if let Some(parent) = path.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                return ToolOutput {
                    content: format!("create_dir_all failed: {error}"),
                    is_error: true,
                };
            }
        }
        match std::fs::write(&path, &args.content) {
            Ok(()) => ToolOutput {
                content: format!("wrote {} bytes to {}", args.content.len(), args.path),
                is_error: false,
            },
            Err(error) => ToolOutput {
                content: format!("write failed: {error}"),
                is_error: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn workspace() -> (tempfile_like::TempDir, ToolContext) {
        let dir = std::env::temp_dir().join(format!("dshrs-tools-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let context = ToolContext {
            cwd: dir.clone(),
            cancel: CancellationToken::new(),
            confined: true,
        };
        (tempfile_like::TempDir(dir), context)
    }

    mod tempfile_like {
        pub struct TempDir(pub std::path::PathBuf);
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn uuid_like() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            .to_string()
    }

    #[tokio::test]
    async fn write_then_read_round_trip() {
        let (_guard, ctx) = workspace();
        let writer = WriteFileTool::new();
        let reader = ReadFileTool::new();
        let wrote = writer
            .execute(
                r#"{"path":"notes/hello.txt","content":"hello harness"}"#,
                &ctx,
            )
            .await;
        assert!(!wrote.is_error, "{}", wrote.content);
        let read = reader
            .execute(r#"{"path":"notes/hello.txt"}"#, &ctx)
            .await;
        assert!(!read.is_error);
        assert_eq!(read.content, "hello harness");
    }

    #[tokio::test]
    async fn traversal_is_rejected() {
        let (_guard, ctx) = workspace();
        let reader = ReadFileTool::new();
        let out = reader.execute(r#"{"path":"../outside.txt"}"#, &ctx).await;
        assert!(out.is_error);
        assert!(out.content.contains("escapes"));
    }

    #[tokio::test]
    async fn missing_file_is_error() {
        let (_guard, ctx) = workspace();
        let reader = ReadFileTool::new();
        let out = reader.execute(r#"{"path":"nope.txt"}"#, &ctx).await;
        assert!(out.is_error);
    }
}
