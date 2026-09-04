//! The `edit` tool: precise string replacement in one text file.
//!
//! 防御纪律(AGENTS.md):
//! - `old_string` 必须**精确出现一次**才动手;0 次或多次都报错,
//!   绝不猜测、绝不默默继续;
//! - `replace_all=true` 显式允许多点替换;
//! - 文件必须 UTF-8(编辑是文本操作);
//! - 替换结果返回计数与行锚点,幂等:再次执行同样的 edit 会因
//!   `old_string` 不存在而报错,不会重复写入。

use async_trait::async_trait;
use denia_core::session::PermissionMode;
use denia_core::tool::ToolSchema;
use serde::Deserialize;

use crate::permission::{denial_marker, escalation_hint};
use crate::{Tool, ToolContext, ToolOutput, parse_args_lenient, resolve_within};

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    /// 要替换的原文,必须精确出现一次(除非 replace_all)。
    old_string: String,
    /// 替换成的新文本。
    new_string: String,
    /// true 时替换所有出现(默认 false,多点出现会报错)。
    #[serde(default)]
    replace_all: bool,
}

/// Replaces one exact string occurrence in a text file.
pub struct EditTool {
    schema: ToolSchema,
}

impl EditTool {
    pub fn new() -> Self {
        Self {
            schema: ToolSchema {
                name: "edit".to_string(),
                description: "Replace an exact substring in one UTF-8 text file. `old_string` must appear exactly once \
                    unless `replace_all` is true (multiple occurrences otherwise fail). The line number of the first \
                    replacement is reported. Prefer read_file first, and re-read to verify the change."
                    .to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File to edit; a relative path resolves in the session workspace." },
                        "old_string": { "type": "string", "description": "Exact existing text to replace (whitespace and newlines are significant)." },
                        "new_string": { "type": "string", "description": "Replacement text." },
                        "replace_all": { "type": "boolean", "description": "Replace every occurrence instead of requiring exactly one (default false)." },
                        "sandbox_permissions": {
                            "type": "string",
                            "enum": ["workspace-write", "danger-full-access"],
                            "description": "The wider sandbox mode this file operation needs. Only valid as a one-shot retry of an operation the sandbox just denied; requires justification and user approval."
                        },
                        "justification": {
                            "type": "string",
                            "description": "Required with sandbox_permissions: one sentence for the user explaining why this exact file operation needs the wider access."
                        }
                    },
                    "required": ["path", "old_string", "new_string"]
                }),
            },
        }
    }
}

impl Default for EditTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for EditTool {
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn execute(&self, arguments: &str, ctx: &ToolContext) -> ToolOutput {
        let args: EditArgs = match parse_args_lenient(arguments) {
            Ok(args) => args,
            Err(error) => {
                return ToolOutput {
                    content: format!("invalid arguments: {error}"),
                    is_error: true,
                };
            }
        };
        if args.old_string.is_empty() {
            return ToolOutput {
                content: "old_string must not be empty".to_string(),
                is_error: true,
            };
        }
        let effective = ctx.effective_permission();
        if effective == PermissionMode::ReadOnly {
            return ToolOutput {
                content: format!(
                    "{}\n{}",
                    denial_marker(effective),
                    escalation_hint("operation")
                ),
                is_error: true,
            };
        }
        let path = match resolve_within(&ctx.cwd, &args.path, false) {
            Ok(path) => path,
            Err(message) => {
                return ToolOutput {
                    content: message,
                    is_error: true,
                };
            }
        };
        if effective == PermissionMode::WorkspaceWrite && !path.starts_with(&ctx.cwd) {
            return ToolOutput {
                content: format!(
                    "{}\n{}",
                    denial_marker(effective),
                    escalation_hint("operation")
                ),
                is_error: true,
            };
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                return ToolOutput {
                    content: format!("read failed: {error}"),
                    is_error: true,
                };
            }
        };

        // CRLF 容错(对齐 dsh fs-e2b):内容与 old/new 都在 LF 视图上精确匹配,
        // 写回时按原文件主导换行风格还原,避免 Windows 文件被搞成混合换行。
        let crlf = detect_crlf(&text);
        let view = normalize_line_endings(&text);
        let old_view = normalize_line_endings(&args.old_string);
        let new_view = normalize_line_endings(&args.new_string);

        let count = view.matches(&old_view).count();
        if count == 0 {
            return ToolOutput {
                content: format!(
                    "old_string not found in {}: {:?}",
                    display(&path, &ctx.cwd),
                    args.old_string
                ),
                is_error: true,
            };
        }
        if count > 1 && !args.replace_all {
            return ToolOutput {
                content: format!(
                    "old_string occurs {count} times in {}; specify more context or set replace_all=true",
                    display(&path, &ctx.cwd)
                ),
                is_error: true,
            };
        }

        let first_line = line_of(&view, &old_view);
        let updated_view = if args.replace_all {
            view.replace(&old_view, &new_view)
        } else {
            view.replacen(&old_view, &new_view, 1)
        };
        let updated = restore_line_endings(&updated_view, crlf);

        if let Some(file_history) = &ctx.file_history {
            if let Err(message) = file_history.track_before_write(&path).await {
                return ToolOutput {
                    content: format!("file history backup failed: {message}"),
                    is_error: true,
                };
            }
        }
        if let Err(error) = std::fs::write(&path, &updated) {
            return ToolOutput {
                content: format!("write failed: {error}"),
                is_error: true,
            };
        }
        let verb = if count > 1 {
            format!("{count} occurrences")
        } else {
            "1 occurrence".to_string()
        };
        ToolOutput {
            content: format!(
                "Replaced {verb} of {:?} in {} (first replacement on line {})",
                args.old_string,
                display(&path, &ctx.cwd),
                first_line
            ),
            is_error: false,
        }
    }
}

fn display(path: &std::path::Path, cwd: &std::path::Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// 1-based line number of the first occurrence (newline counting only).
fn line_of(text: &str, needle: &str) -> usize {
    let pos = text.find(needle).unwrap_or(0);
    1 + text[..pos].bytes().filter(|byte| *byte == b'\n').count()
}

/// 把 CRLF 统一成 LF(对齐 dsh `normalizeLineEndings`)。
fn normalize_line_endings(value: &str) -> String {
    value.replace("\r\n", "\n")
}

/// 采样前 4096 个字符,判断文件主导换行风格是否为 CRLF(对齐 dsh `detectsCrlf`)。
fn detect_crlf(value: &str) -> bool {
    let sample: String = value.chars().take(4096).collect();
    let crlf = sample.matches("\r\n").count();
    let lf_total = sample.matches('\n').count();
    let lf = lf_total - crlf;
    crlf > lf
}

/// 按原文件主导风格还原换行:CRLF 文件把 LF 视图写回 `\r\n`(对齐 dsh `restoreLineEndings`)。
fn restore_line_endings(value: &str, crlf: bool) -> String {
    if crlf {
        normalize_line_endings(value).replace('\n', "\r\n")
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn temp_root() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "denia-edit-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn context(cwd: std::path::PathBuf) -> ToolContext {
        ToolContext {
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

    #[tokio::test]
    async fn replaces_single_occurrence() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "hello\nworld\ngoodbye world\n").unwrap();
        let ctx = context(root.clone());
        let tool = EditTool::new();
        let out = tool
            .execute(
                r#"{"path":"a.txt","old_string":"hello","new_string":"goodbye"}"#,
                &ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("line 1"), "{}", out.content);
        let after = std::fs::read_to_string(root.join("a.txt")).unwrap();
        assert_eq!(after, "goodbye\nworld\ngoodbye world\n");
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn ambiguous_occurrence_fails_without_replace_all() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "world world\n").unwrap();
        let ctx = context(root.clone());
        let tool = EditTool::new();
        let out = tool
            .execute(
                r#"{"path":"a.txt","old_string":"world","new_string":"earth"}"#,
                &ctx,
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("2 times"), "{}", out.content);
        // 文件未被改动。
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "world world\n"
        );
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn replace_all_replaces_every_occurrence() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "x x x\n").unwrap();
        let ctx = context(root.clone());
        let tool = EditTool::new();
        let out = tool
            .execute(
                r#"{"path":"a.txt","old_string":"x","new_string":"y","replace_all":true}"#,
                &ctx,
            )
            .await;
        assert!(!out.is_error);
        assert!(out.content.contains("3 occurrences"), "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "y y y\n"
        );
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn missing_old_string_is_an_error() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "hello\n").unwrap();
        let ctx = context(root.clone());
        let tool = EditTool::new();
        let out = tool
            .execute(
                r#"{"path":"a.txt","old_string":"nope","new_string":"y"}"#,
                &ctx,
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("not found"));
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn edits_crlf_file_with_lf_old_string() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "hello\r\nworld\r\ngoodbye world\r\n").unwrap();
        let ctx = context(root.clone());
        let tool = EditTool::new();
        let out = tool
            .execute(
                r#"{"path":"a.txt","old_string":"hello\nworld","new_string":"goodbye\nworld"}"#,
                &ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        let after = std::fs::read_to_string(root.join("a.txt")).unwrap();
        assert_eq!(after, "goodbye\r\nworld\r\ngoodbye world\r\n");
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }
}
