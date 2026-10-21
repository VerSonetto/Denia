//! 回归验证:把真实抓取的网关帧全量喂给 denia 的翻译器。
//!
//! 输入:tests/fixtures/reasoning_capture.jsonl(每行 `<秒>\t<原始 SSE 行>`),
//! 截自一次真实抓取。期望:全部推理帧都被解析成推理增量,推理块先于正文块
//! 出现,且首字延迟不再伴随“界面全静默”。
//!
//! 这份抓取来自一家把推理放在 `delta.reasoning` / `delta.reasoning_details`
//! 的网关;解析器按语义识别,不依赖具体供应商。

use denia_core::stream::{ContentBlock, StreamChunk};
use denia_llm::{CompletionsStream, EventTranslator, UsageStyle};
use std::fs;

fn load_frames(path: &str) -> Vec<(f64, String)> {
    // cargo 跑集成测试时 cwd 是包目录,不是仓库根,所以用绝对路径。
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(path);
    let text = fs::read_to_string(&path).expect("应能读到抓取样本");
    text.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, '\t');
            let t: f64 = parts.next()?.parse().ok()?;
            let body = parts.next()?;
            Some((t, body.to_string()))
        })
        .collect()
}

#[test]
fn real_capture_yields_reasoning_then_text() {
    let frames = load_frames("reasoning_capture.jsonl");
    assert!(frames.len() > 30, "抓取帧数异常: {}", frames.len());

    let mut translator = CompletionsStream::new(UsageStyle::OpenAi);
    let mut all = Vec::new();
    let mut first_reasoning_at = None;
    let mut first_text_at = None;

    for (t, line) in &frames {
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload == "[DONE]" {
            all.extend(translator.feed("[DONE]").unwrap());
            continue;
        }
        let emitted = translator.feed(payload).unwrap();
        for chunk in &emitted {
            match chunk {
                StreamChunk::ReasoningDelta { text, .. } if !text.is_empty() => {
                    // get_or_insert 只在首次写入,语义就是「记下首次出现的时间点」。
                    let _ = first_reasoning_at.get_or_insert(*t);
                }
                StreamChunk::TextDelta { text, .. } if !text.is_empty() => {
                    let _ = first_text_at.get_or_insert(*t);
                }
                _ => {}
            }
        }
        all.extend(emitted);
    }
    all.extend(translator.finish().unwrap());

    let reasoning_deltas = all
        .iter()
        .filter(|c| matches!(c, StreamChunk::ReasoningDelta { .. }))
        .count();
    assert!(
        reasoning_deltas >= 30,
        "全部推理帧应被解析,实际 {reasoning_deltas}"
    );

    // 推理块必须先于正文块开启。
    let reasoning_block_start = all.iter().position(|c| {
        matches!(
            c,
            StreamChunk::BlockStart {
                block_type: denia_core::stream::BlockType::Reasoning,
                ..
            }
        )
    });
    let text_block_start = all.iter().position(|c| {
        matches!(
            c,
            StreamChunk::BlockStart {
                block_type: denia_core::stream::BlockType::Text,
                ..
            }
        )
    });
    assert!(reasoning_block_start.is_some(), "应开启推理块");
    assert!(text_block_start.is_some(), "应开启正文块");
    assert!(
        reasoning_block_start < text_block_start,
        "推理块应先于正文块"
    );

    // 修复前:首字延迟 78.7s 期间界面无任何内容(推理被丢弃)。
    // 修复后:推理在 2.9s 就开始流式出现。
    let reasoning_start = first_reasoning_at.expect("应有首个推理增量");
    let text_start = first_text_at.expect("应有首个正文增量");
    assert!(
        reasoning_start < 5.0,
        "推理应在几秒内开始流式出现,实际 +{reasoning_start}s"
    );
    assert!(
        text_start > reasoning_start,
        "正文应在推理之后出现,实际 reasoning=+{reasoning_start}s text=+{text_start}s"
    );

    // 推理块内容非空(界面有东西可展示)。
    let reasoning_text = all
        .iter()
        .filter_map(|c| match c {
            StreamChunk::BlockEnd {
                block: ContentBlock::Reasoning { text, .. },
                ..
            } => Some(text.clone()),
            _ => None,
        })
        .next()
        .expect("应产出推理块内容");
    assert!(
        reasoning_text.len() > 100,
        "推理内容过短: {}",
        reasoning_text.len()
    );
}
