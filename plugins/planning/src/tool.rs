//! `update_plan`.

use std::sync::Arc;

use ch_kernel::reexports::async_trait;
use ch_kernel::{Cx, Tool, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::ID;
use crate::plan::{self, Plan, Status};
use crate::store::PlanStore;

/// Replaces the session's plan.
pub struct UpdatePlan {
    store: Arc<PlanStore>,
}

impl UpdatePlan {
    pub fn new(store: Arc<PlanStore>) -> Self {
        Self { store }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    plan: Plan,
}

#[async_trait]
impl Tool for UpdatePlan {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "update_plan".into(),
            description: "Replace the task plan the user sees with a new list of steps.\n\
                 When: work that takes three or more steps, or when the user gives you \
                 several tasks at once. Create the plan before starting, and update it \
                 each time a step starts or finishes. Skip it for a quick single-step \
                 task or a question.\n\
                 Protocol: send the complete list every time; it replaces the previous \
                 one. Each item has `content` (the step, imperative: \"Run the tests\"), \
                 `activeForm` (the same step in progress: \"Running the tests\") and \
                 `status`: `pending`, `in_progress` or `completed`. Exactly one item is \
                 `in_progress` until all are `completed`. Mark a step `completed` as \
                 soon as it is done, and only when it fully is: not while tests fail or \
                 the change is partial. Add, remove or reword steps when you learn more. \
                 An empty list clears the plan. The current plan is shown to you in a \
                 reminder, so you don't need to repeat it in replies.\n\
                 Error modes: malformed items, an empty `content` or `activeForm`, or \
                 the wrong number of `in_progress` items are rejected and the previous \
                 plan stays. Fix the list and send it again.\n\
                 Side effects: none on the workspace; the plan is shown to the user."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "plan": {
                        "type": "array",
                        "description": "The whole plan, in order.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {"type": "string", "description": "The step, in the imperative."},
                                "activeForm": {"type": "string", "description": "The step while it runs, e.g. \"Running the tests\"."},
                                "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                            },
                            "required": ["content", "activeForm", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["plan"],
                "additionalProperties": false
            }),
            // It changes no files, but two calls in one batch must not race.
            read_only: false,
        }
    }

    async fn call(&self, input: Value, cx: &Cx) -> ToolOutput {
        let Input { plan } = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => {
                return ToolOutput::error(format!(
                    "update_plan: invalid input: {e}. Send {{\"plan\": [{{\"content\": \
                     string, \"activeForm\": string, \"status\": \"pending\" | \
                     \"in_progress\" | \"completed\"}}, ...]}} with the whole list."
                ));
            }
        };
        if let Err(e) = plan::validate(&plan) {
            return ToolOutput::error(format!("update_plan: {e}. The plan was not changed."));
        }
        let summary = plan::summary(&plan);
        let data = json!({ "plan": &plan });
        self.store.set(cx, plan);
        cx.emit_custom(ID, "plan_updated", data.clone());
        ToolOutput::ok(summary).with_metadata(data)
    }

    /// The step in progress, or how many are done.
    fn summarize(&self, input: &Value) -> String {
        let Some(Ok(plan)) = input
            .get("plan")
            .map(|p| serde_json::from_value::<Plan>(p.clone()))
        else {
            return String::new();
        };
        match plan.iter().find(|i| i.status == Status::InProgress) {
            Some(item) => ch_kernel::util::truncate_middle(item.active_form.trim(), 120),
            None => {
                let done = plan
                    .iter()
                    .filter(|i| i.status == Status::Completed)
                    .count();
                format!("{done}/{} completed", plan.len())
            }
        }
    }
}
