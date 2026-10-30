//! 运行中「引导」消息：入队、步骤边界取走、送达广播。
//!
//! 与普通 prompt 的区别只在**时机**：引导不打断当前输出/工具调用，由
//! 驱动器在每个 step 开始时取走并落成真实 `UserMessage`，模型下一次请求
//! 即看到它。队列只驻内存：轮次结束（无论正常收尾、取消还是失败）时清空，
//! 未被取走的条目由前端持久化队列按普通消息重发——服务端不替用户偷偷
//! 补发，避免与前端队列形成双重来源。

use super::*;
use denia_tools::capabilities::SteeringInput;

/// 单会话最多排队的引导条数。超出即拒绝：引导是"补充一句"，不是另一个
/// 无限缓存；普通队列仍在客户端，用户可以排队任意多条。
const MAX_PENDING: usize = 8;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

impl Runtime {
    /// 把一条引导消息排进运行中的会话。
    ///
    /// 幂等：同 id 重复提交（前端重试/双标签页）只保留一条。
    /// 未运行时返回 `session/not-running`，前端据此回落到普通队列表。
    pub async fn steer(&self, id: &str, message: SteeringMessage) -> Result<(), String> {
        let live = self.live(id).await?;
        if live.session.header().subagent.is_some() {
            return Err("subagent/read-only:请从父会话向子代理发送指令".into());
        }
        if message.text.trim().is_empty() && message.images.is_empty() {
            return Err("steering/empty:引导消息不能为空".into());
        }
        if !live.running.load(Ordering::SeqCst) {
            return Err("session/not-running:当前没有进行中的轮次".into());
        }
        let mut map = self.inner.steering.lock().unwrap();
        let queue = map.entry(id.to_string()).or_default();
        if queue.iter().any(|item| item.id == message.id) {
            return Ok(());
        }
        if queue.len() >= MAX_PENDING {
            return Err(format!(
                "steering/queue-full:引导消息最多排队 {MAX_PENDING} 条，请等待当前步骤完成"
            ));
        }
        queue.push_back(message);
        let mut record = crate::trajectory::TraceRecord::new(id, now_ms(), "queue");
        record.operation = Some("steer_accepted".into());
        record.agent_id = Some(id.to_string());
        record.data = Some(json!({"queueId": queue.back().map(|item| item.id.clone())}));
        drop(map);
        self.inner.trace.record(&record);
        Ok(())
    }

    /// 追加一条辅助轨迹记录（队列/子代理等不落在 SessionEvent 里的动作）。
    pub fn record_trace(&self, record: crate::trajectory::TraceRecord) {
        self.inner.trace.record(&record);
    }

    /// 轮次收尾时丢弃未被取走的引导。
    ///
    /// 前端队列是持久化事实源：没送达的条目会以普通消息重发。这里若保留
    /// 到下一轮，会在用户下一条普通消息的 step 边界被误注入，顺序与意图
    /// 都不对。
    pub fn clear_steering(&self, id: &str) {
        self.inner.steering.lock().unwrap().remove(id);
    }

    /// 取走本会话排队的引导消息，并按顺序广播送达事件。
    ///
    /// 广播发生在**落库之前**：前端收到即可把队列项移出；驱动器随后 append
    /// 的 `UserMessage` 是真的会话历史。事件丢失也不影响正确性——下一条
    /// 轮次结束时会话空闲，前端会按普通消息重发未确认的队列项（服务端按
    /// 稳定 id 去重）。
    pub(super) fn drain_steering_inner(&self, session: &str) -> Result<Vec<SteeringInput>, String> {
        let items: Vec<SteeringMessage> = {
            let mut map = self.inner.steering.lock().unwrap();
            map.remove(session)
                .map(|queue| queue.into_iter().collect())
                .unwrap_or_default()
        };
        for item in &items {
            let _ = self.inner.events.send(ServerEvent::SteeringDelivered {
                id: session.to_string(),
                message_id: item.id.clone(),
            });
            let mut record = crate::trajectory::TraceRecord::new(session, now_ms(), "queue");
            record.operation = Some("steer_delivered".into());
            record.agent_id = Some(session.to_string());
            record.data = Some(json!({"queueId": item.id}));
            self.inner.trace.record(&record);
        }
        Ok(items
            .into_iter()
            .map(|item| SteeringInput {
                id: item.id,
                text: item.text,
                images: item.images,
            })
            .collect())
    }
}
