//! SSE stream driver: parses eventsource payloads off one HTTP response,
//! feeds the route's protocol translator, and enforces an idle timeout
//! between events.

use std::collections::VecDeque;
use std::time::Duration;

use denia_core::error::{LlmFailure, codes};
use denia_core::stream::StreamChunk;
use eventsource_stream::Eventsource;
use futures::StreamExt;

use crate::protocols::EventTranslator;

struct SseState {
    events: std::pin::Pin<
        Box<dyn futures::Stream<Item = Result<eventsource_stream::Event, eventsource_stream::EventStreamError<reqwest::Error>>> + Send>,
    >,
    translator: Box<dyn EventTranslator>,
    buffer: VecDeque<StreamChunk>,
    idle_timeout: Duration,
    finished: bool,
}

/// Drives one SSE response into the chunk protocol through `translator`.
/// The stream terminates after the translator's EOF buffer drains, or with a
/// terminal error item on transport, translate, or idle-timeout failures.
pub fn sse_chunk_stream(
    response: reqwest::Response,
    translator: Box<dyn EventTranslator>,
    idle_timeout: Duration,
) -> crate::ChunkStream {
    let state = SseState {
        events: Box::pin(response.bytes_stream().eventsource()),
        translator,
        buffer: VecDeque::new(),
        idle_timeout,
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
            Some(Ok(event)) => match state.translator.feed(&event.data) {
                Ok(chunks) => state.buffer.extend(chunks),
                Err(failure) => {
                    state.finished = true;
                    return Some((Err(failure), state));
                }
            },
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
                // EOF: let the translator emit its closing chunks (or fail on
                // a stream that ended before its terminal event).
                state.finished = true;
                match state.translator.finish() {
                    Ok(chunks) => state.buffer.extend(chunks),
                    Err(failure) => return Some((Err(failure), state)),
                }
            }
        }
    }
}
