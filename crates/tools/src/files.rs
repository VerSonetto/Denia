//! The `read_file` and `write_file` tools.
//!
//! `read_file` 读取图片文件时(魔数识别 png/jpeg/gif/webp/bmp)不返回乱码,
//! 而是把图片作为视觉输入注入会话(需要当前模型标记为可识图),
//! 与 DSH 的"读图工具"一致 —— 只是这里复用 read_file 一个入口。

use std::fs::File;
use std::io::{BufRead, BufReader};

use async_trait::async_trait;
use denia_core::session::PermissionMode;
use denia_core::tool::ToolSchema;
use serde::Deserialize;

use crate::permission::{denial_marker, escalation_hint};
use crate::{Tool, ToolContext, ToolOutput, parse_args_lenient, resolve_within, truncate};

/// 单次 read_file 返回的最大字符数(安全阀,防止单行超长把上下文撑爆)。
const READ_CAP: usize = 32_000;
/// read_file 默认最多读取的行数。
const DEFAULT_READ_LIMIT: u64 = 400;

/// 图片魔数表:(magic 前缀, mime)。
const IMAGE_MAGICS: &[(&[u8], &str)] = &[
    (&[0x89, b'P', b'N', b'G'], "image/png"),
    (&[0xFF, 0xD8, 0xFF], "image/jpeg"),
    (&[0x47, 0x49, 0x46, 0x38], "image/gif"),
    (&[0x42, 0x4D], "image/bmp"),
];

/// 嗅探图片类型并尽力解析尺寸(失败给 None)。
/// WebP 需要 8..12 的 "WEBP" 标记,单独判断。
fn sniff_image(data: &[u8]) -> Option<(&'static str, Option<u32>, Option<u32>)> {
    for (magic, mime) in IMAGE_MAGICS {
        if data.len() >= magic.len() && data.starts_with(magic) {
            let (width, height) = match *mime {
                "image/png" if data.len() >= 24 => (
                    u32::from_be_bytes(data[16..20].try_into().ok()?),
                    u32::from_be_bytes(data[20..24].try_into().ok()?),
                ),
                "image/gif" if data.len() >= 10 => (
                    u16::from_le_bytes(data[6..8].try_into().ok()?) as u32,
                    u16::from_le_bytes(data[8..10].try_into().ok()?) as u32,
                ),
                "image/bmp" if data.len() >= 26 => (
                    i32::from_le_bytes(data[18..22].try_into().ok()?) as u32,
                    i32::from_le_bytes(data[22..26].try_into().ok()?) as u32,
                ),
                "image/jpeg" => {
                    // 扫描 SOF0/SOF2 标记段拿尺寸(尽力而为)。
                    let mut offset = 2usize;
                    let mut size = (0u32, 0u32);
                    while offset + 9 < data.len() {
                        if data[offset] != 0xFF {
                            offset += 1;
                            continue;
                        }
                        let marker = data[offset + 1];
                        if marker == 0xD8 || marker == 0xD9 {
                            offset += 2;
                            continue;
                        }
                        let seg_len =
                            u16::from_be_bytes(data[offset + 2..offset + 4].try_into().ok()?) as usize;
                        if marker == 0xC0 || marker == 0xC2 {
                            size = (
                                u16::from_be_bytes(data[offset + 7..offset + 9].try_into().ok()?) as u32,
                                u16::from_be_bytes(data[offset + 5..offset + 7].try_into().ok()?) as u32,
                            );
                            break;
                        }
                        offset += 2 + seg_len;
                    }
                    if size.0 > 0 { (size.0, size.1) } else { (0, 0) }
                }
                _ => (0, 0),
            };
            let width = (width != 0).then_some(width);
            let height = (height != 0).then_some(height);
            return Some((mime, width, height));
        }
    }
    if data.len() > 12 && &data[8..12] == b"WEBP" {
        return Some(("image/webp", None, None));
    }
    None
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    /// 1-based 起始行;默认 1。
    #[serde(default = "default_read_offset")]
    offset: u64,
    /// 最多读取行数;默认 400。
    #[serde(default = "default_read_limit")]
    limit: u64,
}

fn default_read_offset() -> u64 {
    1
}

fn default_read_limit() -> u64 {
    DEFAULT_READ_LIMIT
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
                    "properties": {
                        "path": { "type": "string" },
                        "offset": {
                            "type": "integer",
                            "minimum": 1,
                            "default": 1,
                            "description": "1-based start line. Defaults to 1."
                        },
                        "limit": {
                            "type": "integer",
                            "minimum": 1,
                            "default": 400,
                            "description": "Maximum number of lines to read. Defaults to 400."
                        }
                    },
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
        let args: ReadArgs = match parse_args_lenient(arguments) {
            Ok(args) => args,
            Err(error) => {
                return ToolOutput {
                    content: format!("invalid arguments: {error}"),
                    is_error: true,
                };
            }
        };
        if args.offset < 1 || args.limit < 1 {
            return ToolOutput {
                content: "invalid arguments: offset and limit must be >= 1".to_string(),
                is_error: true,
            };
        }
        let path = match resolve_within(&ctx.cwd, &args.path, ctx.confined) {
            Ok(path) => path,
            Err(message) => return ToolOutput { content: message, is_error: true },
        };
        // 图片文件:不走文本截断,而是作为视觉输入注入会话。
        if let Ok(data) = std::fs::read(&path) {
            if let Some((mime, width, height)) = sniff_image(&data) {
                if !ctx.vision_supported {
                    return ToolOutput {
                        content: format!(
                            "图片需要识图模型:{} 是 {}(尺寸 {:?}x{:?});当前模型未标记为可识图,请切换到支持图片输入的模型后再读取。",
                            args.path,
                            mime,
                            width,
                            height
                        ),
                        is_error: true,
                    };
                }
                let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data);
                let image = denia_core::message::ImageData {
                    mime: mime.to_string(),
                    data: b64,
                };
                // 注入视觉输入(下一条模型请求即可见),与 dsh read-image 的语义一致。
                if let Some(sink) = &ctx.emit_event {
                    sink(denia_core::session::SessionEvent::UserMessage {
                        text: format!("[harness] 读取图片:{}({})", args.path, mime),
                        injected: true,
                        images: vec![image],
                    });
                }
                let dimension = match (width, height) {
                    (Some(w), Some(h)) => format!("{w}x{h}"),
                    _ => "unknown size".to_string(),
                };
                return ToolOutput {
                    content: format!(
                        "已读取图片 {} ({} · {} · {} bytes),图片内容已作为视觉输入注入会话,后续步骤可见。",
                        args.path,
                        mime,
                        dimension,
                        data.len()
                    ),
                    is_error: false,
                };
            }
        }
        // 文本文件:按行读取 offset/limit,再用字符硬顶兜底。
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) => {
                return ToolOutput {
                    content: format!("read failed: {error}"),
                    is_error: true,
                };
            }
        };
        let reader = BufReader::new(file);
        let mut lines: Vec<String> = Vec::new();
        let mut current: u64 = 0;
        for line in reader.lines() {
            current += 1;
            if current < args.offset {
                continue;
            }
            if lines.len() as u64 >= args.limit {
                break;
            }
            match line {
                Ok(text) => lines.push(text),
                Err(error) => {
                    return ToolOutput {
                        content: format!("read failed: {error}"),
                        is_error: true,
                    };
                }
            }
        }
        let count = lines.len();
        let end = if count == 0 {
            args.offset
        } else {
            args.offset + count as u64 - 1
        };
        let body = truncate(&lines.join("\n"), READ_CAP);
        ToolOutput {
            content: format!(
                "{} 第 {}-{} 行（{} 行）:\n{}",
                args.path,
                args.offset,
                end,
                count,
                body
            ),
            is_error: false,
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
                        "content": { "type": "string" },
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
            Err(message) => return ToolOutput { content: message, is_error: true },
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
        if let Some(parent) = path.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                return ToolOutput {
                    content: format!("create_dir_all failed: {error}"),
                    is_error: true,
                };
            }
        }
        if let Some(file_history) = &ctx.file_history {
            if let Err(message) = file_history.track_before_write(&path).await {
                return ToolOutput {
                    content: format!("file history backup failed: {message}"),
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
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn workspace() -> (tempfile_like::TempDir, ToolContext) {
        let dir = std::env::temp_dir().join(format!("denia-tools-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let context = ToolContext {
            cwd: dir.clone(),
            cancel: CancellationToken::new(),
            confined: true,
            vision_supported: true,
            emit_event: None,
            file_history: None,
            permission_mode: PermissionMode::WorkspaceWrite,
            permission_override: None,
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
        assert!(read.content.contains("hello harness"));
        assert!(read.content.contains("第 1-1 行（1 行）"));
    }

    #[tokio::test]
    async fn read_file_respects_offset_and_limit() {
        let (_guard, ctx) = workspace();
        let content = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let path = ctx.cwd.join("lines.txt");
        std::fs::write(&path, &content).unwrap();
        let reader = ReadFileTool::new();
        let out = reader
            .execute(r#"{"path":"lines.txt","offset":3,"limit":4}"#, &ctx)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("第 3-6 行（4 行）"));
        assert!(out.content.contains("line3"));
        assert!(out.content.contains("line6"));
        assert!(!out.content.contains("line2"));
        assert!(!out.content.contains("line7"));
    }

    #[tokio::test]
    async fn read_file_defaults_to_400_lines() {
        let (_guard, ctx) = workspace();
        let content = (1..=410)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(ctx.cwd.join("big.txt"), &content).unwrap();
        let reader = ReadFileTool::new();
        let out = reader.execute(r#"{"path":"big.txt"}"#, &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("第 1-400 行（400 行）"));
        assert!(out.content.contains("line1"));
        assert!(out.content.contains("line400"));
        assert!(!out.content.contains("line401"));
    }

    #[tokio::test]
    async fn read_only_denies_write_until_override() {
        let (_guard, mut ctx) = workspace();
        ctx.permission_mode = PermissionMode::ReadOnly;
        let writer = WriteFileTool::new();
        let denied = writer
            .execute(r#"{"path":"a.txt","content":"x"}"#, &ctx)
            .await;
        assert!(denied.is_error, "{}", denied.content);
        assert!(denied.content.contains("[sandbox: file access denied under read-only mode]"), "{}", denied.content);

        // 一次性升权为完整权限后同一次调用可写。
        ctx.permission_override = Some(PermissionMode::DangerFullAccess);
        let allowed = writer
            .execute(r#"{"path":"a.txt","content":"x"}"#, &ctx)
            .await;
        assert!(!allowed.is_error, "{}", allowed.content);
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

    #[tokio::test]
    async fn image_read_injects_visual_input_and_dimensions() {
        // 1x1 PNG(真实最小文件)。
        let png = [
            0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A,
            0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
            0x08, 0x04, 0x00, 0x00, 0x00, 0xB5, 0x1C, 0x0C,
            0x02, 0x00, 0x00, 0x00, 0x0B, 0x49, 0x44, 0x41,
            0x54, 0x78, 0x9C, 0x63, 0xE4, 0x0F, 0x00, 0x00,
            0x00, 0x00, 0x00, 0xFF, 0xFF, 0x03, 0x00, 0x06,
            0x00, 0x0A, 0x71, 0xFB, 0xA2, 0x65, 0x00, 0x00,
            0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42,
            0x60, 0x82,
        ];
        let dir = std::env::temp_dir().join(format!(
            "denia-img-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let image_path = dir.join("pixel.png");
        std::fs::write(&image_path, &png).unwrap();

        // 可识图:注入视觉输入事件 + 报告尺寸。
        let emitted: Arc<std::sync::Mutex<Vec<denia_core::session::SessionEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let emitted = emitted.clone();
            let ctx = ToolContext {
                cwd: dir.clone(),
                cancel: CancellationToken::new(),
                confined: true,
                vision_supported: true,
                emit_event: Some(Arc::new(move |event| emitted.lock().unwrap().push(event))),
                file_history: None,
                permission_mode: PermissionMode::WorkspaceWrite,
                permission_override: None,
            };
            let reader = ReadFileTool::new();
            let out = reader.execute(r#"{"path":"pixel.png"}"#, &ctx).await;
            assert!(!out.is_error, "{}", out.content);
            assert!(out.content.contains("image/png"), "{}", out.content);
            assert!(out.content.contains("1x1"), "{}", out.content);
        }
        let events = emitted.lock().unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            denia_core::session::SessionEvent::UserMessage { injected: true, images, .. } => {
                assert_eq!(images.len(), 1);
                assert_eq!(images[0].mime, "image/png");
                assert!(!images[0].data.is_empty());
            }
            other => panic!("expected injected user message, got {other:?}"),
        }

        // 不识图:明确报错,不注入。
        let ctx = ToolContext {
            cwd: dir.clone(),
            cancel: CancellationToken::new(),
            confined: true,
            vision_supported: false,
            emit_event: None,
            file_history: None,
            permission_mode: PermissionMode::WorkspaceWrite,
            permission_override: None,
        };
        let reader = ReadFileTool::new();
        let out = reader.execute(r#"{"path":"pixel.png"}"#, &ctx).await;
        assert!(out.is_error);
        assert!(out.content.contains("识图"), "{}", out.content);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
