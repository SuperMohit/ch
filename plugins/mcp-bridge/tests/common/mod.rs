//! A session with the bridge and a model that calls the tools a test asks for.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ch_kernel::reexports::CancellationToken;
use ch_kernel::*;
use futures::stream;
use serde_json::{Value, json};

/// The test server binary.
pub const SERVER: &str = env!("CARGO_BIN_EXE_ch-mcp-test-server");

#[derive(Clone, Default)]
pub struct Model {
    replies: Arc<Mutex<VecDeque<ModelResponse>>>,
    pub requests: Arc<Mutex<Vec<ModelRequest>>>,
}

#[async_trait::async_trait]
impl ModelProvider for Model {
    fn id(&self) -> &str {
        "model"
    }

    async fn stream(
        &self,
        request: ModelRequest,
        _cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        self.requests.lock().unwrap().push(request);
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| text("done"));
        Ok(Box::pin(stream::iter(vec![Ok(ModelEvent::Completed(
            reply,
        ))])))
    }

    fn context_window(&self, _model: &str) -> Option<u64> {
        Some(1_000_000)
    }
}

struct ModelPlugin(Model);

impl Plugin for ModelPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new("model", "test model")
    }
    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        cx.add_provider(self.0.clone());
        Ok(())
    }
}

fn text(t: &str) -> ModelResponse {
    ModelResponse {
        message: Message::assistant(t),
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }
}

pub struct Harness {
    pub session: Session,
    pub model: Model,
}

/// `[[mcp_servers]]` running the test server as `name`, with `extra` TOML lines.
pub fn server_toml(name: &str, args: &[&str], extra: &str) -> String {
    let args: Vec<String> = args.iter().map(|a| format!("{a:?}")).collect();
    format!(
        "[[mcp_servers]]\nname = \"{name}\"\ncommand = {SERVER:?}\nargs = [{}]\n{extra}\n",
        args.join(", ")
    )
}

/// A started session with the bridge configured by `config`.
pub async fn harness(workspace: &Path, config: &str, services: Services) -> Harness {
    let model = Model::default();
    let plugins: Vec<Arc<dyn Plugin>> = vec![
        Arc::new(ModelPlugin(model.clone())),
        ch_mcp_bridge::plugin(),
    ];
    // `[model]` first: the servers' `[[mcp_servers]]` tables follow it.
    let config = Config::parse(&format!("[model]\nid = \"test-model\"\n{config}")).unwrap();
    let registry =
        RegistryBuilder::build_with_services(plugins, &config, workspace, services).unwrap();
    let mut session = Session::builder(Arc::new(registry), Arc::new(config))
        .workspace(workspace)
        .build()
        .unwrap();
    session.start().await;
    Harness { session, model }
}

impl Harness {
    /// Has the model call `tool` with `input` and returns the result.
    pub async fn call(&mut self, tool: &str, input: Value) -> (String, bool) {
        let id = format!("call-{}", self.session.history().len());
        self.model.replies.lock().unwrap().push_back(ModelResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: id.clone(),
                    name: tool.into(),
                    input,
                }],
            },
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        });
        self.session.run_turn("go").await;
        self.result(&id)
            .unwrap_or_else(|| panic!("no result for {tool}"))
    }

    pub fn result(&self, id: &str) -> Option<(String, bool)> {
        self.session.history().messages().iter().find_map(|m| {
            m.content.iter().find_map(|b| match b {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } if tool_use_id == id => Some((content.clone(), *is_error)),
                _ => None,
            })
        })
    }

    pub fn status(&self, name: &str) -> Option<SourceStatus> {
        let id = format!("mcp:{name}");
        self.session
            .tool_sources()
            .into_iter()
            .find(|(i, _)| *i == id)
            .map(|(_, s)| s)
    }

    /// Waits up to 5 s for `name`'s status to satisfy `ok`.
    pub async fn wait_status(
        &self,
        name: &str,
        ok: impl Fn(&SourceStatus) -> bool,
    ) -> SourceStatus {
        for _ in 0..250 {
            if let Some(s) = self.status(name)
                && ok(&s)
            {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{name}: status stayed {:?}", self.status(name));
    }

    /// The tool names of the last model request.
    pub fn last_tools(&self) -> Vec<String> {
        let requests = self.model.requests.lock().unwrap();
        requests
            .last()
            .map(|r| r.tools.iter().map(|t| t.name.clone()).collect())
            .unwrap_or_default()
    }

    pub async fn command(&mut self, line: &str) -> String {
        match self.session.run_command(line).await {
            Some(Ok(Outcome::Message(m))) => m,
            other => panic!("{line}: {other:?}"),
        }
    }
}

/// The value at `key` in the JSON text of the `env` tool.
pub fn env_report(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| json!({"raw": text}))
}
