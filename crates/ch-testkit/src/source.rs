//! A scripted [`ToolSource`] for tests: it offers the tools it is given, and a test can
//! change them, make it exit, fail or start slowly.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ch_kernel::reexports::async_trait;
use ch_kernel::{
    Cx, Plugin, PluginManifest, RegisterCx, Tool, ToolOutput, ToolSink, ToolSource, ToolSpec,
};
use serde_json::{Value, json};

/// A tool source driven by the test. Clones share their state, so keep one to drive the
/// source after the session started it.
#[derive(Clone)]
pub struct ScriptedSource {
    id: String,
    prefix: String,
    read_only: Vec<String>,
    startup_timeout: Duration,
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    sink: Option<ToolSink>,
    tools: Vec<Arc<dyn Tool>>,
    fail: Option<String>,
    delay: Duration,
    starts: usize,
    stops: usize,
}

impl ScriptedSource {
    /// A source named `id` whose tools start with `prefix`, such as `mcp__fake__`.
    pub fn new(id: &str, prefix: &str) -> Self {
        Self {
            id: id.into(),
            prefix: prefix.into(),
            read_only: Vec::new(),
            startup_timeout: Duration::from_secs(10),
            state: Arc::default(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The tools reported when the source starts.
    pub fn with_tools(self, tools: Vec<Arc<dyn Tool>>) -> Self {
        self.state().tools = tools;
        self
    }

    /// Makes `start` fail with `reason`.
    pub fn failing(self, reason: &str) -> Self {
        self.state().fail = Some(reason.into());
        self
    }

    /// Makes `start` take `delay` before reporting its tools.
    pub fn slow(self, delay: Duration, startup_timeout: Duration) -> Self {
        self.state().delay = delay;
        Self {
            startup_timeout,
            ..self
        }
    }

    /// Tool names the user's config marks read-only.
    pub fn read_only(mut self, names: &[&str]) -> Self {
        self.read_only = names.iter().map(|n| n.to_string()).collect();
        self
    }

    /// Reports a new list of tools, as a server does after `tools/list_changed`.
    pub fn set_tools(&self, tools: Vec<Arc<dyn Tool>>) {
        let sink = {
            let mut st = self.state();
            st.tools = tools.clone();
            st.sink.clone()
        };
        sink.expect("the source wasn't started").set_tools(tools);
    }

    /// Reports that the source stopped on its own.
    pub fn exit(&self, reason: &str) {
        let sink = self.state().sink.clone();
        sink.expect("the source wasn't started").exited(reason);
    }

    pub fn starts(&self) -> usize {
        self.state().starts
    }

    pub fn stops(&self) -> usize {
        self.state().stops
    }

    /// A plugin with id `plugin` that registers `sources`.
    pub fn plugin(plugin: &str, sources: Vec<ScriptedSource>) -> Arc<dyn Plugin> {
        Arc::new(SourcePlugin {
            id: plugin.into(),
            sources,
        })
    }
}

#[async_trait]
impl ToolSource for ScriptedSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn prefix(&self) -> String {
        self.prefix.clone()
    }

    fn read_only_tools(&self) -> Vec<String> {
        self.read_only.clone()
    }

    fn startup_timeout(&self) -> Duration {
        self.startup_timeout
    }

    async fn start(&self, sink: ToolSink, _cx: &Cx) -> anyhow::Result<()> {
        let (delay, fail) = {
            let mut st = self.state();
            st.starts += 1;
            st.sink = Some(sink.clone());
            (st.delay, st.fail.clone())
        };
        tokio::time::sleep(delay).await;
        if let Some(reason) = fail {
            anyhow::bail!("{reason}");
        }
        let tools = self.state().tools.clone();
        sink.set_tools(tools);
        Ok(())
    }

    async fn stop(&self) {
        self.state().stops += 1;
    }
}

struct SourcePlugin {
    id: String,
    sources: Vec<ScriptedSource>,
}

impl Plugin for SourcePlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new(&self.id, "scripted tool sources")
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        for s in &self.sources {
            cx.add_tool_source(Arc::new(s.clone()));
        }
        Ok(())
    }
}

/// A tool named `name` that answers `<name> ran with <input>`. It says it is read-only,
/// which a session ignores for tools from a source.
pub fn echo_tool(name: &str) -> Arc<dyn Tool> {
    echo_tool_described(name, "Echoes its input.")
}

/// [`echo_tool`] with a chosen description.
pub fn echo_tool_described(name: &str, description: &str) -> Arc<dyn Tool> {
    Arc::new(Echo(ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema: json!({"type": "object", "properties": {"x": {"type": "string"}}}),
        read_only: true,
    }))
}

struct Echo(ToolSpec);

#[async_trait]
impl Tool for Echo {
    fn spec(&self) -> ToolSpec {
        self.0.clone()
    }

    async fn call(&self, input: Value, _cx: &Cx) -> ToolOutput {
        ToolOutput::ok(format!("{} ran with {input}", self.0.name))
    }
}
