//! SSE stream driver: parses eventsource payloads off one HTTP response,
//! feeds the wire translator, and enforces an idle timeout between events.

use std::collections::VecDeque;
use std::time::Duration;

use dshrs_core::error::{LlmFailure, codes};
use dshrs_core::stream::StreamChunk;
use eventsource_stream::Eventsource;
use futures::StreamExt;

use crate::wire::{StreamTranslator, UsageStyle, WireChunk};

const DONE_MARKER: &str = "[DONE]";

struct SseState {
    events: std::pin::Pin<
        Box<dyn futures::Stream<Item = Result<eventsource_stream::Event, eventsource_stream::EventStreamError<reqwest::Error>>> + Send>,
    >,
    translator: StreamTranslator,
    buffer: VecDeque<StreamChunk>,
    style: UsageStyle,
    idle_timeout: Duration,
    saw_done: bool,
    finished: bool,
}

/// Drives one SSE response into the chunk protocol. The stream terminates
/// after the translated `[DONE]` buffer drains, or with a terminal error
/// item on transport, parse, or idle-timeout failures.
pub fn sse_chunk_stream(
    response: reqwest::Response,
    style: UsageStyle,
    idle_timeout: Duration,
) -> crate::ChunkStream {
    let state = SseState {
        events: Box::pin(response.bytes_stream().eventsource()),
        translator: StreamTranslator::default(),
        buffer: VecDeque::new(),
        style,
        idle_timeout,
        saw_done: false,
        finished: false,
    };
    Box::pin(futures::stream::unfold(state, step))
}

async fn step(mut state: SseState) -> Option<(Result<StreamChunk, LlmFailure>, SseState)> {
    loop {
        if let Some(chunk) = state.buffer.pop_front() {
            return Some((Ok(chunk), state));
        }
        if state.finished {
            return None;
        }
        let next = tokio::time::timeout(state.idle_timeout, state.events.next()).await;
        let event = match next {
            Err(_) => {
                state.finished = true;
                return Some((
                    Err(LlmFailure::new(
                        codes::TIMEOUT,
                        "no stream activity within the idle timeout",
                    )),
                    state,
                ));
            }
            Ok(event) => event,
        };
        match event {
            Some(Ok(event)) => {
                if event.data == DONE_MARKER {
                    state.saw_done = true;
                    state.buffer.extend(state.translator.finalize());
                    continue;
                }
                if event.data.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<WireChunk>(&event.data) {
                    Ok(wire) => {
                        let style = state.style;
                        state.buffer.extend(state.translator.feed(&wire, style));
                    }
                    Err(error) => {
                        state.finished = true;
                        return Some((
                            Err(LlmFailure::new(
                                codes::MALFORMED_RESPONSE,
                                format!("malformed SSE payload: {error}"),
                            )),
                            state,
                        ));
                    }
                }
            }
            Some(Err(error)) => {
                state.finished = true;
                return Some((
                    Err(LlmFailure::new(
                        codes::TRANSPORT,
                        format!("SSE transport error: {error}"),
                    )),
                    state,
                ));
            }
            None => {
                state.finished = true;
                if !state.saw_done {
                    return Some((
                        Err(LlmFailure::new(
                            codes::STREAM_CLOSED,
                            "stream ended before the [DONE] marker",
                        )),
                        state,
                    ));
                }
                return None;
            }
        }
    }
}
