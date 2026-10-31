//! The plan through a real session: one copy per prompt, outside the history.

use std::sync::Arc;

use ch_kernel::{Config, Plugin, RegistryBuilder, Session, TerminationReason};
use ch_planning::NO_PLAN_REMINDER;
use ch_provider_scripted::{ScriptStep, ScriptedPlugin};
use serde_json::{Value, json};

fn step(content: &str, status: &str) -> Value {
    json!({"content": content, "activeForm": format!("{content}ing"), "status": status})
}

#[tokio::test]
async fn each_prompt_holds_one_current_plan() {
    let scripted = ScriptedPlugin::from_steps(vec![
        ScriptStep::tool(
            "update_plan",
            json!({"plan": [step("Read", "in_progress"), step("Fix", "pending")]}),
        ),
        ScriptStep::tool(
            "update_plan",
            json!({"plan": [step("Read", "completed"), step("Fix", "in_progress")]}),
        ),
        ScriptStep::text("done"),
        ScriptStep::text("still on it"),
    ]);
    let provider = scripted.provider().unwrap();
    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(scripted), ch_planning::plugin()];
    let config = Arc::new(Config::parse("plugins.enable = [\"provider-scripted\"]").unwrap());
    let tmp = tempfile::tempdir().unwrap();
    let registry = RegistryBuilder::build(plugins, &config, tmp.path()).unwrap();
    let mut session = Session::builder(Arc::new(registry), config)
        .workspace(tmp.path())
        .build()
        .unwrap();

    for prompt in ["fix it", "and?"] {
        let result = session.run_turn(prompt).await;
        assert_eq!(result.reason, TerminationReason::Done);
    }

    let prompts: Vec<String> = provider
        .requests()
        .iter()
        .map(|r| serde_json::to_string(&r.messages).unwrap())
        .collect();
    assert_eq!(prompts.len(), 4);
    assert!(prompts[0].contains(NO_PLAN_REMINDER));
    assert!(!prompts[0].contains("The current plan"));
    for (i, prompt) in prompts.iter().enumerate().skip(1) {
        assert!(!prompt.contains(NO_PLAN_REMINDER), "request {i}");
        assert_eq!(prompt.matches("The current plan").count(), 1, "request {i}");
        assert_eq!(prompt.matches("[in_progress]").count(), 1, "request {i}");
    }
    assert!(prompts[1].contains("1. [in_progress] Read"));
    for prompt in &prompts[2..] {
        assert!(prompt.contains("1. [completed] Read\\n2. [in_progress] Fix"));
    }
}
