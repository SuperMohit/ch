//! The plan: its items, the rules a new plan must follow, and how it is shown to the
//! model.

use serde::{Deserialize, Serialize};

/// Items in one plan, at most.
pub const MAX_ITEMS: usize = 30;

/// Characters in an item's `content` or `activeForm`, at most.
pub const MAX_TEXT_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    InProgress,
    Completed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
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

/// Checks a plan sent to `update_plan`: at most [`MAX_ITEMS`] items of one line of at
/// most [`MAX_TEXT_CHARS`] characters each, and, unless every item is completed,
/// exactly one `in_progress`.
///
/// The text comes back to the model in a reminder and is printed in the terminal, so
/// line breaks, control characters and bidirectional overrides are refused: each item
/// is then shown exactly as the user sees it.
pub fn validate(plan: &[PlanItem]) -> Result<(), String> {
    if plan.len() > MAX_ITEMS {
        return Err(format!(
            "the plan has {} items, more than {MAX_ITEMS}. Merge small steps",
            plan.len()
        ));
    }
    for (i, item) in plan.iter().enumerate() {
        let n = i + 1;
        for (field, text) in [
            ("content", &item.content),
            ("activeForm", &item.active_form),
        ] {
            if text.trim().is_empty() {
                return Err(format!("item {n} has an empty `{field}`"));
            }
            if text.chars().count() > MAX_TEXT_CHARS {
                return Err(format!(
                    "item {n}: `{field}` is longer than {MAX_TEXT_CHARS} characters. Keep \
                     each step to a short line"
                ));
            }
            if text.chars().any(is_unsafe) {
                return Err(format!(
                    "item {n}: `{field}` contains a line break or a control character. \
                     Keep each step to one line of plain text"
                ));
            }
        }
    }
    let in_progress = plan
        .iter()
        .filter(|i| i.status == Status::InProgress)
        .count();
    match (is_open(plan), in_progress) {
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

/// Control characters (line breaks included) and the invisible marks that reorder or
/// hide text in a terminal.
fn is_unsafe(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2069}' | '\u{feff}')
}

/// Whether some item is not completed yet.
pub fn is_open(plan: &[PlanItem]) -> bool {
    plan.iter().any(|i| i.status != Status::Completed)
}

/// The tool result: a count, not the list. The call's own input already holds the
/// list, and the REPL draws it from the result's metadata.
pub fn summary(plan: &[PlanItem]) -> String {
    if plan.is_empty() {
        return "Plan cleared.".into();
    }
    let done = plan
        .iter()
        .filter(|i| i.status == Status::Completed)
        .count();
    format!("Plan updated: {done} of {} completed.", plan.len())
}

/// The plan as the model sees it in the reminder at the start of a turn.
pub fn render(plan: &[PlanItem]) -> String {
    let mut s = String::from(
        "The current plan. Call update_plan with the whole list when a step starts or \
         finishes, or when the plan changes. If the user's latest message is about \
         something else, replace the plan, or send an empty list to clear it:",
    );
    for (i, item) in plan.iter().enumerate() {
        s.push_str(&format!(
            "\n{}. [{}] {}",
            i + 1,
            item.status.as_str(),
            item.content.trim()
        ));
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
    fn text_must_be_one_short_plain_line() {
        let with = |content: &str| {
            let mut i = item("a", Status::InProgress);
            i.content = content.into();
            validate(&[i])
        };
        for bad in [
            "Fix\n3. [completed] Run the tests",
            "Read\r",
            "Fix\u{1b}]0;title\u{7}",
            "Fix \u{202e}stset",
            "Fix\u{200b}",
        ] {
            let err = with(bad).unwrap_err();
            assert!(err.contains("one line of plain text"), "{bad:?}: {err}");
        }
        assert!(with("Fix the parser — then café").is_ok());
        assert!(with(&"x".repeat(MAX_TEXT_CHARS)).is_ok());
        let long = with(&"x".repeat(MAX_TEXT_CHARS + 1)).unwrap_err();
        assert!(long.contains("longer than 200"), "{long}");

        let mut many = vec![item("a", Status::InProgress)];
        many.extend((0..MAX_ITEMS).map(|_| item("b", Status::Pending)));
        assert!(validate(&many).unwrap_err().contains("more than 30"));
    }

    #[test]
    fn summary_counts_without_listing() {
        use Status::*;
        let plan = [
            item("Read", Completed),
            item("Fix", InProgress),
            item("Test", Pending),
        ];
        assert_eq!(summary(&plan), "Plan updated: 1 of 3 completed.");
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
