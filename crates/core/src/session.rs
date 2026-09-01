//! The event-sourced session vocabulary and its model-history projection.
//!
//! The append-only log is the source of truth; [`derive_messages`] projects
//! the wire history from it. Envelopes serialize flat and internally tagged:
//! `{ "seq", "time", "type", ...payload }`.

use serde::{Deserialize, Serialize};

use crate::error::LlmFailure;
use crate::message::{ChatMessage, ToolCallRef};
use crate::stream::{ContentBlock, StreamChunk, TokenUsage};

/// On-disk and wire format version; pinned while unreleased.
pub const SESSION_FORMAT_VERSION: u32 = 0;

/// First JSONL line of every session file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionHeader {
    #[serde(rename = "type")]
    pub kind: SessionHeaderKind,
    pub version: u32,
    pub id: String,
    pub created_at: u64,
    pub cwd: String,
    /// Confines file tools to `cwd`; orthogonal to the directory choice.
    #[serde(default = "default_true")]
    pub sandbox: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionHeaderKind {
    Session,
}

/// Why one turn closed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TurnEndReason {
    Completed,
    Aborted,
    MaxTokens,
    Error { failure: LlmFailure },
}

/// The durable event vocabulary, internally tagged on `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum SessionEvent {
    TurnStart {
        turn: u32,
    },
    TurnEnd {
        turn: u32,
        reason: TurnEndReason,
    },
    StepStart {
        turn: u32,
        step: u32,
    },
    StepEnd {
        turn: u32,
        step: u32,
    },
    UserMessage {
        text: String,
        /// harness 注入的纠错/上下文消息,非用户手打;UI 弱化渲染。
        #[serde(default)]
        injected: bool,
    },
    /// 模型请求使用的系统提示词(用户可见副本,不含优先级框架)。
    SystemPrompt {
        turn: u32,
        step: u32,
        text: String,
    },
    AssistantChunk {
        turn: u32,
        step: u32,
        chunk: StreamChunk,
    },
    AssistantMessage {
        turn: u32,
        step: u32,
        blocks: Vec<ContentBlock>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        interrupted: bool,
    },
    ToolCall {
        turn: u32,
        step: u32,
        call_id: String,
        name: String,
        arguments: String,
    },
    ToolResult {
        turn: u32,
        step: u32,
        call_id: String,
        content: String,
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

/// One log entry: monotonic coordinates plus the event payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEnvelope {
    /// Contiguous, starts at 1.
    pub seq: u64,
    /// Unix epoch milliseconds.
    pub time: u64,
    #[serde(flatten)]
    pub event: SessionEvent,
}

/// Content of the synthetic result appended for tool calls the log never
/// answered (aborts, crashes) so the derived wire history stays valid.
pub const INTERRUPTED_TOOL_RESULT: &str = "[tool execution was interrupted]";

/// Projects the model-facing history from the log.
///
/// `user-message` becomes a user message; `assistant-message` becomes an
/// assistant message (skipped when it carries neither text nor tool calls);
/// `tool-result` becomes a tool-role message. Chunks and boundary events
/// project nothing. Tool calls left unanswered by the end of the log get a
/// synthetic interrupted result, in call order, after everything else.
pub fn derive_messages(events: &[SessionEnvelope]) -> Vec<ChatMessage> {
    let mut messages: Vec<ChatMessage> = Vec::new();
    let mut unanswered: Vec<String> = Vec::new();

    for envelope in events {
        match &envelope.event {
            SessionEvent::UserMessage { text, .. } => messages.push(ChatMessage::user(text)),
            SessionEvent::AssistantMessage { blocks, .. } => {
                let text: String = blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                let reasoning: String = blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Reasoning { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
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
                if text.is_empty() && calls.is_empty() {
                    continue;
                }
                for call in &calls {
                    unanswered.push(call.id.clone());
                }
                messages.push(ChatMessage::assistant(
                    text,
                    (!reasoning.is_empty()).then_some(reasoning),
                    calls,
                ));
            }
            SessionEvent::ToolResult {
                call_id, content, ..
            } => {
                unanswered.retain(|id| id != call_id);
                messages.push(ChatMessage::tool_result(call_id, content));
            }
            _ => {}
        }
    }

    for call_id in unanswered {
        messages.push(ChatMessage::tool_result(call_id, INTERRUPTED_TOOL_RESULT));
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::BlockType;

    fn envelope(seq: u64, event: SessionEvent) -> SessionEnvelope {
        SessionEnvelope {
            seq,
            time: 1_700_000_000_000 + seq,
            event,
        }
    }

    #[test]
    fn header_round_trips_flat_json() {
        let header = SessionHeader {
            kind: SessionHeaderKind::Session,
            version: SESSION_FORMAT_VERSION,
            id: "0f8c".to_string(),
            created_at: 1_700_000_000_000,
            cwd: "/tmp/work".to_string(),
            sandbox: true,
        };
        let json = serde_json::to_string(&header).unwrap();
        assert_eq!(
            json,
            r#"{"type":"session","version":0,"id":"0f8c","created_at":1700000000000,"cwd":"/tmp/work","sandbox":true}"#
        );
        assert_eq!(serde_json::from_str::<SessionHeader>(&json).unwrap(), header);
    }

    #[test]
    fn system_prompt_event_round_trips() {
        let env = envelope(
            3,
            SessionEvent::SystemPrompt {
                turn: 1,
                step: 1,
                text: "你是 agent".into(),
            },
        );
        assert_eq!(
            serde_json::to_string(&env).unwrap(),
            r#"{"seq":3,"time":1700000000003,"type":"system-prompt","turn":1,"step":1,"text":"你是 agent"}"#
        );
    }

    #[test]
    fn envelope_serializes_type_tagged_flat() {
        let env = envelope(1, SessionEvent::TurnStart { turn: 1 });
        assert_eq!(
            serde_json::to_string(&env).unwrap(),
            r#"{"seq":1,"time":1700000000001,"type":"turn-start","turn":1}"#
        );
        let chunk_env = envelope(
            2,
            SessionEvent::AssistantChunk {
                turn: 1,
                step: 1,
                chunk: StreamChunk::BlockStart {
                    index: 0,
                    block_type: BlockType::Text,
                },
            },
        );
        let json = serde_json::to_string(&chunk_env).unwrap();
        assert!(json.contains(r#""type":"assistant-chunk""#));
        assert!(json.contains(r#""chunk":{"type":"block-start""#));
        assert_eq!(
            serde_json::from_str::<SessionEnvelope>(&json).unwrap(),
            chunk_env
        );
    }

    #[test]
    fn tool_events_round_trip() {
        let call = envelope(
            3,
            SessionEvent::ToolCall {
                turn: 1,
                step: 1,
                call_id: "call_1".to_string(),
                name: "bash".to_string(),
                arguments: r#"{"command":"echo hi"}"#.to_string(),
            },
        );
        let json = serde_json::to_string(&call).unwrap();
        assert_eq!(
            serde_json::from_str::<SessionEnvelope>(&json).unwrap(),
            call
        );
    }

    #[test]
    fn derive_projects_user_assistant_and_tool() {
        let events = vec![
            envelope(1, SessionEvent::TurnStart { turn: 1 }),
            envelope(2, SessionEvent::UserMessage { text: "hi".into(), injected: false }),
            envelope(3, SessionEvent::StepStart { turn: 1, step: 1 }),
            envelope(
                4,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 1,
                    blocks: vec![
                        ContentBlock::Reasoning { text: "hmm".into() },
                        ContentBlock::Text { text: "running".into() },
                        ContentBlock::ToolCall {
                            id: "c1".into(),
                            name: "bash".into(),
                            arguments: "{}".into(),
                        },
                    ],
                    usage: None,
                    interrupted: false,
                },
            ),
            envelope(
                5,
                SessionEvent::ToolCall {
                    turn: 1,
                    step: 1,
                    call_id: "c1".into(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                },
            ),
            envelope(
                6,
                SessionEvent::ToolResult {
                    turn: 1,
                    step: 1,
                    call_id: "c1".into(),
                    content: "exit code: 0".into(),
                    is_error: false,
                    error: None,
                },
            ),
            envelope(
                7,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 2,
                    blocks: vec![ContentBlock::Text { text: "done".into() }],
                    usage: Some(TokenUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                        cache_read_tokens: None,
                        reasoning_tokens: None,
                    }),
                    interrupted: false,
                },
            ),
            envelope(
                8,
                SessionEvent::TurnEnd {
                    turn: 1,
                    reason: TurnEndReason::Completed,
                },
            ),
        ];
        let messages = derive_messages(&events);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0], ChatMessage::user("hi"));
        let assistant = &messages[1];
        assert_eq!(assistant.content, "running");
        assert_eq!(assistant.reasoning_content.as_deref(), Some("hmm"));
        assert_eq!(assistant.tool_calls.len(), 1);
        assert_eq!(
            messages[2],
            ChatMessage::tool_result("c1", "exit code: 0")
        );
        assert_eq!(messages[3].content, "done");
    }

    #[test]
    fn derive_skips_empty_assistant_and_synthesizes_missing_results() {
        let events = vec![
            envelope(1, SessionEvent::UserMessage { text: "go".into(), injected: false }),
            envelope(
                2,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 1,
                    blocks: vec![],
                    usage: Some(TokenUsage::default()),
                    interrupted: false,
                },
            ),
            envelope(
                3,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 2,
                    blocks: vec![ContentBlock::ToolCall {
                        id: "cX".into(),
                        name: "bash".into(),
                        arguments: "{}".into(),
                    }],
                    usage: None,
                    interrupted: true,
                },
            ),
        ];
        let messages = derive_messages(&events);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1].tool_calls[0].id, "cX");
        assert_eq!(
            messages[2],
            ChatMessage::tool_result("cX", INTERRUPTED_TOOL_RESULT)
        );
    }

    #[test]
    fn finish_reason_error_round_trips() {
        let reason = TurnEndReason::Error {
            failure: LlmFailure::new(crate::error::codes::STEP_LIMIT, "too many steps"),
        };
        let json = serde_json::to_string(&reason).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"error","failure":{"message":"too many steps","code":"STEP_LIMIT"}}"#
        );
    }
}
