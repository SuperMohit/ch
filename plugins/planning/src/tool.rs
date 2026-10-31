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
                 When: work with three or more steps, or several tasks at once. Skip it \
                 for a question or a quick single-step change.\n\
                 Protocol: send the complete list every time; it replaces the previous \
                 one. Each item has `content` (the step, imperative: \"Run the tests\"), \
                 `activeForm` (the step in progress: \"Running the tests\") and `status` \
                 (`pending`, `in_progress` or `completed`). Exactly one item is \
                 `in_progress` until all are `completed`. Each text is one plain line of \
                 at most 200 characters, and a plan has at most 30 items. An empty list \
                 clears the plan.\n\
                 Error modes: an invalid list is rejected with the reason and the previous \
                 plan stays. Fix the list and send it again.\n\
                 Side effects: none on the workspace; the plan is shown to the user."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "plan": {
                        "type": "array",
                        "description": "The whole plan, in order.",
                        "maxItems": plan::MAX_ITEMS,
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {"type": "string", "minLength": 1, "maxLength": plan::MAX_TEXT_CHARS, "description": "The step, in the imperative."},
                                "activeForm": {"type": "string", "minLength": 1, "maxLength": plan::MAX_TEXT_CHARS, "description": "The step while it runs, e.g. \"Running the tests\"."},
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

    /// The step in progress, or how many are done. Empty for an invalid plan, so the
    /// terminal never prints text that `call` would refuse.
    fn summarize(&self, input: &Value) -> String {
        let Some(Ok(plan)) = input
            .get("plan")
            .map(|p| serde_json::from_value::<Plan>(p.clone()))
        else {
            return String::new();
        };
        if plan::validate(&plan).is_err() {
            return String::new();
        }
        match plan.iter().find(|i| i.status == Status::InProgress) {
            Some(item) => item.active_form.trim().to_string(),
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
