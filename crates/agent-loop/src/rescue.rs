//! 文本伪工具调用救援:部分模型/网关缺少原生函数调用支持,把工具调用
//! 渲染成正文标签(`<tool_call><function=name>…`),wire 响应没有
//! tool_calls 字段——实测 qwen3.8-flash(cat 网关)的故障形态。
//!
//! 三层策略(见 [`rescue_fake_tool_calls`] 与 turn 主循环):
//! 1. 标签可完整解析 → 直接代为执行(权限/沙箱/审批与原生调用一致);
//! 2. 解析失败但检测到伪调用痕迹 → 注入自纠反馈,让模型改用原生字段;
//! 3. 自纠配额耗尽 → 按普通文本收尾,不死循环。

use denia_core::stream::ContentBlock;

/// 从正文提取"文本伪工具调用"中声明的函数名列表。
///
/// 仅当文本同时出现 `<tool_call` 标签与 `<function=` 赋值时才判定为
/// 伪调用,降低对普通正文(如讲解标签格式)的误报。
pub(crate) fn fake_tool_call_names(text: &str) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    if !lower.contains("<tool_call") {
        return Vec::new();
    }
    let mut names = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find("<function=") {
        let start = from + rel + "<function=".len();
        let mut end = start;
        while end < lower.len()
            && (lower.as_bytes()[end].is_ascii_alphanumeric()
                || matches!(lower.as_bytes()[end], b'_' | b'-' | b'.'))
        {
            end += 1;
        }
        let name = &lower[start..end];
        if !name.is_empty() && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
        match end.checked_add(1) {
            Some(next) if next <= lower.len() => from = next,
            _ => break,
        }
    }
    names
}

/// 文本伪工具调用的自纠提示:与提供方拒绝共用一个反馈通道,让模型
/// 看见"调用没被执行"的原因后改用原生 tool_calls 字段。
pub(crate) fn fake_tool_call_feedback(names: &[String]) -> String {
    let listed = names
        .iter()
        .map(|name| format!("{name}()"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "[harness] 你上一条输出把工具调用写成了正文标签(<tool_call>/<function=…>),这些调用没有被执行:{listed}。\n请改用响应的原生 tool_calls 字段发起工具调用,不要输出 <tool_call> 之类的文本标签;若确实不需要调用工具,直接给出文字回答。"
    )
}

/// 从正文标签里救援出来的一个工具调用。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RescuedCall {
    pub name: String,
    pub arguments: serde_json::Value,
}

/// 从全部 Text 块中提取伪调用函数名(供自纠提示点名)。
pub(crate) fn detect_fake_names(blocks: &[ContentBlock]) -> Vec<String> {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .flat_map(fake_tool_call_names)
        .collect()
}

/// 救援成功的结构化日志(观察网关伪调用频率)。
pub(crate) fn log_rescued(calls: &[RescuedCall]) {
    tracing::info!(
        names = ?calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        "rescued text-rendered tool calls"
    );
}

/// 尝试把"文本伪工具调用"(`<tool_call><function=…><parameter=…>`)解析成
/// 可执行的调用列表。任一块格式解析失败返回 None(整体放弃,退回自纠
/// 注入,避免半吊子执行);整段没有伪调用也返回 None。参数值优先按
/// JSON 解析,失败按字符串处理——bash 的 command 这类自由文本参数
/// 正好落入后者。`.to_ascii_lowercase()` 不改变字节长度,lower 上算出的
/// 索引可直接切原文。
pub(crate) fn rescue_fake_tool_calls(text: &str) -> Option<Vec<RescuedCall>> {
    let lower = text.to_ascii_lowercase();
    let mut calls = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find("<tool_call") {
        let block_start = from + rel;
        let tail_start = block_start + "<tool_call".len();
        let Some(rrel) = lower[tail_start..].find("</tool_call>") else {
            // 尾部不完整块(截断/格式烂):整体放弃,交由自纠注入。
            return None;
        };
        let block_end = tail_start + rrel;
        let body = &lower[block_start..block_end];
        // <function=name>
        let Some(frel) = body.find("<function=") else {
            return None;
        };
        let name_start = frel + "<function=".len();
        let mut name_end = name_start;
        while name_end < body.len()
            && (body.as_bytes()[name_end].is_ascii_alphanumeric()
                || matches!(body.as_bytes()[name_end], b'_' | b'-' | b'.'))
        {
            name_end += 1;
        }
        if name_end == name_start {
            return None;
        }
        // <parameter=key>value</parameter> 反复提取。
        let mut args = serde_json::Map::new();
        let mut psearch = 0usize;
        loop {
            let Some(prel) = body[psearch..].find("<parameter=") else {
                break;
            };
            let pkey_start = psearch + prel + "<parameter=".len();
            let mut pkey_end = pkey_start;
            while pkey_end < body.len()
                && (body.as_bytes()[pkey_end].is_ascii_alphanumeric()
                    || matches!(body.as_bytes()[pkey_end], b'_' | b'-' | b'.'))
            {
                pkey_end += 1;
            }
            if pkey_end == pkey_start {
                return None;
            }
            // <parameter=key> 的 key 结束于 '>' 之前;值从 '>' 之后开始。
            let Some(gt_rel) = body[pkey_end..].find('>') else {
                return None;
            };
            let value_start = pkey_end + gt_rel + 1;
            let Some(rrrel) = body[value_start..].find("</parameter>") else {
                return None; // 参数块未闭合。
            };
            let raw = text[block_start + value_start..block_start + value_start + rrrel].trim();
            let value = serde_json::from_str::<serde_json::Value>(raw)
                .unwrap_or_else(|_| serde_json::Value::String(raw.to_string()));
            args.insert(body[pkey_start..pkey_end].to_string(), value);
            psearch = value_start + rrrel + "</parameter>".len();
        }
        calls.push(RescuedCall {
            name: body[name_start..name_end].to_string(),
            arguments: serde_json::Value::Object(args),
        });
        from = block_end + "</tool_call>".len();
    }
    if calls.is_empty() { None } else { Some(calls) }
}

/// 对一条 assistant 消息的全部文本块尝试救援:每块都必须解析成功且
/// 至少产出一次调用,否则整体放弃(None)。
pub(crate) fn rescue_from_blocks(blocks: &[ContentBlock]) -> Option<Vec<RescuedCall>> {
    let mut combined: Vec<RescuedCall> = Vec::new();
    let mut parse_ok = true;
    for block in blocks {
        if let ContentBlock::Text { text } = block {
            match rescue_fake_tool_calls(text) {
                Some(rescued) => combined.extend(rescued),
                None => parse_ok = false,
            }
        }
    }
    if parse_ok && !combined.is_empty() {
        Some(combined)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rescue_parses_tag_blocks_into_calls() {
        // 完整模板:函数名 + 多参数;自由文本参数按字符串,JSON 参数原样。
        let text = "\n<tool_call>\n<function=edit>\n<parameter=path>\nsrc/lib.rs\n</parameter>\n<parameter=old_string>\n{\"a\": 1}\n</parameter>\n</function>\n</tool_call>\n<tool_call>\n<function=bash>\n<parameter=command>\nGet-ChildItem\n</parameter>\n</function>\n</tool_call>";
        let calls = rescue_fake_tool_calls(text).expect("blocks must parse");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "edit");
        assert_eq!(calls[0].arguments["path"], "src/lib.rs");
        // 值本身是合法 JSON(对象)时原样保留,不是一律字符串。
        assert_eq!(calls[0].arguments["old_string"], serde_json::json!({"a": 1}));
        assert_eq!(calls[1].name, "bash");
        assert_eq!(calls[1].arguments["command"], "Get-ChildItem");
        // 大小写不敏感,函数名归一为小写。
        let upper = rescue_fake_tool_calls(
            "<TOOL_CALL><FUNCTION=Echo><PARAMETER=text>x</PARAMETER></FUNCTION></TOOL_CALL>",
        )
        .expect("upper-case tags must parse");
        assert_eq!(upper[0].name, "echo");
        // 纯正文 → None。
        assert!(rescue_fake_tool_calls("普通文本,没有工具标签").is_none());
        // 未闭合的 parameter 块 → 整体放弃。
        assert!(
            rescue_fake_tool_calls("<tool_call><function=bash><parameter=command>x</tool_call>")
                .is_none()
        );
        // 无参数调用也能救(空对象)。
        let bare = rescue_fake_tool_calls("<tool_call><function=baseline></tool_call>")
            .expect("bare call must parse");
        assert_eq!(bare[0].arguments, serde_json::json!({}));
    }

    #[test]
    fn fake_tool_call_names_detects_tags() {
        // Qwen 系"文本模拟工具调用"模板:识别并提取函数名。
        let text = "先看目录\n<tool_call>\n<function=bash>\n<parameter=command>\nGet-ChildItem\n</parameter>\n</function>\n</tool_call>\n<tool_call>\n<function=read_file>\n</function>\n</tool_call>";
        assert_eq!(
            fake_tool_call_names(text),
            vec!["bash".to_string(), "read_file".to_string()]
        );
        // 大小写不敏感。
        assert_eq!(
            fake_tool_call_names("<TOOL_CALL><FUNCTION=Echo>"),
            vec!["echo".to_string()]
        );
        // 纯正文没有标签 → 空。
        assert!(fake_tool_call_names("普通文本,没有工具标签").is_empty());
        // 有 <tool_call> 但无 <function= → 空(避免误报)。
        assert!(fake_tool_call_names("提到了 <tool_call> 但没函数").is_empty());
        // 同一函数重复声明只列一次。
        assert_eq!(
            fake_tool_call_names(
                "<tool_call><function=bash></tool_call><tool_call><function=bash></tool_call>"
            ),
            vec!["bash".to_string()]
        );
        // 无名字的裸 <function=> 不 panic。
        assert!(fake_tool_call_names("<tool_call><function=></tool_call>").is_empty());
    }

    #[test]
    fn rescue_from_blocks_requires_all_blocks_parse() {
        let good = ContentBlock::Text {
            text: "<tool_call><function=echo><parameter=text>x</parameter></function></tool_call>"
                .into(),
        };
        let bad = ContentBlock::Text {
            text: "<tool_call><function=bash><parameter=command>ls</tool_call>".into(),
        };
        assert!(rescue_from_blocks(&[good.clone()]).is_some());
        // 任一块解析失败 → 整体放弃。
        assert!(rescue_from_blocks(&[good, bad]).is_none());
        // 纯文本块(没有伪调用)→ None。
        assert!(
            rescue_from_blocks(&[ContentBlock::Text { text: "普通".into() }]).is_none()
        );
    }
}
