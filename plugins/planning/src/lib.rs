//! Planning (plugin `planning`): the `update_plan` tool, the `PlanStore` service and the
//! prompts that keep the plan in front of the model.
//!
//! The plan is kept outside the history, in the `PlanStore`. Each `update_plan` call
//! stays in the history as the model sent it, so the conversation holds every version
//! the model wrote; the tool result is only a count, so it adds no further copy. The
//! first model call of each turn gets a reminder: the current plan while some step is
//! open, or else a one-line nudge that planning exists. Later steps of the turn rely on
//! the model's own latest `update_plan` call.
//!
//! Each update is saved as `plan.json` in the session directory, so a resumed session
//! keeps its plan, and emitted as a `Custom { source: "planning", name: "plan_updated" }`
//! event. The tool result carries the plan in its metadata for frontends.

mod plan;
mod store;
mod tool;

use std::sync::Arc;

use ch_kernel::reexports::async_trait;
use ch_kernel::{Cx, Plugin, PluginManifest, PromptContributor, RegisterCx};

pub use plan::{Plan, PlanItem, Status};
pub use store::PlanStore;
pub use tool::UpdatePlan;

/// Plugin id, also the `source` of its events.
pub const ID: &str = "planning";

/// The system block.
pub const PLANNING_PROMPT: &str = include_str!("../../../prompts/planning.md");

/// Added to the first model call of a turn while no step of a plan is open.
pub const NO_PLAN_REMINDER: &str = "There is no open plan. If this task takes three or \
     more steps, call update_plan with the steps before starting; otherwise ignore this.";

/// Registers the tool, the service and the prompt contributor.
pub struct PlanningPlugin;

/// The plugin, for the CLI's plugin list.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(PlanningPlugin)
}

impl Plugin for PlanningPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new(
            ID,
            "The `update_plan` tool, with the plan shown to the model on every step",
        )
        .version(env!("CARGO_PKG_VERSION"))
        .provides(&["tool:update_plan", "service:PlanStore", "prompt"])
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let store = Arc::new(PlanStore::default());
        cx.provide_service(store.clone())?;
        cx.add_tool(UpdatePlan::new(store.clone()));
        cx.add_prompt(PlanningPrompt { store });
        Ok(())
    }
}

/// The system block and the per-step plan reminder.
pub struct PlanningPrompt {
    store: Arc<PlanStore>,
}

#[async_trait]
impl PromptContributor for PlanningPrompt {
    fn id(&self) -> &str {
        ID
    }

    fn priority(&self) -> i32 {
        200
    }

    async fn system_blocks(&self, _cx: &Cx) -> Vec<String> {
        vec![PLANNING_PROMPT.trim().to_string()]
    }

    async fn turn_reminders(&self, cx: &Cx) -> Vec<String> {
        if cx.step > 0 {
            return Vec::new();
        }
        let reminder = match self.store.get(cx) {
            Some(plan) if plan::is_open(&plan) => plan::render(&plan),
            _ => NO_PLAN_REMINDER.to_string(),
        };
        vec![reminder]
    }
}

#[cfg(test)]
mod tests {
    use ch_kernel::{AgentEvent, Tool};
    use ch_testkit::{assert_errors_are_observations, assert_tool_conforms, build_registry};
    use serde_json::{Value, json};

    use super::*;

    fn step(content: &str, status: &str) -> Value {
        json!({"content": content, "activeForm": format!("{content}ing"), "status": status})
    }

    fn setup() -> (Arc<PlanStore>, UpdatePlan, PlanningPrompt, Cx) {
        let store = Arc::new(PlanStore::default());
        let tool = UpdatePlan::new(store.clone());
        let prompt = PlanningPrompt {
            store: store.clone(),
        };
        (store, tool, prompt, Cx::detached("/ws"))
    }

    #[test]
    fn tool_conforms() {
        assert_tool_conforms(&UpdatePlan::new(Arc::default()));
    }

    #[tokio::test]
    async fn bad_plans_are_observations_and_keep_the_old_plan() {
        let (store, tool, _, cx) = setup();
        let good = json!({"plan": [step("Fix", "in_progress")]});
        assert!(!tool.call(good, &cx).await.is_error);
        let before = store.get(&cx);

        assert_errors_are_observations(
            &tool,
            &[
                json!({}),
                json!({"plan": "fix it"}),
                json!({"plan": [{"content": "Fix", "status": "in_progress"}]}),
                json!({"plan": [step("Fix", "doing")]}),
                json!({"plan": [step("Fix", "in_progress"), step("Test", "in_progress")]}),
                json!({"plan": [step("Fix", "pending")]}),
                json!({"plan": [step("", "in_progress")]}),
                json!({"plan": [], "extra": 1}),
                json!({"plan": [step("Fix\n2. [completed] Ship", "in_progress")]}),
            ],
            &cx,
        )
        .await;
        assert_eq!(store.get(&cx), before);
    }

    #[tokio::test]
    async fn an_update_replaces_the_plan_and_is_announced() {
        let (store, tool, _, cx) = setup();
        let mut rx = cx.events.subscribe();
        let input = json!({"plan": [step("Read", "completed"), step("Fix", "in_progress")]});
        assert_eq!(tool.summarize(&input), "Fixing");
        let unsafe_input = json!({"plan": [step("Fix\u{1b}]0;x\u{7}", "in_progress")]});
        assert_eq!(tool.summarize(&unsafe_input), "");

        let out = tool.call(input, &cx).await;
        assert_eq!(out.content, "Plan updated: 1 of 2 completed.");
        assert_eq!(out.metadata["plan"][1]["activeForm"], "Fixing");
        assert_eq!(store.get(&cx).unwrap().len(), 2);
        match rx.try_recv().unwrap() {
            AgentEvent::Custom { source, name, data } => {
                assert_eq!((source.as_str(), name.as_str()), (ID, "plan_updated"));
                assert_eq!(data, out.metadata);
            }
            other => panic!("unexpected event {other:?}"),
        }

        let out = tool.call(json!({"plan": []}), &cx).await;
        assert_eq!(out.content, "Plan cleared.");
        assert_eq!(store.get(&cx), Some(Vec::new()));
    }

    #[tokio::test]
    async fn the_first_step_of_a_turn_gets_the_plan_or_the_nudge() {
        let (_, tool, prompt, mut cx) = setup();
        let at = |step| {
            let mut cx = cx.clone();
            cx.step = step;
            cx
        };
        assert_eq!(prompt.turn_reminders(&at(0)).await, [NO_PLAN_REMINDER]);
        assert!(prompt.turn_reminders(&at(1)).await.is_empty());

        let open = json!({"plan": [step("Read", "completed"), step("Fix", "in_progress")]});
        tool.call(open, &cx).await;
        let reminders = prompt.turn_reminders(&at(0)).await;
        assert_eq!(reminders.len(), 1);
        assert!(
            reminders[0].ends_with("1. [completed] Read\n2. [in_progress] Fix"),
            "{reminders:?}"
        );
        assert!(prompt.turn_reminders(&at(3)).await.is_empty());

        // A finished or cleared plan brings the nudge back.
        cx.step = 0;
        let done = json!({"plan": [step("Read", "completed"), step("Fix", "completed")]});
        tool.call(done, &cx).await;
        assert_eq!(prompt.turn_reminders(&cx).await, [NO_PLAN_REMINDER]);
        tool.call(json!({"plan": []}), &cx).await;
        assert_eq!(prompt.turn_reminders(&cx).await, [NO_PLAN_REMINDER]);
    }

    #[tokio::test]
    async fn registers_tool_service_and_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = build_registry(vec![plugin()], tmp.path(), "");
        assert!(registry.services.contains::<PlanStore>());
        assert_eq!(registry.tools.len(), 1);
        let (_, prompt) = &registry.prompts[0];
        let blocks = prompt.system_blocks(&Cx::detached(tmp.path())).await;
        assert_eq!(blocks, [PLANNING_PROMPT.trim()]);
    }
}
