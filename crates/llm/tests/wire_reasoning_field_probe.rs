//! 集成验证:用 denia 自己的 chat-completions 翻译器解析「非标准推理字段」的帧。
//!
//! 背景(实测抓取,见 target/tmp/ort_raw_max.jsonl):不少网关把推理放在
//! `delta.reasoning` / `delta.reasoning_details`,而不是标准的
//! `delta.reasoning_content`。修复前 denia 的 wire 解析器认不出这些字段,整段
//! 推理被丢弃:首字延迟期间上游在持续吐推理帧,denia 侧却一个推理块都不开,
//! 用户只能等推理全部结束才看到正文。
//!
//! 这里锁定的是「按语义识别推理字段」这一行为,不针对任何一家供应商:
//! 换一个字段名(或换一家网关)同样应当被解析成推理块。

use denia_core::stream::{ContentBlock, StreamChunk};
use denia_llm::{CompletionsStream, EventTranslator, UsageStyle};

/// 非标准推理字段的原生帧(截自真实抓取,首帧即带推理)。
const FRAME_REASONING: &str = r#"{"id":"gen-1792457555","object":"chat.completion.chunk","created":1792457555,"model":"minimax/minimax-m3.1-flash:free","provider":"Minimax","choices":[{"index":0,"delta":{"content":"","role":"assistant","reasoning":"We need answer likely","reasoning_details":[{"type":"reasoning.text","text":"We need answer likely","format":"unknown","index":0}]},"finish_reason":null,"native_finish_reason":null}]}"#;

const FRAME_REASONING_2: &str = r#"{"id":"gen-1792457555","object":"chat.completion.chunk","created":1792457555,"model":"minimax/minimax-m3.1-flash:free","provider":"Minimax","choices":[{"index":0,"delta":{"content":"","role":"assistant","reasoning":" create HTML file in repo.","reasoning_details":[{"type":"reasoning.text","text":" create HTML file in repo.","format":"unknown","index":0}]},"finish_reason":null,"native_finish_reason":null}]}"#;

/// 上游在推理结束后才开始吐正文。
const FRAME_TEXT: &str = r#"{"id":"gen-1792457555","object":"chat.completion.chunk","created":1792457555,"model":"minimax/minimax-m3.1-flash:free","provider":"Minimax","choices":[{"index":0,"delta":{"content":"好的，我来创建","role":"assistant"},"finish_reason":null,"native_finish_reason":null}]}"#;

#[test]
fn nonstandard_reasoning_frames_yield_reasoning_block() {
    let mut translator = CompletionsStream::new(UsageStyle::OpenAi);
    let mut all = Vec::new();
    all.extend(translator.feed(FRAME_REASONING).unwrap());
    all.extend(translator.feed(FRAME_REASONING_2).unwrap());
    all.extend(translator.feed(FRAME_TEXT).unwrap());
    all.extend(translator.feed("[DONE]").unwrap());
    all.extend(translator.finish().unwrap());

    let reasoning_deltas = all
        .iter()
        .filter(|c| matches!(c, StreamChunk::ReasoningDelta { .. }))
        .count();
    assert_eq!(reasoning_deltas, 2, "非标准推理字段应被解析成推理增量");

    let reasoning_text: String = all
        .iter()
        .filter_map(|c| match c {
            StreamChunk::ReasoningDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(reasoning_text.contains("We need answer likely"));
    assert!(reasoning_text.contains("create HTML file in repo."));

    // 同一段的扁平字段与结构化字段内容相同,只能记一遍。
    assert!(
        !reasoning_text.contains("We need answer likelyWe need answer likely"),
        "推理正文不得因两个字段并存而被重复记录"
    );

    let reasoning_blocks = all
        .iter()
        .filter(|c| {
            matches!(
                c,
                StreamChunk::BlockEnd {
                    block: ContentBlock::Reasoning { .. },
                    ..
                }
            )
        })
        .count();
    assert_eq!(reasoning_blocks, 1, "应产出且只产出一个推理块");
}

#[test]
fn usage_still_reports_reasoning_tokens() {
    // usage 里的 reasoning_tokens 能到(实测 8140),修复后流里也应有推理增量——
    // 两者口径必须一致。
    let mut translator = CompletionsStream::new(UsageStyle::OpenAi);
    let mut all = Vec::new();
    all.extend(translator.feed(FRAME_REASONING).unwrap());
    all.extend(
        translator
            .feed(
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":234,"completion_tokens":14655,"prompt_tokens_details":{"cached_tokens":195},"completion_tokens_details":{"reasoning_tokens":8140}}}"#,
            )
            .unwrap(),
    );
    all.extend(translator.feed("[DONE]").unwrap());
    all.extend(translator.finish().unwrap());

    let usage = all
        .iter()
        .find_map(|c| match c {
            StreamChunk::Usage { usage } => Some(*usage),
            _ => None,
        })
        .expect("应产出 usage 帧");
    assert_eq!(usage.reasoning_tokens, Some(8140));
    assert_eq!(usage.cache_read_tokens, Some(195));
    assert_eq!(usage.input_tokens, 234 - 195);
}
