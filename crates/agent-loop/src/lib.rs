//! The session driver: turn/step loop over the llm registry and tools.
//!
//! A step is one model request plus the tools it called; a turn keeps
//! stepping while the assistant message carries tool-call blocks and closes
//! on a tool-call-free message. Every event is appended to the session log
//! first and only then mirrored to `emit` — the log is the ordering
//! authority. A turn whose appends fail mid-flight is left open; session
//! load closes it with a synthetic aborted `turn-end`.

mod runtime_context;

use std::path::PathBuf;
use std::sync::Arc;

use denia_core::config::ModelSelection;
use denia_core::error::{LlmFailure, codes};
use denia_core::message::ToolCallRef;
use denia_core::session::{SessionEnvelope, SessionEvent, TurnEndReason};
use denia_core::stream::{ContentBlock, FinishReason, StreamChunk, TokenUsage};
use denia_llm::{GenerateRequest, LlmRegistry};
use denia_session::Session;
use denia_system_prompt::{
    AssembleContext, SystemPrompt, frame_system_prompt_for_model, render_prompt,
};
use denia_tools::{ToolContext, ToolRegistry};
use futures::StreamExt;
use runtime_context::RuntimeContextProjection;
use tokio_util::sync::CancellationToken;

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

/// Drives user turns on one session at a time.
pub struct SessionDriver {
    registry: Arc<LlmRegistry>,
    tools: Arc<ToolRegistry>,
    system_prompt: Arc<SystemPrompt>,
}

fn should_log_system_prompt(session: &Session, step: u32, text: &str) -> bool {
    if step == 1 {
        return true;
    }
    // O(1):Session 在 append 时维护最近一次系统提示词,不再遍历日志。
    session.last_system_prompt().as_deref() != Some(text)
}

impl SessionDriver {
    pub fn new(
        registry: Arc<LlmRegistry>,
        tools: Arc<ToolRegistry>,
        system_prompt: Arc<SystemPrompt>,
    ) -> Self {
        Self {
            registry,
            tools,
            system_prompt,
        }
    }

    /// 估算当前提示词的组成部分大小(字节):系统提示词(含模型框架)与
    /// 工具声明。供前端"上下文窗口占用"面板展示占比。
    pub fn prompt_parts(
        &self,
        cwd: &str,
        selection: &ModelSelection,
    ) -> Result<(usize, usize), String> {
        let assembly = self
            .system_prompt
            .assemble(&AssembleContext {
                cwd: Some(cwd.to_string()),
                model: Some(selection.model.clone()),
                provider: Some(selection.provider.clone()),
            })
            .map_err(|error| error.to_string())?;
        let system = render_prompt(&assembly);
        let framed = frame_system_prompt_for_model(&system);
        let tools = serde_json::to_string(&assembly.tools).map_err(|error| error.to_string())?;
        Ok((system.len() + framed.len(), tools.len()))
    }

    /// Runs one user turn to completion and returns the reason it ended.
    ///
    /// `session` is shared (`Arc`) because tools like `todo_write` append
    /// log-only events through a `'static` sink wired back to the same log.
    /// `images` are inline pasted images (vision models); `files` are paths of
    /// uploaded files, injected as a context message before the prompt.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_turn(
        &self,
        session: &Arc<Session>,
        selection: &ModelSelection,
        prompt: &str,
        images: Vec<denia_core::message::ImageData>,
        files: Vec<String>,
        // 当前模型是否标记为可识图(read_file 图片注入与图片发送的依据)。
        vision_supported: bool,
        cancel: CancellationToken,
        emit: Arc<dyn Fn(&SessionEnvelope) + Send + Sync>,
    ) -> TurnEndReason {
        match self
            .run_turn_inner(session, selection, prompt, images, files, vision_supported, cancel, emit)
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
        session: &Arc<Session>,
        selection: &ModelSelection,
        prompt: &str,
        images: Vec<denia_core::message::ImageData>,
        files: Vec<String>,
        vision_supported: bool,
        cancel: CancellationToken,
        emit: Arc<dyn Fn(&SessionEnvelope) + Send + Sync>,
    ) -> Result<TurnEndReason, LlmFailure> {
        let turn = session.next_turn_number();
        if !files.is_empty() {
            let list = files
                .iter()
                .map(|path| format!("- {path}"))
                .collect::<Vec<_>>()
                .join("\n");
            append(
                session,
                &emit,
                SessionEvent::UserMessage {
                    text: format!("[harness] 用户上传了文件:\n{list}\n这些文件已保存,可随时用工具读取。"),
                    injected: true,
                    images: Vec::new(),
                },
            )?;
        }
        append(
            session,
            &emit,
            SessionEvent::UserMessage {
                text: prompt.to_string(),
                injected: false,
                images,
            },
        )?;
        append(session, &emit, SessionEvent::TurnStart { turn })?;

        let mut step: u32 = 0;
        let mut feedback: u32 = 0;
        let mut runtime_projection = RuntimeContextProjection::restore(session);
        loop {
            step += 1;
            append(session, &emit, SessionEvent::StepStart { turn, step })?;

            let cwd = session.header().cwd.clone();
            let assembly = self
                .system_prompt
                .assemble(&AssembleContext {
                    cwd: Some(cwd.clone()),
                    model: Some(selection.model.clone()),
                    provider: Some(selection.provider.clone()),
                })
                .map_err(|error| LlmFailure::new(codes::UNKNOWN, error))?;

            if let Some(snapshot) = runtime_projection.project(&assembly) {
                append(
                    session,
                    &emit,
                    SessionEvent::UserMessage { text: snapshot, injected: true, images: Vec::new() },
                )?;
            }

            let prompt_body = render_prompt(&assembly);
            if should_log_system_prompt(session, step, &prompt_body) {
                append(
                    session,
                    &emit,
                    SessionEvent::SystemPrompt {
                        turn,
                        step,
                        text: prompt_body.clone(),
                    },
                )?;
            }

            let request = GenerateRequest {
                model: selection.model.clone(),
                reasoning_effort: selection.reasoning_effort.clone(),
                messages: session.derive_messages(),
                system: Some(frame_system_prompt_for_model(&prompt_body)),
                tools: assembly.tools,
                temperature: None,
                max_tokens: None,
                stop: Vec::new(),
            };
            let mut stream = match self.registry.stream(&selection.provider, &request).await {
                Ok(stream) => stream,
                Err(error) => {
                    let failure = error.failure.clone();
                    append(session, &emit, SessionEvent::StepEnd { turn, step })?;
                    if feedback < MAX_FEEDBACK {
                        feedback += 1;
                        append(
                            session,
                            &emit,
                            SessionEvent::UserMessage { text: feedback_text(&failure), injected: true, images: Vec::new() },
                        )?;
                        continue;
                    }
                    let reason = TurnEndReason::Error { failure };
                    append(session, &emit, SessionEvent::TurnEnd { turn, reason: reason.clone() })?;
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
                            &emit,
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
                    &emit,
                    SessionEvent::AssistantMessage {
                        turn,
                        step,
                        blocks,
                        usage,
                        interrupted: true,
                    },
                )?;
                append(session, &emit, SessionEvent::StepEnd { turn, step })?;
                let reason = TurnEndReason::Aborted;
                append(session, &emit, SessionEvent::TurnEnd { turn, reason })?;
                return Ok(TurnEndReason::Aborted);
            }
            if let Some(failure) = stream_error {
                append(
                    session,
                    &emit,
                    SessionEvent::AssistantMessage {
                        turn,
                        step,
                        blocks,
                        usage,
                        interrupted: true,
                    },
                )?;
                append(session, &emit, SessionEvent::StepEnd { turn, step })?;
                if feedback < MAX_FEEDBACK {
                    feedback += 1;
                    append(
                        session,
                        &emit,
                        SessionEvent::UserMessage { text: feedback_text(&failure), injected: true, images: Vec::new() },
                    )?;
                    continue;
                }
                let reason = TurnEndReason::Error { failure };
                append(session, &emit, SessionEvent::TurnEnd { turn, reason: reason.clone() })?;
                return Ok(reason);
            }

            append(
                session,
                &emit,
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
                append(session, &emit, SessionEvent::StepEnd { turn, step })?;
                let reason = if hit_max_tokens {
                    TurnEndReason::MaxTokens
                } else {
                    TurnEndReason::Completed
                };
                append(session, &emit, SessionEvent::TurnEnd { turn, reason: reason.clone() })?;
                return Ok(reason);
            }

            // Sequential dispatch; every logged call gets exactly one result,
            // including calls caught by a mid-dispatch abort.
            let cwd = PathBuf::from(session.header().cwd.clone());
            for call in &calls {
                append(
                    session,
                    &emit,
                    SessionEvent::ToolCall {
                        turn,
                        step,
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    },
                )?;
                let output = if cancel.is_cancelled() {
                    denia_tools::ToolOutput {
                        content: "aborted before dispatch".to_string(),
                        is_error: true,
                    }
                } else if let Some(tool) = self.tools.get(&call.name) {
                    // Log-only event sink for tools like todo_write: appends
                    // through the same session and echoes to SSE followers.
                    let sink_session = session.clone();
                    let sink_emit = emit.clone();
                    let context = ToolContext {
                        cwd: cwd.clone(),
                        cancel: cancel.child_token(),
                        confined: session.header().sandbox,
                        vision_supported,
                        emit_event: Some(Arc::new(move |event: SessionEvent| {
                            if let Ok(envelope) = sink_session.append(event) {
                                sink_emit(&envelope);
                            }
                        })),
                    };
                    tool.execute(&call.arguments, &context).await
                } else {
                    denia_tools::ToolOutput {
                        content: format!("unknown tool: {}", call.name),
                        is_error: true,
                    }
                };
                append(
                    session,
                    &emit,
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
            append(session, &emit, SessionEvent::StepEnd { turn, step })?;
        }
    }
}

fn append(
    session: &Session,
    emit: &Arc<dyn Fn(&SessionEnvelope) + Send + Sync>,
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

    #[test]
    fn system_prompt_frames_runtime_authority() {
        let (prompt, _) = denia_tools::default_shipped();
        let assembly = prompt
            .assemble(&denia_system_prompt::AssembleContext {
                cwd: Some("/tmp/ws".to_string()),
                model: Some("mock".to_string()),
                provider: Some("mock".to_string()),
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

    use async_trait::async_trait;
    use denia_core::stream::{BlockType, FinishReason};
    use denia_core::tool::ToolSchema;
    use denia_core::error::LlmError;
    use denia_llm::{ChunkStream, LlmAdapter, LlmModelInfo, LlmResolvedModelInfo, ProviderInfo};
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
                    Err(denia_core::error::LlmError::from_failure(failure))
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
    impl denia_tools::Tool for EchoTool {
        fn schema(&self) -> &ToolSchema {
            // Leaked once for the &'static contract of the test trait object.
            Box::leak(Box::new(ToolSchema {
                name: "echo".to_string(),
                description: "echoes".to_string(),
                parameters: serde_json::json!({ "type": "object" }),
            }))
        }

        async fn execute(&self, arguments: &str, _ctx: &ToolContext) -> denia_tools::ToolOutput {
            denia_tools::ToolOutput {
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
                denia_llm::RetryPolicy::default(),
            )
            .unwrap();
        let mut tools = ToolRegistry::default();
        tools.register(Arc::new(EchoTool));
        let mut prompt = SystemPrompt::new(denia_system_prompt::SystemPromptConfig {
            include_runtime_context: false,
            ..Default::default()
        });
        let schemas = tools.schemas();
        prompt.tools(move |_| denia_system_prompt::ToolProviderResult {
            schemas: schemas.clone(),
            known_names: None,
        });
        prompt.variable("cwd", |context| context.cwd.clone()).unwrap();
        prompt.variable("model", |context| context.model.clone()).unwrap();
        prompt.variable("provider", |context| context.provider.clone()).unwrap();
        (
            SessionDriver::new(
                registry.clone(),
                Arc::new(tools),
                Arc::new(prompt),
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
        Arc::new(Session::create(&dir, uuid::Uuid::new_v4().to_string(), &dir, true).unwrap())
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
            .run_turn(&session, &selection(), "hello", Vec::new(), Vec::new(), true, CancellationToken::new(), noop_emit())
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
        let (driver, _registry) = driver(vec![MockScript::Chunks(tool_script()), MockScript::Chunks(text_script("after tool"))]);
        let session = temp_session();
        let reason = driver
            .run_turn(&session, &selection(), "use the tool", Vec::new(), Vec::new(), true, CancellationToken::new(), noop_emit())
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
        assert!(messages.iter().any(|m| m.role == denia_core::message::ChatRole::Tool));
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
            .run_turn(&session, &selection(), "go", Vec::new(), Vec::new(), true, CancellationToken::new(), noop_emit())
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
            .register(&["mock".to_string()], Arc::new(PendingAdapter), denia_llm::RetryPolicy::default())
            .unwrap();
        let mut prompt = SystemPrompt::new(denia_system_prompt::SystemPromptConfig {
            include_runtime_context: false,
            ..Default::default()
        });
        prompt.variable("cwd", |context| context.cwd.clone()).unwrap();
        prompt.variable("model", |context| context.model.clone()).unwrap();
        prompt.variable("provider", |context| context.provider.clone()).unwrap();
        let driver = SessionDriver::new(
            registry,
            Arc::new(ToolRegistry::default()),
            Arc::new(prompt),
        );
        let session = temp_session();
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel_clone.cancel();
        });
        let reason = driver
            .run_turn(&session, &selection(), "go", Vec::new(), Vec::new(), true, cancel, noop_emit())
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
    async fn request_failure_is_fed_back_for_self_correction() {
        let (driver, _registry) = driver(vec![
            MockScript::Fail(LlmFailure::new("INVALID_REQUEST", "bad params")),
            MockScript::Chunks(text_script("fixed")),
        ]);
        let session = temp_session();
        let reason = driver
            .run_turn(&session, &selection(), "go", Vec::new(), Vec::new(), true, CancellationToken::new(), noop_emit())
            .await;
        assert_eq!(reason, TurnEndReason::Completed);
        // 纠错提示以 injected 用户消息落日志,模型看得见。
        let injected = session.events().iter().filter_map(|e| match &e.event {
            SessionEvent::UserMessage { text, injected, .. } if *injected => Some(text.clone()),
            _ => None,
        }).collect::<Vec<_>>();
        assert_eq!(injected.len(), 1);
        assert!(injected[0].contains("INVALID_REQUEST"));
    }
}
