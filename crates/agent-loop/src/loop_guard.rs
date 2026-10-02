use denia_core::message::ToolCallRef;
use serde_json::Value;

pub const LOOP_THRESHOLD: u32 = 10;
pub const LOOP_WARN_THRESHOLD: u32 = 3;
pub const LOOP_WARN_TEXT: &str = "检测到无进展：同一动作与结果已连续重复三次。请利用现有证据改变排查策略、缩小范围或说明阻塞；结果有真实变化时可继续查询。";

#[derive(Debug, Default)]
pub struct LoopGuard { last: Option<String>, repeats: u32 }

#[derive(Debug, PartialEq)]
pub enum LoopVerdict { Continue, Warn { repeats: u32 }, Block { repeats: u32 } }

impl LoopGuard {
    pub fn reset(&mut self) { *self = Self::default(); }

    pub fn observe(&mut self, calls: &[ToolCallRef], results: &[(String, bool, Option<Value>)]) -> LoopVerdict {
        if calls.is_empty() || calls.len() != results.len() { self.reset(); return LoopVerdict::Continue; }
        let evidence: Vec<Value> = calls.iter().zip(results).map(|(call, (text, error, meta))| {
            let normalized = normalize(text);
            let volatile = text.lines().any(|line| ["timestamp:", "updated_at:", "elapsed_ms:", "duration_ms:", "polled_at:"].iter().any(|prefix| line.trim().to_ascii_lowercase().starts_with(prefix)));
            let digest = if serde_json::from_str::<Value>(text).is_ok() || volatile { Value::Null } else {
                meta.as_ref().and_then(|meta| meta["outputArtifact"]["streams"].as_object()).map(|streams| Value::Object(streams.iter().map(|(name, info)| (name.clone(), info["digest"].clone())).collect())).unwrap_or(Value::Null)
            };
            let arguments = serde_json::from_str::<Value>(&call.arguments).map(|value| value.to_string()).unwrap_or_else(|_| call.arguments.trim().to_string());
            serde_json::json!([call.name, arguments, normalized, error, digest])
        }).collect();
        let fingerprint = Value::Array(evidence).to_string();
        if self.last.as_deref() == Some(&fingerprint) { self.repeats += 1; } else { self.last = Some(fingerprint); self.repeats = 1; }
        if self.repeats >= LOOP_THRESHOLD { LoopVerdict::Block { repeats: self.repeats } }
        else if self.repeats == LOOP_WARN_THRESHOLD { LoopVerdict::Warn { repeats: self.repeats } }
        else { LoopVerdict::Continue }
    }
}

fn stable(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(object.iter().filter(|(key, _)| !matches!(key.as_str(), "timestamp" | "updatedAt" | "updated_at" | "polledAt" | "polled_at" | "elapsed_ms" | "elapsedMs" | "duration_ms" | "durationMs")).map(|(key, value)| (key.clone(), stable(value))).collect()),
        Value::Array(items) => Value::Array(items.iter().map(stable).collect()),
        value => value.clone(),
    }
}

fn normalize(text: &str) -> String {
    let text = text.trim();
    if let Ok(value) = serde_json::from_str::<Value>(text) { return stable(&value).to_string(); }
    text.lines().filter(|line| {
        let lower = line.trim().to_ascii_lowercase();
        !lower.starts_with("[输出产物:") && !["timestamp:", "updated_at:", "elapsed_ms:", "duration_ms:", "polled_at:"].iter().any(|prefix| lower.starts_with(prefix))
    }).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn calls() -> Vec<ToolCallRef> { vec![ToolCallRef { id: "call".into(), name: "job_output".into(), arguments: r#"{"id":"job"}"#.into() }] }
    #[test]
    fn warns_at_three_and_stops_at_ten_after_execution() {
        let mut guard = LoopGuard::default();
        let results = vec![("same error".into(), true, None)];
        for round in 1..=10 {
            let verdict = guard.observe(&calls(), &results);
            assert_eq!(verdict, match round { 3 => LoopVerdict::Warn { repeats: 3 }, 10 => LoopVerdict::Block { repeats: 10 }, _ => LoopVerdict::Continue });
        }
    }
    #[test]
    fn progress_resets_but_display_timestamps_do_not() {
        let mut guard = LoopGuard::default();
        for timestamp in 1..=10 {
            let verdict = guard.observe(&calls(), &[(serde_json::json!({"progress":1,"timestamp":timestamp}).to_string(), false, None)]);
            if timestamp == 10 { assert_eq!(verdict, LoopVerdict::Block { repeats: 10 }); }
        }
        assert_eq!(guard.observe(&calls(), &[(r#"{"progress":2}"#.into(), false, None)]), LoopVerdict::Continue);
        guard.reset();
        assert_eq!(guard.observe(&calls(), &[(r#"{"progress":2}"#.into(), false, None)]), LoopVerdict::Continue);
    }
    #[test]
    fn new_actions_and_no_tool_evidence_reset() {
        let mut guard = LoopGuard::default();
        let results = vec![("same".into(), false, None)];
        for _ in 0..2 { guard.observe(&calls(), &results); }
        let mut other = calls(); other[0].arguments = r#"{"id":"other"}"#.into();
        assert_eq!(guard.observe(&other, &results), LoopVerdict::Continue);
        assert_eq!(guard.observe(&[], &[]), LoopVerdict::Continue);
    }

    #[test]
    fn hidden_middle_changes_are_progress_but_new_artifact_ids_are_not() {
        let mut guard = LoopGuard::default();
        let result = |id: &str, digest: &str| (format!("same preview\n[输出产物: {id}]"), false, Some(serde_json::json!({"outputArtifact":{"streams":{"stdout":{"digest":digest}}}})));
        assert_eq!(guard.observe(&calls(), &[result("1", "a")]), LoopVerdict::Continue);
        assert_eq!(guard.observe(&calls(), &[result("2", "a")]), LoopVerdict::Continue);
        assert_eq!(guard.observe(&calls(), &[result("3", "a")]), LoopVerdict::Warn { repeats: 3 });
        assert_eq!(guard.observe(&calls(), &[result("4", "b")]), LoopVerdict::Continue);
    }
}
