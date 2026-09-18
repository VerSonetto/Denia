//! 会话标题生成(抄 dsh `session-title` + `session-title-first-prompt-llm`):
//! 第一轮用户消息发出后,后台用同路由模型生成一行标题,落 `session-title`
//! 事件(latest-wins,仅日志,不进模型历史)。
//!
//! 成本纪律:思考强度取模型声明档位里的最低档(优先直接 `off` 关闭思考);
//! **不设 max_tokens**——思考型模型的思考过程会被 max_tokens 截断,标题
//! 反而生不出来。整个调用 best-effort:超时/失败静默放弃,侧栏由首条
//! 消息 excerpt 兜底,绝不打扰会话本身。

use std::time::Duration;

use denia_core::config::ModelSelection;
use denia_core::stream::StreamChunk;
use denia_llm::{GenerateRequest, LlmRegistry};
use futures::StreamExt;

use crate::state::{LiveSession, ServerEvent};

/// 单条标题输入的 UTF-8 字节上限(dsh `maxInputBytes`):超出从头部截断。
const MAX_INPUT_BYTES: usize = 4096;
/// 单条标题的 UTF-8 字节上限(dsh `maxTitleBytes`):防御模型超长输出。
const MAX_TITLE_BYTES: usize = 80;
/// 端到端生成时限(dsh `timeoutMs: 60000`)。
const TIMEOUT: Duration = Duration::from_secs(60);

/// 思考档位从低到高的已知顺序(对齐 deepseek `ReasoningEffort` 枚举)。
const EFFORT_ORDER_LOW_TO_HIGH: [&str; 6] = ["off", "low", "medium", "high", "xhigh", "max"];

/// 标题生成系统提示:语义对齐 dsh `systemPrompt()`,中文措辞。
const SYSTEM_PROMPT: &str = "为一次 AI 编码助手会话起一个简洁标题,依据提供的人类消息。\n\
只返回一行标题:纯自然语言文本,不带引号、前缀、解释、Markdown、XML 或终端控制字符;不允许代码。\n\
使用消息的语言。\n\
非 CJK 语言约 5 个词,CJK 语言不超过 10 个字。";

/// 调度一次标题生成(后台任务):门限已由调用方判定,任务内只做生成与
/// 落盘;任何失败都静默降级(tracing::debug),不影响会话与轮次。
pub fn schedule(
    registry: std::sync::Arc<LlmRegistry>,
    live: std::sync::Arc<LiveSession>,
    events: tokio::sync::broadcast::Sender<ServerEvent>,
    selection: ModelSelection,
    first_prompt: String,
) {
    tokio::spawn(async move {
        if let Err(error) = generate_and_store(&registry, &live, &events, &selection, &first_prompt)
            .await
        {
            tracing::debug!(session = %live.session.id(), error = %error, "会话标题未生成");
        }
    });
}

async fn generate_and_store(
    registry: &LlmRegistry,
    live: &LiveSession,
    events: &tokio::sync::broadcast::Sender<ServerEvent>,
    selection: &ModelSelection,
    first_prompt: &str,
) -> Result<(), String> {
    // 竞态兜底:标题事件只在会话还没有标题时落盘(latest-wins 语义下
    // 重复生成无害,但不必要的模型调用省则省)。
    if live.session.title().is_some() {
        return Ok(());
    }
    let title = generate(registry, selection, first_prompt).await?;
    let session = live.session.clone();
    let envelope = tokio::task::spawn_blocking(move || session.set_title(title))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    // follow 订阅者即时可见;侧栏列表走 sessions-updated 全量刷新。
    let _ = live.followers.send(envelope);
    let _ = events.send(ServerEvent::SessionsUpdated);
    Ok(())
}

/// 一次标题生成:同路由解析 → 最低思考档 → 流式收集文本 → 归一化。
async fn generate(
    registry: &LlmRegistry,
    selection: &ModelSelection,
    first_prompt: &str,
) -> Result<String, String> {
    let resolved = registry
        .resolve_call(&selection.provider, &selection.model, None)
        .await
        .map_err(|e| e.to_string())?;
    // 无思考声明的模型无法强制档位,交路由默认;有声明则挑最低。
    let reasoning_effort = resolved
        .reasoning
        .as_ref()
        .and_then(|reasoning| lowest_effort(&reasoning.efforts));
    let request = GenerateRequest {
        model: selection.model.clone(),
        reasoning_effort,
        messages: vec![denia_core::message::ChatMessage::user(frame_input(
            first_prompt,
        ))],
        system: Some(SYSTEM_PROMPT.into()),
        tools: Vec::new(),
        temperature: None,
        // 刻意不设 max_tokens:思考型模型的思考过程会被截断,标题反而不完整。
        max_tokens: None,
        stop: Vec::new(),
    };
    let text = tokio::time::timeout(
        TIMEOUT,
        collect_text(registry, &selection.provider, &request),
    )
    .await
    .map_err(|_| "标题生成超时".to_string())??;
    let title = normalize_title(&text, MAX_TITLE_BYTES);
    if title.is_empty() {
        return Err("标题产出为空".into());
    }
    Ok(title)
}

/// 消费一次流式调用,只收集正文文本(思考 delta 不进标题)。
async fn collect_text(
    registry: &LlmRegistry,
    provider: &str,
    request: &GenerateRequest,
) -> Result<String, String> {
    let mut stream = registry
        .stream(provider, request, None)
        .await
        .map_err(|e| e.to_string())?;
    let mut text = String::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(StreamChunk::TextDelta { text: delta, .. }) => text.push_str(&delta),
            Ok(_) => {}
            Err(failure) => return Err(failure.message),
        }
    }
    Ok(text)
}

/// 从模型声明的思考档位里挑最低强度:优先 `off`(直接关闭思考),缺失时
/// 按已知低→高顺序取首个可用档;全部未知则取声明首档兜底。
fn lowest_effort(efforts: &[denia_llm::ReasoningEffortInfo]) -> Option<String> {
    let available: std::collections::HashSet<&str> = efforts
        .iter()
        .map(|effort| effort.id.as_str())
        .collect();
    EFFORT_ORDER_LOW_TO_HIGH
        .iter()
        .find(|id| available.contains(**id))
        .map(|id| (*id).to_string())
        .or_else(|| efforts.first().map(|effort| effort.id.clone()))
}

/// dsh `frameMessages`:消息按 JSON 数组框定,用户文本破坏不了结构分隔符。
fn frame_input(prompt: &str) -> String {
    // 超长输入从头部截断(dsh 超限报错;标题取开头足够,best-effort 不打回)。
    let text = truncate_utf8(prompt.trim(), MAX_INPUT_BYTES);
    let messages = serde_json::json!([{ "seq": 1, "text": text }]);
    format!("根据以下 JSON 数组中的人类消息生成会话标题:\n{messages}")
}

/// 标题归一化(抄 dsh `normalizeSessionTitle`):剥终端转义序列与控制字符、
/// 空白归一、UTF-8 字节预算内截断,产出单行安全标题(可能为空)。
fn normalize_title(input: &str, max_bytes: usize) -> String {
    let stripped = strip_escape_sequences(input);
    let cleaned: String = stripped.chars().filter(|c| !is_stripped_control(*c)).collect();
    let normalized = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_utf8(&normalized, max_bytes)
        .trim_end()
        .to_string()
}

/// OSC/DCS/SOS/PM/APC 序列:消费到 BEL 或 ESC\ 终止;未闭合则吞到末尾。
fn consumes_osc_sequence(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    let mut prev = '\0';
    for c in chars.by_ref() {
        if c == '\u{7}' || (prev == '\u{1b}' && c == '\\') {
            return;
        }
        prev = c;
    }
}

/// CSI 序列:消费参数段直到最终字节 @-~。
fn consumes_csi_sequence(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for c in chars.by_ref() {
        if ('@'..='~').contains(&c) {
            return;
        }
    }
}

/// 剥离 OSC/CSI/两字节 ESC 转义序列(对齐 dsh 三条正则的语义)。
fn strip_escape_sequences(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek() {
                Some(']') => {
                    chars.next();
                    consumes_osc_sequence(&mut chars);
                }
                Some('[') => {
                    chars.next();
                    consumes_csi_sequence(&mut chars);
                }
                // 两字节 ESC 序列(ESC[@-_);其余裸 ESC 由控制字符过滤兜底。
                Some(&f) if ('@'..='_').contains(&f) => {
                    chars.next();
                }
                _ => {}
            },
            // C1 单字符 CSI(\u{9b})与单字符 OSC(\u{9d})。
            '\u{9b}' => consumes_csi_sequence(&mut chars),
            '\u{9d}' => consumes_osc_sequence(&mut chars),
            _ => out.push(c),
        }
    }
    out
}

/// dsh `CONTROL_CHARACTER` + `DIRECTIONAL_CONTROL`:除 \t\n\r(空白归一
/// 阶段处理)外的 C0/C1 控制字符,以及方向性/零宽不可见字符。
fn is_stripped_control(c: char) -> bool {
    matches!(c,
        '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' | '\u{7f}'..='\u{9f}'
        | '\u{200b}' | '\u{200e}' | '\u{200f}'
        | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{206f}' | '\u{feff}')
}

/// UTF-8 边界安全截断:返回不超过 `max_bytes` 的最长码点前缀(dsh
/// `truncateTitleUtf8`)。
fn truncate_utf8(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    let mut used = 0usize;
    let mut out = String::new();
    for c in input.chars() {
        let len = c.len_utf8();
        if used + len > max_bytes {
            break;
        }
        out.push(c);
        used += len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_controls_and_collapses_whitespace() {
        assert_eq!(normalize_title("  你好\t世界 \n 生成 ", 80), "你好 世界 生成");
        assert_eq!(normalize_title("\u{1b}[31m红\u{1b}[0m色", 80), "红色");
        assert_eq!(normalize_title("\u{200e}隐藏\u{feff}字符", 80), "隐藏字符");
        assert_eq!(normalize_title("\u{7}\u{1b}", 80), "");
        // 未闭合的 OSC 序列连同尾部一起吞掉。
        assert_eq!(normalize_title("\u{1b}]0;evil", 80), "");
    }

    #[test]
    fn normalize_truncates_on_utf8_boundary() {
        // 80 字节 = 26 个汉字 + 2 字节余量:第 27 个汉字(3 字节)放不下。
        let long = "标".repeat(30);
        let title = normalize_title(&long, MAX_TITLE_BYTES);
        assert_eq!(title.chars().count(), 26);
        assert_eq!(title.len(), 78);
    }

    #[test]
    fn normalize_keeps_short_title_intact() {
        let title = normalize_title("\"重构登录流程\"", MAX_TITLE_BYTES);
        // 引号由提示词禁止,但归一化层不做语义剥离,只做安全清理。
        assert_eq!(title, "\"重构登录流程\"");
    }

    #[test]
    fn lowest_effort_prefers_off_then_low() {
        let effort = |id: &str| denia_llm::ReasoningEffortInfo {
            id: id.into(),
            name: id.into(),
            description: None,
        };
        let full: Vec<_> = ["high", "off", "medium"].iter().map(|id| effort(id)).collect();
        assert_eq!(lowest_effort(&full).as_deref(), Some("off"));
        let no_off: Vec<_> = ["max", "medium", "low"].iter().map(|id| effort(id)).collect();
        assert_eq!(lowest_effort(&no_off).as_deref(), Some("low"));
        let unknown: Vec<_> = ["turbo"].iter().map(|id| effort(id)).collect();
        assert_eq!(lowest_effort(&unknown).as_deref(), Some("turbo"));
        assert_eq!(lowest_effort(&[]), None);
    }

    #[test]
    fn frame_input_clamps_long_prompt() {
        let long = "x".repeat(MAX_INPUT_BYTES * 2);
        let framed = frame_input(&long);
        // JSON 包装后总长应只是上限级别的量级,而不是两倍上限。
        assert!(framed.len() < MAX_INPUT_BYTES + 256);
    }
}
