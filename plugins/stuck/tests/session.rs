//! Stuck detection through a real session.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use ch_kernel::reexports::async_trait;
use ch_kernel::{
    Config, ContentBlock, Cx, Hook, HookDecision, HookEvent, HookPoint, Plugin, PluginManifest,
    RegisterCx, RegistryBuilder, Role, Session, TerminationReason, Tool, ToolOutput, ToolSpec,
};
use ch_provider_scripted::{ScriptStep, ScriptedPlugin};
use serde_json::{Value, json};

/// `check`: fails every time (succeeds for `{"path": "ok"}`), counting how often it
/// really ran.
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
                "properties": {"path": {"type": "string"}, "depth": {"type": "integer"}},
                "required": ["path"],
                "additionalProperties": false
            }),
            read_only: false,
        }
    }

    async fn call(&self, input: Value, _cx: &Cx) -> ToolOutput {
        self.0.fetch_add(1, Ordering::SeqCst);
        if input["path"] == "ok" {
            ToolOutput::ok("fine")
        } else {
            ToolOutput::error("exit 1: test_parse failed")
        }
    }
}

/// Denies the second call it sees, like a permission check would.
#[derive(Default)]
struct DenySecond(AtomicU32);

#[async_trait]
impl Hook for DenySecond {
    fn id(&self) -> &str {
        "policy"
    }
    fn points(&self) -> &[HookPoint] {
        &[HookPoint::PreTool]
    }
    fn priority(&self) -> i32 {
        -100
    }
    async fn on(&self, _event: &HookEvent, _cx: &Cx) -> HookDecision {
        match self.0.fetch_add(1, Ordering::SeqCst) {
            1 => HookDecision::deny("not allowed"),
            _ => HookDecision::Continue,
        }
    }
}

struct CheckPlugin(Arc<AtomicU32>, bool);

impl Plugin for CheckPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new("check", "test tool")
    }
    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        cx.add_tool(Check(self.0.clone()));
        if self.1 {
            cx.add_hook(DenySecond::default());
        }
        Ok(())
    }
}

/// Runs one turn of `steps` and returns the result, the session and how often `check`
/// really ran.
async fn run(steps: Vec<ScriptStep>) -> (TerminationReason, Session, u32) {
    run_turns(steps, &["fix it"], false).await
}

/// Runs a turn per prompt; `deny_second` adds a hook that denies the second call.
async fn run_turns(
    steps: Vec<ScriptStep>,
    prompts: &[&str],
    deny_second: bool,
) -> (TerminationReason, Session, u32) {
    let runs = Arc::new(AtomicU32::new(0));
    let plugins: Vec<Arc<dyn Plugin>> = vec![
        Arc::new(ScriptedPlugin::from_steps(steps)),
        Arc::new(CheckPlugin(runs.clone(), deny_second)),
        ch_stuck::plugin(),
    ];
    let config = Arc::new(Config::parse("plugins.enable = [\"provider-scripted\"]").unwrap());
    let tmp = tempfile::tempdir().unwrap();
    let registry = RegistryBuilder::build(plugins, &config, tmp.path()).unwrap();
    let mut session = Session::builder(Arc::new(registry), config)
        .workspace(tmp.path())
        .build()
        .unwrap();
    let mut reason = TerminationReason::Done;
    for prompt in prompts {
        reason = session.run_turn(*prompt).await.reason;
    }
    let n = runs.load(Ordering::SeqCst);
    (reason, session, n)
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
            reason: "`check` failed 2 times in a row with the same input and was refused 6 \
                     more times"
                .into(),
        }
    );
    assert_eq!(runs, 2, "only the first two identical calls run");
    let results = results(&session);
    assert_eq!(results.len(), 8);
    assert!(
        results[2].starts_with(
            "Permission denied (stuck): this exact `check` call failed 2 times in a row"
        ),
        "{}",
        results[2]
    );
    assert!(
        results[2].contains("exit 1: test_parse failed"),
        "{}",
        results[2]
    );
    assert!(
        results[4].contains("5 times in a row, and it has not succeeded once"),
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

#[tokio::test]
async fn optional_nulls_and_extra_keys_are_not_malformed() {
    let ok = |extra: Value| {
        let mut input = json!({"path": "ok"});
        input
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        ScriptStep::tool("check", input)
    };
    let steps = vec![
        ok(json!({"depth": null})),
        ok(json!({"depth": 2.0})),
        ok(json!({"description": "look"})),
        ok(json!({"depth": null, "x": 1})),
        ScriptStep::text("done"),
    ];
    let (reason, _, runs) = run(steps).await;
    assert_eq!(reason, TerminationReason::Done);
    assert_eq!(runs, 4);
}

#[tokio::test]
async fn a_new_prompt_starts_the_counts_again() {
    let same = || ScriptStep::tool("check", json!({"path": "src"}));
    let steps = vec![
        same(),
        same(),
        ScriptStep::text("stuck"),
        same(),
        ScriptStep::text("tried again"),
    ];
    let (reason, session, runs) = run_turns(steps, &["fix it", "try again"], false).await;
    assert_eq!(reason, TerminationReason::Done);
    assert_eq!(runs, 3, "the first call of the new turn runs");
    assert!(!results(&session)[2].contains("stuck"));
}

#[tokio::test]
async fn a_permission_denial_does_not_count() {
    let same = || ScriptStep::tool("check", json!({"path": "src"}));
    let steps = vec![same(), same(), same(), same(), ScriptStep::text("done")];
    let (_, session, runs) = run_turns(steps, &["fix it"], true).await;
    let results = results(&session);
    assert!(
        results[1].starts_with("Permission denied (policy)"),
        "{}",
        results[1]
    );
    // Two real failures (calls 1 and 3) around the denial, then the refusal.
    assert_eq!(runs, 2);
    assert!(
        results[3].starts_with("Permission denied (stuck)"),
        "{}",
        results[3]
    );
}
