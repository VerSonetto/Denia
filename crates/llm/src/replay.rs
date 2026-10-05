use crate::GenerateRequest;
use denia_core::error::LlmFailure;
use std::sync::Mutex;

pub const REASONING_REJECTED: &str = "HISTORY_REASONING_REJECTED";

pub(crate) fn explicit_context_overflow(message: &str) -> bool {
    [
        "context_length_exceeded",
        "maximum context length",
        "context window exceeded",
        "exceeds the context window",
        "exceeded the context window",
        "input is too long",
        "prompt is too long",
        "prompt too long",
        "too many input tokens",
    ]
    .iter()
    .any(|phrase| message.contains(phrase))
}

pub(crate) fn explicit_reasoning_rejection(failure: &LlmFailure) -> bool {
    if failure.code == REASONING_REJECTED {
        return true;
    }
    if !matches!(failure.status, Some(400 | 422)) {
        return false;
    }
    let message = failure.message.to_ascii_lowercase();
    if message.contains("reasoning_effort")
        || message.contains("budget_tokens")
        || message.contains("thinking.budget")
        || explicit_context_overflow(&message)
    {
        return false;
    }
    let rejection = [
        "unsupported",
        "not supported",
        "not allowed",
        "unexpected",
        "unknown",
        "unrecognized",
        "invalid",
        "must",
        "cannot",
        "not permitted",
        "required",
        "missing",
    ]
    .iter()
    .any(|phrase| message.contains(phrase));
    let history = message.contains("reasoning_content")
        || message.contains("encrypted_content")
        || message.contains("redacted_thinking")
        || (message.contains("thinking")
            && ["block", "signature", "messages", "content"]
                .iter()
                .any(|phrase| message.contains(phrase)))
        || (message.contains("reasoning")
            && ["item", "input[", "input.", "histor", "replay"]
                .iter()
                .any(|phrase| message.contains(phrase)));
    rejection && history
}

#[derive(Default)]
pub struct ReplayPolicy {
    state: Mutex<(String, bool)>,
}

impl ReplayPolicy {
    pub fn prepare(&self, route: &str, request: &GenerateRequest) -> GenerateRequest {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.0 != route {
            *state = (route.to_string(), false);
        }
        let mut request = request.clone();
        if state.1 {
            for message in &mut request.messages {
                message.reasoning_content = None;
                message.reasoning_replay.clear();
            }
            request.messages.retain(|message| {
                message.role != denia_core::message::ChatRole::Assistant
                    || !message.content.is_empty()
                    || !message.tool_calls.is_empty()
            });
        }
        request
    }

    pub fn downgrade(&self, route: &str, request: &GenerateRequest, failure: &LlmFailure) -> bool {
        if !explicit_reasoning_rejection(failure)
            || !request.messages.iter().any(|message| {
                message.reasoning_content.is_some() || !message.reasoning_replay.is_empty()
            })
        {
            return false;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.0 != route {
            *state = (route.to_string(), false);
        }
        if state.1 {
            return false;
        }
        state.1 = true;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::error::codes;
    #[test]
    fn classification_is_narrow() {
        for message in [
            "Unknown field reasoning_content in messages",
            "Invalid signature in thinking block",
            "reasoning item not allowed in input",
        ] {
            assert!(
                explicit_reasoning_rejection(
                    &LlmFailure::new(codes::INVALID_REQUEST, message).with_status(400)
                ),
                "{message}"
            );
        }
        for message in [
            "Invalid thinking.budget_tokens in content",
            "Invalid reasoning_effort",
            "max_tokens must be at least 1",
            "Invalid length",
            "reasoning is not supported",
            "maximum context length exceeded",
        ] {
            assert!(
                !explicit_reasoning_rejection(
                    &LlmFailure::new(codes::INVALID_REQUEST, message).with_status(400)
                ),
                "{message}"
            );
        }
        assert!(!explicit_reasoning_rejection(
            &LlmFailure::new(codes::AUTH, "Unknown field reasoning_content").with_status(401)
        ));
        assert!(!explicit_context_overflow(
            "max_tokens has an invalid length"
        ));
        assert!(explicit_context_overflow(
            "maximum context length is 1000 tokens"
        ));
    }

    #[test]
    fn downgrade_is_once_per_turn_and_route_without_mutating_history() {
        let policy = ReplayPolicy::default();
        let request = GenerateRequest {
            model: "any".into(),
            reasoning_effort: None,
            messages: vec![denia_core::message::ChatMessage::assistant(
                "answer",
                Some("reason".into()),
                vec![],
            )],
            system: None,
            tools: vec![],
            temperature: None,
            max_tokens: None,
            stop: vec![],
        };
        let failure = LlmFailure::new(
            codes::INVALID_REQUEST,
            "Unknown field reasoning_content in messages",
        )
        .with_status(400);
        assert!(policy.downgrade("route", &request, &failure));
        assert!(!policy.downgrade("route", &request, &failure));
        let prepared = policy.prepare("route", &request);
        assert!(prepared.messages[0].reasoning_content.is_none());
        assert_eq!(prepared.messages[0].content, "answer");
        assert_eq!(
            request.messages[0].reasoning_content.as_deref(),
            Some("reason")
        );
        assert!(
            policy.prepare("other-protocol", &request).messages[0]
                .reasoning_content
                .is_some()
        );
        assert!(
            ReplayPolicy::default().prepare("route", &request).messages[0]
                .reasoning_content
                .is_some()
        );
    }
}
