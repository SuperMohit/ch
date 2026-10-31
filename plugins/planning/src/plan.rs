//! The plan: its items, the rules a new plan must follow, and how it is shown to the
//! model.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanItem {
    /// The step, in the imperative: "Run the tests".
    pub content: String,
    pub status: Status,
    /// The step while it runs: "Running the tests".
    #[serde(rename = "activeForm")]
    pub active_form: String,
}

/// The whole plan. `update_plan` replaces it; an empty plan clears it.
pub type Plan = Vec<PlanItem>;

/// Checks a plan sent to `update_plan`. Unless every item is completed, exactly one is
/// `in_progress`.
pub fn validate(plan: &[PlanItem]) -> Result<(), String> {
    for (i, item) in plan.iter().enumerate() {
        let n = i + 1;
        if item.content.trim().is_empty() {
            return Err(format!("item {n} has an empty `content`"));
        }
        if item.active_form.trim().is_empty() {
            return Err(format!("item {n} has an empty `activeForm`"));
        }
    }
    let in_progress = plan
        .iter()
        .filter(|i| i.status == Status::InProgress)
        .count();
    let open = plan.iter().any(|i| i.status != Status::Completed);
    match (open, in_progress) {
        (false, _) | (true, 1) => Ok(()),
        (true, 0) => Err(
            "no item is `in_progress`. Mark the step you are working on \
             `in_progress` (exactly one), or mark every item `completed`"
                .into(),
        ),
        (true, n) => Err(format!(
            "{n} items are `in_progress`. Keep exactly one `in_progress` and set the \
             others to `pending` or `completed`"
        )),
    }
}

/// Whether some item is not completed yet.
pub fn is_open(plan: &[PlanItem]) -> bool {
    plan.iter().any(|i| i.status != Status::Completed)
}

/// The tool result: a count, not the list, so the plan appears in a prompt only once
/// (in the reminder).
pub fn summary(plan: &[PlanItem]) -> String {
    if plan.is_empty() {
        return "Plan cleared.".into();
    }
    let done = plan
        .iter()
        .filter(|i| i.status == Status::Completed)
        .count();
    let mut s = format!("Plan updated: {done} of {} completed.", plan.len());
    if let Some(item) = plan.iter().find(|i| i.status == Status::InProgress) {
        s.push_str(&format!(" In progress: {}.", item.content.trim()));
    }
    s
}

/// The plan as the model sees it in the per-step reminder.
pub fn render(plan: &[PlanItem]) -> String {
    let mut s = String::from(
        "The current plan. Call update_plan with the whole list when a step starts or \
         finishes, or when the plan changes:",
    );
    for (i, item) in plan.iter().enumerate() {
        let status = match item.status {
            Status::Pending => "pending",
            Status::InProgress => "in_progress",
            Status::Completed => "completed",
        };
        s.push_str(&format!("\n{}. [{status}] {}", i + 1, item.content.trim()));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(content: &str, status: Status) -> PlanItem {
        PlanItem {
            content: content.into(),
            status,
            active_form: format!("{content}ing"),
        }
    }

    #[test]
    fn exactly_one_in_progress_while_open() {
        use Status::*;
        assert!(validate(&[item("a", InProgress), item("b", Pending)]).is_ok());
        assert!(validate(&[item("a", Completed), item("b", Completed)]).is_ok());
        assert!(validate(&[]).is_ok());

        let none = validate(&[item("a", Pending), item("b", Completed)]).unwrap_err();
        assert!(none.contains("no item is `in_progress`"), "{none}");
        let two = validate(&[item("a", InProgress), item("b", InProgress)]).unwrap_err();
        assert!(two.starts_with("2 items are `in_progress`"), "{two}");
    }

    #[test]
    fn empty_text_is_rejected() {
        let mut blank = item("a", Status::InProgress);
        blank.content = "  ".into();
        assert_eq!(
            validate(&[blank]).unwrap_err(),
            "item 1 has an empty `content`"
        );
        let mut blank = item("a", Status::InProgress);
        blank.active_form.clear();
        assert_eq!(
            validate(&[blank]).unwrap_err(),
            "item 1 has an empty `activeForm`"
        );
    }

    #[test]
    fn summary_counts_without_listing() {
        use Status::*;
        let plan = [
            item("Read", Completed),
            item("Fix", InProgress),
            item("Test", Pending),
        ];
        assert_eq!(
            summary(&plan),
            "Plan updated: 1 of 3 completed. In progress: Fix."
        );
        assert_eq!(summary(&[]), "Plan cleared.");
    }

    #[test]
    fn render_numbers_items_with_status() {
        use Status::*;
        let plan = [item("Read", Completed), item("Fix", InProgress)];
        let text = render(&plan);
        assert!(
            text.ends_with("\n1. [completed] Read\n2. [in_progress] Fix"),
            "{text}"
        );
    }
}
