//! 完成通知的重试队列。结果身份来自子会话终态，领取事实来自父会话日志。
use std::{collections::{BTreeMap, HashMap, HashSet}, sync::Mutex, time::{Duration, Instant}};
use tokio::sync::Notify;

#[derive(Clone)]
pub(crate) struct Notice {
    pub child: String,
    pub parent: String,
    pub id: String,
    pub legacy_id: String,
    pub text: String,
}

struct Pending {
    notice: Notice,
    retry_at: Instant,
    attempts: u32,
}

#[derive(Default)]
struct State {
    pending: BTreeMap<String, Pending>,
    seen: HashSet<String>,
    /// 待执行的资源清理:通知身份 -> (子代理 id, 是否已开始)。
    /// 按**通知身份**记,而不是按子代理:同一个子代理续跑后的每一次
    /// 结束都各有自己的遗留任务要收,按会话记会让后续轮次漏清理。
    cleaning: HashMap<String, (String, bool)>,
    waking: HashSet<String>,
}

#[derive(Default)]
pub(crate) struct Delivery {
    state: Mutex<State>,
    pub changed: Notify,
}

impl Delivery {
    pub fn push(&self, notice: Notice, cleanup: bool) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if !state.seen.insert(notice.id.clone()) {
            return false;
        }
        if cleanup {
            state
                .cleaning
                .entry(notice.id.clone())
                .or_insert((notice.child.clone(), false));
        }
        state.pending.insert(notice.id.clone(), Pending {
            notice, retry_at: Instant::now(), attempts: 0,
        });
        drop(state);
        self.changed.notify_one();
        true
    }

    pub fn due(&self) -> Vec<Notice> {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.pending.values_mut().filter_map(|pending| {
            if pending.retry_at > now { return None; }
            pending.attempts = pending.attempts.saturating_add(1);
            pending.retry_at = now + Duration::from_millis(
                (100u64 << pending.attempts.min(6)).min(5_000),
            );
            Some(pending.notice.clone())
        }).collect()
    }

    pub fn acknowledge(&self, id: &str) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).pending.remove(id);
    }

    pub fn expedite(&self, parent: &str) {
        for pending in self.state.lock().unwrap_or_else(|p| p.into_inner()).pending.values_mut() {
            if pending.notice.parent == parent { pending.retry_at = Instant::now(); }
        }
        self.changed.notify_one();
    }

    /// 取走尚未开始的清理任务(通知身份 + 子代理 id),并标记为已开始。
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
        self.state.lock().unwrap_or_else(|p| p.into_inner()).waking.insert(id.into())
    }

    pub fn finish_wake(&self, id: &str) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).waking.remove(id);
    }

    pub fn forget(&self, id: &str) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.pending.retain(|_, p| p.notice.parent != id && p.notice.child != id);
        state.seen.retain(|key| !key.starts_with(&format!("settled:{id}:")));
        state.cleaning.retain(|_, (child, _)| child != id);
        state.waking.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_retry_without_reusing_terminal_identity() {
        let queue = Delivery::default();
        let notice = |id: &str| Notice { child: "child".into(), parent: "parent".into(),
            id: id.into(), legacy_id: "legacy".into(), text: id.into() };
        assert!(queue.push(notice("t1"), true));
        assert!(!queue.push(notice("t1"), true));
        assert_eq!(queue.due().len(), 1);
        assert!(queue.due().is_empty());
        queue.expedite("parent");
        assert_eq!(queue.due().len(), 1);
        queue.acknowledge("t1");
        assert!(!queue.push(notice("t1"), false));
        assert!(queue.push(notice("t2"), false));
        assert_eq!(queue.due()[0].text, "t2");
        assert_eq!(
            queue.claim_cleanups(),
            vec![("t1".to_string(), "child".to_string())]
        );
        assert!(queue.claim_cleanups().is_empty());
        assert!(queue.is_cleaning("child"));
        queue.finish_cleanup("t1");
        assert!(!queue.is_cleaning("child"));
        // 同一个子代理的下一次结束各自登记一次清理:按通知身份记,
        // 后续轮次的遗留任务不会被"这个会话已经清过"吞掉。
        assert!(queue.push(
            Notice {
                child: "child".into(),
                parent: "parent".into(),
                id: "t3".into(),
                legacy_id: "legacy-3".into(),
                text: "t3".into(),
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
}
