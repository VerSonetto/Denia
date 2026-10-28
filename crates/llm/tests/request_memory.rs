//! Request preparation allocations, not an estimate of total process RSS.
use std::alloc::{GlobalAlloc, Layout, System};
use std::borrow::Cow;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use denia_core::message::ChatMessage;
use denia_llm::{GenerateRequest, ReplayPolicy};

struct CountedAllocator;
static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, old: Layout, size: usize) -> *mut u8 {
        let next = unsafe { System.realloc(pointer, old, size) };
        if !next.is_null() {
            LIVE_BYTES.fetch_add(size, Ordering::Relaxed);
            LIVE_BYTES.fetch_sub(old.size(), Ordering::Relaxed);
        }
        next
    }
}

#[global_allocator]
static ALLOCATOR: CountedAllocator = CountedAllocator;

#[test]
fn long_and_parallel_request_preparation_borrows_history() {
    let request = GenerateRequest {
        model: "memory-fixture".into(),
        reasoning_effort: None,
        messages: (0..96)
            .map(|_| ChatMessage::user("x".repeat(64 * 1024)))
            .collect(),
        system: Some("fixture system".into()),
        tools: vec![],
        temperature: None,
        max_tokens: Some(4096),
        stop: vec![],
    };
    for parallel in [1, 8] {
        let before = LIVE_BYTES.load(Ordering::Relaxed);
        let started = Instant::now();
        let cloned: Vec<_> = (0..parallel).map(|_| black_box(request.clone())).collect();
        let clone_time = started.elapsed();
        let clone_bytes = LIVE_BYTES.load(Ordering::Relaxed).saturating_sub(before);
        assert!(clone_bytes >= parallel * 6 * 1024 * 1024);
        assert_eq!(cloned[0].messages, request.messages);
        drop(cloned);

        let policy = ReplayPolicy::default();
        let before = LIVE_BYTES.load(Ordering::Relaxed);
        let started = Instant::now();
        let prepared: Vec<_> = (0..parallel)
            .map(|_| black_box(policy.prepare("fixture-route", &request)))
            .collect();
        let borrowed_time = started.elapsed();
        let borrowed_bytes = LIVE_BYTES.load(Ordering::Relaxed).saturating_sub(before);
        for value in &prepared {
            assert!(matches!(value, Cow::Borrowed(_)));
            assert!(std::ptr::eq(value.as_ref(), &request));
        }
        assert!(borrowed_bytes < 64 * 1024);
        println!(
            "{parallel} request(s), 6 MiB history each: preparation allocations {:.3} -> {:.3} MiB; elapsed {:?} -> {:?}",
            clone_bytes as f64 / 1048576.0,
            borrowed_bytes as f64 / 1048576.0,
            clone_time,
            borrowed_time,
        );
    }
}
