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
    // Turn 1: the nudge on its first request only.
    assert!(requests[0].contains(NO_PLAN_REMINDER));
    for r in &requests[1..] {
        assert!(!r.contains(NO_PLAN_REMINDER));
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
    assert!(!resumed_turn.contains(NO_PLAN_REMINDER));
    let after_clear = text(&requests[1]);
    assert!(after_clear.contains(NO_PLAN_REMINDER), "{after_clear}");
    assert!(!after_clear.contains("The current plan"));
}
