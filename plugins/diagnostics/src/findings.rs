//! What a checker reported, keyed so that the same problem matches before and after an
//! edit even when the edit moved it to another line.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

/// One problem a checker reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The message without its location: what "the same problem" means.
    pub key: String,
    /// `path:L:C: message`, shown to the model.
    pub display: String,
}

/// How often each key occurs.
pub type Counts = HashMap<String, usize>;

/// Counts the keys of `findings`.
pub fn counts(findings: &[Finding]) -> Counts {
    let mut counts = Counts::new();
    add(&mut counts, findings.iter());
    counts
}

/// Adds the keys of `findings` to `counts`.
pub fn add<'a>(counts: &mut Counts, findings: impl Iterator<Item = &'a Finding>) {
    for f in findings {
        *counts.entry(f.key.clone()).or_default() += 1;
    }
}

/// The findings in `after` beyond what the baselines account for. A key may occur as
/// often as in the baseline where it occurs most; repeats beyond that are new.
///
/// Two baselines matter: the content before this step, and the turn's: its content
/// before the turn's first change plus what was reported since. Checkers often hide
/// some problems while another is present (a syntax error hides undefined names), so a
/// problem from before the turn, or one already reported, can be missing from this
/// step's "before" and come back when the hiding error is fixed.
pub fn beyond<'a>(baselines: &[&Counts], after: &'a [Finding]) -> Vec<&'a Finding> {
    let mut allowed = Counts::new();
    for base in baselines {
        for (key, n) in *base {
            let a = allowed.entry(key.clone()).or_default();
            *a = (*a).max(*n);
        }
    }
    after
        .iter()
        .filter(|f| match allowed.get_mut(&f.key) {
            Some(n) if *n > 0 => {
                *n -= 1;
                false
            }
            _ => true,
        })
        .collect()
}

/// A finding line: a file name (no spaces, colons, brackets or parentheses) at the start,
/// then `:L:`, `:L:C:` or `(L,C)`, then the message.
static LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[^\s:()\[\]]+(?::(\d+)(?::(\d+))?:|\((\d+)(?:,\s*(\d+))?\)\s*:?)\s*(\S.*)$")
        .expect("valid regex")
});

/// Line numbers inside a message, as in "already defined on line 3".
static LINE_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\blines? \d+").expect("valid regex"));

/// Terminal colour and cursor codes.
static ANSI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").expect("valid regex"));

/// The findings in a checker's output, on stdout and stderr, shown with `rel` as the
/// file's path. Lines that don't start with a location (summaries, stack traces, code
/// frames) are skipped.
pub fn parse(rel: &str, stdout: &str, stderr: &str) -> Vec<Finding> {
    stdout
        .lines()
        .chain(stderr.lines())
        .filter_map(|line| {
            let line = ANSI.replace_all(line, "");
            let caps = LINE.captures(line.trim())?;
            let row = caps.get(1).or(caps.get(3))?.as_str();
            let col = caps.get(2).or(caps.get(4));
            // Control characters could rewrite how the reminder reads.
            let message: String = caps[5]
                .trim_end()
                .chars()
                .filter(|c| !c.is_control())
                .collect();
            let message = message.as_str();
            let location = match col {
                Some(c) => format!("{row}:{}", c.as_str()),
                None => row.to_string(),
            };
            Some(Finding {
                key: LINE_REF.replace_all(message, "line N").into_owned(),
                display: format!("{rel}:{location}: {message}"),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(findings: &[Finding]) -> Vec<&str> {
        findings.iter().map(|f| f.key.as_str()).collect()
    }

    #[test]
    fn only_lines_that_start_with_a_location_are_findings() {
        let stdout = "\
src/a.ts(3,5): error TS2304: Cannot find name 'x'.
<stdin>:7:1: no-undef 'y' is not defined
lib.c:12: warning: unused
\x1b[1ma.py\x1b[0m:2:12: F821 Undefined name `x`
Found 2 errors in 1 file (checked 3:00)
[12:03:44] Linting...
http://localhost:8080/docs
  4 |     let v = vec(3);
  3:5  error 'x' is not defined
    at Parser.parse (/home/u/node_modules/eslint/lib/parser.js:812:15)
";
        let found = parse(
            "src/a.ts",
            stdout,
            "a.py:9:1: redefinition of `f` from line 3\n",
        );
        assert_eq!(
            keys(&found),
            [
                "error TS2304: Cannot find name 'x'.",
                "no-undef 'y' is not defined",
                "warning: unused",
                "F821 Undefined name `x`",
                "redefinition of `f` from line N",
            ]
        );
        // The path shown is always the edited file's, whatever the checker printed.
        assert_eq!(
            found[1].display,
            "src/a.ts:7:1: no-undef 'y' is not defined"
        );
        assert_eq!(found[2].display, "src/a.ts:12: warning: unused");
    }

    fn f(key: &str, line: u32) -> Finding {
        Finding {
            key: key.into(),
            display: format!("a:{line}: {key}"),
        }
    }

    fn shown(new: Vec<&Finding>) -> Vec<&str> {
        new.into_iter().map(|f| f.display.as_str()).collect()
    }

    #[test]
    fn only_new_findings_count_and_repeats_are_counted() {
        let before = counts(&[f("old", 1), f("dup", 2), f("dup", 3)]);
        // `old` moved down two lines, and a third `dup` appeared.
        let after = [
            f("old", 3),
            f("dup", 4),
            f("dup", 5),
            f("dup", 6),
            f("new", 7),
        ];
        assert_eq!(shown(beyond(&[&before], &after)), ["a:6: dup", "a:7: new"]);
        assert!(beyond(&[&counts(&after)], &[f("old", 1)]).is_empty());
    }

    #[test]
    fn a_problem_hidden_in_the_step_before_is_not_new_if_the_turn_had_it() {
        // The turn started with one `x`, and one more was reported since.
        let mut turn = counts(&[f("undefined x", 2)]);
        add(&mut turn, [f("undefined x", 9)].iter());
        // The step before had a syntax error that hid both.
        let step_before = counts(&[f("syntax error", 3)]);
        let after = [f("undefined x", 2), f("undefined x", 9)];
        assert!(beyond(&[&step_before, &turn], &after).is_empty());
        // A third is new.
        let more = [
            f("undefined x", 2),
            f("undefined x", 9),
            f("undefined x", 12),
        ];
        assert_eq!(
            shown(beyond(&[&step_before, &turn], &more)),
            ["a:12: undefined x"]
        );
    }
}
