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
use crate::{ExecutionReport, Tool, ToolContext, ToolOutput, end_reason, resolve_within};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
struct EditArgs {
    path: String,
    /// 单处形式:要替换的原文。与 edits 二选一。
    old_string: Option<String>,
    /// 单处形式:替换成的新文本。
    new_string: Option<String>,
    /// 单处形式的替换全部出现开关;用 Option 保留“字段是否出现”的信息，
    /// 这样 edits 形式带上 replace_all:false 也能明确报混用错误。
    #[serde(default)]
    replace_all: Option<bool>,
    /// 多处形式:按顺序依次应用的编辑列表,与 old_string 二选一。
    /// 用 Option 区分“未提供”与显式传入空数组，避免空数组被当成单处形式。
    #[serde(default)]
    edits: Option<Vec<EditItem>>,
}

/// 一条校验通过的编辑指令(两种形式归一后的统一形态)。
struct EditSpec {
    old: String,
    new: String,
    all: bool,
}

/// 把两种入参形式归一成编辑列表;形式混用与逐项校验都在这里 fail loud。
fn parse_specs(args: &EditArgs) -> Result<Vec<EditSpec>, (String, String)> {
    if let Some(edits) = args.edits.as_ref() {
        if args.old_string.is_some() || args.new_string.is_some() || args.replace_all.is_some() {
            return Err((
                "edits 与 old_string/new_string/replace_all 不能同时提供".to_string(),
                "单处编辑用 old_string + new_string + 可选 replace_all;多处编辑只用 edits 数组,二选一".to_string(),
            ));
        }
        if edits.is_empty() {
            return Err((
                "edits 不能为空".to_string(),
                "多处编辑至少提供一个 edits 项;如果只改一处,改用 old_string 与 new_string"
                    .to_string(),
            ));
        }
        return edits
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
            "单处编辑给 old_string 与 new_string;同一文件的多处编辑给 edits 数组;两种形式不要混用"
                .to_string(),
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
        all: args.replace_all.unwrap_or(false),
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
                description: "对工作区内的一个 UTF-8 文本文件做精确字符串替换,单次调用可完成多处编辑。两种参数形式必须二选一:只改一处时给 old_string + new_string,可选 replace_all;同一文件改多处时只给 edits 数组,至少一个编辑项,不要同时给两种形式。old_string 必须与文件原文完全一致(空白与换行都算),默认只能出现一次;replace_all=true 时替换全部出现。多处编辑按数组顺序应用,后一项匹配前一项改完后的内容;任一项失败则整次调用不写入。返回替换发生的行号。先 read_file 确认原文,改完可再读校验。写前会校验文件是否仍是你上次读取的那一版(内容指纹),被其他写入者改过时拒写,需重新 read_file 后重提。".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "oneOf": [
                        {
                            "type": "object",
                            "description": "单处编辑:只替换一个精确片段。",
                            "properties": {
                                "path": { "type": "string", "description": "要编辑的文件路径;相对路径锚定会话工作区。" },
                                "old_string": { "type": "string", "minLength": 1, "description": "要被替换的原文,必须与文件内容逐字符一致(包含缩进、空格与换行);不能是空串。" },
                                "new_string": { "type": "string", "description": "替换成的新文本;删除内容时给空串。" },
                                "replace_all": { "type": "boolean", "description": "可选;true 时替换 old_string 的全部出现,默认只允许恰好出现一次。" }
                            },
                            "required": ["path", "old_string", "new_string"],
                            "additionalProperties": false
                        },
                        {
                            "type": "object",
                            "description": "多处编辑:按顺序原子应用同一文件的多个替换。",
                            "properties": {
                                "path": { "type": "string", "description": "要编辑的文件路径;相对路径锚定会话工作区。" },
                                "edits": {
                                    "type": "array",
                                    "minItems": 1,
                                    "description": "至少一个编辑项;不要同时提供 old_string/new_string/replace_all。",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "old_string": { "type": "string", "minLength": 1, "description": "要被替换的原文,必须与当前内容逐字符一致(包含缩进、空格与换行),默认只能出现一次。" },
                                            "new_string": { "type": "string", "description": "替换成的新文本;删除内容时给空串。" },
                                            "replace_all": { "type": "boolean", "description": "可选;true 时替换该项的全部出现,默认只允许恰好出现一次。" }
                                        },
                                        "required": ["old_string", "new_string"],
                                        "additionalProperties": false
                                    }
                                }
                            },
                            "required": ["path", "edits"],
                            "additionalProperties": false
                        }
                    ]
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
                    "参数必须是 JSON 对象。单处编辑格式:{\"path\":\"...\",\"old_string\":\"...\",\"new_string\":\"...\"};多处编辑格式:{\"path\":\"...\",\"edits\":[{\"old_string\":\"...\",\"new_string\":\"...\"}]};两种格式不要混用",
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
        // 路径级临界区:同一路径的"读取 → 匹配替换 → 写前校验 → 备份 → 写入 →
        // 刷新读状态"必须原子。guard 跨 await 持有(读写在 spawn_blocking 里),
        // 不同路径各持一把锁,互不阻塞。
        //
        // 没有它,并发的两次 edit 会各自读到同一份旧内容、各自替换一处再先后
        // 写回:两次调用都报成功,文件里却只剩一处修改。
        //
        // 它**只**盖住经工具层发生的写入。arbitrary shell 命令、外部编辑器与
        // 其他进程的写入不受它约束,那些变化靠下面的写前内容指纹发现。
        //
        // **等锁必须可取消**:持锁方可能正卡在慢速的文件历史备份上,此时用户
        // 中断本次调用,等待方不能等到锁释放才返回(那样界面上"已中断"的卡片
        // 会挂在那里,直到另一侧几秒甚至更久之后收工)。取锁与取消赛跑,取消
        // 分支与 bash 走同一套语义(错误文本 + `cancelled` 结束原因)。
        //
        // 赌注只在"等锁"这一段:guard 一旦拿到,下面的读取 → 匹配替换 → 写前
        // 校验 → 备份 → 写入 → 刷新读状态仍全部在它的覆盖之下,临界区不因加了
        // select 而缩小。
        let _guard = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => {
                return tool_error(
                    "编辑被用户中断",
                    "中断发生在等待同一路径上的其他写入完成时;文件未被改动,可重试",
                )
                .with_report(ExecutionReport::ended(end_reason::CANCELLED));
            }
            guard = crate::coordination::PathLocks::shared().lock(&path) => guard,
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
        // 之后被用户、外部编辑器或其他进程改动,而改动恰好没触及
        // old_string。此时写入会把过期的修改合并进新版本,产生难以察觉的
        // 内容回退。
        //
        // 不一致时**拒写**(本次刻意变化:以前只在结果里追一句提示,写入
        // 照常发生)。同一路径上经工具层的写入已被上面的路径锁串行化,这一
        // 层挡的是锁管不到的那些写入者:要求重读后重提,是唯一能保证"不把
        // 过期内容合并进新版本"的做法。
        let check = if let Some(shared) = ctx.read_state.as_ref() {
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
        match check {
            crate::read_state::WriteCheck::Stale => {
                return tool_error(
                    format!(
                        "{} 在你上次读取之后被改动过(可能由用户、编辑器或其他进程写入),本次编辑未写入",
                        display(&path, &ctx.cwd)
                    ),
                    "重新 read_file 获取最新内容,再按最新内容重提编辑",
                );
            }
            crate::read_state::WriteCheck::ContentChanged => {
                return tool_error(
                    format!(
                        "{} 的内容在你上次读取之后被改写(大小与修改时间未变,内容指纹不符),本次编辑未写入",
                        display(&path, &ctx.cwd)
                    ),
                    "重新 read_file 获取最新内容,再按最新内容重提编辑",
                );
            }
            // NeverRead / PartialView:edit 的 old_string 精确匹配本身就是
            // 内容依据(不像 write_file 是整文件覆盖),维持既有行为。
            _ => {}
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
            output_store: None,
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
            task_ledger: None,
            read_state: None,
        }
    }

    #[test]
    fn schema_separates_single_and_multiple_forms() {
        let tool = EditTool::new();
        let parameters = &tool.schema().parameters;
        assert_eq!(parameters["type"], "object");
        let branches = parameters["oneOf"].as_array().expect("oneOf schema");
        assert_eq!(branches.len(), 2);
        assert_eq!(
            branches[0]["required"],
            serde_json::json!(["path", "old_string", "new_string"])
        );
        assert_eq!(
            branches[1]["required"],
            serde_json::json!(["path", "edits"])
        );
        assert_eq!(branches[1]["properties"]["edits"]["minItems"], 1);
        for branch in branches {
            assert_eq!(branch["additionalProperties"], false);
        }
    }

    #[tokio::test]
    async fn unknown_top_level_fields_are_rejected() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "hello\n").unwrap();
        let ctx = context(root.clone());
        let tool = EditTool::new();
        let out = tool
            .execute(
                r#"{"path":"a.txt","old_string":"hello","new_string":"X","unexpected":true}"#,
                &ctx,
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("参数解析失败"), "{}", out.content);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
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
                    "replace_all":false,
                    "edits":[{"old_string":"a","new_string":"b"}]}"#,
                &ctx,
            )
            .await;
        assert!(out.is_error);
        assert!(
            out.content
                .contains("edits 与 old_string/new_string/replace_all 不能同时提供"),
            "{}",
            out.content
        );
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn edits_array_rejects_empty_and_missing_content() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "hello\n").unwrap();
        let ctx = context(root.clone());
        let tool = EditTool::new();
        let out = tool.execute(r#"{"path":"a.txt","edits":[]}"#, &ctx).await;
        assert!(out.is_error);
        assert!(out.content.contains("edits 不能为空"), "{}", out.content);
        let out = tool.execute(r#"{"path":"a.txt"}"#, &ctx).await;
        assert!(out.is_error);
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    #[tokio::test]
    async fn top_level_replace_all_is_rejected_with_edits() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "hello\n").unwrap();
        let ctx = context(root.clone());
        let tool = EditTool::new();
        let out = tool
            .execute(
                r#"{"path":"a.txt","replace_all":false,"edits":[{"old_string":"hello","new_string":"X"}]}"#,
                &ctx,
            )
            .await;
        assert!(out.is_error);
        assert!(
            out.content
                .contains("edits 与 old_string/new_string/replace_all"),
            "{}",
            out.content
        );
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

    /// 写前停顿的备份后端:把"读完之后、写回之前"的窗口撑开,让并发调用
    /// 真正重叠在临界区里(不打这个补丁时,两次调用可能恰好一前一后跑完,
    /// 让种丢失更新的 bug 漏过测试)。
    struct PausingHistory;

    #[async_trait]
    impl crate::FileHistoryBackend for PausingHistory {
        async fn track_before_write(&self, _path: &std::path::Path) -> Result<(), String> {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            Ok(())
        }
    }

    /// 模拟"模型已经读过这个文件":写入一条带内容指纹的读记录。
    fn record_read(shared: &crate::read_state::SharedReadState, path: &std::path::Path) {
        let stamp = crate::read_state::FileStamp::of(path).unwrap();
        let body = std::fs::read_to_string(path).unwrap();
        shared.lock().unwrap().record(
            crate::read_state::ReadKey::new(path, None, None),
            crate::read_state::ReadEntry {
                mtime_ms: stamp.mtime_ms,
                size_bytes: stamp.size_bytes,
                fingerprint: stamp.fingerprint,
                is_partial_view: false,
                is_full_read: true,
                read_at: std::time::SystemTime::now(),
                content: Some(body),
                call_id: None,
            },
        );
    }

    fn with_read_state(
        ctx: &ToolContext,
        shared: &crate::read_state::SharedReadState,
    ) -> ToolContext {
        ToolContext {
            read_state: Some(shared.clone()),
            ..ctx.clone()
        }
    }

    /// 并发编辑**同一**文件:要么两份修改都保留,要么明确冲突;绝不能出现
    /// "两次调用都成功、文件里只剩一处修改"。
    ///
    /// 关键窗口是"读到旧内容"与"写回"之间。这里用 file_history 后端在写前
    /// 停顿,确认写入确实落在同一路径锁的临界区里:持锁的那次写完并释放后,
    /// 后一次才会读到更新后的内容。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_edits_of_one_file_never_lose_a_change() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "base\nALPHA\nOMEGA\nend\n").unwrap();
        let history: std::sync::Arc<dyn crate::FileHistoryBackend> =
            std::sync::Arc::new(PausingHistory);
        let ctx = ToolContext {
            file_history: Some(history),
            ..context(root.clone())
        };
        let tool = std::sync::Arc::new(EditTool::new());

        let first = {
            let tool = tool.clone();
            let ctx = ctx.clone();
            tokio::spawn(async move {
                tool.execute(
                    r#"{"path":"a.txt","old_string":"ALPHA","new_string":"alpha"}"#,
                    &ctx,
                )
                .await
            })
        };
        let second = {
            let tool = tool.clone();
            let ctx = ctx.clone();
            tokio::spawn(async move {
                tool.execute(
                    r#"{"path":"a.txt","old_string":"OMEGA","new_string":"omega"}"#,
                    &ctx,
                )
                .await
            })
        };
        let (first, second) = tokio::join!(first, second);
        let first = first.unwrap();
        let second = second.unwrap();

        let after = std::fs::read_to_string(root.join("a.txt")).unwrap();
        // 两次调用都报成功 → 两处修改都必须还在文件里(顺序无关,结果唯一)。
        assert!(!first.is_error, "第一次编辑失败:{}", first.content);
        assert!(!second.is_error, "第二次编辑失败:{}", second.content);
        assert!(
            after.contains("alpha"),
            "第一次编辑的结果被覆盖掉了:{after}"
        );
        assert!(
            after.contains("omega"),
            "第二次编辑的结果被覆盖掉了:{after}"
        );
        assert_eq!(after, "base\nalpha\nomega\nend\n");
        std::fs::remove_dir_all(&ctx.cwd).unwrap();
    }

    /// 读到的内容还是最新的 → 照旧可以编辑(拒写不能误伤正常路径)。
    #[tokio::test]
    async fn edit_succeeds_when_the_read_is_still_fresh() {
        let root = temp_root();
        let path = root.join("a.txt");
        std::fs::write(&path, "hello\nworld\n").unwrap();
        let shared = crate::read_state::shared();
        record_read(&shared, &path);
        let ctx = with_read_state(&context(root.clone()), &shared);

        let tool = EditTool::new();
        let out = tool
            .execute(
                r#"{"path":"a.txt","old_string":"world","new_string":"earth"}"#,
                &ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\nearth\n");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// 连续编辑同一文件:写成功后读状态会刷新,第二次编辑不能因为自己的上一次
    /// 写入而被判成"文件被外部改动"(否则每次编辑都得先重读)。
    #[tokio::test]
    async fn consecutive_edits_of_one_file_are_not_self_blocked() {
        let root = temp_root();
        let path = root.join("a.txt");
        std::fs::write(&path, "hello\nworld\n").unwrap();
        let shared = crate::read_state::shared();
        record_read(&shared, &path);
        let ctx = with_read_state(&context(root.clone()), &shared);
        let tool = EditTool::new();

        let first = tool
            .execute(
                r#"{"path":"a.txt","old_string":"hello","new_string":"hej"}"#,
                &ctx,
            )
            .await;
        assert!(!first.is_error, "{}", first.content);
        let second = tool
            .execute(
                r#"{"path":"a.txt","old_string":"world","new_string":"verden"}"#,
                &ctx,
            )
            .await;
        assert!(
            !second.is_error,
            "第二次编辑被自己的上一次写入误判:{}",
            second.content
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hej\nverden\n");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// 读取之后文件被改动过 → **拒写**(本次刻意行为变化:以前只追一句提示,
    /// 写入照常发生,过期内容会被静默合并进新版本)。
    #[tokio::test]
    async fn edit_refuses_when_the_file_changed_after_the_read() {
        let root = temp_root();
        let path = root.join("a.txt");
        std::fs::write(&path, "hello\nworld\n").unwrap();
        let shared = crate::read_state::shared();
        record_read(&shared, &path);
        // 读取之后文件被外部改动(多出一行 ⇒ size 也变了)。
        std::fs::write(&path, "hello\nworld\nagain\n").unwrap();
        let ctx = with_read_state(&context(root.clone()), &shared);

        let tool = EditTool::new();
        let out = tool
            .execute(
                r#"{"path":"a.txt","old_string":"world","new_string":"earth"}"#,
                &ctx,
            )
            .await;
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("重新 read_file"), "{}", out.content);
        // 拒写就是没写:磁盘内容一个字都没动。
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "hello\nworld\nagain\n"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// 同尺寸改写 + mtime 拨回原值:`(mtime, size)` 完全一样,只有内容指纹
    /// 能发现这次改动(而 old_string 仍然匹配,所以必须由指纹挡下)。
    #[tokio::test]
    async fn edit_refuses_a_same_size_rewrite_hidden_by_the_same_mtime() {
        let root = temp_root();
        let path = root.join("a.txt");
        std::fs::write(&path, "hello\nworld\n").unwrap();
        let shared = crate::read_state::shared();
        record_read(&shared, &path);
        let recorded = shared
            .lock()
            .unwrap()
            .get(&crate::read_state::ReadKey::new(&path, None, None))
            .cloned()
            .expect("读记录必须存在");

        // 同尺寸改写 "hello" → "hella"(old_string "world" 仍然在)。
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, "hella\nworld\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();

        // 前提先立住:stat 口径没变,内容确实变了。
        let stamp = crate::read_state::FileStamp::of(&path).unwrap();
        assert_eq!(stamp.mtime_ms, recorded.mtime_ms, "前提:mtime 必须拨回原值");
        assert_eq!(stamp.size_bytes, recorded.size_bytes, "前提:必须是同尺寸");
        assert_ne!(stamp.fingerprint, recorded.fingerprint, "前提:内容确实变了");

        let ctx = with_read_state(&context(root.clone()), &shared);
        let tool = EditTool::new();
        let out = tool
            .execute(
                r#"{"path":"a.txt","old_string":"world","new_string":"earth"}"#,
                &ctx,
            )
            .await;
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("内容指纹不符"), "{}", out.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hella\nworld\n");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// 等锁期间被取消:必须立刻返回,而不是等到持锁方释放。
    ///
    /// 场景:A 持着该路径的锁不放(对应另一会话正卡在慢速的文件历史备份上),
    /// B 提交一次对同一文件的编辑、并在等锁时被用户中断。测试把"A 全程不放锁"
    /// 和"等待加超时"配在一起,让判据不依赖时序:锁确实被占着,所以 B 能及时
    /// 返回只可能来自取消分支——裸 `lock().await` 在这里会等到超时。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancel_while_waiting_for_the_path_lock_returns_immediately() {
        let root = temp_root();
        let path = root.join("a.txt");
        std::fs::write(&path, "hello\n").unwrap();
        let cancel = CancellationToken::new();
        let ctx = ToolContext {
            cancel: cancel.clone(),
            ..context(root.clone())
        };

        // A:拿住这把锁,直到测试结束才释放。
        let held = crate::coordination::PathLocks::shared().lock(&path).await;

        let tool = EditTool::new();
        let waiter = tokio::spawn(async move {
            tool.execute(
                r#"{"path":"a.txt","old_string":"hello","new_string":"goodbye"}"#,
                &ctx,
            )
            .await
        });
        // 让 B 抵达取锁点。这个等待只决定"取消落在哪一步":取消令牌一旦触发,
        // 即使 B 还没开始,biased 的 select 也会走取消分支。
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        cancel.cancel();

        let out = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .expect("等锁被取消后必须立刻返回,不该等到锁释放")
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("中断"), "{}", out.content);
        assert_eq!(
            out.report
                .as_ref()
                .and_then(|report| report.end_reason.as_deref()),
            Some(end_reason::CANCELLED)
        );
        // 被取消的那次连锁都没拿到,文件自然没动。
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");

        drop(held);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
