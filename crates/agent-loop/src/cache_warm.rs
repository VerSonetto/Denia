//! Provider prompt-cache warming.
//!
//! A coding turn often pauses long enough (a build, a test run, the user
//! reading) for a 5-minute provider cache entry to expire. Replaying the last
//! request at ~90% of its lifetime keeps the prefix hot; the replay asks for a
//! single output token and is never written to the session log.
//!
//! Scheduling lives at the agent-loop layer because only the turn knows the
//! phase: `streaming` protects a prefix while the turn is still working,
//! `idle` also covers the gap after it settles.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use denia_llm::{GenerateRequest, LlmRegistry};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

/// Who is allowed to keep a prefix warm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheWarmingMode {
    /// Never replay.
    #[default]
    Off,
    /// Replay only while a turn is running.
    Streaming,
    /// Also replay after the turn settles.
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheWarmingSettings {
    pub mode: CacheWarmingMode,
    /// Provider cache lifetime the replay targets. The timer fires at 90% of
    /// this value, preserving at least ten seconds of margin.
    pub ttl_seconds: u64,
}

impl Default for CacheWarmingSettings {
    fn default() -> Self {
        Self {
            mode: CacheWarmingMode::Off,
            ttl_seconds: 300,
        }
    }
}

/// Replay phase, chosen by the caller that schedules it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmPhase {
    Streaming,
    Idle,
}

#[derive(Clone)]
struct WarmRequest {
    provider: String,
    request: GenerateRequest,
}

/// Per-session last request plus the one in-flight refresh task.
#[derive(Default)]
pub struct CacheWarmer {
    last: Mutex<HashMap<String, WarmRequest>>,
    tasks: Mutex<HashMap<String, JoinHandle<()>>>,
}

impl CacheWarmer {
    /// Remember the request shape to replay. A bounded map keeps a long-lived
    /// process from retaining every session's full history.
    pub fn remember(&self, session: &str, provider: &str, request: &GenerateRequest) {
        const MAX_REMEMBERED: usize = 64;
        let mut last = self.last.lock().unwrap_or_else(|poison| poison.into_inner());
        if last.len() >= MAX_REMEMBERED && !last.contains_key(session) {
            if let Some(evicted) = last.keys().next().cloned() {
                last.remove(&evicted);
            }
        }
        last.insert(
            session.to_string(),
            WarmRequest {
                provider: provider.to_string(),
                request: request.clone(),
            },
        );
    }

    /// Drop the pending refresh (new turn, compaction, or a mode change).
    pub fn cancel(&self, session: &str) {
        if let Some(handle) = self
            .tasks
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(session)
        {
            handle.abort();
        }
    }

    /// Arm a refresh for `session`. Replaces any refresh already armed for it.
    pub fn schedule(
        &self,
        session: &str,
        phase: WarmPhase,
        registry: &Arc<LlmRegistry>,
        settings: &CacheWarmingSettings,
    ) {
        if settings.mode == CacheWarmingMode::Off
            || (phase == WarmPhase::Idle && settings.mode != CacheWarmingMode::Idle)
        {
            self.cancel(session);
            return;
        }
        self.tasks
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .retain(|_, handle| !handle.is_finished());
        let warm = match self
            .last
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(session)
            .cloned()
        {
            Some(warm) => warm,
            None => return,
        };
        self.cancel(session);
        let delay = Duration::from_secs((settings.ttl_seconds.max(10) * 9) / 10);
        let registry = Arc::clone(registry);
        let handle = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let WarmRequest { provider, request } = warm;
            let request = warming_request(request);
            if let Ok(mut stream) = registry.stream(&provider, &request, None).await {
                // Drain the tiny stream so the provider commits the cache
                // read before the connection closes.
                while stream.next().await.is_some() {}
            }
        });
        self.tasks
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(session.to_string(), handle);
    }

    #[cfg(test)]
    fn has_pending(&self, session: &str) -> bool {
        self.tasks
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .contains_key(session)
    }
}

/// Shape of a cache refresh: one output token, no sampling/reasoning controls,
/// and no cache key. A stateful Responses chain must not adopt the one-token
/// output as the next real turn's baseline.
fn warming_request(mut request: GenerateRequest) -> GenerateRequest {
    request.cache_key = None;
    request.max_tokens = Some(1);
    request.reasoning_effort = None;
    request.temperature = None;
    request.stop.clear();
    request
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::message::ChatMessage;
    use denia_core::tool::ToolSchema;

    fn request() -> GenerateRequest {
        GenerateRequest {
            model: "fixture".to_string(),
            reasoning_effort: None,
            messages: vec![ChatMessage::user("hello")],
            system: Some("system".to_string()),
            tools: Vec::<ToolSchema>::new(),
            temperature: None,
            max_tokens: None,
            stop: Vec::new(),
            cache_key: Some("session-1".to_string()),
        }
    }

    #[tokio::test]
    async fn off_and_streaming_idle_phase_do_not_arm() {
        let warmer = CacheWarmer::default();
        let registry = Arc::new(LlmRegistry::new());
        warmer.remember("session-1", "fixture-route", &request());

        warmer.schedule(
            "session-1",
            WarmPhase::Streaming,
            &registry,
            &CacheWarmingSettings {
                mode: CacheWarmingMode::Off,
                ttl_seconds: 10,
            },
        );
        assert!(!warmer.has_pending("session-1"));

        warmer.schedule(
            "session-1",
            WarmPhase::Idle,
            &registry,
            &CacheWarmingSettings {
                mode: CacheWarmingMode::Streaming,
                ttl_seconds: 10,
            },
        );
        assert!(!warmer.has_pending("session-1"));
    }

    #[tokio::test]
    async fn idle_mode_arms_and_cancel_drops() {
        let warmer = CacheWarmer::default();
        let registry = Arc::new(LlmRegistry::new());
        warmer.remember("session-1", "fixture-route", &request());
        warmer.schedule(
            "session-1",
            WarmPhase::Idle,
            &registry,
            &CacheWarmingSettings {
                mode: CacheWarmingMode::Idle,
                ttl_seconds: 10,
            },
        );
        assert!(warmer.has_pending("session-1"));
        warmer.cancel("session-1");
        assert!(!warmer.has_pending("session-1"));
    }

    #[test]
    fn warming_request_caps_output_and_drops_chain_identity() {
        let mut request = request();
        request.reasoning_effort = Some("high".to_string());
        request.temperature = Some(0.7);
        request.stop = vec!["x".to_string()];
        let warm = warming_request(request);
        assert_eq!(warm.max_tokens, Some(1));
        assert!(warm.cache_key.is_none());
        assert!(warm.reasoning_effort.is_none());
        assert!(warm.temperature.is_none());
        assert!(warm.stop.is_empty());
    }
}
