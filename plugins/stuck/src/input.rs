//! Whether a tool call's input is malformed.

use serde_json::Value;

/// Whether `input` is something no tool accepts: JSON the provider couldn't parse (kept
/// as `{"_invalid_json": raw}`), not an object, or a required key that is missing, null
/// or of the wrong type.
///
/// Optional keys and extra keys aren't checked: the tools accept `null` for an optional
/// key, numbers like `30.0` for integers, and ignore keys they don't know, so flagging
/// those would end turns whose calls all worked.
pub fn is_malformed(input: &Value, schema: &Value) -> bool {
    let Some(obj) = input.as_object() else {
        return true;
    };
    if obj.contains_key("_invalid_json") {
        return true;
    }
    let props = schema.get("properties");
    schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|key| match obj.get(key) {
            None | Some(Value::Null) => true,
            Some(v) => {
                let ty = props.and_then(|p| p.get(key)).and_then(|p| p.get("type"));
                !type_matches(v, ty)
            }
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
        "integer" | "number" => v.is_number(),
        "boolean" => v.is_boolean(),
        "array" => v.is_array(),
        "object" => v.is_object(),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn only_what_no_tool_accepts_is_malformed() {
        let schema = json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "timeout_seconds": {"type": "integer"},
                "cwd": {"type": "string"},
                "limit": {"type": "integer"}
            },
            "required": ["command"],
            "additionalProperties": false
        });
        let accepted = [
            json!({"command": "ls"}),
            json!({"command": "ls", "cwd": null, "timeout_seconds": null}),
            json!({"command": "ls", "timeout_seconds": 30.0}),
            json!({"command": "ls", "description": "list files"}),
            json!({"command": "ls", "limit": "3"}),
        ];
        for input in accepted {
            assert!(!is_malformed(&input, &schema), "{input}");
        }
        let malformed = [
            json!({"_invalid_json": "{\"command\": "}),
            json!({}),
            json!({"command": null}),
            json!({"command": 1}),
            json!("ls"),
        ];
        for input in malformed {
            assert!(is_malformed(&input, &schema), "{input}");
        }
        let either = json!({
            "properties": {"n": {"type": ["integer", "string"]}},
            "required": ["n"]
        });
        assert!(!is_malformed(&json!({"n": "1"}), &either));
        assert!(is_malformed(&json!({"n": true}), &either));
    }
}
