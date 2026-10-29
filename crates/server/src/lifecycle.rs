//! 后台任务与子代理的统一完成生命周期。
//!
//! 生产者(后台任务注册表、子代理终态)只负责登记一条 [`Completion`]；
//! 何时把结果交回模型由本队列统一裁决：
//!
//! - 目标模型正在输出(`AssistantChunk` … `AssistantMessage`)或正在执行
//!   工具(`ToolCall` … `StepEnd`)时，结果留在队列里等待，绝不打断当前动作；
//! - 目标模型空闲(轮次之间、步与步之间)或已经不在运行时，立即投递并唤醒。
//!
//! 投递事实仍由父会话日志里的 `AgentInbox`/`AgentDelivery` 表达：重试与重启
//! 恢复都以日志为准，因此同一份结果不会因为等待或补投而重复交付。结果身份
//! 由生产者给出(任务 id / 子代理轮次)，续跑不会复用上一次的结果身份。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::Notify;

/// 一条待投递的执行结果。后台任务与子代理共用这一种记录，不再各写一套
/// 重试/唤醒逻辑。
#[derive(Clone, Debug)]
pub(crate) struct Completion {
    /// 资源归属：子代理 id；后台任务记发起会话自身(存在性检查与清理用)。
    pub child: String,
    /// 结果去向：发起任务的会话(后台任务)或父会话(子代理)。
    pub target: String,
    /// 稳定身份：跨重试、跨日志恢复都不变。
    pub id: String,
    /// 旧身份：兼容旧日志里已经领取过的通知。
    pub legacy_id: String,
    /// 注入正文。
    pub text: String,
    /// 注入来源(`subagent-settled` / `job-completed` / …)。
    pub source: String,
}

/// 这些来源是执行结果，不是普通对话消息：它们必须越过连续唤醒预算，
/// 否则同一用户轮次后分批完成的结果会被限流在收件箱里。
pub(crate) fn is_completion_source(source: &str) -> bool {
    source == "subagent-settled" || source == "job-completed"
}

struct Pending {
    completion: Completion,
    retry_at: Instant,
    attempts: u32,
}

#[derive(Default)]
struct State {
    pending: BTreeMap<String, Pending>,
    /// 已经登记过的结果身份。重试/恢复重复登记同一身份必须无副作用。
    seen: HashSet<String>,
    /// 待执行的资源清理：通知身份 -> (子代理 id, 是否已开始)。
    /// 按**通知身份**记，而不是按子代理：同一个子代理续跑后的每一次
    /// 结束都各有自己的遗留任务要收，按会话记会让后续轮次漏清理。
    cleaning: HashMap<String, (String, bool)>,
    waking: HashMap<String, bool>,
}

#[derive(Default)]
pub(crate) struct CompletionQueue {
    state: Mutex<State>,
    pub changed: Notify,
}

impl CompletionQueue {
    pub fn push(&self, completion: Completion, cleanup: bool) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if !state.seen.insert(completion.id.clone()) {
            return false;
        }
        if cleanup {
            state
                .cleaning
                .entry(completion.id.clone())
                .or_insert((completion.child.clone(), false));
        }
        state.pending.insert(
            completion.id.clone(),
            Pending {
                completion,
                retry_at: Instant::now(),
                attempts: 0,
            },
        );
        drop(state);
        self.changed.notify_one();
        true
    }

    pub fn due(&self) -> Vec<Completion> {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state
            .pending
            .values_mut()
            .filter_map(|pending| {
                if pending.retry_at > now {
                    return None;
                }
                pending.attempts = pending.attempts.saturating_add(1);
                pending.retry_at =
                    now + Duration::from_millis((100u64 << pending.attempts.min(6)).min(5_000));
                Some(pending.completion.clone())
            })
            .collect()
    }

    pub fn acknowledge(&self, id: &str) {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pending
            .remove(id);
    }

    /// 目标模型刚到达一个动作边界：让它的待投递结果不再等退避计时。
    pub fn expedite(&self, target: &str) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let mut hit = false;
        for pending in state.pending.values_mut() {
            if pending.completion.target == target {
                pending.retry_at = Instant::now();
                hit = true;
            }
        }
        drop(state);
        if hit {
            self.changed.notify_one();
        }
    }

    /// 取走尚未开始的清理任务(通知身份 + 子代理 id)，并标记为已开始。
    pub fn claim_cleanups(&self) -> Vec<(String, String)> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state
            .cleaning
            .iter_mut()
            .filter_map(|(id, (child, started))| {
                if *started {
                    return None;
                }
                *started = true;
                Some((id.clone(), child.clone()))
            })
            .collect()
    }

    pub fn is_cleaning(&self, child: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .cleaning
            .values()
            .any(|(id, _)| id == child)
    }

    pub fn finish_cleanup(&self, notice: &str) {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .cleaning
            .remove(notice);
        self.changed.notify_one();
    }

    pub fn start_wake(&self, id: &str) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        match state.waking.entry(id.into()) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                *entry.get_mut() = true;
                false
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(false);
                true
            }
        }
    }

    /// 检查后续请求与释放所有权必须原子完成，否则空闲边沿会丢失。
    pub fn finish_wake(&self, id: &str) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(pending) = state.waking.get_mut(id) {
            if std::mem::take(pending) {
                return true;
            }
        }
        state.waking.remove(id);
        false
    }

    pub fn forget(&self, id: &str) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state
            .pending
            .retain(|_, p| p.completion.target != id && p.completion.child != id);
        state
            .seen
            .retain(|key| !key.starts_with(&format!("settled:{id}:")));
        state.cleaning.retain(|_, (child, _)| child != id);
        state.waking.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completion(id: &str, target: &str) -> Completion {
        Completion {
            child: "child".into(),
            target: target.into(),
            id: id.into(),
            legacy_id: "legacy".into(),
            text: id.into(),
            source: "subagent-settled".into(),
        }
    }

    #[test]
    fn wake_requests_are_coalesced_without_losing_the_idle_edge() {
        let queue = CompletionQueue::default();
        assert!(queue.start_wake("parent"));
        assert!(!queue.start_wake("parent"));
        assert!(!queue.start_wake("parent"));
        assert!(queue.finish_wake("parent"));
        // 后续请求由原 worker 继续处理，不能同时启动第二个 worker。
        assert!(!queue.start_wake("parent"));
        assert!(queue.finish_wake("parent"));
        assert!(!queue.finish_wake("parent"));
        assert!(queue.start_wake("parent"));
        assert!(!queue.finish_wake("parent"));
    }

    #[test]
    fn completions_retry_without_reusing_terminal_identity() {
        let queue = CompletionQueue::default();
        assert!(queue.push(completion("t1", "parent"), true));
        assert!(!queue.push(completion("t1", "parent"), true));
        assert_eq!(queue.due().len(), 1);
        assert!(queue.due().is_empty());
        queue.expedite("parent");
        assert_eq!(queue.due().len(), 1);
        queue.acknowledge("t1");
        assert!(!queue.push(completion("t1", "parent"), false));
        assert!(queue.push(completion("t2", "parent"), false));
        assert_eq!(queue.due()[0].text, "t2");
        assert_eq!(
            queue.claim_cleanups(),
            vec![("t1".to_string(), "child".to_string())]
        );
        assert!(queue.claim_cleanups().is_empty());
        assert!(queue.is_cleaning("child"));
        queue.finish_cleanup("t1");
        assert!(!queue.is_cleaning("child"));
        // 同一个子代理的下一次结束各自登记一次清理：按通知身份记，
        // 后续轮次的遗留任务不会被"这个会话已经清过"吞掉。
        assert!(queue.push(
            Completion {
                child: "child".into(),
                target: "parent".into(),
                id: "t3".into(),
                legacy_id: "legacy-3".into(),
                text: "t3".into(),
                source: "subagent-settled".into(),
            },
            true
        ));
        assert_eq!(
            queue.claim_cleanups(),
            vec![("t3".to_string(), "child".to_string())]
        );
        queue.finish_cleanup("t3");
        assert!(!queue.is_cleaning("child"));
    }

    #[test]
    fn expedite_only_touches_the_targets_queue() {
        let queue = CompletionQueue::default();
        assert!(queue.push(completion("a", "parent"), false));
        assert!(queue.push(completion("b", "other"), false));
        let _ = queue.due();
        queue.expedite("other");
        let due: Vec<_> = queue.due().into_iter().map(|c| c.id).collect();
        assert_eq!(due, vec!["b".to_string()]);
    }

    #[test]
    fn job_and_subagent_completions_bypass_the_message_wake_budget() {
        assert!(is_completion_source("job-completed"));
        assert!(is_completion_source("subagent-settled"));
        assert!(!is_completion_source("agent:someone"));
    }
}
