//! The streak counters, without the hook plumbing.

use crate::StuckConfig;

const REPEAT: &str = include_str!("../../../prompts/stuck-repeat.md");
const FAILING: &str = include_str!("../../../prompts/stuck-failing.md");

/// Error text kept for the refusal message.
const MAX_ERROR_CHARS: usize = 800;

/// How a call ended, as `post_tool` sees it.
pub enum Outcome<'a> {
    Ok,
    Failed(&'a str),
    /// Not run because this hook refused it.
    Refused,
    /// Not run for another reason (a permission denial, a cancelled turn): not counted.
    Skipped,
}

/// What `post_tool` should do.
#[derive(Debug, PartialEq)]
pub enum After {
    Nothing,
    Note(String),
    Stop(String),
}

/// Streaks over consecutive tool calls. The same call is the same tool with the same
/// canonical input.
#[derive(Debug, Default)]
pub struct Tracker {
    last: Option<String>,
    /// Calls in a row equal to `last`, refused ones included.
    repeats: u32,
    /// Of those, how many failed in a row at the end.
    failures: u32,
    last_error: String,
    /// Model responses in a row whose tool calls were all malformed.
    malformed: u32,
}

impl Tracker {
    /// The reason to refuse a call (`pre_tool`): it failed `refuse_after` times in a row.
    pub fn refusal(&self, tool: &str, key: &str, cfg: &StuckConfig) -> Option<String> {
        let repeating = self.last.as_deref() == Some(key);
        if cfg.refuse_after == 0 || !repeating || self.failures < cfg.refuse_after {
            return None;
        }
        Some(format!(
            "this exact `{tool}` call already failed {} times in a row, and it will fail the \
             same way again. The last error was:\n{}\nChange the input or take a different \
             approach.",
            self.failures, self.last_error
        ))
    }

    /// Counts one finished call (`post_tool`).
    pub fn record(
        &mut self,
        tool: &str,
        key: String,
        outcome: Outcome,
        cfg: &StuckConfig,
    ) -> After {
        if matches!(outcome, Outcome::Skipped) {
            return After::Nothing;
        }
        if self.last.as_deref() != Some(key.as_str()) {
            self.last = Some(key);
            self.repeats = 0;
            self.failures = 0;
        }
        self.repeats += 1;
        match outcome {
            Outcome::Ok => self.failures = 0,
            Outcome::Failed(error) => {
                self.failures += 1;
                self.last_error = error.trim().chars().take(MAX_ERROR_CHARS).collect();
            }
            Outcome::Refused => self.failures += 1,
            Outcome::Skipped => unreachable!("returned above"),
        }
        if cfg.stop_after > 0 && self.failures >= cfg.stop_after {
            return After::Stop(format!(
                "`{tool}` failed {} times in a row with the same input",
                self.failures
            ));
        }
        if cfg.remind_after > 0 && self.repeats == cfg.remind_after {
            let template = if self.failures == self.repeats {
                FAILING
            } else {
                REPEAT
            };
            let text = template
                .trim()
                .replace("{tool}", tool)
                .replace("{count}", &self.repeats.to_string());
            return After::Note(text);
        }
        After::Nothing
    }

    /// Counts one model response (`after_model`): `Some(true)` when it called tools and
    /// every call was malformed, `Some(false)` when some call was fine or there were no
    /// calls, `None` to leave the count alone. Returns the reason to stop the turn.
    pub fn response(&mut self, all_malformed: Option<bool>, cfg: &StuckConfig) -> Option<String> {
        match all_malformed? {
            false => self.malformed = 0,
            true => self.malformed += 1,
        }
        (cfg.malformed_steps > 0 && self.malformed >= cfg.malformed_steps).then(|| {
            format!(
                "the model sent malformed tool input in {} responses in a row",
                self.malformed
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> StuckConfig {
        StuckConfig::default()
    }

    /// Runs `n` identical calls, refusing them the way the hook does.
    fn run(t: &mut Tracker, key: &str, n: u32, fail: bool) -> Vec<After> {
        (0..n)
            .map(|_| {
                let outcome = if t.refusal("bash", key, &cfg()).is_some() {
                    Outcome::Refused
                } else if fail {
                    Outcome::Failed("exit 1: boom")
                } else {
                    Outcome::Ok
                };
                t.record("bash", key.into(), outcome, &cfg())
            })
            .collect()
    }

    #[test]
    fn identical_failures_are_refused_then_stop_the_turn() {
        let mut t = Tracker::default();
        assert_eq!(run(&mut t, "k", 2, true), [After::Nothing, After::Nothing]);
        let refusal = t.refusal("bash", "k", &cfg()).unwrap();
        assert!(refusal.contains("failed 2 times in a row"), "{refusal}");
        assert!(refusal.contains("exit 1: boom"), "{refusal}");
        assert!(t.refusal("bash", "other", &cfg()).is_none());

        let rest = run(&mut t, "k", 6, true);
        assert_eq!(rest[..2], [After::Nothing, After::Nothing]);
        let After::Note(note) = &rest[2] else {
            panic!("expected the reminder on call 5, got {:?}", rest[2]);
        };
        assert!(
            note.starts_with("`bash` has failed 5 times in a row"),
            "{note}"
        );
        assert_eq!(rest[3..5], [After::Nothing, After::Nothing]);
        assert_eq!(
            rest[5],
            After::Stop("`bash` failed 8 times in a row with the same input".into())
        );
    }

    #[test]
    fn identical_successes_only_get_one_reminder() {
        let mut t = Tracker::default();
        let out = run(&mut t, "k", 12, false);
        let notes: Vec<_> = out
            .iter()
            .enumerate()
            .filter(|(_, a)| **a != After::Nothing)
            .collect();
        assert_eq!(notes.len(), 1);
        let (i, After::Note(note)) = notes[0] else {
            panic!("{notes:?}");
        };
        assert_eq!(i, 4);
        assert!(note.contains("exactly the same input 5 times"), "{note}");
    }

    #[test]
    fn another_call_or_a_success_resets_the_failures() {
        let mut t = Tracker::default();
        run(&mut t, "k", 2, true);
        run(&mut t, "other", 1, false);
        assert!(t.refusal("bash", "k", &cfg()).is_none());
        run(&mut t, "k", 1, true);
        run(&mut t, "k", 1, false);
        run(&mut t, "k", 1, true);
        assert!(t.refusal("bash", "k", &cfg()).is_none());
    }

    #[test]
    fn skipped_calls_change_nothing() {
        let mut t = Tracker::default();
        run(&mut t, "k", 1, true);
        assert_eq!(
            t.record("bash", "x".into(), Outcome::Skipped, &cfg()),
            After::Nothing
        );
        run(&mut t, "k", 1, true);
        assert!(t.refusal("bash", "k", &cfg()).is_some());
    }

    #[test]
    fn zero_disables_each_limit() {
        let off = StuckConfig {
            remind_after: 0,
            stop_after: 0,
            refuse_after: 0,
            malformed_steps: 0,
        };
        let mut t = Tracker::default();
        for _ in 0..20 {
            assert!(t.refusal("bash", "k", &off).is_none());
            let a = t.record("bash", "k".into(), Outcome::Failed("e"), &off);
            assert_eq!(a, After::Nothing);
            assert_eq!(t.response(Some(true), &off), None);
        }
    }

    #[test]
    fn malformed_responses_in_a_row_stop() {
        let mut t = Tracker::default();
        assert_eq!(t.response(Some(true), &cfg()), None);
        assert_eq!(t.response(Some(true), &cfg()), None);
        assert_eq!(t.response(Some(false), &cfg()), None);
        assert_eq!(t.response(Some(true), &cfg()), None);
        assert_eq!(t.response(None, &cfg()), None);
        assert_eq!(t.response(Some(true), &cfg()), None);
        assert_eq!(
            t.response(Some(true), &cfg()).as_deref(),
            Some("the model sent malformed tool input in 3 responses in a row")
        );
    }
}
