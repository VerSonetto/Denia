//! Compaction circuit breakers belong to a session, including across its turns.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

#[derive(Default)]
pub(crate) struct CompactionState {
    pub failures: AtomicU32,
    pub rapid_refills: AtomicU32,
    pub tool_turns: AtomicU32,
    has_compacted: AtomicBool,
}

impl CompactionState {
    pub fn record_success(&self, rapid_threshold: u32) -> u32 {
        self.failures.store(0, Ordering::SeqCst);
        let turns = self.tool_turns.swap(0, Ordering::SeqCst);
        if self.has_compacted.swap(true, Ordering::SeqCst) && turns < rapid_threshold {
            self.rapid_refills.fetch_add(1, Ordering::SeqCst) + 1
        } else {
            self.rapid_refills.store(0, Ordering::SeqCst);
            0
        }
    }
}
