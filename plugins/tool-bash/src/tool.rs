//! The `bash` tool.

use std::time::Duration;

use async_trait::async_trait;
use ch_kernel::{Cx, ExecRequest, Tool, ToolOutput, ToolSpec};
use serde_json::{Value, json};

use crate::BashConfig;
use crate::cwd::resolve_cwd;
use crate::description;
use crate::format::render;

/// Which description the model sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescriptionVariant {
    /// The dedicated file tools exist; steer file work to them.
    WithFileTools,
    /// Bash-only mode: the shell does all file work and nothing validates edits.
    BashOnly,
}

/// Runs shell commands through the session's [`ch_kernel::Executor`].
#[derive(Debug, Clone)]
pub struct BashTool {
    config: BashConfig,
    variant: DescriptionVariant,
}

impl BashTool {
    pub fn new(config: BashConfig, variant: DescriptionVariant) -> Self {
        Self { config, variant }
    }

    pub fn variant(&self) -> DescriptionVariant {
        self.variant
    }

    /// Effective timeout plus an optional note when the request was adjusted.
    fn timeout(&self, input: &Value) -> Result<(u64, Option<String>), String> {
        let max = self.config.max_timeout_secs.max(1);
        let default = self.config.default_timeout_secs.clamp(1, max);
        let Some(raw) = input.get("timeout_seconds").filter(|v| !v.is_null()) else {
            return Ok((default, None));
        };
        let Some(requested) = raw.as_f64() else {
            return Err(format!(
                "timeout_seconds must be a number of seconds (got {raw}). Omit it to use \
                 the default of {default}s."
            ));
        };
        if requested < 1.0 {
            return Ok((
                1,
                Some(format!(
                    "[note: timeout_seconds {requested} is below the minimum; using 1s]"
                )),
            ));
        }
        let secs = requested.ceil() as u64;
        if secs > max {
            return Ok((
                max,
                Some(format!(
                    "[note: timeout_seconds {secs} is above the maximum of {max}; using \
                     {max}s. For longer jobs, run them in the background and poll.]"
                )),
            ));
        }
        Ok((secs, None))
    }
}

#[async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        let (default, max) = (
            self.config.default_timeout_secs,
            self.config.max_timeout_secs,
        );
        let description = match self.variant {
            DescriptionVariant::WithFileTools => description::with_file_tools(default, max),
            DescriptionVariant::BashOnly => description::bash_only(default, max),
        };
        ToolSpec {
            name: "bash".into(),
            description,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The command to run with /bin/sh -c."
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": max,
                        "description": format!("Seconds before the command is killed (default {default}, max {max}).")
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Directory to run in, relative to the workspace root (default: the root)."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            read_only: false,
        }
    }

    async fn call(&self, input: Value, cx: &Cx) -> ToolOutput {
        let Some(command) = input
            .get("command")
            .and_then(Value::as_str)
            .filter(|c| !c.trim().is_empty())
        else {
            return ToolOutput::error(
                "Missing `command`: pass the shell command to run as a non-empty string.",
            );
        };
        let (timeout_secs, note) = match self.timeout(&input) {
            Ok(t) => t,
            Err(e) => return ToolOutput::error(e),
        };
        let cwd_arg = match input.get("cwd") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.as_str()),
            Some(other) => {
                return ToolOutput::error(format!(
                    "cwd must be a string path relative to the workspace (got {other})."
                ));
            }
        };
        let cwd = match resolve_cwd(cx.workspace(), cwd_arg) {
            Ok(dir) => dir,
            Err(e) => return ToolOutput::error(e),
        };
        let Some(executor) = cx.executor.clone() else {
            return ToolOutput::error(
                "No executor is configured, so bash cannot run commands. Enable the \
                 `executor-local` plugin (or another executor) in the config.",
            );
        };

        let request = ExecRequest {
            command: command.to_string(),
            cwd,
            timeout: Duration::from_secs(timeout_secs),
            env: Vec::new(),
            stdin: None,
        };
        let result = match executor.run(request, cx.cancel.clone()).await {
            Ok(r) => r,
            Err(e) => {
                return ToolOutput::error(format!(
                    "Failed to start the command: {e:#}. Check that the cwd exists and \
                     the executor `{}` is working.",
                    executor.id()
                ));
            }
        };

        let mut content = render(&result, timeout_secs);
        if let Some(note) = note {
            content = format!("{note}\n{content}");
        }
        let failed = !result.success() || result.timed_out || result.cancelled;
        let metadata = json!({
            "exit_code": result.exit_code,
            "timed_out": result.timed_out,
            "cancelled": result.cancelled,
            "duration_ms": result.duration_ms,
        });
        let output = if failed {
            ToolOutput::error(content)
        } else {
            ToolOutput::ok(content)
        };
        output.with_metadata(metadata)
    }

    fn summarize(&self, input: &Value) -> String {
        let command = input.get("command").and_then(Value::as_str).unwrap_or("");
        let mut lines = command.trim().lines();
        let first = lines.next().unwrap_or("").trim();
        let more = lines.next().is_some();
        let max = 80;
        let mut line: String = first.chars().take(max).collect();
        if more || first.chars().count() > max {
            line.push('…');
        }
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> BashTool {
        BashTool::new(BashConfig::default(), DescriptionVariant::WithFileTools)
    }

    #[test]
    fn timeout_defaults_and_clamps() {
        let t = tool();
        assert_eq!(t.timeout(&json!({})).unwrap(), (120, None));
        assert_eq!(
            t.timeout(&json!({"timeout_seconds": 30})).unwrap(),
            (30, None)
        );
        let (secs, note) = t.timeout(&json!({"timeout_seconds": 9000})).unwrap();
        assert_eq!(secs, 600);
        assert!(note.unwrap().contains("maximum of 600"));
        let (secs, note) = t.timeout(&json!({"timeout_seconds": 0})).unwrap();
        assert_eq!(secs, 1);
        assert!(note.is_some());
        assert!(t.timeout(&json!({"timeout_seconds": "soon"})).is_err());
    }

    #[test]
    fn summarize_uses_first_line() {
        let t = tool();
        assert_eq!(t.summarize(&json!({"command": "cargo test"})), "cargo test");
        assert_eq!(
            t.summarize(&json!({"command": "cd a &&\nmake"})),
            "cd a &&…"
        );
        let long = "x".repeat(200);
        assert_eq!(t.summarize(&json!({ "command": long })).chars().count(), 81);
    }
}
