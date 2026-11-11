//! What a checker reported, keyed so that the same problem matches before and after an
//! edit even when the edit moved it to another line.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

/// One problem a checker reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The line without its locations: what "the same problem" means.
    pub key: String,
    /// The line as the checker printed it, shown to the model.
    pub display: String,
}

/// The findings in `after` that `before` doesn't have. Repeats count: two identical
/// findings before and three after give one.
pub fn introduced<'a>(before: &[Finding], after: &'a [Finding]) -> Vec<&'a Finding> {
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for f in before {
        *seen.entry(f.key.as_str()).or_default() += 1;
    }
    after
        .iter()
        .filter(|f| match seen.get_mut(f.key.as_str()) {
            Some(n) if *n > 0 => {
                *n -= 1;
                false
            }
            _ => true,
        })
        .collect()
}

/// A location inside a line: `:L`, `:L:C` or `(L,C)`.
static LOCATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r":\d+(?::\d+)?\b|\(\d+(?:,\s*\d+)?\)").expect("valid regex"));

/// The findings in a checker's output: every line on stdout or stderr that carries a
/// location. Lines without one (summaries, banners) are skipped.
pub fn parse(stdout: &str, stderr: &str) -> Vec<Finding> {
    stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .filter(|line| LOCATION.is_match(line))
        .map(|line| Finding {
            key: LOCATION.replace_all(line, "").into_owned(),
            display: line.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_with_a_location_are_findings_keyed_without_it() {
        let stdout = "src/a.ts(3,5): error TS2304: Cannot find name 'x'.\n2 problems\n";
        let stderr = "src/a.ts:7:1: no-undef 'y' is not defined\nlib.c:12: warning: unused\n";
        let found = parse(stdout, stderr);
        let keys: Vec<_> = found.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "src/a.ts: error TS2304: Cannot find name 'x'.",
                "src/a.ts: no-undef 'y' is not defined",
                "lib.c: warning: unused",
            ]
        );
        assert_eq!(
            found[1].display,
            "src/a.ts:7:1: no-undef 'y' is not defined"
        );
    }

    #[test]
    fn only_new_findings_count_and_repeats_are_counted() {
        let f = |key: &str, line: u32| Finding {
            key: key.into(),
            display: format!("a:{line}: {key}"),
        };
        let before = [f("old", 1), f("dup", 2), f("dup", 3)];
        // `old` moved down two lines, and a third `dup` appeared.
        let after = [
            f("old", 3),
            f("dup", 4),
            f("dup", 5),
            f("dup", 6),
            f("new", 7),
        ];
        let new: Vec<_> = introduced(&before, &after)
            .into_iter()
            .map(|f| f.display.as_str())
            .collect();
        assert_eq!(new, ["a:6: dup", "a:7: new"]);
        assert!(introduced(&after, &before).is_empty());
    }
}
