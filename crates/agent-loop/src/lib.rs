//! The session driver: turn/step loop over the llm registry and tools.
//!
//! A step is one model request plus the tools it called; a turn keeps
//! stepping while the assistant message carries tool-call blocks and closes
//! on a tool-call-free message. Every event is appended to the session log
//! first and only then mirrored to `emit` — the log is the ordering
//! authority. A turn whose appends fail mid-flight is left open; session
//! load closes it with a synthetic aborted `turn-end`.

use std::path::PathBuf;
use std::sync::Arc;

use dshrs_core::config::ModelSelection;
use dshrs_core::error::{LlmFailure, codes};
use dshrs_core::message::ToolCallRef;
use dshrs_core::session::{SessionEnvelope, SessionEvent, TurnEndReason};
use dshrs_core::stream::{ContentBlock, FinishReason, StreamChunk, TokenUsage};
use dshrs_llm::{GenerateRequest, LlmRegistry};
use dshrs_session::Session;
use dshrs_tools::shell;
use dshrs_tools::{ToolContext, ToolRegistry};
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

/// 常驻系统提示;工作目录/平台/日期上下文按会话注入(抄 dsh 的 context
/// 插件:环境事实进提示,模型不用猜)。中文优先,与控制台 zh-source 一致。
pub fn build_system_prompt(cwd: &str) -> String {
    let shell_note = shell::shell_system_prompt_note(&shell::shell_runtime());
    format!(
        "你是运行在 dsh-rs 里的编码 agent。\n\
         工作目录:{cwd}(该目录存在,相对路径以它为根)。\n\
         平台:{os} ({arch})。日期:{date}。\n\
         工具:bash、read_file、write_file。{shell_note}\n\
         规矩:不要猜文件路径;读取失败时先用 bash 列目录再重试;每步聚焦一件事;能回答时就停止调用工具。\n\
         始终使用简体中文回复,除非用户明确要求其他语言。",
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        date = today_string(),
    )
}

/// 请求失败时回注给模型的纠错提示(抄 dsh inject 上下文思路):
/// 不中断,让模型看见拒绝原因自己纠正;每轮最多 MAX_FEEDBACK 次防死循环。
const MAX_FEEDBACK: u32 = 2;

fn feedback_text(failure: &LlmFailure) -> String {
    format!(
        "[harness] 上一次模型请求被提供方拒绝({code}:{message})。         请检查并纠正上一条输出(尤其是工具参数格式)后继续,不要原样重复。",
        code = failure.code,
        message = failure.message,
    )
}

/// 公历日期 yyyy-mm-dd,不引 chrono:unix 秒 → civil 算法。
fn today_string() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// Drives user turns on one session at a time.
pub struct SessionDriver {
    registry: Arc<LlmRegistry>,
    tools: Arc<ToolRegistry>,
}

impl SessionDriver {
    pub fn new(registry: Arc<LlmRegistry>, tools: Arc<ToolRegistry>) -> Self {
        Self { registry, tools }
    }

    /// Runs one user turn to completion and returns the reason it ended.
    /// `max_steps` comes from the console settings (dsh-style validated
    /// config, not a hardcoded tunable).
    pub async fn run_turn(
        &self,
        session: &Session,
        selection: &ModelSelection,
        prompt: &str,
        max_steps: u32,
        cancel: CancellationToken,
        emit: &(dyn Fn(&SessionEnvelope) + Send + Sync),
    ) -> TurnEndReason {
        match self
            .run_turn_inner(session, selection, prompt, max_steps, cancel, emit)
            .await
        {
            Ok(reason) => reason,
            // Append or dispatch failure: the turn stays open in the log and
            // session load closes it with a synthetic aborted turn-end.
            Err(failure) => TurnEndReason::Error { failure },
        }
    }

    async fn run_turn_inner(
        &self,
        session: &Session,
        selection: &ModelSelection,
        prompt: &str,
        max_steps: u32,
        cancel: CancellationToken,
        emit: &(dyn Fn(&SessionEnvelope) + Send + Sync),
    ) -> Result<TurnEndReason, LlmFailure> {
        let turn = next_turn_number(session);
        append(session, emit, SessionEvent::UserMessage { text: prompt.to_string(), injected: false })?;
        append(session, emit, SessionEvent::TurnStart { turn })?;

        let mut step: u32 = 0;
        let mut feedback: u32 = 0;
        loop {
            step += 1;
            if step > max_steps {
                let reason = TurnEndReason::Error {
                    failure: LlmFailure::new(
                        codes::STEP_LIMIT,
                        format!("单轮超过 {} 步上限", max_steps),
                    ),
                };
                append(session, emit, SessionEvent::TurnEnd { turn, reason: reason.clone() })?;
                return Ok(reason);
            }
            append(session, emit, SessionEvent::StepStart { turn, step })?;

            let request = GenerateRequest {
                model: selection.model.clone(),
                reasoning_effort: selection.reasoning_effort.clone(),
                messages: session.derive_messages(),
                system: Some(build_system_prompt(&session.header().cwd)),
                tools: self.tools.schemas(),
                temperature: None,
                max_tokens: None,
                stop: Vec::new(),
            };
            let mut stream = match self.registry.stream(&selection.provider, &request).await {
                Ok(stream) => stream,
                Err(error) => {
                    let failure = error.failure.clone();
                    append(session, emit, SessionEvent::StepEnd { turn, step })?;
                    if feedback < MAX_FEEDBACK {
                        feedback += 1;
                        append(
                            session,
                            emit,
                            SessionEvent::UserMessage {
                                text: feedback_text(&failure),
                                injected: true,
                            },
                        )?;
                        continue;
                    }
                    let reason = TurnEndReason::Error { failure };
                    append(session, emit, SessionEvent::TurnEnd { turn, reason: reason.clone() })?;
                    return Ok(reason);
                }
            };

            let mut blocks: Vec<ContentBlock> = Vec::new();
            let mut usage: Option<TokenUsage> = None;
            let mut finish: Option<FinishReason> = None;
            let mut stream_error: Option<LlmFailure> = None;
            let mut interrupted = false;
            loop {
                let next = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        interrupted = true;
                        break;
                    }
                    item = stream.next() => item,
                };
                match next {
                    Some(Ok(chunk)) => {
                        append(
                            session,
                            emit,
                            SessionEvent::AssistantChunk {
                                turn,
                                step,
                                chunk: chunk.clone(),
                            },
                        )?;
                        match chunk {
                            StreamChunk::BlockEnd { block, .. } => blocks.push(block),
                            StreamChunk::Usage { usage: next_usage } => usage = Some(next_usage),
                            StreamChunk::Finish { reason } => finish = Some(reason),
                            _ => {}
                        }
                    }
                    Some(Err(failure)) => {
                        stream_error = Some(failure);
                        break;
                    }
                    None => break,
                }
            }

            if interrupted {
                append(
                    session,
                    emit,
                    SessionEvent::AssistantMessage {
                        turn,
                        step,
                        blocks,
                        usage,
                        interrupted: true,
                    },
                )?;
                append(session, emit, SessionEvent::StepEnd { turn, step })?;
                let reason = TurnEndReason::Aborted;
                append(session, emit, SessionEvent::TurnEnd { turn, reason })?;
                return Ok(TurnEndReason::Aborted);
            }
            if let Some(failure) = stream_error {
                append(
                    session,
                    emit,
                    SessionEvent::AssistantMessage {
                        turn,
                        step,
                        blocks,
                        usage,
                        interrupted: true,
                    },
                )?;
                append(session, emit, SessionEvent::StepEnd { turn, step })?;
                if feedback < MAX_FEEDBACK {
                    feedback += 1;
                    append(
                        session,
                        emit,
                        SessionEvent::UserMessage {
                            text: feedback_text(&failure),
                            injected: true,
                        },
                    )?;
                    continue;
                }
                let reason = TurnEndReason::Error { failure };
                append(session, emit, SessionEvent::TurnEnd { turn, reason: reason.clone() })?;
                return Ok(reason);
            }

            append(
                session,
                emit,
                SessionEvent::AssistantMessage {
                    turn,
                    step,
                    blocks: blocks.clone(),
                    usage,
                    interrupted: false,
                },
            )?;

            let calls: Vec<ToolCallRef> = blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolCall {
                        id,
                        name,
                        arguments,
                    } => Some(ToolCallRef {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    }),
                    _ => None,
                })
                .collect();
            let hit_max_tokens = matches!(finish, Some(FinishReason::MaxTokens));

            if calls.is_empty() || hit_max_tokens {
                append(session, emit, SessionEvent::StepEnd { turn, step })?;
                let reason = if hit_max_tokens {
                    TurnEndReason::MaxTokens
                } else {
                    TurnEndReason::Completed
                };
                append(session, emit, SessionEvent::TurnEnd { turn, reason: reason.clone() })?;
                return Ok(reason);
            }

            // Sequential dispatch; every logged call gets exactly one result,
            // including calls caught by a mid-dispatch abort.
            let cwd = PathBuf::from(session.header().cwd.clone());
            for call in &calls {
                append(
                    session,
                    emit,
                    SessionEvent::ToolCall {
                        turn,
                        step,
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    },
                )?;
                let output = if cancel.is_cancelled() {
                    dshrs_tools::ToolOutput {
                        content: "aborted before dispatch".to_string(),
                        is_error: true,
                    }
                } else if let Some(tool) = self.tools.get(&call.name) {
                    let context = ToolContext {
                        cwd: cwd.clone(),
                        cancel: cancel.child_token(),
                        confined: session.header().sandbox,
                    };
                    tool.execute(&call.arguments, &context).await
                } else {
                    dshrs_tools::ToolOutput {
                        content: format!("unknown tool: {}", call.name),
                        is_error: true,
                    }
                };
                append(
                    session,
                    emit,
                    SessionEvent::ToolResult {
                        turn,
                        step,
                        call_id: call.id.clone(),
                        content: output.content,
                        is_error: output.is_error,
                        error: None,
                    },
                )?;
            }
            append(session, emit, SessionEvent::StepEnd { turn, step })?;
        }
    }
}

fn next_turn_number(session: &Session) -> u32 {
    session
        .events()
        .iter()
        .filter_map(|envelope| match &envelope.event {
            SessionEvent::TurnStart { turn } => Some(*turn),
            _ => None,
        })
        .max()
        .map_or(1, |turn| turn + 1)
}

fn append(
    session: &Session,
    emit: &(dyn Fn(&SessionEnvelope) + Send + Sync),
    event: SessionEvent,
) -> Result<(), LlmFailure> {
    let envelope = session
        .append(event)
        .map_err(|error| LlmFailure::new(codes::UNKNOWN, error.to_string()))?;
    emit(&envelope);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use dshrs_core::stream::{BlockType, FinishReason};
    use dshrs_core::tool::ToolSchema;
    use dshrs_core::error::LlmError;
    use dshrs_llm::{ChunkStream, LlmAdapter, LlmModelInfo, LlmResolvedModelInfo, ProviderInfo};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Scripted adapter: each `stream` call pops the next queued chunk run.
    enum MockScript {
        Chunks(Vec<StreamChunk>),
        Fail(LlmFailure),
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
                    Err(dshrs_core::error::LlmError::from_failure(failure))
                }
                Some(MockScript::Chunks(chunks)) => Ok(Box::pin(
                    futures::stream::iter(chunks.into_iter().map(Ok)),
                )),
                None => Ok(Box::pin(futures::stream::empty())),
            }
        }
    }

    struct EchoTool;

    #[async_trait]
    impl dshrs_tools::Tool for EchoTool {
        fn schema(&self) -> &ToolSchema {
            // Leaked once for the &'static contract of the test trait object.
            Box::leak(Box::new(ToolSchema {
                name: "echo".to_string(),
                description: "echoes".to_string(),
                parameters: serde_json::json!({ "type": "object" }),
            }))
        }

        async fn execute(&self, arguments: &str, _ctx: &ToolContext) -> dshrs_tools::ToolOutput {
            dshrs_tools::ToolOutput {
                content: format!("echo:{arguments}"),
                is_error: false,
            }
        }
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
        let registry = Arc::new(LlmRegistry::new());
        registry
            .register(
                &["mock".to_string()],
                Arc::new(MockAdapter {
                    scripts: Mutex::new(VecDeque::from(scripts)),
                }),
                dshrs_llm::RetryPolicy::default(),
            )
            .unwrap();
        let mut tools = ToolRegistry::default();
        tools.register(Arc::new(EchoTool));
        (
            SessionDriver::new(registry.clone(), Arc::new(tools)),
            registry,
        )
    }

    fn temp_session() -> Session {
        let dir = std::env::temp_dir().join(format!(
            "dshrs-loop-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Session::create(&dir, uuid::Uuid::new_v4().to_string(), &dir, true).unwrap()
    }

    fn selection() -> ModelSelection {
        ModelSelection {
            provider: "mock".to_string(),
            model: "mock-1".to_string(),
            reasoning_effort: None,
        }
    }

    fn noop_emit() -> Box<dyn Fn(&SessionEnvelope) + Send + Sync> {
        Box::new(|_| {})
    }

    #[tokio::test]
    async fn plain_text_turn_completes() {
        let (driver, _registry) = driver(vec![MockScript::Chunks(text_script("done!"))]);
        let session = temp_session();
        let reason = driver
            .run_turn(&session, &selection(), "hello", 25, CancellationToken::new(), &noop_emit())
            .await;
        assert_eq!(reason, TurnEndReason::Completed);

        let kinds: Vec<&str> = session
            .events()
            .iter()
            .map(|envelope| match &envelope.event {
                SessionEvent::UserMessage { .. } => "user",
                SessionEvent::TurnStart { .. } => "turn-start",
                SessionEvent::StepStart { .. } => "step-start",
                SessionEvent::AssistantMessage { .. } => "assistant",
                SessionEvent::StepEnd { .. } => "step-end",
                SessionEvent::TurnEnd { .. } => "turn-end",
                _ => "chunk",
            })
            .filter(|kind| *kind != "chunk")
            .collect();
        assert_eq!(
            kinds,
            vec!["user", "turn-start", "step-start", "assistant", "step-end", "turn-end"]
        );
        let messages = session.derive_messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].content, "done!");
    }

    #[tokio::test]
    async fn tool_call_continues_to_second_step() {
        let (driver, _registry) = driver(vec![MockScript::Chunks(tool_script()), MockScript::Chunks(text_script("after tool"))]);
        let session = temp_session();
        let reason = driver
            .run_turn(&session, &selection(), "use the tool", 25, CancellationToken::new(), &noop_emit())
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
        assert!(messages.iter().any(|m| m.role == dshrs_core::message::ChatRole::Tool));
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
        let (driver, _registry) = driver(vec![MockScript::Chunks(unknown), MockScript::Chunks(text_script("ok"))]);
        let session = temp_session();
        let reason = driver
            .run_turn(&session, &selection(), "go", 25, CancellationToken::new(), &noop_emit())
            .await;
        assert_eq!(reason, TurnEndReason::Completed);
        let result = session.events().iter().find_map(|envelope| match &envelope.event {
            SessionEvent::ToolResult { content, is_error, .. } => {
                Some((content.clone(), *is_error))
            }
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
            async fn resolve_model(&self, p: &str, m: &str) -> Result<LlmResolvedModelInfo, LlmError> {
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
            async fn stream(&self, _p: &str, _r: &GenerateRequest) -> Result<ChunkStream, LlmError> {
                Ok(Box::pin(futures::stream::pending()))
            }
        }
        let registry = Arc::new(LlmRegistry::new());
        registry
            .register(&["mock".to_string()], Arc::new(PendingAdapter), dshrs_llm::RetryPolicy::default())
            .unwrap();
        let driver = SessionDriver::new(registry, Arc::new(ToolRegistry::default()));
        let session = temp_session();
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel_clone.cancel();
        });
        let reason = driver
            .run_turn(&session, &selection(), "go", 25, cancel, &noop_emit())
            .await;
        assert_eq!(reason, TurnEndReason::Aborted);
        let has_interrupted = session.events().iter().any(|envelope| {
            matches!(
                &envelope.event,
                SessionEvent::AssistantMessage { interrupted: true, .. }
            )
        });
        assert!(has_interrupted);
    }

    #[tokio::test]
    async fn step_limit_ends_turn_with_error() {
        let scripts: Vec<Vec<StreamChunk>> = (0..5).map(|_| tool_script()).collect();
        let (driver, _registry) = driver(scripts.into_iter().map(MockScript::Chunks).collect());
        let session = temp_session();
        let reason = driver
            .run_turn(&session, &selection(), "loop", 2, CancellationToken::new(), &noop_emit())
            .await;
        match reason {
            TurnEndReason::Error { failure } => assert_eq!(failure.code, codes::STEP_LIMIT),
            other => panic!("expected step limit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_failure_is_fed_back_for_self_correction() {
        let (driver, _registry) = driver(vec![
            MockScript::Fail(LlmFailure::new("INVALID_REQUEST", "bad params")),
            MockScript::Chunks(text_script("fixed")),
        ]);
        let session = temp_session();
        let reason = driver
            .run_turn(&session, &selection(), "go", 25, CancellationToken::new(), &noop_emit())
            .await;
        assert_eq!(reason, TurnEndReason::Completed);
        // 纠错提示以 injected 用户消息落日志,模型看得见。
        let injected = session.events().iter().filter_map(|e| match &e.event {
            SessionEvent::UserMessage { text, injected } if *injected => Some(text.clone()),
            _ => None,
        }).collect::<Vec<_>>();
        assert_eq!(injected.len(), 1);
        assert!(injected[0].contains("INVALID_REQUEST"));
    }
}
