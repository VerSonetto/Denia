//! 全局事件的**可轮询**副本:给推送通道套不起来的链路用的。
//!
//! ## 为什么需要
//!
//! 实测(2026-09-16,scripts/probe-*.mjs):经 cloudflared 快速隧道访问时,
//! `/api/events` 的 SSE 响应头能到(200)但**一个 data 帧都透不过来** ——
//! 首帧垫 2KB / 16KB、心跳提到 1 秒,四种变体全是 0 块;同一条隧道上挂 45 秒
//! 的普通 JSON 响应却完好穿透。也就是说手机经隧道时,SSE 不是慢,是根本不通。
//!
//! 后果很具体:前端 follow 流永远收不到帧 → 32 秒看门狗超时 → 重连 + 重拉
//! 全量快照(普通 GET 能过)→ 用户看到的是"内容每 ~32 秒跳一次"。
//!
//! ## 为什么不是"每条广播重新订阅一次"
//!
//! `tokio::sync::broadcast` 的接收端只能拿到"从订阅点开始"的后续事件,没有
//! 供客户端回放的序号。长轮询必须能从客户端给的游标接着读,否则挂起期间的事件
//! 会重放或漏掉。这里给每条事件盖一个单调序号,留最近的若干条在环里,客户端按
//! `after=<seq>` 取增量。
//!
//! 发送点保持原样:环由**一个**订阅者喂,不要求任何调用方改签名。

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::broadcast;

use crate::state::ServerEvent;

/// 环容量:够覆盖一次长轮询挂起期间的突发(一轮 turn 的失效通知远少于这个数)。
const RING_CAPACITY: usize = 256;

/// 全局事件的可轮询副本。
///
/// 并发形态:单写(`spawn` 里的订阅任务)`多读`(每个 poll 请求一次 `after`)。
/// 序号由写侧独占分配,所以环内序号必然连续 —— 除了一种情况:`Lagged`,即
/// 广播侧来不及入环就丢了若干条。那些事件不会占序号,序号看着还是连续的,
/// 但内容缺了一块,所以必须另记断档边界,见 `gap_from`。
pub struct EventPulse {
    /// 已分配的最新序号;0 = 还没有任何事件。
    latest: AtomicU64,
    /// 最后一次断档的序号边界。游标 `after < gap_from` 时,`after` 之后的区间
    /// 不完整,客户端应当重取权威状态而不是只吃增量。
    ///
    /// 用"边界"而不是"是否丢过"的布尔或累计计数:后者一旦置位就永久为真,
    /// 客户端会被永远推进重快照循环,通道再也回不到增量。边界会随游标推进而
    /// 自然失效。
    gap_from: AtomicU64,
    ring: Mutex<VecDeque<(u64, ServerEvent)>>,
}

impl EventPulse {
    fn mark_gap(&self, from: u64) {
        self.gap_from.fetch_max(from, Ordering::SeqCst);
    }

    fn push(&self, event: ServerEvent) {
        let seq = self.latest.fetch_add(1, Ordering::SeqCst) + 1;
        let mut ring = self.ring.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if ring.len() == RING_CAPACITY {
            let (oldest_seq, _) = ring.pop_front().expect("容量非零时必有元素");
            self.mark_gap(oldest_seq);
        }
        ring.push_back((seq, event));
    }

    /// 取 `after` 之后的事件;返回 (带序号的增量, 最新序号, 断档)。
    ///
    /// 序号必须随条目一起出去:调用方交付一批后要回游标,而断档时环里最旧的
    /// 序号可能远大于 `after + 1`,用"after + 条数"反推会把游标停在错误的低位,
    /// 客户端于是反复拿到同一批旧事件(或反过来跳过没读到的)。
    pub fn after(&self, after: u64) -> (Vec<(u64, ServerEvent)>, u64, bool) {
        let latest = self.latest.load(Ordering::SeqCst);
        let ring = self.ring.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let events = ring
            .iter()
            .filter(|(seq, _)| *seq > after)
            .cloned()
            .collect();
        let gap = after < self.gap_from.load(Ordering::SeqCst);
        (events, latest, gap)
    }

    /// 起订阅任务:把广播里的每条事件抄进环。
    ///
    /// 落后于广播缓冲区时必须记断档:那 `skipped` 条根本没进环,序号看着连续、
    /// 内容却缺了一块。不记的话客户端会永久停在断档处而毫无察觉。
    pub fn spawn(events: broadcast::Sender<ServerEvent>) -> std::sync::Arc<Self> {
        let pulse = std::sync::Arc::new(Self {
            latest: AtomicU64::new(0),
            gap_from: AtomicU64::new(0),
            ring: Mutex::new(VecDeque::with_capacity(RING_CAPACITY)),
        });
        let mut receiver = events.subscribe();
        let task_pulse = pulse.clone();
        tokio::spawn(async move {
            loop {
                match receiver.recv().await {
                    Ok(event) => task_pulse.push(event),
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "event pulse lagged behind broadcast; clients will resync");
                        // 下一个序号起的内容没抄到:从这个边界开始算断档。
                        task_pulse.mark_gap(task_pulse.latest.load(Ordering::SeqCst) + 1);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        pulse
    }
}

impl Default for EventPulse {
    fn default() -> Self {
        Self {
            latest: AtomicU64::new(0),
            gap_from: AtomicU64::new(0),
            ring: Mutex::new(VecDeque::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assigns_monotonic_seq_and_returns_increment() {
        let pulse = EventPulse::default();
        pulse.push(ServerEvent::SettingsUpdated);
        pulse.push(ServerEvent::LlmUpdated);
        let (events, latest, gap) = pulse.after(0);
        assert_eq!(latest, 2);
        assert!(!gap);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, 1);
        assert_eq!(events[1].0, 2);
        // 游标之后的增量:已读过的不重发。
        let (tail, _, _) = pulse.after(1);
        assert_eq!(tail.len(), 1, "after(1) 应当只给第 2 条");
        assert_eq!(tail[0].0, 2);
        assert!(matches!(tail[0].1, ServerEvent::LlmUpdated));
        // 游标已最新:空增量 + 不断档,长轮询据此挂起等待。
        let (none, _, gap_at_tail) = pulse.after(latest);
        assert!(none.is_empty());
        assert!(!gap_at_tail);
    }

    #[test]
    fn overflow_reports_gap_until_cursor_passes_it() {
        let pulse = EventPulse::default();
        for _ in 0..(RING_CAPACITY + 5) {
            pulse.push(ServerEvent::McpUpdated);
        }
        let (events, latest, gap) = pulse.after(0);
        assert!(gap, "被覆盖掉的区间必须报断档,否则客户端会以为拿到了完整增量");
        assert_eq!(events.len(), RING_CAPACITY, "环只保留最近若干条");
        assert_eq!(latest, (RING_CAPACITY + 5) as u64);
        // 断档时增量是从环里最旧那条开始的,其序号大于 after+1 —— 这正是
        // "交付后必须按真实序号回游标"的理由。
        assert_eq!(
            events.first().map(|(seq, _)| *seq),
            Some(6),
            "前 5 条已被覆盖,环从第 6 条起"
        );
        // 关键回归点:游标推进到断档边界之后必须恢复"连续增量"语义。
        // 早先用累计丢弃数判断时,这里会永久报断档 → 客户端永久重快照。
        let (_, _, gap_after_resync) = pulse.after(latest);
        assert!(
            !gap_after_resync,
            "断档是会随游标推进失效的边界,不是永久的标记"
        );
    }

    #[test]
    fn fresh_pulse_yields_empty_not_gap() {
        // 冷启动:没有任何事件。若这里报断档,首个 poll 就会被判成需要重快照。
        let pulse = EventPulse::default();
        let (events, latest, gap) = pulse.after(0);
        assert!(events.is_empty());
        assert_eq!(latest, 0);
        assert!(!gap);
    }
}
