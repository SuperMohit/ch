//! Planning (plugin `planning`): the `update_plan` tool, the `PlanStore` service and the
//! prompts that keep the plan in front of the model.
//!
//! The plan is kept outside the history, in the `PlanStore`. Each `update_plan` call
//! stays in the history as the model sent it, so the conversation holds every version
//! the model wrote; the tool result is only a count, so it adds no further copy. The
//! first model call of each turn gets a reminder: the current plan while some step is
//! open, or else a one-line nudge that planning exists. Later steps of the turn rely on
//! the model's own latest `update_plan` call, until `refresh_after_steps` model calls
//! pass without one while a step is open: then the plan is shown again, so it doesn't
//! fade in a long turn (in `ch -p` the whole task is one turn). Each refresh the model
//! doesn't follow with an update doubles the wait for the next (10, 30, 70, 150 calls
//! with the default), since like every reminder each copy stays in the history.
//!
//! Each update is saved as `plan.json` in the session directory, so a resumed session
//! keeps its plan, and emitted as a `Custom { source: "planning", name: "plan_updated" }`
//! event. The tool result carries the plan in its metadata for frontends. A refresh
//! emits `plan_refreshed` with the number of model calls since the plan was last
//! shown, when the reminder is added; a turn can still end before that request is sent.
//!
//! ```toml
//! [plugins.planning]
//! refresh_after_steps = 10    # 0 shows the plan only at the start of a turn; not 1
//! ```

mod plan;
mod store;
mod tool;

use std::sync::Arc;

use anyhow::bail;
use ch_kernel::reexports::async_trait;
use ch_kernel::{Cx, Plugin, PluginManifest, PromptContributor, RegisterCx};
use serde::Deserialize;
use serde_json::json;

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

/// `[plugins.planning]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlanningConfig {
    /// Model calls in a turn without the plan before it is first shown again; each
    /// refresh the model doesn't follow with an update doubles the wait. 0 never.
    pub refresh_after_steps: u32,
}

impl PlanningConfig {
    /// Rejects a refresh on every call.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.refresh_after_steps == 1 {
            bail!(
                "plugins.planning.refresh_after_steps = 1 would add the plan to every call; \
                 use 2 or more, or 0 to turn it off"
            );
        }
        Ok(())
    }
}

impl Default for PlanningConfig {
    fn default() -> Self {
        Self {
            refresh_after_steps: 10,
        }
    }
}

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
            "The `update_plan` tool, with the plan shown to the model at the start of each turn",
        )
        .version(env!("CARGO_PKG_VERSION"))
        .provides(&["tool:update_plan", "service:PlanStore", "prompt"])
        .project_settings(&["refresh_after_steps"])
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let config: PlanningConfig = cx.config()?;
        config.validate()?;
        let store = Arc::new(PlanStore::default());
        cx.provide_service(store.clone())?;
        cx.add_tool(UpdatePlan::new(store.clone()));
        cx.add_prompt(PlanningPrompt {
            store,
            refresh_after_steps: config.refresh_after_steps,
        });
        Ok(())
    }
}

/// The system block and the plan reminders.
pub struct PlanningPrompt {
    store: Arc<PlanStore>,
    refresh_after_steps: u32,
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
        let open = self.store.get(cx).filter(|plan| plan::is_open(plan));
        if cx.step == 0 {
            self.store.turn_started(cx);
            return vec![open.map_or_else(|| NO_PLAN_REMINDER.to_string(), |p| plan::render(&p))];
        }
        let Some(plan) = open else {
            return Vec::new();
        };
        if self.refresh_after_steps == 0 {
            return Vec::new();
        }
        let Some(since) = self.store.refresh_due(cx, self.refresh_after_steps) else {
            return Vec::new();
        };
        cx.emit_custom(ID, "plan_refreshed", json!({ "steps_since": since }));
        vec![plan::render_refresh(&plan)]
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
            refresh_after_steps: 10,
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
    async fn the_plan_is_shown_again_after_steps_without_it() {
        let (_, tool, mut prompt, cx) = setup();
        let at = |turn, step| {
            let mut cx = cx.clone();
            (cx.turn, cx.step) = (turn, step);
            cx
        };
        let refreshes = |r: Vec<String>| {
            r.iter()
                .filter(|r| r.starts_with("Your plan, shown again"))
                .count()
        };
        // No open plan, no refresh.
        assert_eq!(prompt.turn_reminders(&at(1, 0)).await, [NO_PLAN_REMINDER]);
        assert!(prompt.turn_reminders(&at(1, 12)).await.is_empty());

        // The request after an update is the first to hold it.
        let open = json!({"plan": [step("Read", "in_progress"), step("Fix", "pending")]});
        tool.call(open, &at(1, 2)).await;
        let mut rx = cx.events.subscribe();
        assert!(prompt.turn_reminders(&at(1, 12)).await.is_empty());
        let reminders = prompt.turn_reminders(&at(1, 13)).await;
        assert!(
            reminders[0].ends_with("1. [in_progress] Read\n2. [pending] Fix"),
            "{reminders:?}"
        );
        match rx.try_recv().unwrap() {
            AgentEvent::Custom { source, name, data } => {
                assert_eq!((source.as_str(), name.as_str()), (ID, "plan_refreshed"));
                assert_eq!(data, json!({"steps_since": 10}));
            }
            other => panic!("unexpected event {other:?}"),
        }
        // Each refresh without an update doubles the wait for the next.
        assert_eq!(refreshes(prompt.turn_reminders(&at(1, 32)).await), 0);
        assert_eq!(refreshes(prompt.turn_reminders(&at(1, 33)).await), 1);
        match rx.try_recv().unwrap() {
            AgentEvent::Custom { data, .. } => assert_eq!(data, json!({"steps_since": 20})),
            other => panic!("unexpected event {other:?}"),
        }
        assert_eq!(refreshes(prompt.turn_reminders(&at(1, 72)).await), 0);
        assert_eq!(refreshes(prompt.turn_reminders(&at(1, 73)).await), 1);
        // An update starts over.
        let next = json!({"plan": [step("Read", "completed"), step("Fix", "in_progress")]});
        tool.call(next, &at(1, 75)).await;
        assert_eq!(refreshes(prompt.turn_reminders(&at(1, 85)).await), 0);
        assert_eq!(refreshes(prompt.turn_reminders(&at(1, 86)).await), 1);
        assert_eq!(refreshes(prompt.turn_reminders(&at(1, 105)).await), 0);

        // So does a new turn, whose first request shows the plan.
        assert_eq!(prompt.turn_reminders(&at(2, 0)).await.len(), 1);
        assert_eq!(refreshes(prompt.turn_reminders(&at(2, 9)).await), 0);
        assert_eq!(refreshes(prompt.turn_reminders(&at(2, 10)).await), 1);

        prompt.refresh_after_steps = 0;
        assert!(prompt.turn_reminders(&at(2, 90)).await.is_empty());
        prompt.refresh_after_steps = 10;
        let done = json!({"plan": [step("Read", "completed"), step("Fix", "completed")]});
        tool.call(done, &at(2, 91)).await;
        assert!(prompt.turn_reminders(&at(2, 200)).await.is_empty());
    }

    #[test]
    fn a_refresh_on_every_call_and_unknown_keys_are_rejected() {
        let build = |toml: &str| {
            let tmp = tempfile::tempdir().unwrap();
            let config = ch_kernel::Config::parse(toml).unwrap();
            ch_kernel::RegistryBuilder::build(vec![plugin()], &config, tmp.path()).map(|_| ())
        };
        assert!(build("").is_ok());
        assert!(build("[plugins.planning]\nrefresh_after_steps = 0").is_ok());
        assert!(build("[plugins.planning]\nrefresh_after_steps = 2").is_ok());
        let err = build("[plugins.planning]\nrefresh_after_steps = 1").unwrap_err();
        assert!(format!("{err:#}").contains("every call"), "{err:#}");
        assert!(build("[plugins.planning]\nrefresh_after_step = 3").is_err());
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
