//! The `read_file` and `write_file` tools.
//!
//! `read_file` 读取图片文件时(魔数识别 png/jpeg/gif/webp/bmp)不返回乱码,
//! 而是把图片作为视觉输入注入会话(需要当前模型标记为可识图),
//! 与 DSH 的"读图工具"一致 —— 只是这里复用 read_file 一个入口。
//!
//! 性能与容错约定(见 [`crate::support`]):
//! - 阻塞 fs 调用全部走 `spawn_blocking`,不挡异步运行时;
//! - 参数解析走 [`parse_tool_args`](宽容:别名提升 + 字符串数字强转);
//! - 字符级截断不在这里做——统一由落盘层的输出预算收敛。

use std::io::{BufRead, BufReader};

use async_trait::async_trait;
use denia_core::tool::ToolSchema;
use serde::Deserialize;

use crate::support::{parse_tool_args, tool_error};
use crate::{Tool, ToolContext, ToolOutput, resolve_within};

/// read_file 默认最多读取的行数。
const DEFAULT_READ_LIMIT: u64 = 400;
/// 单行最大保留字符数:超长行(如压缩过的 JS)截断并标注,防止一行撑爆预算。
const MAX_LINE_CHARS: usize = 2_000;

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
                            u16::from_be_bytes(data[offset + 2..offset + 4].try_into().ok()?)
                                as usize;
                        if marker == 0xC0 || marker == 0xC2 {
                            size = (
                                u16::from_be_bytes(data[offset + 7..offset + 9].try_into().ok()?)
                                    as u32,
                                u16::from_be_bytes(data[offset + 5..offset + 7].try_into().ok()?)
                                    as u32,
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
                description: "读取会话工作区中的一个 UTF-8 文本文件。相对路径锚定会话工作区;大文件用 offset/limit 分页读取。读取图片文件(png/jpeg/gif/webp/bmp)时图片会作为视觉输入注入会话(需要可识图模型)。".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "文件路径;相对路径锚定会话工作区。" },
                        "offset": {
                            "type": "integer",
                            "minimum": 1,
                            "default": 1,
                            "description": "1-based 起始行号,从 1 开始计数。默认 1。"
                        },
                        "limit": {
                            "type": "integer",
                            "minimum": 1,
                            "default": 400,
                            "description": "本次最多读取的行数。默认 400;需要看更多内容时用更大的 offset 继续读取。"
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

/// 单行防护:超长行截断并标注(按字符截断,不撕 UTF-8)。
fn guard_line(line: &str) -> String {
    let mut chars = line.chars();
    let head: String = chars.by_ref().take(MAX_LINE_CHARS).collect();
    if chars.next().is_some() {
        format!("{head}…[本行超长,已截断]")
    } else {
        head
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn execute(&self, arguments: &str, ctx: &ToolContext) -> ToolOutput {
        let args: ReadArgs = match parse_tool_args(arguments) {
            Ok(args) => args,
            Err(error) => {
                return tool_error(
                    format!("参数解析失败:{error}"),
                    "参数必须是 JSON 对象,必填字段为 path(字符串)",
                );
            }
        };
        if args.offset < 1 || args.limit < 1 {
            return tool_error(
                "offset 和 limit 必须 ≥ 1",
                "offset 是 1-based 起始行号(从 1 开始);limit 是读取行数",
            );
        }
        let path = match resolve_within(&ctx.cwd, &args.path, ctx.confined) {
            Ok(path) => path,
            Err(message) => {
                return tool_error(
                    message,
                    format!(
                        "相对路径锚定会话工作区 {};确认文件是否存在",
                        ctx.cwd.display()
                    ),
                );
            }
        };
        // 图片嗅探 + 文本读取都是阻塞 IO,整体丢进 blocking 池。
        let path_for_sniff = path.clone();
        let sniffed = tokio::task::spawn_blocking(move || std::fs::read(&path_for_sniff).ok())
            .await
            .ok()
            .flatten();
        // 图片文件:不走文本截断,而是作为视觉输入注入会话。
        if let Some(data) = &sniffed
            && let Some((mime, width, height)) = sniff_image(data)
        {
            if !ctx.vision_supported {
                return tool_error(
                    format!(
                        "图片需要识图模型:{} 是 {}(尺寸 {:?}x{:?}),当前模型未标记为可识图",
                        args.path, mime, width, height
                    ),
                    "切换到支持图片输入的模型后再读取该文件",
                );
            }
            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, data);
            let image = denia_core::message::ImageData {
                mime: mime.to_string(),
                data: b64,
            };
            // 注入视觉输入(下一条模型请求即可见),与 dsh read-image 的语义一致。
            if let Some(sink) = &ctx.emit_event {
                sink(denia_core::session::SessionEvent::UserMessage {
                    text: format!("[harness] 读取图片:{}({})", args.path, mime),
                    injected: true,
                    channel: Some("image".into()),
                    images: vec![image],
                });
            }
            let dimension = match (width, height) {
                (Some(w), Some(h)) => format!("{w}x{h}"),
                _ => "尺寸未知".to_string(),
            };
            return ToolOutput::text(format!(
                "已读取图片 {} ({} · {} · {} 字节),图片内容已作为视觉输入注入会话,后续步骤可见。",
                args.path,
                mime,
                dimension,
                data.len()
            ));
        }
        // 文本文件:按行读取 offset/limit;字符级预算由落盘层统一兜底。
        let read = {
            let path = path.clone();
            let offset = args.offset;
            let limit = args.limit;
            tokio::task::spawn_blocking(move || read_lines(&path, offset, limit)).await
        };
        match read {
            Ok(Ok((lines, truncated_lines))) => {
                let count = lines.len();
                let end = if count == 0 {
                    args.offset
                } else {
                    args.offset + count as u64 - 1
                };
                let body = lines.join("\n");
                let mut content = format!(
                    "{} 第 {}-{} 行（{} 行）:\n{}",
                    args.path, args.offset, end, count, body
                );
                if truncated_lines > 0 {
                    content.push_str(&format!(
                        "\n\n({truncated_lines} 行因单行超长被截断;超长单行可用 bash 处理)"
                    ));
                }
                if count == args.limit as usize {
                    content.push_str(&format!(
                        "\n\n(已达 limit={};文件可能还有更多行,用 offset={} 继续读取)",
                        args.limit,
                        end + 1
                    ));
                }
                ToolOutput::text(content)
            }
            Ok(Err(message)) => tool_error(
                format!("读取 {} 失败:{message}", args.path),
                "确认文件存在且是 UTF-8 文本;目录请用 glob 列出",
            ),
            Err(join_error) => tool_error(format!("读取任务失败:{join_error}"), "请重试一次"),
        }
    }
}

/// 阻塞读行:返回 (行文本列表, 因单行超长被截断的行数)。
fn read_lines(
    path: &std::path::Path,
    offset: u64,
    limit: u64,
) -> Result<(Vec<String>, usize), String> {
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let reader = BufReader::new(file);
    let mut lines: Vec<String> = Vec::new();
    let mut truncated_lines = 0usize;
    let mut current: u64 = 0;
    for line in reader.lines() {
        current += 1;
        if current < offset {
            continue;
        }
        if lines.len() as u64 >= limit {
            break;
        }
        let text = line.map_err(|error| error.to_string())?;
        let guarded = guard_line(&text);
        if guarded.len() != text.len() {
            truncated_lines += 1;
        }
        lines.push(guarded);
    }
    Ok((lines, truncated_lines))
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
                description: "在会话工作区写入一个 UTF-8 文本文件:自动创建父目录,已存在的文件会被整体覆盖。新建文件或全量重写用本工具;对现有文件做局部修改请优先用 edit。".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "目标文件路径;相对路径锚定会话工作区。" },
                        "content": { "type": "string", "description": "完整文件内容(整体覆盖写入)。" }
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
        let args: WriteArgs = match parse_tool_args(arguments) {
            Ok(args) => args,
            Err(error) => {
                return tool_error(
                    format!("参数解析失败:{error}"),
                    "参数必须是 JSON 对象,必填字段为 path 和 content(都是字符串)",
                );
            }
        };
        // confined 语义与读取类工具一致:沙箱开启时路径必须落在 cwd 内。
        // (写权限门控在派发处的策略引擎;沙箱锚定在这里兜底。)
        let path = match resolve_within(&ctx.cwd, &args.path, ctx.confined) {
            Ok(path) => path,
            Err(message) => {
                return tool_error(
                    message,
                    "相对路径锚定会话工作区;沙箱开启时不能写工作区之外的路径",
                );
            }
        };
        if let Some(parent) = path.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                return tool_error(
                    format!("创建父目录失败:{error}"),
                    "确认路径合法且当前权限允许写该目录",
                );
            }
        }
        if let Some(file_history) = &ctx.file_history
            && let Err(message) = file_history.track_before_write(&path).await
        {
            return tool_error(
                format!("文件历史备份失败:{message}"),
                "备份失败时不会写入;可重试一次,持续失败请报告",
            );
        }
        // 阻塞写盘丢进 blocking 池。
        let write = {
            let path = path.clone();
            let content = args.content.clone();
            tokio::task::spawn_blocking(move || std::fs::write(&path, content.as_bytes())).await
        };
        match write {
            Ok(Ok(())) => ToolOutput::text(format!(
                "已写入 {} 字节到 {}",
                args.content.len(),
                args.path
            )),
            Ok(Err(error)) => tool_error(
                format!("写入 {} 失败:{error}", args.path),
                "确认目标路径可写;文件被占用时稍后重试",
            ),
            Err(join_error) => tool_error(format!("写入任务失败:{join_error}"), "请重试一次"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::session::PermissionMode;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn workspace() -> (tempfile_like::TempDir, ToolContext) {
        let dir = std::env::temp_dir().join(format!("denia-tools-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let context = ToolContext {
            session_id: None,
            selection: None,
            cwd: dir.clone(),
            cancel: CancellationToken::new(),
            confined: true,
            vision_supported: true,
            emit_event: None,
            file_history: None,
            permission_mode: PermissionMode::AutoEdit,
            ask: None,
            call_id: None,
            goal_reader: None,
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
        let read = reader.execute(r#"{"path":"notes/hello.txt"}"#, &ctx).await;
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
    async fn read_file_accepts_string_offset_and_path_alias() {
        // 常见错误调用兼容:"offset" 传字符串数字、路径放在 file 字段。
        let (_guard, ctx) = workspace();
        let content = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(ctx.cwd.join("lines.txt"), &content).unwrap();
        let reader = ReadFileTool::new();
        let out = reader
            .execute(r#"{"file":"lines.txt","offset":"3","limit":"4"}"#, &ctx)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("第 3-6 行（4 行）"));
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
        // 分页提示:告诉模型如何继续读。
        assert!(out.content.contains("offset=401"), "{}", out.content);
    }

    #[tokio::test]
    async fn overly_long_line_is_guarded() {
        let (_guard, ctx) = workspace();
        let long = "x".repeat(MAX_LINE_CHARS + 500);
        std::fs::write(ctx.cwd.join("long.txt"), &long).unwrap();
        let reader = ReadFileTool::new();
        let out = reader.execute(r#"{"path":"long.txt"}"#, &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("本行超长,已截断"), "{}", out.content);
        assert!(out.content.contains("1 行因单行超长被截断"), "{}", out.content);
    }

    #[tokio::test]
    async fn confined_write_outside_workspace_is_rejected() {
        // 写权限门控已上移到策略引擎(exec 派发处);工具内保留沙箱锚定
        // 兜底:confined 时绝对路径出工作区必须拒绝。
        let (_guard, ctx) = workspace();
        let outside = std::env::temp_dir().join("denia-outside-probe.txt");
        let writer = WriteFileTool::new();
        let raw = format!(
            r#"{{"path":"{}","content":"x"}}"#,
            outside.to_string_lossy().replace('\\', "/")
        );
        let denied = writer.execute(&raw, &ctx).await;
        assert!(denied.is_error, "{}", denied.content);
        assert!(denied.content.contains("工作区"), "{}", denied.content);
    }

    #[tokio::test]
    async fn traversal_is_rejected() {
        let (_guard, ctx) = workspace();
        let reader = ReadFileTool::new();
        let out = reader.execute(r#"{"path":"../outside.txt"}"#, &ctx).await;
        assert!(out.is_error);
        assert!(out.content.contains("越出了会话工作区"), "{}", out.content);
    }

    #[tokio::test]
    async fn missing_file_is_error_with_hint() {
        let (_guard, ctx) = workspace();
        let reader = ReadFileTool::new();
        let out = reader.execute(r#"{"path":"nope.txt"}"#, &ctx).await;
        assert!(out.is_error);
        assert!(out.content.contains("[工具错误]"), "{}", out.content);
        assert!(out.content.contains("建议:"), "{}", out.content);
    }

    #[tokio::test]
    async fn image_read_injects_visual_input_and_dimensions() {
        // 1x1 PNG(真实最小文件)。
        let png = [
            0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00,
            0x00, 0xB5, 0x1C, 0x0C, 0x02, 0x00, 0x00, 0x00, 0x0B, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0xE4, 0x0F, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0x03, 0x00, 0x06,
            0x00, 0x0A, 0x71, 0xFB, 0xA2, 0x65, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44,
            0xAE, 0x42, 0x60, 0x82,
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
                session_id: None,
                selection: None,
                cwd: dir.clone(),
                cancel: CancellationToken::new(),
                confined: true,
                vision_supported: true,
                emit_event: Some(Arc::new(move |event| emitted.lock().unwrap().push(event))),
                file_history: None,
                permission_mode: PermissionMode::AutoEdit,
                ask: None,
                call_id: None,
                goal_reader: None,
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
            denia_core::session::SessionEvent::UserMessage {
                injected: true,
                channel,
                images,
                ..
            } => {
                assert_eq!(images.len(), 1);
                assert_eq!(images[0].mime, "image/png");
                assert!(!images[0].data.is_empty());
                assert_eq!(channel.as_deref(), Some("image"));
            }
            other => panic!("expected injected user message, got {other:?}"),
        }

        // 不识图:明确报错,不注入。
        let ctx = ToolContext {
            session_id: None,
            selection: None,
            cwd: dir.clone(),
            cancel: CancellationToken::new(),
            confined: true,
            vision_supported: false,
            emit_event: None,
            file_history: None,
            permission_mode: PermissionMode::AutoEdit,
            ask: None,
            call_id: None,
            goal_reader: None,
        };
        let reader = ReadFileTool::new();
        let out = reader.execute(r#"{"path":"pixel.png"}"#, &ctx).await;
        assert!(out.is_error);
        assert!(out.content.contains("识图"), "{}", out.content);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
