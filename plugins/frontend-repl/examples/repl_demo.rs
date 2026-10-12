//! Try the REPL without a model: `cargo run -p ch-frontend-repl --example repl_demo`.
//!
//! A fake provider streams some markdown, then calls a fake `edit_file` tool that needs
//! approval and returns a diff, then answers again. Useful for checking rendering,
//! the spinner, approvals and interrupts by hand.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ch_kernel::reexports::CancellationToken;
use ch_kernel::*;
use serde_json::{Value, json};

struct Demo(AtomicUsize);

#[async_trait::async_trait]
impl ModelProvider for Demo {
    fn id(&self) -> &str {
        "demo"
    }
    async fn stream(
        &self,
        _request: ModelRequest,
        _cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        let (text, calls) = if n.is_multiple_of(2) {
            (
                "## Plan\n\nI'll **fix** the `greet` function:\n\n- read it\n- edit it\n\n```rust\nfn greet() {}\n```\n",
                vec![ContentBlock::ToolUse {
                    id: format!("call-{n}"),
                    name: "edit_file".into(),
                    input: json!({"path": "src/lib.rs", "old_text": "Hello", "new_text": "Hi"}),
                }],
            )
        } else {
            ("Done. The greeting now says *Hi*.", vec![])
        };
        let mut content = vec![ContentBlock::text(text)];
        content.extend(calls.clone());
        let resp = ModelResponse {
            message: Message {
                role: Role::Assistant,
                content,
            },
            stop_reason: if calls.is_empty() {
                StopReason::EndTurn
            } else {
                StopReason::ToolUse
            },
            usage: Usage {
                input_tokens: 1200,
                output_tokens: 80,
                ..Default::default()
            },
        };
        let chunks: Vec<String> = text
            .as_bytes()
            .chunks(7)
            .map(|c| String::from_utf8_lossy(c).to_string())
            .collect();
        let stream = futures::stream::unfold(
            (chunks.into_iter(), Some(resp), true),
            |(mut it, mut resp, first)| async move {
                if first {
                    tokio::time::sleep(Duration::from_millis(600)).await;
                }
                if let Some(c) = it.next() {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    return Some((Ok(ModelEvent::TextDelta(c)), (it, resp, false)));
                }
                resp.take()
                    .map(|r| (Ok(ModelEvent::Completed(r)), (it, None, false)))
            },
        );
        Ok(Box::pin(stream))
    }
}

struct FakeEdit;

#[async_trait::async_trait]
impl Tool for FakeEdit {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".into(),
            description: "fake".into(),
            input_schema: json!({"type": "object"}),
            read_only: false,
        }
    }
    async fn call(&self, _input: Value, _cx: &Cx) -> ToolOutput {
        tokio::time::sleep(Duration::from_millis(800)).await;
        ToolOutput::ok("Edited src/lib.rs").with_metadata(json!({
            "path": "src/lib.rs",
            "diff": "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn greet() -> &'static str {\n-    \"Hello\"\n+    \"Hi\"\n }\n"
        }))
    }
    fn summarize(&self, input: &Value) -> String {
        input["path"].as_str().unwrap_or("").to_string()
    }
}

struct AskEdits;

#[async_trait::async_trait]
impl Hook for AskEdits {
    fn id(&self) -> &str {
        "ask-edits"
    }
    fn points(&self) -> &[HookPoint] {
        &[HookPoint::PreTool]
    }
    async fn on(&self, _event: &mut HookEvent, _cx: &Cx) -> HookDecision {
        HookDecision::ask("edits need approval", Some("edit_file".into()))
    }
}

struct DemoPlugin;

impl Plugin for DemoPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new("demo", "demo provider and tools")
    }
    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        cx.add_provider(Demo(AtomicUsize::new(0)));
        cx.add_tool(FakeEdit);
        cx.add_hook(AskEdits);
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let workspace = tempfile::tempdir()?;
    let cfg = Config::parse(
        "[model]\nprovider = \"demo\"\nid = \"demo-model\"\n[model.pricing]\ninput = 3.0\noutput = 15.0\n",
    )?;
    let registry = Arc::new(RegistryBuilder::build(
        vec![Arc::new(DemoPlugin), ch_frontend_repl::plugin()],
        &cfg,
        workspace.path(),
    )?);
    let session = Session::builder(registry.clone(), Arc::new(cfg))
        .workspace(workspace.path())
        .approver("repl")
        .build()?;
    let code = registry.frontend(Some("repl"))?.run(session).await?;
    std::process::exit(code);
}
