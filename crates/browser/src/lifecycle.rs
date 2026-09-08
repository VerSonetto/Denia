//! 浏览器实例生命周期:代次看护,避免旧看护误杀新实例.
//!
//! 每个成功拉起的 Chrome 占用一个单调递增的 generation。事件泵退出时只
//! 宣告「这一代」死了;看护任务必须同时看到「我看护的那一代仍是 live」
//! 和「泵宣告的正是那一代」才允许收尸。启动清场、上一代残留的退出信号,
//! 都不得杀掉刚写进槽位的新实例——这是 `newTab` 成功后立刻
//! `backend_unavailable` 的根因。

/// 看护任务是否应该收尸当前槽位里的实例。
///
/// `watchdog_generation`:本看护出生时盯着的那一代。
/// `live_generation`:管理器认为当前还活着的一代(0 = 槽位空)。
/// `exited_generation`:事件泵最近宣告退出的一代(0 = 尚无退出)。
pub fn should_reap(watchdog_generation: u64, live_generation: u64, exited_generation: u64) -> bool {
    watchdog_generation != 0
        && live_generation == watchdog_generation
        && exited_generation == watchdog_generation
}

/// 看护任务是否已被更新的实例取代,应自行退出(不得清 sidecar 状态)。
pub fn is_superseded(watchdog_generation: u64, live_generation: u64) -> bool {
    live_generation != watchdog_generation
}

/// 收尸时是否允许取走槽位里的 Inner。
///
/// `expected_generation = None` 表示调用方主动清场(启动前/关机/关到最后一个
/// tab),取走无论哪一代;`Some(g)` 表示看护只许取走自己的那一代。
pub fn may_take_inner(inner_generation: Option<u64>, expected_generation: Option<u64>) -> bool {
    match (inner_generation, expected_generation) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(inner), Some(expected)) => inner == expected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_live_and_exit_reaps() {
        assert!(should_reap(3, 3, 3));
        assert!(!is_superseded(3, 3));
    }

    #[test]
    fn leftover_exit_signal_must_not_kill_fresh_instance() {
        // 旧实现:启动清场把 exited 置 true,新看护一醒来就收尸。
        // 新协议:看护 2 只认退出宣告 2,残留的 1 或 0 都不能杀。
        assert!(!should_reap(2, 2, 0));
        assert!(!should_reap(2, 2, 1));
        assert!(may_take_inner(Some(2), Some(2)));
        assert!(!may_take_inner(Some(2), Some(1)));
    }

    #[test]
    fn stale_watchdog_does_not_reap_newer_instance() {
        assert!(!should_reap(1, 2, 1));
        assert!(is_superseded(1, 2));
        assert!(!may_take_inner(Some(2), Some(1)));
    }

    #[test]
    fn empty_slot_is_never_taken() {
        assert!(!may_take_inner(None, None));
        assert!(!may_take_inner(None, Some(1)));
        assert!(!should_reap(1, 0, 1));
        assert!(is_superseded(1, 0));
    }

    #[test]
    fn explicit_cleanup_takes_whatever_is_live() {
        assert!(may_take_inner(Some(4), None));
    }
}
