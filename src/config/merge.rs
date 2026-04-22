// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Deep merge utility for JSON values.

use serde_json::Value;

/// Deep-merges `overlay` into `base` at the JSON object level.
///
/// - Objects: recursively merge keys. Overlay keys win on conflict.
/// - Arrays: overlay replaces base entirely (no concatenation).
/// - Scalars: overlay replaces base.
/// - `Null` overlay replaces base (explicit null = clear).
#[must_use]
pub fn deep_merge(base: &Value, overlay: &Value) -> Value {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            let mut merged = base_map.clone();
            for (key, overlay_val) in overlay_map {
                let merged_val = base_map.get(key).map_or_else(
                    || overlay_val.clone(),
                    |base_val| deep_merge(base_val, overlay_val),
                );
                merged.insert(key.clone(), merged_val);
            }
            Value::Object(merged)
        }
        // Arrays, scalars, null: overlay replaces entirely.
        (_, overlay) => overlay.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_deep_merge_objects() {
        let base = json!({"a": {"x": 1, "y": 2}, "b": 3});
        let overlay = json!({"a": {"y": 99, "z": 100}});

        let result = deep_merge(&base, &overlay);

        assert_eq!(result["a"]["x"], 1); // preserved from base
        assert_eq!(result["a"]["y"], 99); // overlay wins
        assert_eq!(result["a"]["z"], 100); // new from overlay
        assert_eq!(result["b"], 3); // preserved from base
    }

    #[test]
    fn test_deep_merge_arrays_replace() {
        let base = json!({"arr": [1, 2, 3]});
        let overlay = json!({"arr": [4, 5]});

        let result = deep_merge(&base, &overlay);

        assert_eq!(result["arr"], json!([4, 5]));
    }

    #[test]
    fn test_deep_merge_scalars_replace() {
        let base = json!({"x": 1, "y": "hello"});
        let overlay = json!({"x": 42, "y": "world"});

        let result = deep_merge(&base, &overlay);

        assert_eq!(result["x"], 42);
        assert_eq!(result["y"], "world");
    }

    #[test]
    fn test_deep_merge_null_overlay() {
        let base = json!({"x": 1, "y": {"a": 2}});
        let overlay = json!({"x": null, "y": null});

        let result = deep_merge(&base, &overlay);

        assert!(result["x"].is_null());
        assert!(result["y"].is_null());
    }

    #[test]
    fn test_deep_merge_missing_keys_preserved() {
        let base = json!({"a": 1, "b": 2, "c": {"d": 3}});
        let overlay = json!({"b": 20});

        let result = deep_merge(&base, &overlay);

        assert_eq!(result["a"], 1); // base preserved
        assert_eq!(result["b"], 20); // overlay wins
        assert_eq!(result["c"]["d"], 3); // base nested preserved
    }
}
