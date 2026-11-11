//! What a checker reported, keyed so that the same problem matches before and after an
//! edit even when the edit moved it to another line.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

/// One problem a checker reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The rule and message without line or column: what "the same problem" means.
    pub key: String,
    /// The line shown to the model: `path:line:col: message`.
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

/// How a checker's output is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parser {
    /// rustfmt's parse errors on stderr: an `error…` line, then ` --> <stdin>:L:C`.
    Rustfmt,
    /// `ruff check --output-format concise`: `path:L:C: rule message` on stdout.
    Ruff,
    /// Any line with a location (`:L`, `:L:C` or `(L,C)`), on stdout or stderr. The key
    /// is the line with its locations removed.
    Generic,
}

/// `path:L:C: rest`, for ruff.
static CONCISE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^.*?:(\d+:\d+): (.+)$").expect("valid regex"));

/// The ` --> <stdin>:L:C` line under a rustfmt error.
static ARROW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*--> .*?:(\d+:\d+)\s*$").expect("valid regex"));

/// A location inside a line, for the generic parser.
static LOCATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r":\d+(?::\d+)?\b|\(\d+(?:,\s*\d+)?\)").expect("valid regex"));

impl Parser {
    /// The findings in a checker's output, shown with `rel` as the file's path.
    pub fn parse(self, rel: &str, stdout: &str, stderr: &str) -> Vec<Finding> {
        match self {
            Self::Rustfmt => rustfmt(rel, stderr),
            Self::Ruff => stdout
                .lines()
                .filter_map(|line| {
                    let caps = CONCISE.captures(line.trim_end())?;
                    let message = caps[2].trim();
                    Some(Finding {
                        key: message.to_string(),
                        display: format!("{rel}:{}: {message}", &caps[1]),
                    })
                })
                .collect(),
            Self::Generic => stdout
                .lines()
                .chain(stderr.lines())
                .map(str::trim)
                .filter(|line| LOCATION.is_match(line))
                .map(|line| Finding {
                    key: LOCATION.replace_all(line, "").into_owned(),
                    display: line.to_string(),
                })
                .collect(),
        }
    }
}

/// rustfmt prints each parse error as a header line followed by its location. A header
/// with no location in the input (a config error, say) is not a finding.
fn rustfmt(rel: &str, stderr: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut header: Option<&str> = None;
    for line in stderr.lines() {
        if line.starts_with("error") {
            header = Some(line.trim_end());
        } else if line.starts_with("warning") {
            header = None;
        } else if let (Some(h), Some(caps)) = (header, ARROW.captures(line)) {
            findings.push(Finding {
                key: h.to_string(),
                display: format!("{rel}:{}: {h}", &caps[1]),
            });
            header = None;
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(findings: &[Finding]) -> Vec<&str> {
        findings.iter().map(|f| f.key.as_str()).collect()
    }

    #[test]
    fn rustfmt_errors_need_a_location() {
        let stderr = "\
error: expected expression, found `;`
 --> <stdin>:2:21
  |
2 | fn main() { let x = ; }
  |                     ^ expected expression

warning: unknown configuration option `foo`
error: this file contains an unclosed delimiter
 --> <stdin>:9:3
error: invalid rustfmt.toml
";
        let found = Parser::Rustfmt.parse("src/a.rs", "", stderr);
        assert_eq!(
            keys(&found),
            [
                "error: expected expression, found `;`",
                "error: this file contains an unclosed delimiter"
            ]
        );
        assert_eq!(
            found[0].display,
            "src/a.rs:2:21: error: expected expression, found `;`"
        );
    }

    #[test]
    fn ruff_concise_lines() {
        let stdout = "\
a.py:4:5: invalid-syntax: Expected an expression
sub/b.py:2:12: F821 Undefined name `reponse`
Found 2 errors.
";
        let found = Parser::Ruff.parse("app/b.py", stdout, "");
        assert_eq!(
            keys(&found),
            [
                "invalid-syntax: Expected an expression",
                "F821 Undefined name `reponse`"
            ]
        );
        assert_eq!(
            found[1].display,
            "app/b.py:2:12: F821 Undefined name `reponse`"
        );
    }

    #[test]
    fn generic_lines_drop_their_locations_from_the_key() {
        let stdout = "src/a.ts(3,5): error TS2304: Cannot find name 'x'.\n2 problems\n";
        let stderr = "src/a.ts:7:1: no-undef 'y' is not defined\n";
        let found = Parser::Generic.parse("src/a.ts", stdout, stderr);
        assert_eq!(
            keys(&found),
            [
                "src/a.ts: error TS2304: Cannot find name 'x'.",
                "src/a.ts: no-undef 'y' is not defined"
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
