//! Tools the model can call.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cx::Cx;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    /// Written for the model. Built-in tools use the sections
    /// `When:` / `Protocol:` / `Error modes:` / `Side effects:`.
    pub description: String,
    /// JSON Schema (object) for the input.
    pub input_schema: Value,
    /// Read-only tools never change the workspace and may run concurrently.
    #[serde(default)]
    pub read_only: bool,
}

impl ToolSpec {
    /// Checks what model APIs require of a tool, so one bad tool can't make every
    /// request fail: a name of 1 to 64 ASCII letters, digits, `_` or `-`, and an input
    /// schema that is an object with `"type": "object"`, an optional `properties`
    /// object and an optional `required` list of strings.
    pub fn validate(&self) -> Result<(), String> {
        let name = &self.name;
        let valid_name = (1..=64).contains(&name.len())
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !valid_name {
            return Err(format!(
                "tool name `{name}` must be 1 to 64 ASCII letters, digits, `_` or `-`"
            ));
        }
        let schema = &self.input_schema;
        let Some(obj) = schema.as_object() else {
            return Err(format!(
                "tool `{name}`: input schema must be a JSON object, got {schema}"
            ));
        };
        if obj.get("type").and_then(Value::as_str) != Some("object") {
            return Err(format!(
                "tool `{name}`: input schema must have \"type\": \"object\", got {schema}"
            ));
        }
        if obj.get("properties").is_some_and(|p| !p.is_object()) {
            return Err(format!(
                "tool `{name}`: \"properties\" must be an object, got {schema}"
            ));
        }
        if let Some(required) = obj.get("required") {
            let strings = required
                .as_array()
                .is_some_and(|list| list.iter().all(Value::is_string));
            if !strings {
                return Err(format!(
                    "tool `{name}`: \"required\" must be an array of strings, got {required}"
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    /// Text returned to the model.
    pub content: String,
    pub is_error: bool,
    /// Structured data for frontends and telemetry (for example `{"diff": "..."}`).
    /// Never sent to the model.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub metadata: Value,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            metadata: Value::Null,
        }
    }

    /// A recoverable error the model sees as an observation. Say how to fix it.
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            metadata: Value::Null,
        }
    }

    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = metadata;
        self
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    /// Runs the tool. Return errors as `ToolOutput::error`; a panic is also caught and
    /// turned into an error observation, but don't rely on that. `cx.cancel` is
    /// cancelled when the user interrupts the turn.
    async fn call(&self, input: Value, cx: &Cx) -> ToolOutput;

    /// One-line human summary of a call for frontends, e.g. `src/main.rs` for
    /// `read_file`. Defaults to the compact JSON input.
    fn summarize(&self, input: &Value) -> String {
        let s = input.to_string();
        crate::util::truncate_middle(&s, 120)
    }
}
