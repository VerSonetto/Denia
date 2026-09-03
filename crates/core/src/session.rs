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
    /// 分支来源:本会话由哪个父会话 fork 而来(dsh parentSession 血缘)。
    /// 旧日志/普通会话无此字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
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

/// 会话当前权限模式(抄 dsh sandbox-mode + permission-preset)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl PermissionMode {
    /// 是否允许在任意路径写文件(权限最高档)。
    pub fn is_full(self) -> bool {
        matches!(self, Self::DangerFullAccess)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

/// 会话审批策略:ask 遇到需要审批的操作时弹给用户;never 直接拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalPolicy {
    Ask,
    Never,
}

impl ApprovalPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Never => "never",
        }
    }
}

/// 一次审批请求的闭合结果(抄 dsh ApprovalOutcome)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalOutcome {
    AllowedOnce,
    Rejected,
    Cancelled,
    Unavailable,
}

/// 权限预设 → 审批策略的固定映射(抄 dsh base 预设表)。
pub fn approval_policy_for(mode: PermissionMode) -> ApprovalPolicy {
    match mode {
        PermissionMode::DangerFullAccess => ApprovalPolicy::Never,
        _ => ApprovalPolicy::Ask,
    }
}

/// One entry in the session's todo list — the unit of the `todo-write`
/// whole-list snapshot.
///
/// Deliberately minimal: a human-readable `content` line and a three-state
/// `status`. No id, priority, or ordering field — the list is replaced
/// wholesale on every write (last-write-wins), so entries need no identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    /// What the task is — a short imperative line shown in the UI.
    pub content: String,
    /// Lifecycle state; `in_progress` marks work being done right now.
    pub status: TodoStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
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
        /// 用户粘贴/上传的内联图片(仅 vision 模型;旧日志无此字段)。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<crate::message::ImageData>,
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
    /// Whole-list todo snapshot; latest write wins on replay. Log-only UI
    /// state — never part of the derived model history.
    TodoWrite { todos: Vec<TodoItem> },
    /// 会话权限模式切换(抄 dsh sandbox/mode):日志持久、可回放,
    /// 不进入模型历史;driver 读 fold 后的当前值做策略判断。
    PermissionMode { mode: PermissionMode },
    /// 会话审批策略切换(抄 dsh approval/policy);与权限模式一起由预设写。
    ApprovalPolicy { policy: ApprovalPolicy },
    /// 一次待审批的工具调用(抄 dsh approval/asked):driver 阻塞等待
    /// 用户通过 REST 决策,审批期间 UI 依据该事件弹窗。
    ApprovalAsked {
        request_id: String,
        call_id: String,
        tool: String,
        args_preview: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// 一次审批请求的闭合结果(抄 dsh approval/decided)。
    ApprovalDecided {
        request_id: String,
        outcome: ApprovalOutcome,
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

/// 会话分支(抄 dsh `session.fork` 的边界切割语义)在日志前缀上的纯函数:
/// 返回种子事件数量(切片右端,不含)。
///
/// - `at_seq` 给定且未越过日志末尾:锚定到第一个 `seq >= at_seq` 的
///   `turn-end`(锚点落在某个已完成轮次内,切点绝不回退到上一轮)。
/// - `at_seq` 未给或越过末尾:锚定到最后一个 `turn-end`。
/// - 找不到边界(尚无任何已完成轮次)→ `None`,调用方报 fork-unavailable。
///
/// 与 dsh 的差异:dsh 的 turn/end 后可能残留属于该轮的冻结节点(需要
/// 延伸到下一个 turn/start);本仓驱动器把下一轮的用户消息写在 turn-end
/// 与下一轮 turn-start 之间,那些事件属于下一轮,必须排除——切点严格
/// 落在 `turn-end` 之后。
pub fn fork_cut_index(events: &[SessionEnvelope], at_seq: Option<u64>) -> Option<usize> {
    let last_seq = events.last()?.seq;
    let boundary_idx = match at_seq {
        Some(anchor) if anchor <= last_seq => events.iter().position(|envelope| {
            envelope.seq >= anchor && matches!(envelope.event, SessionEvent::TurnEnd { .. })
        }),
        _ => events
            .iter()
            .rposition(|envelope| matches!(envelope.event, SessionEvent::TurnEnd { .. })),
    }?;
    Some(boundary_idx + 1)
}

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
            SessionEvent::UserMessage { text, images, .. } => {
                if images.is_empty() {
                    messages.push(ChatMessage::user(text));
                } else {
                    messages.push(ChatMessage::user_with_images(text, images.clone()));
                }
            }
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
            parent_session: None,
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
            envelope(2, SessionEvent::UserMessage { text: "hi".into(), injected: false, images: Vec::new() }),
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
            envelope(1, SessionEvent::UserMessage { text: "go".into(), injected: false, images: Vec::new() }),
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

    /// 两轮完整对话:1 turn-start, 2 user, 3 assistant, 4 turn-end,
    /// 5 turn-start, 6 user, 7 assistant, 8 turn-end。
    fn two_turn_log() -> Vec<SessionEnvelope> {
        let mut events = vec![
            envelope(1, SessionEvent::TurnStart { turn: 1 }),
            envelope(2, SessionEvent::UserMessage { text: "first".into(), injected: false, images: Vec::new() }),
            envelope(
                3,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 1,
                    blocks: vec![ContentBlock::Text { text: "hi".into() }],
                    usage: None,
                    interrupted: false,
                },
            ),
            envelope(4, SessionEvent::TurnEnd { turn: 1, reason: TurnEndReason::Completed }),
            envelope(5, SessionEvent::TurnStart { turn: 2 }),
            envelope(6, SessionEvent::UserMessage { text: "second".into(), injected: false, images: Vec::new() }),
            envelope(
                7,
                SessionEvent::AssistantMessage {
                    turn: 2,
                    step: 1,
                    blocks: vec![ContentBlock::Text { text: "done".into() }],
                    usage: None,
                    interrupted: false,
                },
            ),
            envelope(8, SessionEvent::TurnEnd { turn: 2, reason: TurnEndReason::Completed }),
        ];
        for (index, envelope) in events.iter_mut().enumerate() {
            envelope.seq = index as u64 + 1;
        }
        events
    }

    #[test]
    fn fork_cut_without_anchor_ends_on_last_completed_turn() {
        let events = two_turn_log();
        // 无锚点:切到最后一个 turn-end(含)= 8 条种子。
        assert_eq!(fork_cut_index(&events, None), Some(8));
    }

    #[test]
    fn fork_cut_anchor_clamps_forward_to_turn_end() {
        let events = two_turn_log();
        // 锚点落在第 1 轮中间:向前找到本轮 turn-end(seq 4),不回退。
        assert_eq!(fork_cut_index(&events, Some(2)), Some(4));
        assert_eq!(fork_cut_index(&events, Some(1)), Some(4));
        // 锚点正好是 turn-end 本身:切在本轮之后。
        assert_eq!(fork_cut_index(&events, Some(4)), Some(4));
        // 锚点指向下一轮的 turn-start:归入第 2 轮之后。
        assert_eq!(fork_cut_index(&events, Some(5)), Some(8));
    }

    #[test]
    fn fork_cut_anchor_beyond_end_falls_back_to_last_turn_end() {
        let events = two_turn_log();
        assert_eq!(fork_cut_index(&events, Some(99)), Some(8));
    }

    #[test]
    fn fork_cut_excludes_next_turn_prelude() {
        // 本仓布局:下一轮的用户消息写在 turn-end 与下一轮 turn-start 之间,
        // 属于下一轮,不进种子(切点严格落在 turn-end 之后)。
        let mut events = two_turn_log();
        events.insert(
            4,
            envelope(0, SessionEvent::UserMessage { text: "next turn prompt".into(), injected: false, images: Vec::new() }),
        );
        for (index, envelope) in events.iter_mut().enumerate() {
            envelope.seq = index as u64 + 1;
        }
        // 锚定第 1 轮:boundary seq 4 → 种子 4 条;下一轮预置消息(seq 5)被排除。
        assert_eq!(fork_cut_index(&events, Some(1)), Some(4));
        // 无锚点:切到最后一个 turn-end(seq 9 → index 8 → 9 条)。
        assert_eq!(fork_cut_index(&events, None), Some(9));
    }

    #[test]
    fn fork_cut_without_any_completed_turn_is_unavailable() {
        let events = vec![
            envelope(1, SessionEvent::TurnStart { turn: 1 }),
            envelope(2, SessionEvent::UserMessage { text: "hi".into(), injected: false, images: Vec::new() }),
        ];
        assert_eq!(fork_cut_index(&events, None), None);
        // 锚点在未完成轮次内且不越过末尾:无边界 → None(dsh fork-unavailable)。
        assert_eq!(fork_cut_index(&events, Some(2)), None);
        // 锚点越过末尾:回退到最后一个 turn-end,仍没有 → None。
        assert_eq!(fork_cut_index(&events, Some(99)), None);
        assert_eq!(fork_cut_index(&[], None), None);
    }
}
