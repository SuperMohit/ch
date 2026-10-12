//! The "tool rules" system block, generated from the tools that are registered.
//!
//! The block never names a tool that isn't registered, so it stays correct whichever
//! tool plugins are enabled. Three shapes:
//!
//! - **full**: `read_file` and `edit_file` exist → `prompts/tools-full.md`;
//! - **bash-only**: `bash` exists and none of the file tools do →
//!   `prompts/tools-bash-only.md`;
//! - **mixed**: anything else → a short generic block built here.
//!
//! Short notes for `web_fetch` and `update_plan` are added when they exist.

use ch_kernel::ToolSpec;

const TOOLS_FULL: &str = include_str!("../../../prompts/tools-full.md");
const TOOLS_BASH_ONLY: &str = include_str!("../../../prompts/tools-bash-only.md");

/// The file and search tools from `tools-fs`, with what each is for.
const FILE_TOOLS: &[(&str, &str)] = &[
    (
        "read_file",
        "read a file (the whole file, or a line range of a large one)",
    ),
    (
        "edit_file",
        "change part of an existing file by exact string replacement",
    ),
    (
        "write_file",
        "create a new file, or replace a small file entirely",
    ),
    ("list_files", "see what a directory contains"),
    ("glob_files", "find files by name pattern"),
    ("grep_text", "search file contents"),
];

const NO_TOOLS: &str = "# Tools\n\n\
No tools are available in this session: you can't read files or run commands. Work \
from what the user shares in the conversation, and say so when an answer depends on \
something you can't check.";

const WEB_FETCH_NOTE: &str = "- `web_fetch` retrieves a web page as markdown. Use it \
for documentation, changelogs or issues you need to check, and for URLs the user gives \
you. Don't guess URLs, and if a fetch fails, say so rather than describing the page \
from memory.";

const UPDATE_PLAN_NOTE: &str = "- `update_plan` keeps a visible task plan. Use it for \
multi-step work: send the complete list on every update, keep exactly one item \
`in_progress`, and mark items `completed` as soon as they are done. Skip it for quick, \
single-step tasks.";

/// Which shape of tool block applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// No tools at all.
    None,
    /// The dedicated file tools exist.
    Full,
    /// Only a shell for file work.
    BashOnly,
    /// Some other combination.
    Mixed,
}

/// Picks the block shape for a set of tools.
pub fn mode(tools: &[ToolSpec]) -> Mode {
    let has = |name: &str| tools.iter().any(|t| t.name == name);
    let any_file_tool = FILE_TOOLS.iter().any(|(n, _)| has(n));
    if tools.is_empty() {
        Mode::None
    } else if has("read_file") && has("edit_file") {
        Mode::Full
    } else if has("bash") && !any_file_tool {
        Mode::BashOnly
    } else {
        Mode::Mixed
    }
}

/// The tool rules block for `tools`, or `None` when there is nothing useful to say.
pub fn tool_rules(tools: &[ToolSpec]) -> Option<String> {
    let has = |name: &str| tools.iter().any(|t| t.name == name);
    let main = match mode(tools) {
        Mode::None => return Some(NO_TOOLS.to_string()),
        Mode::Full => render(TOOLS_FULL, &has, &file_tool_list(&has)),
        Mode::BashOnly => render(TOOLS_BASH_ONLY, &has, ""),
        Mode::Mixed => mixed(tools, &has),
    };

    let mut notes = Vec::new();
    if has("web_fetch") {
        notes.push(WEB_FETCH_NOTE);
    }
    if has("update_plan") {
        notes.push(UPDATE_PLAN_NOTE);
    }

    let mut out = main;
    if !notes.is_empty() {
        let heading = if out.is_empty() {
            "# Using your tools"
        } else {
            "# Other tools"
        };
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(heading);
        out.push_str("\n\n");
        out.push_str(&notes.join("\n"));
    }
    (!out.trim().is_empty()).then_some(out)
}

/// Bullets for the registered file tools, or an empty string if there are none.
fn file_tool_list(has: &dyn Fn(&str) -> bool) -> String {
    let lines: Vec<String> = FILE_TOOLS
        .iter()
        .filter(|(name, _)| has(name))
        .map(|(name, what)| format!("- `{name}`: {what}."))
        .collect();
    if lines.is_empty() {
        String::new()
    } else {
        format!("Which tool for which job:\n\n{}", lines.join("\n"))
    }
}

/// The generic block for tool sets that are neither "full" nor "bash-only".
fn mixed(tools: &[ToolSpec], has: &dyn Fn(&str) -> bool) -> String {
    let mut bullets: Vec<String> = Vec::new();
    let list = file_tool_list(has);
    let any_file_tool = !list.is_empty();
    if has("bash") {
        if any_file_tool {
            bullets.push(
                "- Prefer the file tools above over `bash` for the jobs they cover. Use `bash` \
                 for builds, tests, linters, package managers and git."
                    .into(),
            );
        }
        if !has("edit_file") {
            bullets.push(
                "- File edits go through `bash`, and **nothing validates them**: read the file \
                 back after every change to confirm it says what you meant."
                    .into(),
            );
        }
    }
    if has("edit_file") && !has("read_file") {
        bullets.push(
            "- Edit with exact, unique snippets of the current text, and keep the original \
             indentation."
                .into(),
        );
    }
    if tools.iter().any(|t| t.read_only) {
        bullets.push(
            "- **Batch independent read-only calls in a single step**: they run in parallel."
                .into(),
        );
    }
    if !any_file_tool && bullets.is_empty() {
        return String::new();
    }
    let mut out = String::from("# Using your tools\n\n");
    if any_file_tool {
        out.push_str(&list);
        out.push_str("\n\n");
    }
    out.push_str(&bullets.join("\n"));
    out.trim_end().to_string()
}

/// Renders a prompt template.
///
/// - Lines between `<!-- when: <tool> -->` and `<!-- end -->` are kept only when `has(tool)`.
///   Sections may nest.
/// - Any other HTML comment (single- or multi-line) is dropped.
/// - A line that is exactly `{{file_tools}}` is replaced by `file_tools` (removed when empty).
/// - Runs of blank lines collapse to one, and the result is trimmed.
pub fn render(template: &str, has: &dyn Fn(&str) -> bool, file_tools: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut conditions: Vec<bool> = Vec::new();
    let mut in_comment = false;
    for line in template.lines() {
        let trimmed = line.trim();
        if in_comment {
            if trimmed.contains("-->") {
                in_comment = false;
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("<!--") {
            match rest.split_once("-->") {
                Some((inner, _)) => {
                    let inner = inner.trim();
                    if let Some(tool) = inner.strip_prefix("when:") {
                        conditions.push(has(tool.trim()));
                    } else if inner == "end" {
                        conditions.pop();
                    }
                }
                None => in_comment = true,
            }
            continue;
        }
        if conditions.iter().any(|keep| !keep) {
            continue;
        }
        if trimmed == "{{file_tools}}" {
            if !file_tools.is_empty() {
                out.push(file_tools);
            }
            continue;
        }
        out.push(line);
    }

    let joined = out.join("\n");
    let mut result = String::with_capacity(joined.len());
    let mut blank_run = 0;
    for line in joined.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        result.push_str(line.trim_end());
        result.push('\n');
    }
    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_keeps_or_drops_sections() {
        let t = "a\n<!-- when: x -->\nb\n<!-- end -->\n<!-- note\nmore -->\nc\n\n\n\nd";
        assert_eq!(render(t, &|n| n == "x", ""), "a\nb\nc\n\nd");
        assert_eq!(render(t, &|_| false, ""), "a\nc\n\nd");
    }

    #[test]
    fn render_nested_sections() {
        let t = "<!-- when: a -->\n1\n<!-- when: b -->\n2\n<!-- end -->\n3\n<!-- end -->\n4";
        assert_eq!(render(t, &|n| n == "a", ""), "1\n3\n4");
        assert_eq!(render(t, &|_| true, ""), "1\n2\n3\n4");
    }

    #[test]
    fn render_replaces_file_tools() {
        let t = "h\n\n{{file_tools}}\n\nz";
        assert_eq!(render(t, &|_| true, "LIST"), "h\n\nLIST\n\nz");
        assert_eq!(render(t, &|_| true, ""), "h\n\nz");
    }
}
