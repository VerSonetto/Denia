//! 流式兼容性:各网关实测出现过的字段形态,解析器都要认。
//!
//! 这里的每一条都来自真实抓取或官方文档(见 context7 / OpenRouter 文档):
//! 推理字段名没有统一标准、content 有三种形态、拒答单独成字段、
//! 工具调用有新旧两种形状、帧里可以带 error、usage 的缓存口径各家不同。
//! 兼容性出问题时的症状都是同一个:内容**静默丢失**——所以这些用例
//! 一律断言“内容确实拿到了”,而不只是“没报错”。

use denia_core::stream::{ContentBlock, FinishReason, StreamChunk};
use denia_llm::{CompletionsStream, EventTranslator, UsageStyle};

fn feed_all(frames: &[&str]) -> Vec<StreamChunk> {
    let mut translator = CompletionsStream::new(UsageStyle::OpenAi);
    let mut all = Vec::new();
    for frame in frames {
        all.extend(translator.feed(frame).expect("帧应能解析"));
    }
    all.extend(translator.feed("[DONE]").expect("结束标记应被接受"));
    all.extend(translator.finish().expect("流应能收尾"));
    all
}

fn text_of(all: &[StreamChunk]) -> String {
    all.iter()
        .filter_map(|c| match c {
            StreamChunk::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn reasoning_of(all: &[StreamChunk]) -> String {
    all.iter()
        .filter_map(|c| match c {
            StreamChunk::ReasoningDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// `reasoning.summary` 条目把正文放在 `summary` 而不是 `text`。
#[test]
fn reasoning_summary_entry_uses_summary_field() {
    let all = feed_all(&[
        r#"{"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.summary","summary":"先分析约束","id":"r1","format":"anthropic-claude-v1","index":0}]}}]}"#,
    ]);
    assert_eq!(reasoning_of(&all), "先分析约束");
}

/// 推理条目用 `content` 字段承载正文(另一种实测形态)。
#[test]
fn reasoning_entry_content_field_is_read() {
    let all = feed_all(&[
        r#"{"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","content":"通过 content 字段"}]}}]}"#,
    ]);
    assert_eq!(reasoning_of(&all), "通过 content 字段");
}

/// 密文类条目没有可读正文,不应产出空增量,也不该把密文当正文显示。
#[test]
fn encrypted_reasoning_entry_produces_nothing() {
    let all = feed_all(&[
        r#"{"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.encrypted","data":"eyJlbmNyeXB0ZWQiOnRydWV9"}]}}]}"#,
    ]);
    assert!(reasoning_of(&all).is_empty());
    assert!(text_of(&all).is_empty());
}

/// `content` 发数组(多模态 parts)时,文本部分仍要拿到。
#[test]
fn content_array_parts_yield_text() {
    let all = feed_all(&[
        r#"{"choices":[{"delta":{"content":[{"type":"text","text":"第一段"},{"type":"text","text":"第二段"}]}}]}"#,
    ]);
    assert_eq!(text_of(&all), "第一段第二段");
}

/// 数组里的非文本 part(图片)要被跳过,不能当成正文。
#[test]
fn content_array_skips_non_text_parts() {
    let all = feed_all(&[
        r#"{"choices":[{"delta":{"content":[{"type":"image_url","image_url":{"url":"data:,"}},{"type":"text","text":"只有这段"}]}}]}"#,
    ]);
    assert_eq!(text_of(&all), "只有这段");
}

/// `content` 发对象时,`text` 键要被认出来。
#[test]
fn content_object_yield_text() {
    let all = feed_all(&[r#"{"choices":[{"delta":{"content":{"text":"对象形态"}}}]}"#]);
    assert_eq!(text_of(&all), "对象形态");
}

/// 拒答是独立字段,不能丢,也不能当正文混进去。
#[test]
fn refusal_field_is_preserved() {
    let all = feed_all(&[
        r#"{"choices":[{"delta":{"role":"assistant","content":null,"refusal":"我不能协助这个请求"}}]}"#,
    ]);
    let joined: String = all
        .iter()
        .filter_map(|c| match c {
            StreamChunk::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        joined.contains("我不能协助这个请求"),
        "拒答内容应保留,实际: {joined:?}"
    );
}

/// 旧式单函数调用(`delta.function_call`)要被认成工具调用。
#[test]
fn legacy_function_call_becomes_tool_call() {
    let all = feed_all(&[
        r##"{"choices":[{"delta":{"function_call":{"name":"get_weather","arguments":"{\"city\":\"北京\"}"}}}]}"##,
    ]);
    let call = all
        .iter()
        .find_map(|c| match c {
            StreamChunk::BlockEnd {
                block:
                    ContentBlock::ToolCall {
                        name, arguments, ..
                    },
                ..
            } => Some((name.clone(), arguments.clone())),
            _ => None,
        })
        .expect("应产出工具调用块");
    assert_eq!(call.0, "get_weather");
    assert!(call.1.contains("北京"));
}

/// `n>1` 时一帧里有多个候选,只取主候选(index 0),
/// 否则多个候选的正文会拼成一段乱码。
#[test]
fn multiple_choices_use_only_primary() {
    let all = feed_all(&[
        r#"{"choices":[{"index":1,"delta":{"content":"候选一的内容"}},{"index":0,"delta":{"content":"主候选"}}]}"#,
    ]);
    assert_eq!(text_of(&all), "主候选");
}

/// 没有 index 标记时退回第一条,不能把内容全丢掉。
#[test]
fn choices_without_index_fall_back_to_first() {
    let all = feed_all(&[r#"{"choices":[{"delta":{"content":"唯一候选"}}]}"#]);
    assert_eq!(text_of(&all), "唯一候选");
}

/// 帧里带 error 时整步必须失败,不能当成一次成功。
#[test]
fn error_frame_fails_the_step() {
    let mut translator = CompletionsStream::new(UsageStyle::OpenAi);
    translator
        .feed(r#"{"choices":[{"delta":{"content":"部分内容"}}]}"#)
        .expect("正常帧应能解析");
    let error = translator
        .feed(r#"{"error":{"code":"rate_limit_exceeded","message":"too many requests"}}"#)
        .expect_err("带 error 的帧必须失败");
    assert_eq!(error.code, "rate_limit_exceeded");
    assert!(error.message.contains("too many requests"));
}

/// 收尾帧上的 error 也不能被放过。
#[test]
fn error_in_final_chunk_fails_the_step() {
    let mut translator = CompletionsStream::new(UsageStyle::OpenAi);
    translator
        .feed(
            r#"{"choices":[{"delta":{"content":"正文"},"finish_reason":"stop"}],"error":{"type":"server_error","message":"boom"}}"#,
        )
        .expect_err("收尾帧带 error 必须失败");
}

/// DeepSeek 只报未命中数时,缓存读可以反推出来。
#[test]
fn deepseek_cache_miss_backfills_cache_read() {
    let mut translator = CompletionsStream::new(UsageStyle::DeepSeek);
    let mut all = Vec::new();
    all.extend(
        translator
            .feed(
                r#"{"choices":[{"delta":{"content":"hi"}}],"usage":{"prompt_tokens":100,"completion_tokens":5,"prompt_cache_miss_tokens":20}}"#,
            )
            .expect("帧应能解析"),
    );
    all.extend(translator.feed("[DONE]").expect("结束标记应被接受"));
    all.extend(translator.finish().expect("流应能收尾"));

    let usage = all
        .iter()
        .find_map(|c| match c {
            StreamChunk::Usage { usage } => Some(*usage),
            _ => None,
        })
        .expect("应产出 usage 帧");
    assert_eq!(usage.cache_read_tokens, Some(80));
    assert_eq!(usage.input_tokens, 20);
}

/// 命中数与未命中数同时存在时,以命中数为权威口径。
#[test]
fn deepseek_cache_hit_wins_over_miss() {
    let mut translator = CompletionsStream::new(UsageStyle::DeepSeek);
    let mut all = Vec::new();
    all.extend(
        translator
            .feed(
                r#"{"choices":[{"delta":{"content":"hi"}}],"usage":{"prompt_tokens":100,"completion_tokens":5,"prompt_cache_hit_tokens":60,"prompt_cache_miss_tokens":99}}"#,
            )
            .expect("帧应能解析"),
    );
    all.extend(translator.feed("[DONE]").expect("结束标记应被接受"));
    all.extend(translator.finish().expect("流应能收尾"));

    let usage = all
        .iter()
        .find_map(|c| match c {
            StreamChunk::Usage { usage } => Some(*usage),
            _ => None,
        })
        .expect("应产出 usage 帧");
    assert_eq!(usage.cache_read_tokens, Some(60));
    assert_eq!(usage.input_tokens, 40);
}

/// 推理与正文可以同帧出现,两者都要拿到。
#[test]
fn reasoning_and_text_in_same_frame() {
    let all = feed_all(&[r#"{"choices":[{"delta":{"reasoning":"想一想","content":"答"}}]}"#]);
    assert_eq!(reasoning_of(&all), "想一想");
    assert_eq!(text_of(&all), "答");
}

/// 收尾的 finish 原因要如实反映上游。
#[test]
fn finish_reason_length_maps_to_max_tokens() {
    let all =
        feed_all(&[r#"{"choices":[{"delta":{"content":"被截断"},"finish_reason":"length"}]}"#]);
    let finish = all.last().expect("应有 finish 帧");
    match finish {
        StreamChunk::Finish {
            reason: FinishReason::MaxTokens,
        } => {}
        other => panic!("期望 MaxTokens,实际 {other:?}"),
    }
}
