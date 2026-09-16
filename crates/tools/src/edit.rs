//! The `edit` tool: precise string replacement in one text file.
//!
//! 防御纪律(AGENTS.md):
//! - `old_string` 必须**精确出现一次**才动手;0 次或多次都报错,
//!   绝不猜测、绝不默默继续;
//! - `replace_all=true` 显式允许多点替换;
//! - 多处编辑走 `edits` 数组:按顺序依次应用(后一项匹配前一项改完后的
//!   内容),**全有或全无**——任一处匹配失败就报错且不写入,绝不留下
//!   改了一半的文件;
//! - 单处形式(old_string/new_string)与 edits 形式二选一,混用报错;
//! - old_string 与 new_string 相同是无op编辑,直接拒绝;
//! - 文件必须 UTF-8(编辑是文本操作);
//! - 替换结果返回计数与行锚点,幂等:再次执行同样的 edit 会因
//!   `old_string` 不存在而报错,不会重复写入。
//!
//! 性能与容错约定(见 [`crate::support`]):阻塞 fs 走 `spawn_blocking`;
//! 参数解析走宽容入口;错误统一 `建议:` 格式,给模型可执行的下一步。

use async_trait::async_trait;
use denia_core::tool::ToolSchema;
use serde::Deserialize;

use crate::support::{parse_tool_args, tool_error};
use crate::{Tool, ToolContext, ToolOutput, resolve_within};

#[derive(Deserialize)]
struct EditItem {
    /// 要替换的原文,必须精确出现一次(除非 replace_all)。
    old_string: String,
    /// 替换成的新文本。
    new_string: String,
    /// true 时替换该项的全部出现(默认 false)。
    #[serde(default)]
    replace_all: bool,
}

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    /// 单处形式:要替换的原文。与 edits 二选一。
    old_string: Option<String>,
    /// 单处形式:替换成的新文本。
    new_string: Option<String>,
    /// 单处形式的替换全部出现开关。
    #[serde(default)]
    replace_all: bool,
    /// 多处形式:按顺序依次应用的编辑列表,与 old_string 二选一。
    #[serde(default)]
    edits: Vec<EditItem>,
}

/// 一条校验通过的编辑指令(两种形式归一后的统一形态)。
struct EditSpec {
    old: String,
    new: String,
    all: bool,
}

/// 把两种入参形式归一成编辑列表;形式混用与逐项校验都在这里 fail loud。
fn parse_specs(args: &EditArgs) -> Result<Vec<EditSpec>, (String, String)> {
    if !args.edits.is_empty() {
        if args.old_string.is_some() || args.new_string.is_some() {
            return Err((
                "edits 与 old_string/new_string 不能同时提供".to_string(),
                "单处编辑用 old_string/new_string,多处编辑用 edits 数组,二选一".to_string(),
            ));
        }
        if args.replace_all {
            return Err((
                "edits 形式下不接受顶层 replace_all".to_string(),
                "把 replace_all 放进需要全部替换的那个 edits 项里".to_string(),
            ));
        }
        return args
            .edits
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let ordinal = format!("第 {} 处编辑", index + 1);
                if item.old_string.is_empty() {
                    return Err((
                        format!("{ordinal}的 old_string 不能为空"),
                        "删除内容用空 new_string;整文件替换请用 write_file".to_string(),
                    ));
                }
                if item.old_string == item.new_string {
                    return Err((
                        format!("{ordinal}的 old_string 与 new_string 相同,不会产生任何变更"),
                        "确认要修改的内容;若只是想看文件,用 read_file".to_string(),
                    ));
                }
                Ok(EditSpec {
                    old: item.old_string.clone(),
                    new: item.new_string.clone(),
                    all: item.replace_all,
                })
            })
            .collect();
    }
    let Some(old) = args.old_string.as_deref() else {
        return Err((
            "缺少编辑内容".to_string(),
            "单处编辑给 old_string 与 new_string;同一文件的多处编辑给 edits 数组".to_string(),
        ));
    };
    let Some(new) = args.new_string.as_deref() else {
        return Err((
            "缺少 new_string".to_string(),
            "old_string 与 new_string 成对提供;删除内容时 new_string 给空串".to_string(),
        ));
    };
    if old.is_empty() {
        return Err((
            "old_string 不能为空".to_string(),
            "整文件替换请用 write_file;old_string 是要被替换的原文字符串".to_string(),
        ));
    }
    if old == new {
        return Err((
            "old_string 与 new_string 相同,不会产生任何变更".to_string(),
            "确认要修改的内容;若只是想看文件,用 read_file".to_string(),
        ));
    }
    Ok(vec![EditSpec {
        old: old.to_string(),
        new: new.to_string(),
        all: args.replace_all,
    }])
}

/// Replaces exact string occurrences in a text file; one call may carry
/// several ordered edits applied atomically.
pub struct EditTool {
    schema: ToolSchema,
}

impl EditTool {
    pub fn new() -> Self {
        Self {
            schema: ToolSchema {
                name: "edit".to_string(),
                description: "对工作区内的一个 UTF-8 文本文件做精确字符串替换,单次调用可完成多处编辑。单处编辑:old_string 必须与文件原文完全一致(空白与换行都算)且默认只能出现一次,多点出现会报错;replace_all=true 时替换全部出现。多处编辑:edits 数组按顺序依次应用,后一项匹配前一项改完后的内容;任一处匹配失败则整次调用不写入。返回替换发生的行号。先 read_file 确认原文,改完可再读校验。".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "要编辑的文件路径;相对路径锚定会话工作区。" },
                        "old_string": { "type": "string", "description": "单处编辑:要被替换的原文,必须与文件内容逐字符一致(包含缩进、空格与换行);与 edits 二选一。" },
                        "new_string": { "type": "string", "description": "单处编辑:替换成的新文本。" },
                        "replace_all": { "type": "boolean", "description": "单处编辑:替换全部出现而不是要求恰好一次(默认 false)。" },
                        "edits": {
                            "type": "array",
                            "description": "多处编辑:按顺序依次应用到同一文件,任一处失败则整次调用不写入;与 old_string 二选一。",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "old_string": { "type": "string", "description": "要被替换的原文,必须与当前内容逐字符一致(包含缩进、空格与换行),默认只能出现一次。" },
                                    "new_string": { "type": "string", "description": "替换成的新文本。" },
                                    "replace_all": { "type": "boolean", "description": "替换该项的全部出现(默认 false)。" }
                                },
                                "required": ["old_string", "new_string"]
                            }
                        }
                    },
                    "required": ["path"]
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
                    "参数必须是 JSON 对象:必填 path;编辑内容用 old_string/new_string(单处)或 edits 数组(多处)",
                );
            }
        };
        let specs = match parse_specs(&args) {
            Ok(specs) => specs,
            Err((message, hint)) => return tool_error(message, hint),
        };
        // confined 语义与读取类工具一致:沙箱开启时路径必须落在 cwd 内。
        // (写权限门控在派发处的策略引擎;沙箱锚定在这里兜底。)
        let path = match resolve_within(&ctx.cwd, &args.path, ctx.confined) {
            Ok(path) => path,
            Err(message) => {
                return tool_error(
                    message,
                    "相对路径锚定会话工作区;沙箱开启时不能编辑工作区之外的路径",
                );
            }
        };
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
        // edits 按顺序应用:后一项匹配的是前一项改完后的内容。任何一处失败
        // 都直接报错返回,磁盘文件保持原样(全有或全无)。
        let mut view = normalize_line_endings(&text);
        let mut anchors: Vec<usize> = Vec::with_capacity(specs.len());
        let mut replaced_total = 0usize;
        for (index, spec) in specs.iter().enumerate() {
            let old_view = normalize_line_endings(&spec.old);
            let new_view = normalize_line_endings(&spec.new);
            let count = view.matches(&old_view).count();
            let ordinal = index + 1;
            if count == 0 {
                // 尽力给出贴近的失败原因:原文里是否只差空白。
                let relaxed_match = view.split_whitespace().collect::<String>()
                    == old_view.split_whitespace().collect::<String>();
                let detail = if relaxed_match {
                    format!(
                        "第 {ordinal} 处编辑的 old_string 与原文仅空白/换行不同:逐字符核对缩进与换行,直接从 read_file 的输出复制"
                    )
                } else {
                    format!(
                        "第 {ordinal} 处编辑在 {} 中找不到 old_string({:?}):先 read_file 确认当前原文,再从输出中逐字符复制",
                        display(&path, &ctx.cwd),
                        preview(&spec.old)
                    )
                };
                return tool_error(
                    detail,
                    "edits 全有或全无,本次调用未写入任何内容;重新提交时文件仍从当前内容开始按顺序应用,修正该处 old_string 后重发完整 edits".to_string(),
                );
            }
            if count > 1 && !spec.all {
                return tool_error(
                    format!(
                        "第 {ordinal} 处编辑的 old_string({:?})在 {} 中出现了 {count} 次",
                        preview(&spec.old),
                        display(&path, &ctx.cwd)
                    ),
                    "在该处 old_string 里带上更多上下文使其唯一,或确认要全部替换时给该项设置 replace_all=true;edits 全有或全无,本次调用未写入任何内容".to_string(),
                );
            }
            anchors.push(line_of(&view, &old_view));
            view = if spec.all {
                view.replace(&old_view, &new_view)
            } else {
                view.replacen(&old_view, &new_view, 1)
            };
            replaced_total += count;
        }
        let updated = restore_line_endings(&view, crlf);
        let multi = specs.len() > 1;

        // 写前新鲜度校验。
        //
        // Edit 已经隐式要求"文件内容与模型预期一致"(old_string 匹配),
        // 但匹配成功不代表模型看到的是**最新版本**——文件可能在模型读取
        // 之后被用户或工具改动,而改动恰好没触及 old_string。此时写入会把
        // 过期的修改合并进新版本,产生难以察觉的内容回退。
        //
        // 校验不通过时**不阻断编辑**(匹配已成功,内容差异大概率无害),
        // 而是在结果里提示模型重新确认——比硬拒绝更实用。
        let staleness_note = if let Some(shared) = ctx.read_state.as_ref() {
            let stamp = {
                let path = path.clone();
                tokio::task::spawn_blocking(move || crate::read_state::FileStamp::of(&path)).await
            }
            .ok()
            .flatten();
            match stamp {
                Some(stamp) => shared
                    .lock()
                    .map(|state| state.check_writable(&path, &stamp))
                    .unwrap_or(crate::read_state::WriteCheck::Fresh),
                None => crate::read_state::WriteCheck::Fresh,
            }
        } else {
            crate::read_state::WriteCheck::Fresh
        };

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
        // 成功消息给出每处编辑的行锚点(按 edits 顺序,与前端逐 hunk 对齐);
        // 单处形式保持原有文案。行锚点列表很长时截断,总数已在前缀里。
        let content = if multi {
            let listed: Vec<String> = anchors
                .iter()
                .take(8)
                .enumerate()
                .map(|(index, line)| format!("#{} 第 {line} 行", index + 1))
                .collect();
            let mut listing = listed.join("、");
            if anchors.len() > 8 {
                listing.push_str(&format!(" 等 {} 处", anchors.len()));
            }
            format!(
                "已在 {} 应用 {} 处编辑(共替换 {replaced_total} 次):{listing}",
                display(&path, &ctx.cwd),
                specs.len(),
            )
        } else {
            let verb = if replaced_total > 1 {
                format!("{replaced_total} 处")
            } else {
                "1 处".to_string()
            };
            format!(
                "已在 {} 替换 {verb}({:?} → 新文本),首次替换发生在第 {} 行",
                display(&path, &ctx.cwd),
                preview(&specs[0].old),
                anchors[0],
            )
        };
        // 写入成功后刷新读状态:文件内容已是编辑后的版本,后续 Edit
        // 同一文件不会因写前校验而误报过期。
        if let Some(shared) = ctx.read_state.as_ref() {
            let stamp = {
                let path = path.clone();
                tokio::task::spawn_blocking(move || crate::read_state::FileStamp::of(&path)).await
            }
            .ok()
            .flatten();
            if let Some(stamp) = stamp
                && let Ok(mut state) = shared.lock()
            {
                state.record_after_write(&path, updated.clone(), stamp);
            }
        }
        let mut content = content;
        // 写前若检测到文件曾被外部改动,提示模型确认结果符合预期。
        if staleness_note == crate::read_state::WriteCheck::Stale {
            content.push_str(
                "\n\n(注意:该文件在你上次读取之后曾被改动,本次编辑已基于最新内容写入;\
                 如需确认结果请重新读取)",
            );
        }
        ToolOutput::text(content)
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
    use denia_core::session::PermissionMode;
    use tokio_util::sync::CancellationToken;

    fn temp_root() -> std::path::PathBuf {
        // pid + 进程内原子序号:同一次测试里连续调用也保证互不撞名
        // (时钟精度不足时按时间戳命名会给出同一个目录)。
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "denia-edit-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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
            permission_mode: PermissionMode::AutoEdit,
            ask: None,
            call_id: None,
            goal_reader: None,
            read_state: None,
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

    #[tokio::test]
    async fn identical_old_and_new_is_rejected() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "hello\n").unwrap();
        let ctx = context(root.clone());
        let tool = EditTool::new();
        let out = tool
            .execute(
                r#"{"path":"a.txt","old_string":"hello","new_string":"hello"}"#,
                &ctx,
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("不会产生任何变更"), "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "hello\n"
        );
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn edits_array_applies_all_in_order() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let ctx = context(root.clone());
        let tool = EditTool::new();
        let out = tool
            .execute(
                r#"{"path":"a.txt","edits":[
                    {"old_string":"alpha","new_string":"ALPHA"},
                    {"old_string":"beta","new_string":"BETA"},
                    {"old_string":"gamma","new_string":"GAMMA"}
                ]}"#,
                &ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("应用 3 处编辑"), "{}", out.content);
        assert!(out.content.contains("#1 第 1 行"), "{}", out.content);
        assert!(out.content.contains("#2 第 2 行"), "{}", out.content);
        assert!(out.content.contains("#3 第 3 行"), "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "ALPHA\nBETA\nGAMMA\n"
        );
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn edits_array_later_edit_matches_earlier_result() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "hello\n").unwrap();
        let ctx = context(root.clone());
        let tool = EditTool::new();
        let out = tool
            .execute(
                r#"{"path":"a.txt","edits":[
                    {"old_string":"hello","new_string":"goodbye"},
                    {"old_string":"goodbye","new_string":"farewell"}
                ]}"#,
                &ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "farewell\n"
        );
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn edits_array_is_all_or_nothing() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "hello\nworld\n").unwrap();
        let ctx = context(root.clone());
        let tool = EditTool::new();
        let out = tool
            .execute(
                r#"{"path":"a.txt","edits":[
                    {"old_string":"hello","new_string":"X"},
                    {"old_string":"missing","new_string":"Y"}
                ]}"#,
                &ctx,
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("第 2 处编辑"), "{}", out.content);
        assert!(out.content.contains("全有或全无"), "{}", out.content);
        // 文件未被改动:第 1 处虽然匹配成功,也不能落盘。
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "hello\nworld\n"
        );
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn edits_array_item_can_replace_all() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "x x x\ny\n").unwrap();
        let ctx = context(root.clone());
        let tool = EditTool::new();
        let out = tool
            .execute(
                r#"{"path":"a.txt","edits":[
                    {"old_string":"x","new_string":"z","replace_all":true},
                    {"old_string":"y","new_string":"w"}
                ]}"#,
                &ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("共替换 4 次"), "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "z z z\nw\n"
        );
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn edits_array_rejects_mixed_forms() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "hello\n").unwrap();
        let ctx = context(root.clone());
        let tool = EditTool::new();
        let out = tool
            .execute(
                r#"{"path":"a.txt","old_string":"hello","new_string":"X",
                    "edits":[{"old_string":"a","new_string":"b"}]}"#,
                &ctx,
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("二选一"), "{}", out.content);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn edits_array_rejects_empty_and_missing_content() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "hello\n").unwrap();
        let ctx = context(root.clone());
        let tool = EditTool::new();
        let out = tool
            .execute(r#"{"path":"a.txt","edits":[]}"#, &ctx)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("缺少编辑内容"), "{}", out.content);
        let out = tool.execute(r#"{"path":"a.txt"}"#, &ctx).await;
        assert!(out.is_error);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn edits_array_rejects_identical_pair() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "hello\n").unwrap();
        let ctx = context(root.clone());
        let tool = EditTool::new();
        let out = tool
            .execute(
                r#"{"path":"a.txt","edits":[{"old_string":"hello","new_string":"hello"}]}"#,
                &ctx,
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("不会产生任何变更"), "{}", out.content);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }
}
