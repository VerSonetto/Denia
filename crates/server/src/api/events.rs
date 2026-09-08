//! The SSE push channel: settings/credentials/topology invalidations.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use futures::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::state::{AppState, ServerEvent};

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/api/events", get(events_stream))
}

async fn events_stream(
    State(state): State<Arc<AppState>>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    // 先订阅再拍快照:连接瞬间的 running-changed 进缓冲,不会漏;
    // 快照作为首帧,刷新/重连后前端能还原中断按钮,不必等下一次增量。
    let receiver = state.events.subscribe();
    let snapshot = ServerEvent::RunningSnapshot {
        ids: state.live.running_ids(),
    };
    let snapshot_event =
        Event::default().data(serde_json::to_string(&snapshot).unwrap_or_default());
    let live = BroadcastStream::new(receiver).filter_map(|result| async move {
        match result {
            Ok(event) => Some(Ok(
                Event::default().data(serde_json::to_string(&event).unwrap_or_default())
            )),
            // Lagged receivers skip the missed batch; the next state read
            // resynchronizes the console.
            Err(_) => None,
        }
    });
    let stream = futures::stream::once(async move { Ok(snapshot_event) }).chain(live);
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
