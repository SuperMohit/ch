//! The plan through real sessions: what each request holds, resume and `/clear`.

use std::path::Path;
use std::sync::Arc;

use ch_kernel::{
    Config, ModelRequest, Plugin, RegistryBuilder, Session, SessionStore, TerminationReason,
};
use ch_planning::NO_PLAN_REMINDER;
use ch_provider_scripted::{ScriptStep, ScriptedPlugin, ScriptedProvider};
use serde_json::{Value, json};

fn step(content: &str, status: &str) -> Value {
    json!({"content": content, "activeForm": format!("{content}ing"), "status": status})
}

/// A session over `steps`, saved in `store` when given. Returns the provider too, to
/// read the requests.
fn session(steps: Vec<ScriptStep>, ws: &Path, store: Option<&Path>) -> (Session, ScriptedProvider) {
    let scripted = ScriptedPlugin::from_steps(steps);
    let provider = scripted.provider().unwrap();
    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(scripted), ch_planning::plugin()];
    let config = Arc::new(Config::parse("plugins.enable = [\"provider-scripted\"]").unwrap());
    let registry = RegistryBuilder::build(plugins, &config, ws).unwrap();
    let mut builder = Session::builder(Arc::new(registry), config).workspace(ws);
    if let Some(dir) = store {
        builder = builder.store(SessionStore::new(dir));
    }
    (builder.build().unwrap(), provider)
}

fn text(r: &ModelRequest) -> String {
    serde_json::to_string(&r.messages).unwrap()
}

#[tokio::test]
async fn the_plan_reaches_the_model_through_its_calls_and_one_reminder_per_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut session, provider) = session(
        vec![
            ScriptStep::tool(
                "update_plan",
                json!({"plan": [step("Read", "in_progress"), step("Fix", "pending")]}),
            ),
            ScriptStep::tool(
                "update_plan",
                json!({"plan": [step("Read", "completed"), step("Fix", "in_progress")]}),
            ),
            ScriptStep::text("done for now"),
            ScriptStep::text("still on it"),
        ],
        tmp.path(),
        None,
    );
    for prompt in ["fix it", "and?"] {
        assert_eq!(
            session.run_turn(prompt).await.reason,
            TerminationReason::Done
        );
    }

    let requests: Vec<String> = provider.requests().iter().map(text).collect();
    assert_eq!(requests.len(), 4);
    // Turn 1: the nudge is added to its first request only. It stays in the history, so
    // later requests repeat that one copy and add none.
    for r in &requests {
        assert_eq!(r.matches(NO_PLAN_REMINDER).count(), 1);
    }
    // Within a turn the plan is only in the calls the model made: one copy of it per
    // update_plan call, and the tool results add none.
    assert_eq!(requests[1].matches("activeForm").count(), 2);
    assert_eq!(requests[2].matches("activeForm").count(), 4);
    assert!(!requests[1].contains("The current plan"));
    assert!(!requests[2].contains("The current plan"));
    // Turn 2's first request adds the current plan once, as a reminder.
    assert_eq!(requests[3].matches("activeForm").count(), 4);
    assert_eq!(requests[3].matches("The current plan").count(), 1);
    assert!(requests[3].contains("1. [completed] Read\\n2. [in_progress] Fix"));
}

#[tokio::test]
async fn a_resumed_session_keeps_its_plan_and_clear_drops_it() {
    let tmp = tempfile::tempdir().unwrap();
    let sessions = tmp.path().join("sessions");
    let (mut first, _) = session(
        vec![
            ScriptStep::tool(
                "update_plan",
                json!({"plan": [step("Read", "completed"), step("Fix", "in_progress")]}),
            ),
            ScriptStep::text("stopping here"),
        ],
        tmp.path(),
        Some(&sessions),
    );
    assert_eq!(
        first.run_turn("fix it").await.reason,
        TerminationReason::Done
    );
    let id = first.id().to_string();
    first.end().await;
    drop(first);

    // A new process: a fresh registry and PlanStore, restoring the saved session.
    let (mut resumed, provider) = session(
        vec![ScriptStep::text("resumed"), ScriptStep::text("cleared")],
        tmp.path(),
        Some(&sessions),
    );
    resumed
        .restore(SessionStore::new(&sessions).load(&id).unwrap())
        .unwrap();
    resumed.run_turn("go on").await;
    resumed.clear().unwrap();
    resumed.run_turn("something else").await;

    let requests = provider.requests();
    let resumed_turn = text(&requests[0]);
    assert!(
        resumed_turn.contains("2. [in_progress] Fix"),
        "{resumed_turn}"
    );
    // The only nudge is the one the first turn stored; the resumed turn adds the plan.
    assert_eq!(resumed_turn.matches(NO_PLAN_REMINDER).count(), 1);
    let after_clear = text(&requests[1]);
    assert!(after_clear.contains(NO_PLAN_REMINDER), "{after_clear}");
    assert!(!after_clear.contains("The current plan"));
}

fn look() -> ScriptStep {
    ScriptStep::tool("look", json!({}))
}

fn refreshes(request: &str) -> usize {
    request.matches("Your plan, shown again").count()
}

#[tokio::test]
async fn a_long_turn_sees_the_plan_again_and_keeps_each_refresh() {
    let tmp = tempfile::tempdir().unwrap();
    let mut steps = vec![ScriptStep::tool(
        "update_plan",
        json!({"plan": [step("Read", "in_progress"), step("Fix", "pending")]}),
    )];
    steps.extend((0..4).map(|_| look()));
    steps.push(ScriptStep::tool(
        "update_plan",
        json!({"plan": [step("Read", "completed"), step("Fix", "in_progress")]}),
    ));
    steps.extend((0..3).map(|_| look()));
    steps.push(ScriptStep::text("done"));
    let scripted = ScriptedPlugin::from_steps(steps);
    let provider = scripted.provider().unwrap();
    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(scripted), ch_planning::plugin()];
    let config = Config::parse(
        "plugins.enable = [\"provider-scripted\"]\n\
         [plugins.planning]\nrefresh_after_steps = 3",
    )
    .unwrap();
    let config = Arc::new(config);
    let registry = RegistryBuilder::build(plugins, &config, tmp.path()).unwrap();
    let mut session = Session::builder(Arc::new(registry), config)
        .workspace(tmp.path())
        .build()
        .unwrap();
    assert_eq!(
        session.run_turn("fix it").await.reason,
        TerminationReason::Done
    );

    let requests: Vec<String> = provider.requests().iter().map(text).collect();
    let counts: Vec<usize> = requests.iter().map(|r| refreshes(r)).collect();
    // Three requests after the one that first holds an update, the plan is shown
    // again, and the history keeps it.
    assert_eq!(counts, [0, 0, 0, 0, 1, 1, 1, 1, 1, 2]);
    assert!(requests[4].contains("1. [in_progress] Read\\n2. [pending] Fix"));
    let last = &requests[9];
    let second = last.rfind("Your plan, shown again").unwrap();
    assert!(last[second..].contains("1. [completed] Read\\n2. [in_progress] Fix"));
}

#[tokio::test]
async fn by_default_the_plan_comes_back_ten_requests_after_an_update() {
    let tmp = tempfile::tempdir().unwrap();
    let mut steps = vec![ScriptStep::tool(
        "update_plan",
        json!({"plan": [step("Read", "in_progress"), step("Fix", "pending")]}),
    )];
    steps.extend((0..11).map(|_| look()));
    steps.push(ScriptStep::text("done"));
    let (mut session, provider) = session(steps, tmp.path(), None);
    session.run_turn("fix it").await;

    let requests: Vec<String> = provider.requests().iter().map(text).collect();
    assert_eq!(requests.len(), 13);
    assert_eq!(refreshes(&requests[10]), 0);
    assert_eq!(refreshes(&requests[11]), 1);
}
