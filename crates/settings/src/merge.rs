use serde_json::{Map, Value};

/// Recursively merges `patch` over `base`: nested objects merge key by key,
/// arrays and scalars replace wholesale. `null` patches a key to null; it
/// does not erase the key.
pub fn deep_merge(base: &Value, patch: &Value) -> Value {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            let mut out: Map<String, Value> = b.clone();
            for (key, value) in p {
                let merged = match out.get(key) {
                    Some(existing) => deep_merge(existing, value),
                    None => value.clone(),
                };
                out.insert(key.clone(), merged);
            }
            Value::Object(out)
        }
        _ => patch.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::deep_merge;
    use serde_json::json;

    #[test]
    fn objects_merge_recursively() {
        let base = json!({"a": {"x": 1, "y": 2}, "keep": true});
        let patch = json!({"a": {"y": 3, "z": 4}, "added": "v"});
        assert_eq!(
            deep_merge(&base, &patch),
            json!({"a": {"x": 1, "y": 3, "z": 4}, "keep": true, "added": "v"})
        );
    }

    #[test]
    fn scalars_and_arrays_replace() {
        let base = json!({"list": [1, 2], "n": 1});
        let patch = json!({"list": [3], "n": 2});
        assert_eq!(deep_merge(&base, &patch), json!({"list": [3], "n": 2}));
    }
}
