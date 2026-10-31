//! The streak counters, without the hook plumbing.

use std::collections::HashSet;

use ch_kernel::HookDecision;
use ch_kernel::util::truncate_middle;
use serde_json::Value;

use crate::StuckConfig;

const REPEAT: &str = include_str!("../../../prompts/stuck-repeat.md");
const FAILING: &str = include_str!("../../../prompts/stuck-failing.md");
const REFUSED: &str = include_str!("../../../prompts/stuck-refused.md");

/// Error text kept for the refusal message, cut in the middle so a command's stderr
/// and exit status at the end survive.
const MAX_ERROR_CHARS: usize = 800;

/// How a call that reached `post_tool` ended.
pub enum Outcome<'a> {
    Ok,
    Failed(&'a str),
    /// Not run because this hook refused it.
    Refused,
}

/// Streaks over consecutive tool calls. The same call is the same tool with an equal
/// input (`Value` equality ignores key order).
#[derive(Debug, Default)]
pub struct Tracker {
    last: Option<(String, Value)>,
    /// Calls in a row equal to `last`, refused ones included.
    repeats: u32,
    /// Of those, the runs that failed, and the calls refused, since the last success.
    failed: u32,
    refused: u32,
    /// Failures in a row with exactly the same error text.
    same_error: u32,
    last_error: String,
    /// Model responses in a row whose tool calls were all malformed.
    malformed: u32,
    /// Ids of calls refused in `pre_tool`, so `post_tool` knows them.
    refused_ids: HashSet<String>,
}

impl Tracker {
    /// `pre_tool`: refuses the call when the same call just failed `refuse_after` times
    /// in a row with the same error.
    pub fn check(
        &mut self,
        id: &str,
        tool: &str,
        input: &Value,
        cfg: &StuckConfig,
    ) -> HookDecision {
        let repeating = self
            .last
            .as_ref()
            .is_some_and(|(t, i)| t == tool && i == input);
        if cfg.refuse_after == 0 || !repeating || self.same_error < cfg.refuse_after {
            return HookDecision::Continue;
        }
        self.refused_ids.insert(id.to_string());
        HookDecision::deny(
            fill(REFUSED, tool, self.same_error).replace("{error}", &self.last_error),
        )
    }

    /// Whether `pre_tool` refused the call with this id (then forgets it).
    pub fn take_refused(&mut self, id: &str) -> bool {
        self.refused_ids.remove(id)
    }

    /// `post_tool`: counts one call that ran or was refused by this hook.
    pub fn record(
        &mut self,
        tool: &str,
        input: &Value,
        outcome: Outcome,
        cfg: &StuckConfig,
    ) -> HookDecision {
        let same = self
            .last
            .as_ref()
            .is_some_and(|(t, i)| t == tool && i == input);
        if !same {
            *self = Self {
                last: Some((tool.to_string(), input.clone())),
                malformed: self.malformed,
                refused_ids: std::mem::take(&mut self.refused_ids),
                ..Self::default()
            };
        }
        self.repeats += 1;
        match outcome {
            Outcome::Ok => {
                self.failed = 0;
                self.refused = 0;
                self.same_error = 0;
            }
            Outcome::Failed(error) => {
                let error = truncate_middle(error.trim(), MAX_ERROR_CHARS);
                self.same_error = if self.failed > 0 && error == self.last_error {
                    self.same_error + 1
                } else {
                    1
                };
                self.failed += 1;
                self.last_error = error;
            }
            Outcome::Refused => self.refused += 1,
        }
        if cfg.stop_after > 0 && self.failed + self.refused >= cfg.stop_after {
            let mut reason = format!(
                "`{tool}` failed {} times in a row with the same input",
                self.failed
            );
            if self.refused > 0 {
                reason.push_str(&format!(" and was refused {} more times", self.refused));
            }
            return HookDecision::stop(reason);
        }
        if cfg.remind_after > 0 && self.repeats == cfg.remind_after {
            let all_failed = self.failed + self.refused == self.repeats;
            let template = if all_failed { FAILING } else { REPEAT };
            return HookDecision::note(fill(template, tool, self.repeats));
        }
        HookDecision::Continue
    }

    /// `after_model`: counts one response that called tools. Returns the reason to end
    /// the turn after `malformed_steps` responses in a row whose calls were all
    /// malformed.
    pub fn response(&mut self, all_malformed: bool, cfg: &StuckConfig) -> Option<String> {
        self.malformed = if all_malformed { self.malformed + 1 } else { 0 };
        (cfg.malformed_steps > 0 && self.malformed >= cfg.malformed_steps).then(|| {
            format!(
                "the model sent malformed tool input in {} responses in a row",
                self.malformed
            )
        })
    }
}

fn fill(template: &str, tool: &str, count: u32) -> String {
    template
        .trim()
        .replace("{tool}", tool)
        .replace("{count}", &count.to_string())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn cfg() -> StuckConfig {
        StuckConfig::default()
    }

    /// Runs `n` identical calls that fail with `error` (or succeed with `None`), refusing
    /// them the way the hook does.
    fn run(t: &mut Tracker, cmd: &str, n: u32, error: Option<&str>) -> Vec<HookDecision> {
        let input = json!({"command": cmd});
        (0..n)
            .map(|i| {
                let id = format!("{cmd}-{i}");
                let refused = t.check(&id, "bash", &input, &cfg()) != HookDecision::Continue;
                assert_eq!(refused, t.take_refused(&id));
                let outcome = match (refused, error) {
                    (true, _) => Outcome::Refused,
                    (false, Some(e)) => Outcome::Failed(e),
                    (false, None) => Outcome::Ok,
                };
                t.record("bash", &input, outcome, &cfg())
            })
            .collect()
    }

    fn refusal(t: &mut Tracker, cmd: &str) -> Option<String> {
        match t.check("probe", "bash", &json!({"command": cmd}), &cfg()) {
            HookDecision::Deny { reason } => {
                t.take_refused("probe");
                Some(reason)
            }
            _ => None,
        }
    }

    #[test]
    fn identical_failures_are_refused_then_stop_the_turn() {
        let mut t = Tracker::default();
        let first = run(&mut t, "make", 2, Some("exit 1: boom"));
        assert_eq!(first, [HookDecision::Continue, HookDecision::Continue]);
        let reason = refusal(&mut t, "make").unwrap();
        assert!(
            reason.starts_with("this exact `bash` call failed 2 times"),
            "{reason}"
        );
        assert!(reason.contains("not a permission rule"), "{reason}");
        assert!(reason.ends_with("exit 1: boom"), "{reason}");
        assert!(refusal(&mut t, "other").is_none());

        let rest = run(&mut t, "make", 6, Some("exit 1: boom"));
        assert_eq!(rest[..2], [HookDecision::Continue, HookDecision::Continue]);
        let HookDecision::AppendNote { text } = &rest[2] else {
            panic!("expected the reminder on call 5, got {:?}", rest[2]);
        };
        assert!(
            text.contains("5 times in a row, and it has not succeeded once"),
            "{text}"
        );
        assert_eq!(
            rest[5],
            HookDecision::stop(
                "`bash` failed 2 times in a row with the same input and was refused 6 more times"
            )
        );
    }

    #[test]
    fn a_failure_whose_output_changes_is_never_refused() {
        let mut t = Tracker::default();
        let input = json!({"command": "grep -q Finished build.log"});
        for i in 0..7 {
            let id = i.to_string();
            assert_eq!(t.check(&id, "bash", &input, &cfg()), HookDecision::Continue);
            let error = format!("exit 1 after {i}s");
            let d = t.record("bash", &input, Outcome::Failed(&error), &cfg());
            assert!(!matches!(d, HookDecision::Stop { .. }), "call {i}: {d:?}");
        }
        let error = "exit 1 after 7s";
        let d = t.record("bash", &input, Outcome::Failed(error), &cfg());
        assert_eq!(
            d,
            HookDecision::stop("`bash` failed 8 times in a row with the same input")
        );
    }

    #[test]
    fn identical_successes_only_get_one_reminder() {
        let mut t = Tracker::default();
        let out = run(&mut t, "git status", 12, None);
        let notes: Vec<_> = out
            .iter()
            .enumerate()
            .filter(|(_, d)| **d != HookDecision::Continue)
            .collect();
        assert_eq!(notes.len(), 1);
        let (4, HookDecision::AppendNote { text }) = notes[0] else {
            panic!("{notes:?}");
        };
        assert!(text.contains("exactly the same input 5 times"), "{text}");
    }

    #[test]
    fn another_call_or_a_success_resets_the_failures() {
        let mut t = Tracker::default();
        run(&mut t, "make", 2, Some("e"));
        run(&mut t, "ls", 1, None);
        assert!(refusal(&mut t, "make").is_none());
        run(&mut t, "make", 1, Some("e"));
        run(&mut t, "make", 1, None);
        run(&mut t, "make", 1, Some("e"));
        assert!(refusal(&mut t, "make").is_none());
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
        let input = json!({"command": "make"});
        for i in 0..20 {
            let id = i.to_string();
            assert_eq!(t.check(&id, "bash", &input, &off), HookDecision::Continue);
            let d = t.record("bash", &input, Outcome::Failed("e"), &off);
            assert_eq!(d, HookDecision::Continue);
            assert_eq!(t.response(true, &off), None);
        }
    }

    #[test]
    fn malformed_responses_in_a_row_stop() {
        let mut t = Tracker::default();
        assert_eq!(t.response(true, &cfg()), None);
        assert_eq!(t.response(true, &cfg()), None);
        assert_eq!(t.response(false, &cfg()), None);
        assert_eq!(t.response(true, &cfg()), None);
        assert_eq!(t.response(true, &cfg()), None);
        assert_eq!(
            t.response(true, &cfg()).as_deref(),
            Some("the model sent malformed tool input in 3 responses in a row")
        );
    }
}
