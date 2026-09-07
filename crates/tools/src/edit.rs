//! The `edit` tool: precise string replacement in one text file.
//!
//! 防御纪律(AGENTS.md):
//! - `old_string` 必须**精确出现一次**才动手;0 次或多次都报错,
//!   绝不猜测、绝不默默继续;
//! - `replace_all=true` 显式允许多点替换;
//! - 文件必须 UTF-8(编辑是文本操作);
//! - 替换结果返回计数与行锚点,幂等:再次执行同样的 edit 会因
//!   `old_string` 不存在而报错,不会重复写入。
//!
//! 性能与容错约定(见 [`crate::support`]):阻塞 fs 走 `spawn_blocking`;
//! 参数解析走宽容入口;错误统一 `建议:` 格式,给模型可执行的下一步。

use async_trait::async_trait;
use denia_core::session::PermissionMode;
use denia_core::tool::ToolSchema;
use serde::Deserialize;

use crate::permission::{denial_marker, escalation_hint};
use crate::support::{parse_tool_args, tool_error};
use crate::{Tool, ToolContext, ToolOutput, resolve_within};

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
                description: "对工作区内的一个 UTF-8 文本文件做精确字符串替换。old_string 必须与文件原文完全一致(空白与换行都算)且默认只能出现一次,多点出现会报错;replace_all=true 时替换全部出现。返回首次替换发生的行号。先 read_file 确认原文,改完可再读校验。".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "要编辑的文件路径;相对路径锚定会话工作区。" },
                        "old_string": { "type": "string", "description": "要被替换的原文,必须与文件内容逐字符一致(包含缩进、空格与换行)。" },
                        "new_string": { "type": "string", "description": "替换成的新文本。" },
                        "replace_all": { "type": "boolean", "description": "替换全部出现而不是要求恰好一次(默认 false)。" },
                        "sandbox_permissions": {
                            "type": "string",
                            "enum": ["workspace-write", "danger-full-access"],
                            "description": "本次文件操作需要的更宽沙箱模式;仅用于对刚被沙箱拒绝的操作做一次性重试,必须搭配 justification,且需要用户审批。"
                        },
                        "justification": {
                            "type": "string",
                            "description": "与 sandbox_permissions 搭配必填:一句话向用户说明为什么这个文件操作需要更宽的权限。"
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
        let args: EditArgs = match parse_tool_args(arguments) {
            Ok(args) => args,
            Err(error) => {
                return tool_error(
                    format!("参数解析失败:{error}"),
                    "参数必须是 JSON 对象,必填字段为 path、old_string、new_string(都是字符串)",
                );
            }
        };
        if args.old_string.is_empty() {
            return tool_error(
                "old_string 不能为空",
                "整文件替换请用 write_file;old_string 是要被替换的原文字符串",
            );
        }
        let effective = ctx.effective_permission();
        if effective == PermissionMode::ReadOnly {
            return ToolOutput::error(format!(
                "{}\n{}",
                denial_marker(effective),
                escalation_hint("operation")
            ));
        }
        // confined 语义与读取类工具一致:沙箱开启时路径必须落在 cwd 内。
        let path = match resolve_within(&ctx.cwd, &args.path, ctx.confined) {
            Ok(path) => path,
            Err(message) => {
                return tool_error(
                    message,
                    "相对路径锚定会话工作区;沙箱开启时不能编辑工作区之外的路径",
                );
            }
        };
        if effective == PermissionMode::WorkspaceWrite && !path.starts_with(&ctx.cwd) {
            return ToolOutput::error(format!(
                "{}\n{}",
                denial_marker(effective),
                escalation_hint("operation")
            ));
        }
        // 读文件是阻塞 IO,丢进 blocking 池;写回同理。
        let text = {
            let path = path.clone();
            tokio::task::spawn_blocking(move || std::fs::read_to_string(&path)).await
        };
        let text = match text {
            Ok(Ok(text)) => text,
            Ok(Err(error)) => {
                return tool_error(
                    format!("读取 {} 失败:{error}", args.path),
                    "确认文件存在且是 UTF-8 文本;新建文件请用 write_file",
                );
            }
            Err(join_error) => {
                return tool_error(format!("编辑任务失败:{join_error}"), "请重试一次");
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
            // 尽力给出贴近的失败原因:原文里是否只差空白。
            let relaxed_match = view.split_whitespace().collect::<String>()
                == old_view.split_whitespace().collect::<String>();
            let hint = if relaxed_match {
                "原文里存在仅空白/换行不同的相近内容:逐字符核对缩进与换行,直接从 read_file 的输出复制 old_string"
            } else {
                "先 read_file 确认当前原文(文件可能已被之前的编辑改动),再从输出中逐字符复制 old_string"
            };
            return tool_error(
                format!(
                    "在 {} 中找不到 old_string({:?})",
                    display(&path, &ctx.cwd),
                    preview(&args.old_string)
                ),
                hint,
            );
        }
        if count > 1 && !args.replace_all {
            return tool_error(
                format!(
                    "old_string({:?}) 在 {} 中出现了 {count} 次",
                    preview(&args.old_string),
                    display(&path, &ctx.cwd)
                ),
                "在 old_string 里带上更多上下文使其唯一,或确认要全部替换时设置 replace_all=true",
            );
        }

        let first_line = line_of(&view, &old_view);
        let updated_view = if args.replace_all {
            view.replace(&old_view, &new_view)
        } else {
            view.replacen(&old_view, &new_view, 1)
        };
        let updated = restore_line_endings(&updated_view, crlf);

        if let Some(file_history) = &ctx.file_history
            && let Err(message) = file_history.track_before_write(&path).await
        {
            return tool_error(
                format!("文件历史备份失败:{message}"),
                "备份失败时不会写入;可重试一次,持续失败请报告",
            );
        }
        let write = {
            let path = path.clone();
            let updated = updated.clone();
            tokio::task::spawn_blocking(move || std::fs::write(&path, updated.as_bytes())).await
        };
        match write {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                return tool_error(
                    format!("写入 {} 失败:{error}", args.path),
                    "确认目标路径可写;文件被占用时稍后重试",
                );
            }
            Err(join_error) => {
                return tool_error(format!("编辑任务失败:{join_error}"), "请重试一次");
            }
        }
        let verb = if count > 1 {
            format!("{count} 处")
        } else {
            "1 处".to_string()
        };
        ToolOutput::text(format!(
            "已在 {} 替换 {verb}({:?} → 新文本),首次替换发生在第 {first_line} 行",
            display(&path, &ctx.cwd),
            preview(&args.old_string),
        ))
    }
}

fn display(path: &std::path::Path, cwd: &std::path::Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// 错误消息里的 old_string 预览:太长会撑爆工具结果,只留前 120 字符。
fn preview(text: &str) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(120).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
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
        assert!(out.content.contains("第 1 行"), "{}", out.content);
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
        assert!(out.content.contains("出现了 2 次"), "{}", out.content);
        assert!(out.content.contains("replace_all"), "{}", out.content);
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
        assert!(out.content.contains("3 处"), "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "y y y\n"
        );
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn missing_old_string_hints_whitespace_mismatch() {
        let root = temp_root();
        // 原文只有空白差异:建议应指向"逐字符核对缩进与换行"。
        std::fs::write(root.join("a.txt"), "hello  world\n").unwrap();
        let ctx = context(root.clone());
        let tool = EditTool::new();
        let out = tool
            .execute(
                r#"{"path":"a.txt","old_string":"hello world","new_string":"y"}"#,
                &ctx,
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("找不到 old_string"), "{}", out.content);
        assert!(out.content.contains("空白/换行"), "{}", out.content);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn missing_old_string_hints_reread_when_absent() {
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
        assert!(out.content.contains("read_file"), "{}", out.content);
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
