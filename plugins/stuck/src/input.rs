//! Tool inputs: a canonical form to compare calls, and a check for malformed input.

use serde_json::{Map, Value};

/// `tool` plus its input with object keys sorted at every level, so two calls that
/// differ only in key order compare equal.
pub fn call_key(tool: &str, input: &Value) -> String {
    format!("{tool} {}", canonical(input))
}

fn canonical(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k.clone(), canonical(v)))
                    .collect::<Map<_, _>>(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// Whether `input` can't be what the model meant: JSON the provider couldn't parse
/// (kept as `{"_invalid_json": raw}`), or input that breaks the top level of the tool's
/// schema (a missing required key, a key of the wrong type, or an unknown key when the
/// schema forbids extra keys).
pub fn is_malformed(input: &Value, schema: &Value) -> bool {
    let Some(obj) = input.as_object() else {
        return true;
    };
    if obj.contains_key("_invalid_json") {
        return true;
    }
    let props = schema.get("properties").and_then(Value::as_object);
    let required = schema.get("required").and_then(Value::as_array);
    if required
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|k| !obj.contains_key(k))
    {
        return true;
    }
    let closed = schema.get("additionalProperties") == Some(&Value::Bool(false));
    obj.iter().any(|(k, v)| match props.and_then(|p| p.get(k)) {
        Some(prop) => !type_matches(v, prop.get("type")),
        None => closed,
    })
}

/// `type` may be a name or a list of names; anything else isn't checked.
fn type_matches(v: &Value, ty: Option<&Value>) -> bool {
    match ty {
        Some(Value::String(t)) => is_type(v, t),
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).any(|t| is_type(v, t)),
        _ => true,
    }
}

fn is_type(v: &Value, ty: &str) -> bool {
    match ty {
        "string" => v.is_string(),
        "integer" => v.is_i64() || v.is_u64(),
        "number" => v.is_number(),
        "boolean" => v.is_boolean(),
        "array" => v.is_array(),
        "object" => v.is_object(),
        "null" => v.is_null(),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn key_order_does_not_matter() {
        let a = json!({"path": "a", "opts": {"x": 1, "y": [{"b": 1, "a": 2}]}});
        let b: Value =
            serde_json::from_str(r#"{"opts":{"y":[{"a":2,"b":1}],"x":1},"path":"a"}"#).unwrap();
        assert_eq!(call_key("read_file", &a), call_key("read_file", &b));
        assert_ne!(call_key("read_file", &a), call_key("list_files", &a));
        assert_ne!(
            call_key("read_file", &json!({"path": "a"})),
            call_key("read_file", &json!({"path": "b"}))
        );
    }

    #[test]
    fn malformed_inputs() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "limit": {"type": "integer"},
                "any": {}
            },
            "required": ["path"],
            "additionalProperties": false
        });
        let ok = [
            json!({"path": "a"}),
            json!({"path": "a", "limit": 3, "any": [1]}),
        ];
        for input in ok {
            assert!(!is_malformed(&input, &schema), "{input}");
        }
        let bad = [
            json!({"_invalid_json": "{\"path\": "}),
            json!({}),
            json!({"path": 1}),
            json!({"path": "a", "limit": "3"}),
            json!({"path": "a", "extra": true}),
            json!("a"),
        ];
        for input in bad {
            assert!(is_malformed(&input, &schema), "{input}");
        }
        let open = json!({"type": "object", "properties": {}});
        assert!(!is_malformed(&json!({"extra": true}), &open));
        let either = json!({"properties": {"n": {"type": ["integer", "null"]}}});
        assert!(!is_malformed(&json!({"n": null}), &either));
        assert!(is_malformed(&json!({"n": "1"}), &either));
    }
}
