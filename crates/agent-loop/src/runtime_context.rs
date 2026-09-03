//! Durable projection state for dynamic runtime context (DSH-compatible).

use denia_core::session::SessionEvent;
use denia_session::Session;
use denia_system_prompt::{
    is_runtime_context_snapshot, render_context_snapshot, PromptAssembly, RUNTIME_CONTEXT_CLEARED,
};

/// Tracks the last retained runtime-context snapshot without owning session commits.
#[derive(Debug, Default)]
pub struct RuntimeContextProjection {
    retained: Option<String>,
}

impl RuntimeContextProjection {
    /// Restore from the session log, then follow new injected runtime snapshots.
    pub fn restore(session: &Session) -> Self {
        let retained = session
            .events()
            .iter()
            .rev()
            .find_map(|envelope| match &envelope.event {
                SessionEvent::UserMessage { text, injected: true, .. } if is_runtime_context_snapshot(text) => {
                    Some(text.clone())
                }
                _ => None,
            });
        Self { retained }
    }

    /// Returns snapshot text when the rendered runtime context changed.
    pub fn project(&mut self, assembly: &PromptAssembly) -> Option<String> {
        let current = render_context_snapshot(assembly);
        if self.retained.is_none() && current.is_empty() {
            return None;
        }
        let snapshot = if current.is_empty() {
            RUNTIME_CONTEXT_CLEARED.to_string()
        } else {
            current
        };
        if self.retained.as_deref() == Some(snapshot.as_str()) {
            return None;
        }
        self.retained = Some(snapshot.clone());
        Some(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use denia_core::session::SessionEvent;
    use denia_session::Session;
    use denia_system_prompt::{
        AssembledContext, PromptAssembly, RUNTIME_CONTEXT_HEADER,
    };

    use super::*;

    fn temp_session() -> Session {
        let dir = std::env::temp_dir().join(format!(
            "denia-runtime-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Session::create(&dir, uuid::Uuid::new_v4().to_string(), &dir, true, None).unwrap()
    }

    #[test]
    fn projects_changed_runtime_context_only_once() {
        let mut projection = RuntimeContextProjection::default();
        let assembly = PromptAssembly {
            sections: Vec::new(),
            contexts: vec![AssembledContext {
                name: "policy".to_string(),
                text: "Mode: read-only.".to_string(),
            }],
            tools: Vec::new(),
            variables: Default::default(),
        };
        let first = projection.project(&assembly).unwrap();
        assert!(first.starts_with(RUNTIME_CONTEXT_HEADER));
        assert!(projection.project(&assembly).is_none());
    }

    #[test]
    fn restore_reads_last_injected_runtime_snapshot() {
        let session = temp_session();
        let snapshot = format!("{RUNTIME_CONTEXT_HEADER}\n\nMode: on.");
        session
            .append(SessionEvent::UserMessage {
                text: snapshot.clone(),
                injected: true,
                images: Vec::new(),
            })
            .unwrap();
        let projection = RuntimeContextProjection::restore(&session);
        assert_eq!(projection.retained.as_deref(), Some(snapshot.as_str()));
    }
}
