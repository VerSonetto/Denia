//! The SSE push channel: settings/credentials/topology invalidations.
//!
//! 同时提供**长轮询**兜底(`/api/events/poll`):实测经 cloudflared 快速隧道
//! 时,SSE 的响应头能到(200)但**一个 data 帧都透不过来**(首帧垫 2KB/16KB、
//! 心跳提到 1 秒都一样),而挂 45 秒的普通 JSON 响应完好穿透。远程手机上
//! 推送只能走这条路。

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, sse::{Event, KeepAlive, Sse}};
use axum::routing::get;
use axum::{Json, Router};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;
use tokio_stream::wrappers::BroadcastStream;

use crate::state::{AppState, ServerEvent};

/// SSE 心跳周期与帧内容。会话 follow 流(`api::sessions`)用同一组值 —— 两条
/// 通道对前端的语义一样:**收到任何一帧就说明链路活着**。
///
/// 心跳必须是**带 data 的普通帧**,不能是默认那行 `:` 注释:注释行不被
/// EventSource 派发、也不产生 envelope,前端就没有任何信号区分"链路静默但
/// 活着"与"链路已经死了"(隧道上后者恰恰是常态)。具名事件也不行 —— 规范里
/// data buffer 为空的帧整帧不派发。
///
/// 周期 15 秒:再密对蜂窝网络是白耗电,再疏则降级判定变慢。
pub const HEARTBEAT_INTERVAL_SECS: u64 = 15;
pub const HEARTBEAT_FRAME: &str = r#"{"type":"hb"}"#;

/// 长轮询单次挂起上限。实测隧道在 45 秒量级仍正常返回,取 30 秒留一倍余量;
/// 再长会撞上手机浏览器对后台页挂起请求的回收,反而更慢。
const POLL_WAIT_MAX: Duration = Duration::from_secs(30);
const POLL_WAIT_DEFAULT: Duration = Duration::from_secs(25);
/// 单次返回的事件条数上限:积压时宁可让客户端多跑一轮,也不给一个巨型响应。
const POLL_EVENTS_MAX: usize = 64;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/events", get(events_stream))
        .route("/api/events/poll", get(events_poll))
}

#[derive(Debug, Deserialize)]
struct PollQuery {
    /// 客户端已读到的最新序号(0 = 首次)。
    #[serde(default)]
    after: u64,
    /// 本次最多挂起多少秒(客户端按链路质量调;上限 `POLL_WAIT_MAX`)。
    wait: Option<u64>,
}

/// 长轮询版的 `/api/events`:隧道上 SSE 不透传时的兜底推送通道。
///
/// 顺序是**先订阅、再读环**:反过来会在"读环"与"开始等"之间留一个窗口,
/// 这期间产生的事件既不在快照里也没被订阅覆盖 —— 手机上就表现为"卡住不动"。
async fn events_poll(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PollQuery>,
) -> impl IntoResponse {
    let pulse = state.pulse.clone();
    let wait = query
        .wait
        .map(|secs| Duration::from_secs(secs.min(POLL_WAIT_MAX.as_secs())))
        .unwrap_or(POLL_WAIT_DEFAULT);
    // wait=0 表示"别挂起,立刻给我当前增量"(客户端页面切到后台时用):手机
    // 浏览器与 cloudflared 对"挂住不动的请求"回收时机不一致,断在哪一侧都得
    // 等一次超时才发现,白白多扣一轮延迟。
    if wait.is_zero() {
        let (events, _latest, gap) = pulse.after(query.after);
        return reply(events, state.live.running_ids(), query.after, gap);
    }
    // 先订阅:挂起期间的新事件一定会叫醒这次请求。
    let mut receiver = state.events.subscribe();
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let (events, _latest, gap) = pulse.after(query.after);
        if !events.is_empty() || gap {
            return reply(events, state.live.running_ids(), query.after, gap);
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            // 空转到超时:回一个空增量。客户端立刻发起下一轮即可,不必重连。
            let (events, _, gap) = pulse.after(query.after);
            return reply(events, state.live.running_ids(), query.after, gap);
        }
        match tokio::time::timeout(remaining, receiver.recv()).await {
            // 有新事件:回到循环重读环,一次拿全。
            Ok(Ok(_)) => {}
            // 本请求自己落后于广播缓冲区:环里可能缺中间若干条,按断档处理,
            // 让客户端重取权威状态而不是停在断档处。
            Ok(Err(RecvError::Lagged(_))) => {
                let (events, _, _) = pulse.after(query.after);
                return reply(events, state.live.running_ids(), query.after, true);
            }
            Ok(Err(RecvError::Closed)) | Err(_) => {
                let (events, _, gap) = pulse.after(query.after);
                return reply(events, state.live.running_ids(), query.after, gap);
            }
        }
    }
}

/// `seq` 回的是**已交付的最后一条**序号(没有交付就原样回 `after`),不是环的
/// 最新序号 —— 单批被 `POLL_EVENTS_MAX` 截断时,回最新序号会让客户端游标跳过
/// 没拿到的那一截,而且是静默地跳过。
fn reply(
    entries: Vec<(u64, ServerEvent)>,
    running: Vec<String>,
    after: u64,
    gap: bool,
) -> impl IntoResponse {
    // 单批截断:积压时宁可让客户端多跑一轮,也不给一个无上限的响应体。
    let batch: Vec<(u64, ServerEvent)> = entries.into_iter().take(POLL_EVENTS_MAX).collect();
    // 游标只能按**实际交付的最后一条序号**推进。用 after + 条数反推在断档时会
    // 算错(环里最旧序号可能远大于 after+1);取截断后的末位则是准的。
    let next = batch.last().map(|(seq, _)| *seq).unwrap_or(after);
    let events = batch.into_iter().map(|(_, event)| event).collect::<Vec<_>>();
    // no-store:响应带 cookie、内容是"此刻的事件尾部",被中间层缓存下来会把
    // 陈旧事件发给后续请求。
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Json(json!({
            "events": events,
            "seq": next,
            "resync": gap,
            // 每次响应都带:SSE 版靠连接首帧给这份快照,轮询版没有"首帧"这个
            // 概念。没有它,降级到轮询后就再没人告诉前端"哪些会话在跑",
            // ensureFollowing 不触发 → 正文一条都收不到(比原来的 32 秒更糟)。
            "running": running,
        })),
    )
        .into_response()
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
    // 心跳要发成**带 data 的普通帧**,而不是默认那行 `:` 注释:注释行既不被
    // EventSource 派发、也不产生 envelope,前端就没有任何信号区分"链路活着只是
    // 没事件"与"链路已经死了"。有了 hb 帧,降级判定只看"多久没收到任何帧"。
    // (具名事件也不行:规范里 data buffer 为空时整帧不派发,所以必须带 data。)
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS))
            .event(Event::default().data(HEARTBEAT_FRAME)),
    )
}
