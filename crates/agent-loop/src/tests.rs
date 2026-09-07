//! SessionDriver 集成测试:脚本化 adapter 驱动主循环的全部关键路径,
//! 外加死循环检测与输出预算的端到端断言。

use super::*;
use crate::errors::MAX_FEEDBACK;
use async_trait::async_trait;
use denia_core::error::LlmError;
use denia_core::session::{AbortCause, RequestHeaderReason};
use denia_core::stream::{BlockType, ContentBlock, FinishReason, StreamChunk, TokenUsage};
use denia_core::tool::ToolSchema;
use denia_llm::{ChunkStream, GenerateRequest, LlmAdapter, LlmModelInfo, LlmResolvedModelInfo, ProviderInfo};
use denia_system_prompt::SystemPrompt;
use denia_tools::ToolRegistry;
use futures::StreamExt;
use std::collections::VecDeque;
use std::sync::Mutex;

#[test]
fn system_prompt_frames_runtime_authority() {
    let (prompt, _) = denia_tools::default_shipped();
    let assembly = prompt
        .assemble(&denia_system_prompt::AssembleContext {
            cwd: Some("/tmp/ws".to_string()),
            model: Some("mock".to_string()),
            provider: Some("mock".to_string()),
            ..Default::default()
        })
        .unwrap();
    let body = denia_system_prompt::render_prompt(&assembly);
    assert!(!body.contains("最高优先级"));
    assert!(body.contains("/tmp/ws"));

    let model = denia_system_prompt::frame_system_prompt_for_model(&body);
    assert!(model.contains("最高优先级"));
    assert!(model.contains("再次确认"));
    assert!(model.contains("/tmp/ws"));
}

/// Scripted adapter: each `stream` call pops the next queued chunk run.
enum MockScript {
    Chunks(Vec<StreamChunk>),
    Fail(LlmFailure),
    /// 先产出 chunk,再以流错误终止(模拟 [DONE] 前断流)。
    ChunksThenFail(Vec<StreamChunk>, LlmFailure),
}

struct MockAdapter {
    scripts: Mutex<VecDeque<MockScript>>,
}

#[async_trait]
impl LlmAdapter for MockAdapter {
    fn provider_info(&self, provider: &str) -> ProviderInfo {
        ProviderInfo {
            id: provider.to_string(),
            name: "mock".to_string(),
        }
    }

    async fn list_models(&self, provider: &str) -> Result<Vec<LlmModelInfo>, LlmError> {
        Ok(vec![LlmModelInfo {
            provider: provider.to_string(),
            id: "mock-1".to_string(),
            name: "Mock One".to_string(),
            description: None,
            input_modalities: vec!["text".to_string()],
        }])
    }

    async fn resolve_model(
        &self,
        provider: &str,
        model: &str,
    ) -> Result<LlmResolvedModelInfo, LlmError> {
        Ok(LlmResolvedModelInfo {
            info: LlmModelInfo {
                provider: provider.to_string(),
                id: model.to_string(),
                name: model.to_string(),
                description: None,
                input_modalities: vec!["text".to_string()],
            },
            context_window: Some(100_000),
            default_max_tokens: Some(4_000),
            reasoning: None,
        })
    }

    async fn stream(
        &self,
        _provider: &str,
        _request: &GenerateRequest,
    ) -> Result<ChunkStream, LlmError> {
        match self.scripts.lock().unwrap().pop_front() {
            Some(MockScript::Fail(failure)) => {
                Err(denia_core::error::LlmError::from_failure(failure))
            }
            Some(MockScript::Chunks(chunks)) => {
                Ok(Box::pin(futures::stream::iter(chunks.into_iter().map(Ok))))
            }
            Some(MockScript::ChunksThenFail(chunks, failure)) => Ok(Box::pin(
                futures::stream::iter(chunks.into_iter().map(Ok)).chain(futures::stream::once(
                    async move { Err::<StreamChunk, LlmFailure>(failure) },
                )),
            )),
            None => Ok(Box::pin(futures::stream::empty())),
        }
    }
}

struct EchoTool;

#[async_trait]
impl denia_tools::Tool for EchoTool {
    fn schema(&self) -> &ToolSchema {
        // Leaked once for the &'static contract of the test trait object.
        Box::leak(Box::new(ToolSchema {
            name: "echo".to_string(),
            description: "echoes".to_string(),
            parameters: serde_json::json!({ "type": "object" }),
        }))
    }

    async fn execute(&self, arguments: &str, _ctx: &denia_tools::ToolContext) -> denia_tools::ToolOutput {
        denia_tools::ToolOutput {
            content: format!("echo:{arguments}"),
            is_error: false,
        }
    }
}

/// 输出超预算的工具:验证落盘层统一截断。
struct BulkyTool;

#[async_trait]
impl denia_tools::Tool for BulkyTool {
    fn schema(&self) -> &ToolSchema {
        Box::leak(Box::new(ToolSchema {
            name: "bulky".to_string(),
            description: "emits oversized output".to_string(),
            parameters: serde_json::json!({ "type": "object" }),
        }))
    }

    async fn execute(&self, _arguments: &str, _ctx: &denia_tools::ToolContext) -> denia_tools::ToolOutput {
        denia_tools::ToolOutput {
            content: "x".repeat(denia_tools::support::OUTPUT_BUDGET_CHARS + 500),
            is_error: false,
        }
    }
}

/// 一条 finish-error 流:模拟网关空响应(EMPTY_RESPONSE 以 finish 错误产出)。
fn error_finish_script() -> Vec<StreamChunk> {
    vec![StreamChunk::Finish {
        reason: FinishReason::Error {
            failure: LlmFailure::new(
                denia_core::error::codes::EMPTY_RESPONSE,
                "empty response",
            ),
        },
    }]
}

fn text_script(text: &str) -> Vec<StreamChunk> {
    vec![
        StreamChunk::BlockStart {
            index: 0,
            block_type: BlockType::Text,
        },
        StreamChunk::TextDelta {
            index: 0,
            text: text.to_string(),
        },
        StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::Text {
                text: text.to_string(),
            },
        },
        StreamChunk::Usage {
            usage: TokenUsage {
                input_tokens: 5,
                output_tokens: 2,
                cache_read_tokens: None,
                reasoning_tokens: None,
            },
        },
        StreamChunk::Finish {
            reason: FinishReason::Stop,
        },
    ]
}

fn tool_script() -> Vec<StreamChunk> {
    vec![
        StreamChunk::BlockStart {
            index: 0,
            block_type: BlockType::ToolCall,
        },
        StreamChunk::ToolCallDelta {
            index: 0,
            id: "call_1".to_string(),
            name: Some("echo".to_string()),
            arguments_delta: "{\"text\":\"hi\"}".to_string(),
        },
        StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::ToolCall {
                id: "call_1".to_string(),
                name: "echo".to_string(),
                arguments: "{\"text\":\"hi\"}".to_string(),
            },
        },
        StreamChunk::Finish {
            reason: FinishReason::ToolCalls,
        },
    ]
}

fn driver(scripts: Vec<MockScript>) -> (SessionDriver, Arc<LlmRegistry>) {
    driver_with_tools(scripts, |tools| {
        tools.register(Arc::new(EchoTool));
    })
}

fn driver_with_tools(
    scripts: Vec<MockScript>,
    register: impl FnOnce(&mut ToolRegistry),
) -> (SessionDriver, Arc<LlmRegistry>) {
    let registry = Arc::new(LlmRegistry::new());
    registry
        .register(
            &["mock".to_string()],
            Arc::new(MockAdapter {
                scripts: Mutex::new(VecDeque::from(scripts)),
            }),
            denia_llm::RetryPolicy::default(),
        )
        .unwrap();
    let mut tools = ToolRegistry::default();
    register(&mut tools);
    let mut prompt = SystemPrompt::new(denia_system_prompt::SystemPromptConfig {
        include_runtime_context: false,
        ..Default::default()
    });
    let schemas = tools.schemas();
    prompt.tools(move |_| denia_system_prompt::ToolProviderResult {
        schemas: schemas.clone(),
        known_names: None,
    });
    prompt
        .variable("cwd", |context| context.cwd.clone())
        .unwrap();
    prompt
        .variable("model", |context| context.model.clone())
        .unwrap();
    prompt
        .variable("provider", |context| context.provider.clone())
        .unwrap();
    (
        SessionDriver::new(
            registry.clone(),
            Arc::new(tools),
            Arc::new(ArcSwap::from_pointee(prompt)),
        ),
        registry,
    )
}

fn temp_session() -> Arc<Session> {
    let dir = std::env::temp_dir().join(format!(
        "denia-loop-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    Arc::new(Session::create(&dir, uuid::Uuid::new_v4().to_string(), &dir, true, None).unwrap())
}

fn selection() -> ModelSelection {
    ModelSelection {
        provider: "mock".to_string(),
        model: "mock-1".to_string(),
        reasoning_effort: None,
    }
}

fn noop_emit() -> Arc<dyn Fn(&SessionEnvelope) + Send + Sync> {
    Arc::new(|_| {})
}

#[tokio::test]
async fn plain_text_turn_completes() {
    let (driver, _registry) = driver(vec![MockScript::Chunks(text_script("done!"))]);
    let session = temp_session();
    let reason = driver
        .run_turn(
            &session,
            &selection(),
            "hello",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            CancellationToken::new(),
            noop_emit(),
        )
        .await;
    assert_eq!(reason, TurnEndReason::Completed);

    let kinds: Vec<&str> = session
        .events()
        .iter()
        .map(|envelope| match &envelope.event {
            SessionEvent::UserMessage { .. } => "user",
            SessionEvent::TurnStart { .. } => "turn-start",
            SessionEvent::StepStart { .. } => "step-start",
            SessionEvent::SystemPrompt { .. } => "system-prompt",
            SessionEvent::AssistantMessage { .. } => "assistant",
            SessionEvent::StepEnd { .. } => "step-end",
            SessionEvent::TurnEnd { .. } => "turn-end",
            _ => "chunk",
        })
        .filter(|kind| *kind != "chunk")
        .collect();
    assert_eq!(
        kinds,
        vec![
            "user",
            "turn-start",
            "step-start",
            "system-prompt",
            "assistant",
            "step-end",
            "turn-end"
        ]
    );
    let messages = session.derive_messages();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].content, "done!");
}

#[tokio::test]
async fn tool_call_continues_to_second_step() {
    let (driver, _registry) = driver(vec![
        MockScript::Chunks(tool_script()),
        MockScript::Chunks(text_script("after tool")),
    ]);
    let session = temp_session();
    let reason = driver
        .run_turn(
            &session,
            &selection(),
            "use the tool",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            CancellationToken::new(),
            noop_emit(),
        )
        .await;
    assert_eq!(reason, TurnEndReason::Completed);

    let flat: Vec<String> = session
        .events()
        .iter()
        .map(|envelope| match &envelope.event {
            SessionEvent::ToolCall { .. } => "call".to_string(),
            SessionEvent::ToolResult { is_error, .. } => format!("result:{is_error}"),
            SessionEvent::StepStart { step, .. } => format!("step:{step}"),
            _ => "-".to_string(),
        })
        .collect();
    let call_pos = flat.iter().position(|k| k == "call").unwrap();
    let result_pos = flat.iter().position(|k| k == "result:false").unwrap();
    assert!(call_pos < result_pos, "tool/call must precede tool/result");
    assert!(flat.iter().any(|k| k == "step:2"), "expected a second step");

    // The second request saw the tool result in derived history.
    let messages = session.derive_messages();
    assert!(
        messages
            .iter()
            .any(|m| m.role == denia_core::message::ChatRole::Tool)
    );
}

#[tokio::test]
async fn parallel_tool_calls_emit_one_result_each_in_order() {
    // 一次 step 返回 3 个 echo 工具调用:并行执行,但每个 call 恰好一条
    // ToolResult,且结果按 model-order 提交(事件源不变量)。
    fn multi_tool_script() -> Vec<StreamChunk> {
        let mut chunks = vec![StreamChunk::BlockStart {
            index: 0,
            block_type: BlockType::ToolCall,
        }];
        for (i, id) in ["call_a", "call_b", "call_c"].iter().enumerate() {
            chunks.push(StreamChunk::ToolCallDelta {
                index: i as u32,
                id: id.to_string(),
                name: Some("echo".to_string()),
                arguments_delta: format!("{{\"text\":\"{id}\"}}"),
            });
            chunks.push(StreamChunk::BlockEnd {
                index: i as u32,
                block: ContentBlock::ToolCall {
                    id: id.to_string(),
                    name: "echo".to_string(),
                    arguments: format!("{{\"text\":\"{id}\"}}"),
                },
            });
        }
        chunks.push(StreamChunk::Finish {
            reason: FinishReason::ToolCalls,
        });
        chunks
    }
    let (driver, _registry) = driver(vec![
        MockScript::Chunks(multi_tool_script()),
        MockScript::Chunks(text_script("done")),
    ]);
    let session = temp_session();
    let reason = driver
        .run_turn(
            &session,
            &selection(),
            "parallel",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            CancellationToken::new(),
            noop_emit(),
        )
        .await;
    assert_eq!(reason, TurnEndReason::Completed);

    let calls: Vec<String> = session
        .events()
        .iter()
        .filter_map(|envelope| match &envelope.event {
            SessionEvent::ToolCall { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    let results: Vec<String> = session
        .events()
        .iter()
        .filter_map(|envelope| match &envelope.event {
            SessionEvent::ToolResult { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(calls, vec!["call_a", "call_b", "call_c"]);
    // 结果按 model-order 提交,每个 call 恰好一条。
    assert_eq!(results, vec!["call_a", "call_b", "call_c"]);
}

#[tokio::test]
async fn unknown_tool_yields_error_result_and_continues() {
    let mut unknown = tool_script();
    if let StreamChunk::BlockEnd { block, .. } = &mut unknown[2] {
        *block = ContentBlock::ToolCall {
            id: "call_x".to_string(),
            name: "nope".to_string(),
            arguments: "{}".to_string(),
        };
    }
    let (driver, _registry) = driver(vec![
        MockScript::Chunks(unknown),
        MockScript::Chunks(text_script("ok")),
    ]);
    let session = temp_session();
    let reason = driver
        .run_turn(
            &session,
            &selection(),
            "go",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            CancellationToken::new(),
            noop_emit(),
        )
        .await;
    assert_eq!(reason, TurnEndReason::Completed);
    let result = session
        .events()
        .iter()
        .find_map(|envelope| match &envelope.event {
            SessionEvent::ToolResult {
                content, is_error, ..
            } => Some((content.clone(), *is_error)),
            _ => None,
        });
    let (content, is_error) = result.unwrap();
    assert!(is_error);
    assert!(content.contains("unknown tool: nope"));
}

#[tokio::test]
async fn pending_stream_cancel_aborts() {
    struct PendingAdapter;
    #[async_trait]
    impl LlmAdapter for PendingAdapter {
        fn provider_info(&self, provider: &str) -> ProviderInfo {
            ProviderInfo {
                id: provider.to_string(),
                name: "pending".to_string(),
            }
        }
        async fn list_models(&self, _p: &str) -> Result<Vec<LlmModelInfo>, LlmError> {
            Ok(Vec::new())
        }
        async fn resolve_model(
            &self,
            p: &str,
            m: &str,
        ) -> Result<LlmResolvedModelInfo, LlmError> {
            Ok(LlmResolvedModelInfo {
                info: LlmModelInfo {
                    provider: p.to_string(),
                    id: m.to_string(),
                    name: m.to_string(),
                    description: None,
                    input_modalities: vec![],
                },
                context_window: None,
                default_max_tokens: None,
                reasoning: None,
            })
        }
        async fn stream(
            &self,
            _p: &str,
            _r: &GenerateRequest,
        ) -> Result<ChunkStream, LlmError> {
            Ok(Box::pin(futures::stream::pending()))
        }
    }
    let registry = Arc::new(LlmRegistry::new());
    registry
        .register(
            &["mock".to_string()],
            Arc::new(PendingAdapter),
            denia_llm::RetryPolicy::default(),
        )
        .unwrap();
    let mut prompt = SystemPrompt::new(denia_system_prompt::SystemPromptConfig {
        include_runtime_context: false,
        ..Default::default()
    });
    prompt
        .variable("cwd", |context| context.cwd.clone())
        .unwrap();
    prompt
        .variable("model", |context| context.model.clone())
        .unwrap();
    prompt
        .variable("provider", |context| context.provider.clone())
        .unwrap();
    let driver = SessionDriver::new(
        registry,
        Arc::new(ToolRegistry::default()),
        Arc::new(ArcSwap::from_pointee(prompt)),
    );
    let session = temp_session();
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel_clone.cancel();
    });
    let reason = driver
        .run_turn(
            &session,
            &selection(),
            "go",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            cancel,
            noop_emit(),
        )
        .await;
    assert_eq!(
        reason,
        TurnEndReason::Aborted {
            cause: Some(AbortCause::User)
        }
    );
    let has_interrupted = session.events().iter().any(|envelope| {
        matches!(
            &envelope.event,
            SessionEvent::AssistantMessage {
                interrupted: true,
                ..
            }
        )
    });
    assert!(has_interrupted);
}

#[tokio::test]
async fn text_rendered_call_is_rescued_and_executed() {
    // 模型输出文本标签的 bash 调用:harness 代为解析执行,工具结果
    // 回给模型,下一步模型基于真实结果继续——qwen3.8-flash 这类无
    // 原生 FC 能力的模型也能真正干活。
    let fake = "我先看一下\n\n<tool_call>\n<function=echo>\n<parameter=text>\nhello\n</parameter>\n</function>\n</tool_call>";
    let (driver, _registry) = driver(vec![
        MockScript::Chunks(text_script(fake)),
        MockScript::Chunks(text_script("done")),
    ]);
    let session = temp_session();
    let reason = driver
        .run_turn(
            &session,
            &selection(),
            "分析项目",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            CancellationToken::new(),
            noop_emit(),
        )
        .await;
    assert_eq!(reason, TurnEndReason::Completed);
    // 工具被执行,且结果进了派生历史(模型下一步看得到)。
    let result = session.events().iter().find_map(|e| match &e.event {
        SessionEvent::ToolResult {
            content, is_error, ..
        } => Some((content.clone(), *is_error)),
        _ => None,
    });
    let (content, is_error) = result.expect("rescued call must be dispatched");
    assert!(!is_error);
    assert!(content.contains("hello"));
    let messages = session.derive_messages();
    assert!(
        messages
            .iter()
            .any(|m| m.role == denia_core::message::ChatRole::Tool)
    );
    // 自纠注入未发生(救援成功,无需反馈)。
    let injected = session
        .events()
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::UserMessage {
                text,
                injected: true,
                ..
            } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        injected.is_empty(),
        "no feedback needed when rescue succeeded"
    );
}

#[tokio::test]
async fn rescued_unknown_tool_yields_error_result() {
    // 救援不校验工具存在性:未知工具照常走 dispatch,is_error 结果
    // 回给模型自纠(与原生 tool_calls 的行为一致)。
    let fake = "<tool_call>\n<function=nope>\n<parameter=text>\nx\n</parameter>\n</function>\n</tool_call>";
    let (driver, _registry) = driver(vec![
        MockScript::Chunks(text_script(fake)),
        MockScript::Chunks(text_script("ok")),
    ]);
    let session = temp_session();
    let reason = driver
        .run_turn(
            &session,
            &selection(),
            "go",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            CancellationToken::new(),
            noop_emit(),
        )
        .await;
    assert_eq!(reason, TurnEndReason::Completed);
    let result = session.events().iter().find_map(|e| match &e.event {
        SessionEvent::ToolResult {
            content, is_error, ..
        } => Some((content.clone(), *is_error)),
        _ => None,
    });
    let (content, is_error) = result.unwrap();
    assert!(is_error);
    assert!(content.contains("unknown tool: nope"));
}

#[tokio::test]
async fn text_fake_tool_call_is_fed_back_and_recovered() {
    // 伪调用格式烂到无法救援(参数块未闭合)→ 注入自纠,重启一步后
    // 模型改用原生 tool_calls,工具真正被执行。
    let fake = "我先看一下\n\n<tool_call>\n<function=echo>\n<parameter=text>\nhi\n</tool_call>";
    let (driver, _registry) = driver(vec![
        MockScript::Chunks(text_script(fake)),
        MockScript::Chunks(tool_script()),
        MockScript::Chunks(text_script("done")),
    ]);
    let session = temp_session();
    let reason = driver
        .run_turn(
            &session,
            &selection(),
            "优化模型选择框",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            CancellationToken::new(),
            noop_emit(),
        )
        .await;
    assert_eq!(reason, TurnEndReason::Completed);
    // 注入的纠错提示恰一条,且点名了伪调用函数。
    let injected = session
        .events()
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::UserMessage {
                text,
                injected: true,
                ..
            } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(injected.len(), 1, "one self-correction injection expected");
    assert!(injected[0].contains("原生 tool_calls"));
    assert!(injected[0].contains("echo()"));
    // 模型随后真的发了原生调用,工具被执行。
    let has_call = session
        .events()
        .iter()
        .any(|e| matches!(&e.event, SessionEvent::ToolCall { name, .. } if name == "echo"));
    assert!(
        has_call,
        "recovered step must dispatch the native tool call"
    );
}

#[tokio::test]
async fn fake_tool_call_quota_exhausts_then_completes() {
    // 模型连续输出无法救援的伪调用(参数块未闭合):注入 2 次自纠后配额
    // 耗尽,第 3 条相同文本以 Completed 收尾(死循环线只累计到 3 次,未到
    // "三次以上"的拦截阈值,不触发)。
    let fake = "<tool_call><function=bash><parameter=command>ls</tool_call>";
    let (driver, _registry) = driver(vec![
        MockScript::Chunks(text_script(fake)),
        MockScript::Chunks(text_script(fake)),
        MockScript::Chunks(text_script(fake)),
    ]);
    let session = temp_session();
    let reason = driver
        .run_turn(
            &session,
            &selection(),
            "go",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            CancellationToken::new(),
            noop_emit(),
        )
        .await;
    assert_eq!(reason, TurnEndReason::Completed);
    let injected = session
        .events()
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::UserMessage {
                text,
                injected: true,
                ..
            } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        injected.len(),
        MAX_FEEDBACK as usize,
        "feedback quota must cap at MAX_FEEDBACK"
    );
    let calls = session
        .events()
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::ToolCall { .. }))
        .count();
    assert_eq!(calls, 0, "no native tool call ever arrived");
    // 日志平衡:step 数与 step-end 数一致。
    let step_starts = session
        .events()
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::StepStart { .. }))
        .count();
    let step_ends = session
        .events()
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::StepEnd { .. }))
        .count();
    assert_eq!(step_starts, step_ends);
}

#[tokio::test]
async fn request_failure_is_fed_back_for_self_correction() {
    let (driver, _registry) = driver(vec![
        // MALFORMED_RESPONSE:feedback_eligible 且不在可重试集 → 注入自纠。
        MockScript::Fail(LlmFailure::new("MALFORMED_RESPONSE", "bad payload")),
        MockScript::Chunks(text_script("fixed")),
    ]);
    let session = temp_session();
    let reason = driver
        .run_turn(
            &session,
            &selection(),
            "go",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            CancellationToken::new(),
            noop_emit(),
        )
        .await;
    assert_eq!(reason, TurnEndReason::Completed);
    // 纠错提示以 injected 用户消息落日志,模型看得见。
    let injected = session
        .events()
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::UserMessage { text, injected, .. } if *injected => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(injected.len(), 1, "feedback-injected message must exist");
    assert!(injected[0].contains("MALFORMED_RESPONSE"));
}

#[tokio::test]
async fn provider_failure_terminates_without_feedback_waste() {
    // AUTH:不可重试、不可自纠 → 直接 error 终止,不注入反馈(不烧配额)。
    let (driver, _registry) =
        driver(vec![MockScript::Fail(LlmFailure::new("AUTH", "bad key"))]);
    let session = temp_session();
    let reason = driver
        .run_turn(
            &session,
            &selection(),
            "go",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            CancellationToken::new(),
            noop_emit(),
        )
        .await;
    match &reason {
        TurnEndReason::Error { failure } => {
            assert_eq!(failure.code, "AUTH");
            // 中文摘要:结论 + 建议 + 原始信息。
            assert!(failure.message.contains("认证失败"), "{}", failure.message);
            assert!(failure.message.contains("凭据"), "{}", failure.message);
            assert!(failure.message.contains("bad key"), "{}", failure.message);
        }
        other => panic!("expected Error, got {other:?}"),
    }
    let injected = session
        .events()
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::UserMessage { text, injected, .. } if *injected => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        injected.is_empty(),
        "AUTH must not waste the feedback quota"
    );
    // 日志平衡:step-end 与 turn-end 都已落盘。
    let step_starts = session
        .events()
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::StepStart { .. }))
        .count();
    let step_ends = session
        .events()
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::StepEnd { .. }))
        .count();
    assert_eq!(step_starts, step_ends);
}

#[tokio::test]
async fn retryable_setup_failure_retries_and_records_attempts() {
    // INVALID_REQUEST 在可重试集:registry 内部退避重试,重试轨迹落盘。
    let (driver, _registry) = driver(vec![
        MockScript::Fail(LlmFailure::new("INVALID_REQUEST", "openai_error").with_status(404)),
        MockScript::Fail(LlmFailure::new("INVALID_REQUEST", "openai_error").with_status(404)),
        MockScript::Chunks(text_script("ok")),
    ]);
    let session = temp_session();
    let reason = driver
        .run_turn(
            &session,
            &selection(),
            "go",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            CancellationToken::new(),
            noop_emit(),
        )
        .await;
    assert_eq!(reason, TurnEndReason::Completed);
    // 两次失败 → 两次 retry-attempt 落盘(带退避与错误信息)。
    let attempts = session
        .events()
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::RetryAttempt {
                attempt,
                code,
                delay_ms,
                ..
            } => Some((*attempt, code.clone(), *delay_ms)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].0, 1);
    assert_eq!(attempts[1].0, 2);
    assert!(
        attempts
            .iter()
            .all(|(_, code, _)| code == "INVALID_REQUEST")
    );
    assert!(
        attempts[0].2 >= 400 && attempts[1].2 >= 800,
        "backoff must grow"
    );
}

#[tokio::test]
async fn finish_error_is_not_swallowed_as_completed() {
    // finish 带 Error{…}(如 EMPTY_RESPONSE):不再被吞成 Completed;
    // 可重试码空流重试一次后成功(step 内 attempt 循环)。
    let (driver, _registry) = driver(vec![
        MockScript::Chunks(error_finish_script()),
        MockScript::Chunks(text_script("recovered")),
    ]);
    let session = temp_session();
    let reason = driver
        .run_turn(
            &session,
            &selection(),
            "go",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            CancellationToken::new(),
            noop_emit(),
        )
        .await;
    assert_eq!(reason, TurnEndReason::Completed);
    // step 内重试轨迹:1 条 retry-attempt(EMPTY_RESPONSE)。
    let attempts = session
        .events()
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::RetryAttempt { code, .. } => Some(code.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(attempts, vec!["EMPTY_RESPONSE".to_string()]);
}

#[tokio::test]
async fn stream_closed_with_partial_output_retries_and_recovers() {
    // 网关在 [DONE] 前断流(已有部分输出):step 内重试一次后成功。
    // 曾限制"仅无输出时重试",导致收尾步被断流白白掐死——现在有输出
    // 也重放(重放无副作用:半成品 chunk 不进派生历史)。
    let (driver, _registry) = driver(vec![
        MockScript::ChunksThenFail(
            vec![
                StreamChunk::BlockStart {
                    index: 0,
                    block_type: BlockType::Text,
                },
                StreamChunk::TextDelta {
                    index: 0,
                    text: "partial".to_string(),
                },
            ],
            LlmFailure::new(
                denia_core::error::codes::STREAM_CLOSED,
                "stream ended before the [DONE] marker",
            ),
        ),
        MockScript::Chunks(text_script("recovered")),
    ]);
    let session = temp_session();
    let reason = driver
        .run_turn(
            &session,
            &selection(),
            "go",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            CancellationToken::new(),
            noop_emit(),
        )
        .await;
    assert_eq!(reason, TurnEndReason::Completed);
    // 重试轨迹:1 条 STREAM_CLOSED。
    let attempts = session
        .events()
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::RetryAttempt { code, .. } => Some(code.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(attempts, vec!["STREAM_CLOSED".to_string()]);
    // 最终 assistant-message 的 source_event_seqs 只引用第二次尝试的 chunk
    // (partial 尝试的 chunk 不在序列内)——重放未污染派生历史。
    let final_msg = session.events().iter().find_map(|e| match &e.event {
        SessionEvent::AssistantMessage {
            interrupted,
            source_event_seqs,
            ..
        } if !interrupted => Some(source_event_seqs.clone()),
        _ => None,
    });
    let seqs = final_msg.expect("final assistant-message must exist");
    assert_eq!(
        seqs.len(),
        5,
        "source seqs must cover only the recovered attempt"
    );
}

#[tokio::test]
async fn request_header_and_context_are_logged() {
    let (driver, _registry) = driver(vec![MockScript::Chunks(text_script("hi"))]);
    let session = temp_session();
    let reason = driver
        .run_turn(
            &session,
            &selection(),
            "go",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            CancellationToken::new(),
            noop_emit(),
        )
        .await;
    assert_eq!(reason, TurnEndReason::Completed);

    let events = session.events();
    // request-header:initial 快照,带 provider/model/system/tools。
    let (snapshot, header_reason) = events
        .iter()
        .find_map(|envelope| match &envelope.event {
            SessionEvent::RequestHeader { header, reason, .. } => Some((header, reason)),
            _ => None,
        })
        .expect("request-header must be logged on the first request");
    assert_eq!(*header_reason, RequestHeaderReason::Initial);
    assert_eq!(snapshot.config.provider, "mock");
    assert_eq!(snapshot.config.model, "mock-1");
    assert!(
        snapshot
            .system
            .as_deref()
            .unwrap_or("")
            .contains("你是由 denia 驱动的")
    );
    assert!(!snapshot.tools.is_empty());
    // 注册路由后只写一次:同一 step 循环的下一请求不会重复落盘(snapshot 相同)。
    let header_count = events
        .iter()
        .filter(|envelope| matches!(envelope.event, SessionEvent::RequestHeader { .. }))
        .count();
    assert_eq!(header_count, 1);

    // request-context:provider/model/context_window(来自 resolve_call)。
    events
        .iter()
        .find_map(|envelope| match &envelope.event {
            SessionEvent::RequestContext {
                provider,
                model,
                context_window,
                ..
            } => Some((provider, model, context_window)),
            _ => None,
        })
        .map(|(provider, model, window)| {
            assert_eq!(provider, "mock");
            assert_eq!(model, "mock-1");
            assert_eq!(*window, Some(100_000));
        })
        .expect("request-context must be logged");

    // assistant-message 的 source_event_seqs 引用全部 chunk seq(5 个)。
    let seqs = events
        .iter()
        .find_map(|envelope| match &envelope.event {
            SessionEvent::AssistantMessage {
                source_event_seqs, ..
            } => Some(source_event_seqs),
            _ => None,
        })
        .expect("assistant-message must exist");
    assert_eq!(seqs.len(), 5);
    // 引用的 seq 都是 chunk 事件(顺带验证方向正确)。
    let chunk_seqs = events
        .iter()
        .filter(|envelope| matches!(envelope.event, SessionEvent::AssistantChunk { .. }))
        .map(|envelope| envelope.seq)
        .collect::<Vec<_>>();
    assert_eq!(chunk_seqs, *seqs);
}

// —— 死循环检测(LoopGuard 端到端)——

/// 文本相同但工具调用不同:文本线在第 4 次触发(与工具线独立)。
fn mixed_script(text: &str, call_id: &str, arg: &str) -> Vec<StreamChunk> {
    vec![
        StreamChunk::BlockStart { index: 0, block_type: BlockType::Text },
        StreamChunk::TextDelta { index: 0, text: text.to_string() },
        StreamChunk::BlockEnd { index: 0, block: ContentBlock::Text { text: text.to_string() } },
        StreamChunk::BlockStart { index: 1, block_type: BlockType::ToolCall },
        StreamChunk::ToolCallDelta {
            index: 1,
            id: call_id.to_string(),
            name: Some("echo".to_string()),
            arguments_delta: format!(r#"{{"text":"{arg}"}}"#),
        },
        StreamChunk::BlockEnd {
            index: 1,
            block: ContentBlock::ToolCall {
                id: call_id.to_string(),
                name: "echo".to_string(),
                arguments: format!(r#"{{"text":"{arg}"}}"#),
            },
        },
        StreamChunk::Finish { reason: FinishReason::ToolCalls },
    ]
}

#[tokio::test]
async fn identical_text_fourth_time_forces_loop_abort() {
    // 模型连续输出完全相同的文本(工具调用不同,工具线不计):前 3 次
    // 都正常处理,第 4 次出现才触发死循环保护("三次以上"语义)。
    let (driver, _registry) = driver(vec![
        MockScript::Chunks(mixed_script("我在重复", "c1", "a")),
        MockScript::Chunks(mixed_script("我在重复", "c2", "b")),
        MockScript::Chunks(mixed_script("我在重复", "c3", "c")),
        MockScript::Chunks(mixed_script("我在重复", "c4", "d")),
    ]);
    let session = temp_session();
    let reason = driver
        .run_turn(
            &session,
            &selection(),
            "go",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            CancellationToken::new(),
            noop_emit(),
        )
        .await;
    assert_eq!(reason, TurnEndReason::LoopDetected { repeats: 4 });
    // 第 4 条重复消息已落盘。
    let assistant_count = session
        .events()
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::AssistantMessage { .. }))
        .count();
    assert_eq!(assistant_count, 4);
    // 前 3 条的工具调用都被完整执行(放行语义)。
    let executed = session
        .events()
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::ToolCall { .. }))
        .count();
    assert_eq!(executed, 3, "the first three identical calls must execute");
    // turn-end 事件带 loop-detected 标记(前端据此渲染提示)。
    let turn_end = session
        .events()
        .iter()
        .find_map(|e| match &e.event {
            SessionEvent::TurnEnd { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .expect("turn-end must exist");
    assert_eq!(turn_end, TurnEndReason::LoopDetected { repeats: 4 });
    // 日志平衡。
    let step_starts = session
        .events()
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::StepStart { .. }))
        .count();
    let step_ends = session
        .events()
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::StepEnd { .. }))
        .count();
    assert_eq!(step_starts, step_ends);
}

#[tokio::test]
async fn identical_tool_calls_fourth_time_forces_loop_abort() {
    // 模型连续发起完全相同的工具调用(即使文本不同):前 3 次全部执行,
    // 第 4 次出现才拦截("三次以上"语义)。
    let (driver, _registry) = driver(vec![
        MockScript::Chunks(tool_script()),
        MockScript::Chunks(tool_script()),
        MockScript::Chunks(tool_script()),
        MockScript::Chunks(tool_script()),
    ]);
    let session = temp_session();
    let reason = driver
        .run_turn(
            &session,
            &selection(),
            "go",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            CancellationToken::new(),
            noop_emit(),
        )
        .await;
    assert_eq!(reason, TurnEndReason::LoopDetected { repeats: 4 });
    // 前 3 次的工具调用全部执行(第 4 条的调用不再执行)。
    let executed = session
        .events()
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::ToolCall { .. }))
        .count();
    assert_eq!(executed, 3, "the fourth identical call must not execute");
}

#[tokio::test]
async fn varying_output_does_not_trigger_loop_guard() {
    // 内容每轮都不同:正常完成,不误伤。
    let (driver, _registry) = driver(vec![
        MockScript::Chunks(text_script("第一")),
        MockScript::Chunks(text_script("第二")),
        MockScript::Chunks(text_script("第三")),
    ]);
    let session = temp_session();
    let reason = driver
        .run_turn(
            &session,
            &selection(),
            "go",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            CancellationToken::new(),
            noop_emit(),
        )
        .await;
    assert_eq!(reason, TurnEndReason::Completed);
}

#[tokio::test]
async fn identical_text_streak_resets_after_variation() {
    // 两次相同 → 换内容 → 再两次相同:不触发(连续计数被重置)。
    let (driver, _registry) = driver(vec![
        MockScript::Chunks(text_script("同")),
        MockScript::Chunks(text_script("同")),
        MockScript::Chunks(text_script("不同")),
        MockScript::Chunks(text_script("同")),
        MockScript::Chunks(text_script("同")),
        MockScript::Chunks(text_script("收尾")),
    ]);
    let session = temp_session();
    let reason = driver
        .run_turn(
            &session,
            &selection(),
            "go",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            CancellationToken::new(),
            noop_emit(),
        )
        .await;
    assert_eq!(reason, TurnEndReason::Completed);
}

// —— 输出预算兜底截断 ——

#[tokio::test]
async fn oversized_tool_result_is_truncated_with_notice() {
    // 工具输出超过统一预算:落盘内容被截断并附中文提示,截断事实进事件。
    let (driver, _registry) = driver_with_tools(
        vec![
            MockScript::Chunks(tool_script_with("bulky", "call_big")),
            MockScript::Chunks(text_script("done")),
        ],
        |tools| {
            tools.register(Arc::new(EchoTool));
            tools.register(Arc::new(BulkyTool));
        },
    );
    let session = temp_session();
    let reason = driver
        .run_turn(
            &session,
            &selection(),
            "go",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            CancellationToken::new(),
            noop_emit(),
        )
        .await;
    assert_eq!(reason, TurnEndReason::Completed);
    let result = session
        .events()
        .iter()
        .find_map(|e| match &e.event {
            SessionEvent::ToolResult {
                content, truncation, ..
            } => Some((content.clone(), truncation.clone())),
            _ => None,
        })
        .expect("tool result must exist");
    let (content, truncation) = result;
    assert!(
        content.contains("输出已截断"),
        "truncation notice must be model-visible"
    );
    assert!(
        content.contains(&format!(
            "共 {} 字符",
            denia_tools::support::OUTPUT_BUDGET_CHARS + 500
        )),
        "{}",
        &content[content.len() - 300..]
    );
    let truncation = truncation.expect("truncation info must be recorded");
    assert_eq!(
        truncation.total_chars,
        (denia_tools::support::OUTPUT_BUDGET_CHARS + 500) as u64
    );
    assert_eq!(truncation.shown_chars, denia_tools::support::OUTPUT_BUDGET_CHARS as u64);
}

fn tool_script_with(name: &str, id: &str) -> Vec<StreamChunk> {
    vec![
        StreamChunk::BlockStart {
            index: 0,
            block_type: BlockType::ToolCall,
        },
        StreamChunk::ToolCallDelta {
            index: 0,
            id: id.to_string(),
            name: Some(name.to_string()),
            arguments_delta: "{}".to_string(),
        },
        StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: "{}".to_string(),
            },
        },
        StreamChunk::Finish {
            reason: FinishReason::ToolCalls,
        },
    ]
}

// —— 注入通道(channel 字段)——

#[tokio::test]
async fn feedback_injection_carries_channel() {
    // 自纠反馈注入带显式通道名,前端可按通道弱化渲染。
    let (driver, _registry) = driver(vec![
        MockScript::Fail(LlmFailure::new("MALFORMED_RESPONSE", "bad payload")),
        MockScript::Chunks(text_script("fixed")),
    ]);
    let session = temp_session();
    driver
        .run_turn(
            &session,
            &selection(),
            "go",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            CancellationToken::new(),
            noop_emit(),
        )
        .await;
    let has_feedback_channel = session.events().iter().any(|e| match &e.event {
        SessionEvent::UserMessage {
            injected: true,
            channel,
            ..
        } => channel.as_deref() == Some("feedback"),
        _ => false,
    });
    assert!(has_feedback_channel, "feedback injection must carry channel");
}
