//! The text that replaces trimmed content, and the checks that a replacement is one.

use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use ch_kernel::util::{ReadQuirks, ReadTools, line_count, safe_name, thousands};
use serde_json::{Value, json};

/// What replaces an old `<system-reminder>` block.
pub const REMINDER: &str = "[reminder elided]";

/// How every result stub starts.
pub const RESULT_PREFIX: &str = "[Output elided to save context: ";

pub const INPUT_MARKER_START: &str = "[elided from an earlier call to save context: ";
const INPUT_MARKER_END: &str = " characters]";

/// Where a trimmed result can be read again.
#[derive(Debug, Clone, Copy)]
pub enum Saved<'a> {
    /// A copy of the result.
    Copy(&'a Path),
    /// Spill's file, `bytes` long, which holds the whole output the result previewed.
    Spill { path: &'a Path, bytes: u64 },
    /// Nowhere: there is no session directory, or the copy couldn't be saved.
    Nowhere,
}

/// The stub for a tool result with `content`.
pub fn result(content: &str, saved: Saved<'_>, tools: ReadTools) -> String {
    let size = format!(
        "{}, {}",
        count(line_count(content), "line"),
        count(content.chars().count(), "character")
    );
    let how = match tools {
        ReadTools::FileTools { .. } => "; read it with read_file (offset, limit) or grep_text",
        ReadTools::Bash => {
            "; read it from line N on with `tail -n +N <path> | head -n 200`, or search it \
             with `grep -n`"
        }
        ReadTools::None => "",
    };
    match saved {
        Saved::Copy(path) => {
            let notes = ReadQuirks::of(content).notes(tools);
            format!(
                "{RESULT_PREFIX}{size}. It is saved at {}{how}.{notes}]",
                path.display()
            )
        }
        Saved::Spill { path, bytes } => {
            // What spill's notice said about the whole output, which the preview may not
            // show.
            let mut quirks = ReadQuirks::of(content);
            quirks.long_lines |= content.contains("Some lines are longer than read_file shows");
            quirks.nul |= content.contains("It contains NUL bytes");
            let notes = quirks.notes(tools);
            format!(
                "{RESULT_PREFIX}a preview of {size}. The whole output, {}, is saved at \
                 {}{how}.{notes}]",
                count(bytes as usize, "byte"),
                path.display()
            )
        }
        Saved::Nowhere => format!("{RESULT_PREFIX}{size}. Run the command again if you need it.]"),
    }
}

/// `n` and `thing`, plural unless `n` is 1.
fn count(n: usize, thing: &str) -> String {
    let s = if n == 1 { "" } else { "s" };
    format!("{} {thing}{s}", thousands(n as u64))
}

/// The input that replaces a call to `tool` that a later call superseded.
pub fn superseded(tool: &str) -> Value {
    json!({ "superseded": format!("a later {tool} call replaced this") })
}

/// The marker for a trimmed input string of `chars` characters.
pub fn input_marker(chars: usize) -> String {
    format!(
        "{INPUT_MARKER_START}{}{INPUT_MARKER_END}",
        thousands(chars as u64)
    )
}

/// `input` with every string longer than `max` characters replaced by its marker, or
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
            *s = input_marker(n);
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

/// Whether `new` is `original` with some strings replaced by their markers, and nothing
/// else changed.
pub fn is_shortened(original: &Value, new: &Value) -> bool {
    match (original, new) {
        (Value::String(a), Value::String(b)) => a == b || *b == input_marker(a.chars().count()),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| is_shortened(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(k, v)| b.get(k).is_some_and(|w| is_shortened(v, w)))
        }
        (a, b) => a == b,
    }
}

/// Every input marker in `value`.
pub fn markers_in(value: &Value) -> Vec<String> {
    match value {
        Value::String(s) if is_marker(s) => vec![s.clone()],
        Value::Array(items) => items.iter().flat_map(markers_in).collect(),
        Value::Object(map) => map.values().flat_map(markers_in).collect(),
        _ => Vec::new(),
    }
}

/// Whether `s`, trimmed, is an input marker.
pub fn is_marker(s: &str) -> bool {
    s.trim()
        .strip_prefix(INPUT_MARKER_START)
        .and_then(|rest| rest.strip_suffix(INPUT_MARKER_END))
        .is_some_and(|n| {
            n.starts_with(|c: char| c.is_ascii_digit())
                && n.chars().all(|c| c.is_ascii_digit() || c == ',')
        })
}

/// The spill file a result's notice names, and its size: a file in `spill_dir` that
/// spill made for this call (its name starts with the call id) and that starts with the
/// result's head. Only those are trusted, so a result that fakes a notice can't point
/// the model at another file.
pub fn spill_file(content: &str, call_id: &str, spill_dir: &Path) -> Option<(PathBuf, u64)> {
    let prefix = format!("{}/", spill_dir.display());
    let owner = format!("{}-", safe_name(call_id));
    let path = content.match_indices(&prefix).find_map(|(at, _)| {
        let rest = &content[at + prefix.len()..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || "-_.".contains(c)))
            .unwrap_or(rest.len());
        let name = rest[..end].trim_end_matches('.');
        (name.starts_with(&owner) && name.ends_with(".txt")).then(|| spill_dir.join(name))
    })?;
    let meta = fs::symlink_metadata(&path).ok().filter(|m| m.is_file())?;
    // The preview is the output's head, then spill's notice.
    let head = content
        .split("\n[… ")
        .next()
        .unwrap_or("")
        .trim_end_matches('\n');
    let head = &head.as_bytes()[..head.len().min(4096)];
    let mut start = vec![0; head.len()];
    fs::File::open(&path)
        .and_then(|mut f| f.read_exact(&mut start))
        .ok()?;
    (start == head).then_some((path, meta.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILES: ReadTools = ReadTools::FileTools { bash: false };

    #[test]
    fn a_result_stub_says_what_was_removed_and_where_it_is() {
        let content = "a\nb\n".repeat(700);
        let copy = Path::new("/s/1/context/elided/4-1.txt");
        assert_eq!(
            result(&content, Saved::Copy(copy), FILES),
            "[Output elided to save context: 1,400 lines, 2,800 characters. It is saved at \
             /s/1/context/elided/4-1.txt; read it with read_file (offset, limit) or grep_text.]"
        );
        let bash = result(&content, Saved::Copy(copy), ReadTools::Bash);
        assert!(
            bash.contains("from line N on with `tail -n +N <path> | head -n 200`"),
            "{bash}"
        );
        assert!(bash.contains("`grep -n`"), "{bash}");
        assert!(result(&content, Saved::Copy(copy), ReadTools::None).ends_with("4-1.txt.]"));
        assert_eq!(
            result("x", Saved::Nowhere, FILES),
            "[Output elided to save context: 1 line, 1 character. Run the command again if \
             you need it.]"
        );
    }

    #[test]
    fn a_spilled_stub_tells_the_preview_from_the_whole_output() {
        let path = Path::new("/s/1/spill/c1-bash-ab.txt");
        let preview = "head\n\n[… lines 2–9 of 10 omitted. Some lines are longer than read_file \
                       shows (2000 characters).]\n\ntail\n";
        let stub = result(
            preview,
            Saved::Spill {
                path,
                bytes: 123_456,
            },
            ReadTools::FileTools { bash: true },
        );
        assert!(
            stub.starts_with("[Output elided to save context: a preview of 5 lines, "),
            "{stub}"
        );
        assert!(
            stub.contains(
                "The whole output, 123,456 bytes, is saved at /s/1/spill/c1-bash-ab.txt;"
            ),
            "{stub}"
        );
        assert!(
            stub.contains("`head -n N <path> | tail -n 1 | cut -c A-B`"),
            "{stub}"
        );
    }

    #[test]
    fn a_copy_stub_keeps_the_notes_on_long_lines_and_nul_bytes() {
        let content = format!("{}\n\0\n", "x".repeat(2500));
        let stub = result(&content, Saved::Copy(Path::new("/c.txt")), FILES);
        assert!(
            stub.contains("Some lines are longer than read_file shows"),
            "{stub}"
        );
        assert!(stub.contains("It contains NUL bytes"), "{stub}");
    }

    #[test]
    fn long_input_strings_become_markers_that_are_recognized() {
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
        assert!(is_shortened(&input, &short));
        assert_eq!(markers_in(&short).len(), 2);
        assert_eq!(shortened_input(&json!({"a": "b"}), 1000), None);
        // A string input works too.
        let bare = json!("q".repeat(1500));
        assert!(is_shortened(&bare, &shortened_input(&bare, 1000).unwrap()));
    }

    #[test]
    fn a_replacement_that_changes_more_than_markers_is_not_a_shortened_input() {
        let input = json!({"path": "a.rs", "content": "x".repeat(2000)});
        let mut wrong_count = shortened_input(&input, 1000).unwrap();
        wrong_count["content"] = json!(input_marker(1999));
        assert!(!is_shortened(&input, &wrong_count));
        let mut other_path = shortened_input(&input, 1000).unwrap();
        other_path["path"] = json!("b.rs");
        assert!(!is_shortened(&input, &other_path));
        assert!(!is_shortened(&input, &json!({"path": "a.rs"})));
    }

    #[test]
    fn only_a_whole_marker_counts_as_one() {
        let marker = "[elided from an earlier call to save context: 1,000 characters]";
        assert!(is_marker(marker));
        assert!(is_marker(&format!("  {marker}\n")));
        for other in [
            format!("the hook refuses {marker} in an input"),
            "[elided from an earlier call to save context: N characters]".into(),
            "[elided from an earlier call to save context: ,1 characters]".into(),
        ] {
            assert!(!is_marker(&other), "{other}");
        }
    }

    #[test]
    fn a_spill_file_is_trusted_only_for_its_own_call_and_content() {
        let tmp = tempfile::tempdir().unwrap();
        let spill = tmp.path().join("s1/spill");
        fs::create_dir_all(&spill).unwrap();
        let full = "line 1\nline 2\nline 3\n";
        fs::write(spill.join("call_1-bash-ab12.txt"), full).unwrap();
        fs::write(spill.join("call_2-bash-cd34.txt"), "other output\n").unwrap();
        let notice = |path: &Path| {
            format!(
                "line 1\n\n[… The full output is saved at {}.\nRead]",
                path.display()
            )
        };
        let own = spill.join("call_1-bash-ab12.txt");
        assert_eq!(
            spill_file(&notice(&own), "call_1", &spill),
            Some((own.clone(), full.len() as u64))
        );
        // Another call's file, a missing file, one outside the directory, an escape.
        assert_eq!(spill_file(&notice(&own), "call_9", &spill), None);
        let theirs = spill.join("call_2-bash-cd34.txt");
        assert_eq!(spill_file(&notice(&theirs), "call_1", &spill), None);
        let missing = spill.join("call_1-bash-zz.txt");
        assert_eq!(spill_file(&notice(&missing), "call_1", &spill), None);
        let elsewhere = tmp.path().join("other/spill/call_1-bash-ab12.txt");
        assert_eq!(spill_file(&notice(&elsewhere), "call_1", &spill), None);
        let escape = spill.join("call_1-../secret.txt");
        assert_eq!(spill_file(&notice(&escape), "call_1", &spill), None);
        // A file whose start doesn't match the result's head.
        let fake = format!(
            "other head\n\n[… The full output is saved at {}.\nRead]",
            own.display()
        );
        assert_eq!(spill_file(&fake, "call_1", &spill), None);
    }
}
