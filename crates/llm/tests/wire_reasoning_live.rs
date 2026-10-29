//! 端到端验证:走 denia 真实的 HTTP + SSE 驱动,对真实网关发一次请求。
//!
//! 目的:证明修复不只在单元测试里成立——真实网络路径上,上游的非标准推理
//! 字段能变成 `StreamChunk::ReasoningDelta`,界面因此能在首字延迟期间展示思考。
//!
//! 跑法(需要凭据与网络):
//!     cargo test -p denia-llm --test wire_reasoning_live -- --ignored --nocapture
//!
//! 默认跳过:它是联网测试,不属于常规回归。

use denia_core::message::{ChatMessage, ChatRole};
use denia_core::stream::StreamChunk;
use denia_llm::{GenerateRequest, LlmAdapter, OpenAiCompatAdapter};
use std::path::PathBuf;
use std::time::Instant;

fn denia_home() -> PathBuf {
    if let Ok(home) = std::env::var("DENIA_HOME") {
        return PathBuf::from(home);
    }
    let user = std::env::var("USERPROFILE").expect("应有 USERPROFILE");
    PathBuf::from(user).join(".denia")
}

#[tokio::test]
#[ignore = "联网测试:需要凭据与网络"]
async fn live_gateway_streams_reasoning_deltas() {
    let home = denia_home();
    let settings = denia_settings::SettingsStore::open(&home).expect("应能打开设置库");
    // 命名空间是 host 启动时注册的;这里补一次,直接用真实 settings.yaml 的值。
    settings
        .register(
            denia_llm::OPENAI_SETTINGS_NS,
            denia_settings::NamespaceSpec {
                defaults: serde_json::json!({ "providers": {} }),
                validate: Ok,
                secrets: &[],
                applies: denia_settings::Applies::Live,
            },
            serde_json::json!({}),
        )
        .expect("应能注册 llm-openai 命名空间");
    let credentials = denia_credentials::CredentialStore::open(&home).expect("应能打开凭据库");
    let adapter = OpenAiCompatAdapter::new(
        std::sync::Arc::new(settings),
        std::sync::Arc::new(credentials),
    );

    let request = GenerateRequest {
        model: "minimax/minimax-m3.1-flash:free".to_string(),
        reasoning_effort: Some("max".to_string()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "用一个字回答:鹈鹕会游泳吗?先想一想再答。".to_string(),
            cache: denia_core::message::MessageCache::default(),
            images: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning_content: None,
            reasoning_replay: Vec::new(),
        }],
        system: None,
        tools: Vec::new(),
        temperature: None,
        max_tokens: None,
        stop: Vec::new(),
        cache_key: None,
    };

    let started = Instant::now();
    let mut stream = adapter.stream("ort", &request).await.expect("应能建立流");

    let mut reasoning_deltas = 0usize;
    let mut text_deltas = 0usize;
    let mut first_reasoning_at = None;
    let mut first_text_at = None;
    let mut reasoning_preview = String::new();
    let mut text_preview = String::new();

    use futures::StreamExt;
    while let Some(item) = stream.next().await {
        let chunk = item.expect("流不应报错");
        let elapsed = started.elapsed();
        match chunk {
            StreamChunk::ReasoningDelta { text, .. } => {
                reasoning_deltas += 1;
                if first_reasoning_at.is_none() {
                    first_reasoning_at = Some(elapsed);
                }
                if reasoning_preview.len() < 200 {
                    reasoning_preview.push_str(&text);
                }
            }
            StreamChunk::TextDelta { text, .. } => {
                text_deltas += 1;
                if first_text_at.is_none() {
                    first_text_at = Some(elapsed);
                }
                if text_preview.len() < 200 {
                    text_preview.push_str(&text);
                }
            }
            _ => {}
        }
    }

    println!("推理增量数: {reasoning_deltas}");
    println!("正文增量数: {text_deltas}");
    println!("首个推理增量: {first_reasoning_at:?}");
    println!("首个正文增量: {first_text_at:?}");
    println!("推理预览: {reasoning_preview}");
    println!("正文预览: {text_preview}");

    assert!(reasoning_deltas > 0, "真实网关的推理应被解析成推理增量");
    assert!(text_deltas > 0, "应收到正文增量");
    assert!(first_reasoning_at < first_text_at, "推理应先于正文出现");
}
