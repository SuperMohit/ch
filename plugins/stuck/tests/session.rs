//! Stuck detection through a real session.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use ch_kernel::reexports::async_trait;
use ch_kernel::{
    Config, ContentBlock, Cx, Plugin, PluginManifest, RegisterCx, RegistryBuilder, Role, Session,
    TerminationReason, Tool, ToolOutput, ToolSpec,
};
use ch_provider_scripted::{ScriptStep, ScriptedPlugin};
use serde_json::{Value, json};

/// `check`: fails every time, counting how often it really ran.
struct Check(Arc<AtomicU32>);

#[async_trait]
impl Tool for Check {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "check".into(),
            description: "When: tests. Protocol: -. Error modes: always. Side effects: none."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
            read_only: false,
        }
    }

    async fn call(&self, _input: Value, _cx: &Cx) -> ToolOutput {
        self.0.fetch_add(1, Ordering::SeqCst);
        ToolOutput::error("exit 1: test_parse failed")
    }
}

struct CheckPlugin(Arc<AtomicU32>);

impl Plugin for CheckPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new("check", "test tool")
    }
    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        cx.add_tool(Check(self.0.clone()));
        Ok(())
    }
}

/// Runs one turn of `steps` and returns the result, the session and how often `check`
/// really ran.
async fn run(steps: Vec<ScriptStep>) -> (TerminationReason, Session, u32) {
    let runs = Arc::new(AtomicU32::new(0));
    let plugins: Vec<Arc<dyn Plugin>> = vec![
        Arc::new(ScriptedPlugin::from_steps(steps)),
        Arc::new(CheckPlugin(runs.clone())),
        ch_stuck::plugin(),
    ];
    let config = Arc::new(Config::parse("plugins.enable = [\"provider-scripted\"]").unwrap());
    let tmp = tempfile::tempdir().unwrap();
    let registry = RegistryBuilder::build(plugins, &config, tmp.path()).unwrap();
    let mut session = Session::builder(Arc::new(registry), config)
        .workspace(tmp.path())
        .build()
        .unwrap();
    let result = session.run_turn("fix it").await;
    let n = runs.load(Ordering::SeqCst);
    (result.reason, session, n)
}

/// Tool results in the history, in order.
fn results(session: &Session) -> Vec<String> {
    session
        .history()
        .messages()
        .iter()
        .filter(|m| m.role == Role::User)
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_call_that_keeps_failing_is_refused_then_ends_the_turn() {
    let same = || ScriptStep::tool("check", json!({"path": "src"}));
    let (reason, session, runs) = run((0..10).map(|_| same()).collect()).await;

    assert_eq!(
        reason,
        TerminationReason::HookStop {
            source: "stuck".into(),
            reason: "`check` failed 8 times in a row with the same input".into(),
        }
    );
    assert_eq!(runs, 2, "only the first two identical calls run");
    let results = results(&session);
    assert_eq!(results.len(), 8);
    assert!(
        results[2].contains("already failed 2 times in a row"),
        "{}",
        results[2]
    );
    assert!(
        results[2].contains("exit 1: test_parse failed"),
        "{}",
        results[2]
    );
    assert!(
        results[4].contains("`check` has failed 5 times in a row"),
        "{}",
        results[4]
    );
}

#[tokio::test]
async fn three_malformed_responses_end_the_turn() {
    let steps = vec![
        ScriptStep::tool("check", json!({})),
        ScriptStep::tool("check", json!({"_invalid_json": "{\"pa"})),
        ScriptStep::tool("check", json!({"path": 3})),
        ScriptStep::text("unreachable"),
    ];
    let (reason, session, _) = run(steps).await;
    assert_eq!(
        reason,
        TerminationReason::HookStop {
            source: "stuck".into(),
            reason: "the model sent malformed tool input in 3 responses in a row".into(),
        }
    );
    // The third response's call is still answered, so the history stays valid.
    assert_eq!(results(&session).len(), 3);
}

#[tokio::test]
async fn different_calls_never_trip_it() {
    let steps = (0..6)
        .map(|i| ScriptStep::tool("check", json!({"path": format!("src/{i}")})))
        .chain([ScriptStep::text("gave up")])
        .collect();
    let (reason, _, runs) = run(steps).await;
    assert_eq!(reason, TerminationReason::Done);
    assert_eq!(runs, 6);
}
