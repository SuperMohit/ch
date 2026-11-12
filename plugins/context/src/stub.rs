//! The text that replaces trimmed content.

use std::fs;
use std::path::{Path, PathBuf};

use ch_kernel::Cx;
use serde_json::{Value, json};

/// What replaces an old `<system-reminder>` block.
pub const REMINDER: &str = "[reminder elided]";

const INPUT_MARKER_START: &str = "[elided from an earlier call to save context: ";
const INPUT_MARKER_END: &str = " characters]";

/// The tools a stub may point at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hint {
    /// `read_file` and `grep_text`.
    FileTools,
    /// Only `bash`.
    Bash,
    /// Nothing that can read a file.
    None,
}

impl Hint {
    /// From the tools the model has, as spill's notice does.
    pub fn for_tools(cx: &Cx) -> Self {
        if cx.has_tool("read_file") && cx.has_tool("grep_text") {
            Self::FileTools
        } else if cx.has_tool("bash") {
            Self::Bash
        } else {
            Self::None
        }
    }
}

/// Where a trimmed result can be read again.
#[derive(Debug, Clone, Copy)]
pub enum Saved<'a> {
    /// A copy of the result.
    Copy(&'a Path),
    /// Spill's file, which holds the whole output the result previewed.
    Spill(&'a Path),
    /// Nowhere: there is no session directory, or the copy couldn't be saved.
    Nowhere,
}

/// The stub for a tool result with `content`.
pub fn result(content: &str, saved: Saved<'_>, hint: Hint) -> String {
    let size = format!(
        "{} lines, {} characters",
        thousands(line_count(content)),
        thousands(content.chars().count())
    );
    let how = match hint {
        Hint::FileTools => "; read it with read_file (offset, limit) or grep_text",
        Hint::Bash => {
            "; read it with `tail -n +N <path> | head -n 200`, or search it with `grep -n`"
        }
        Hint::None => "",
    };
    let place = match saved {
        Saved::Copy(path) => format!("It is saved at {}{how}.", path.display()),
        Saved::Spill(path) => format!("The full output is saved at {}{how}.", path.display()),
        Saved::Nowhere => "Run the command again if you need it.".into(),
    };
    format!("[Output elided to save context: {size}. {place}]")
}

/// The input that replaces a call to `tool` that a later call superseded.
pub fn superseded(tool: &str) -> Value {
    json!({ "superseded": format!("a later {tool} call replaced this") })
}

/// `input` with every string longer than `max` characters replaced by a marker, or
/// `None` if it has none.
pub fn shortened_input(input: &Value, max: usize) -> Option<Value> {
    let mut out = input.clone();
    shorten(&mut out, max).then_some(out)
}

/// Shortens every long string in `value`; true if there was one.
fn shorten(value: &mut Value, max: usize) -> bool {
    let children: Vec<&mut Value> = match value {
        Value::String(s) => {
            let n = s.chars().count();
            if n <= max {
                return false;
            }
            *s = format!("{INPUT_MARKER_START}{}{INPUT_MARKER_END}", thousands(n));
            return true;
        }
        Value::Array(items) => items.iter_mut().collect(),
        Value::Object(map) => map.values_mut().collect(),
        _ => return false,
    };
    let mut any = false;
    for child in children {
        any |= shorten(child, max);
    }
    any
}

/// Whether some string in `input` is a marker that stands for trimmed input text. A
/// string that only mentions one, such as this source file, is not.
pub fn has_marker(input: &Value) -> bool {
    match input {
        Value::String(s) => is_marker(s),
        Value::Array(items) => items.iter().any(has_marker),
        Value::Object(map) => map.values().any(has_marker),
        _ => false,
    }
}

fn is_marker(s: &str) -> bool {
    s.strip_prefix(INPUT_MARKER_START)
        .and_then(|rest| rest.strip_suffix(INPUT_MARKER_END))
        .is_some_and(|n| {
            n.starts_with(|c: char| c.is_ascii_digit())
                && n.chars().all(|c| c.is_ascii_digit() || c == ',')
        })
}

/// The spill file a result's notice names: a path in `spill_dir` that exists as a file.
/// Only files spill made in this session's directory are trusted, so a result that
/// fakes a notice can at most point at another spill file.
pub fn spill_file(content: &str, spill_dir: &Path) -> Option<PathBuf> {
    let prefix = format!("{}/", spill_dir.display());
    content.match_indices(&prefix).find_map(|(at, _)| {
        let rest = &content[at + prefix.len()..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || "-_.".contains(c)))
            .unwrap_or(rest.len());
        let name = rest[..end].trim_end_matches('.');
        if name.starts_with('.') || !name.ends_with(".txt") {
            return None;
        }
        let path = spill_dir.join(name);
        fs::symlink_metadata(&path)
            .is_ok_and(|m| m.is_file())
            .then_some(path)
    })
}

/// Lines in `s`, counting a last line without a newline.
fn line_count(s: &str) -> usize {
    let breaks = s.matches('\n').count();
    if s.is_empty() || s.ends_with('\n') {
        breaks
    } else {
        breaks + 1
    }
}

/// `n` with a comma between each group of three digits.
pub fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_result_stub_says_what_was_removed_and_where_it_is() {
        let content = "a\nb\n".repeat(700);
        let copy = Path::new("/s/1/context/elided/4-1.txt");
        assert_eq!(
            result(&content, Saved::Copy(copy), Hint::FileTools),
            "[Output elided to save context: 1,400 lines, 2,800 characters. It is saved at \
             /s/1/context/elided/4-1.txt; read it with read_file (offset, limit) or grep_text.]"
        );
        let spill = result(&content, Saved::Spill(copy), Hint::Bash);
        assert!(
            spill.contains("The full output is saved at /s/1/"),
            "{spill}"
        );
        assert!(
            spill.contains("`tail -n +N <path> | head -n 200`"),
            "{spill}"
        );
        assert!(spill.contains("`grep -n`"), "{spill}");
        assert!(result(&content, Saved::Copy(copy), Hint::None).ends_with("4-1.txt.]"));
        assert_eq!(
            result("x", Saved::Nowhere, Hint::FileTools),
            "[Output elided to save context: 1 lines, 1 characters. Run the command again if \
             you need it.]"
        );
    }

    #[test]
    fn long_input_strings_become_markers_that_the_hook_recognizes() {
        let input = json!({
            "path": "src/main.rs",
            "content": "x".repeat(81_220),
            "edits": [{"old": "y".repeat(5), "new": "z".repeat(2000)}],
        });
        let short = shortened_input(&input, 1000).unwrap();
        assert_eq!(short["path"], "src/main.rs");
        assert_eq!(
            short["content"],
            "[elided from an earlier call to save context: 81,220 characters]"
        );
        assert_eq!(short["edits"][0]["old"], "yyyyy");
        assert!(has_marker(&short));
        assert!(!has_marker(&input));
        assert_eq!(shortened_input(&json!({"a": "b"}), 1000), None);
    }

    #[test]
    fn only_a_whole_marker_counts_as_one() {
        let marker = "[elided from an earlier call to save context: 1,000 characters]";
        assert!(has_marker(&json!({"text": marker})));
        assert!(has_marker(&json!([1, {"deep": [marker]}])));
        for other in [
            format!("the hook refuses {marker} in an input"),
            format!("{marker}\n"),
            "[elided from an earlier call to save context: N characters]".into(),
            "[elided from an earlier call to save context: ,1 characters]".into(),
        ] {
            assert!(!has_marker(&json!({"text": other})), "{other}");
        }
    }

    #[test]
    fn a_spill_file_is_found_only_in_the_spill_directory_and_only_if_it_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let spill = tmp.path().join("s1/spill");
        fs::create_dir_all(&spill).unwrap();
        fs::write(spill.join("call_1-bash-ab12.txt"), "full").unwrap();
        let notice = |path: &str| format!("head\n[… The full output is saved at {path}.\nRead]");
        let real = spill.join("call_1-bash-ab12.txt");
        assert_eq!(
            spill_file(&notice(&real.display().to_string()), &spill),
            Some(real)
        );
        let missing = spill.join("call_2-bash-cd34.txt");
        assert_eq!(
            spill_file(&notice(&missing.display().to_string()), &spill),
            None
        );
        let elsewhere = tmp.path().join("other/spill/call_1-bash-ab12.txt");
        assert_eq!(
            spill_file(&notice(&elsewhere.display().to_string()), &spill),
            None
        );
        let escape = format!("{}/../secret.txt", spill.display());
        assert_eq!(spill_file(&notice(&escape), &spill), None);
    }

    #[test]
    fn thousands_are_grouped() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(81_220), "81,220");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }
}
