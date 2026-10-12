//! Parsing command files and rendering their templates.

/// The parts of a command file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CommandFile {
    /// From the `description:` front-matter key, or the first line of the body.
    pub description: String,
    /// From the `argument-hint:` (or `usage:`) front-matter key.
    pub usage: String,
    /// The prompt template (the file without its front matter).
    pub template: String,
}

/// Longest description taken from the first line of the body.
const MAX_DESCRIPTION: usize = 80;

/// Parses a command file. Front matter is optional:
///
/// ```markdown
/// ---
/// description: Review the staged changes
/// argument-hint: [focus]
/// ---
/// Review the staged changes. Focus on: $ARGUMENTS
/// ```
///
/// Only simple `key: value` lines are understood; other keys are ignored.
pub fn parse(text: &str) -> CommandFile {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (front, body) = split_front_matter(text);
    let mut description = String::new();
    let mut usage = String::new();
    for line in front.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = unquote(value.trim());
        match key.trim().to_ascii_lowercase().as_str() {
            "description" => description = value.to_string(),
            "argument-hint" | "argument_hint" | "usage" => usage = value.to_string(),
            _ => {}
        }
    }
    let template = body.trim().to_string();
    if description.is_empty() {
        description = first_line_description(&template);
    }
    CommandFile {
        description,
        usage,
        template,
    }
}

/// Splits `---` front matter from the body. Without a complete front-matter block, the
/// whole text is the body.
fn split_front_matter(text: &str) -> (&str, &str) {
    let mut lines = text.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return ("", text);
    };
    if first.trim_end() != "---" {
        return ("", text);
    }
    let start = first.len();
    let mut pos = start;
    for line in lines {
        if line.trim_end() == "---" {
            return (&text[start..pos], &text[pos + line.len()..]);
        }
        pos += line.len();
    }
    ("", text)
}

fn unquote(s: &str) -> &str {
    for q in ['"', '\''] {
        if let Some(inner) = s.strip_prefix(q).and_then(|r| r.strip_suffix(q)) {
            return inner;
        }
    }
    s
}

fn first_line_description(template: &str) -> String {
    let line = template
        .lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if line.chars().count() <= MAX_DESCRIPTION {
        return line.to_string();
    }
    let cut: String = line.chars().take(MAX_DESCRIPTION - 1).collect();
    format!("{}…", cut.trim_end())
}

/// Fills a template: every `$ARGUMENTS` becomes `args`. When the template has no
/// `$ARGUMENTS` placeholder, non-empty `args` are appended after a blank line so they
/// aren't lost.
pub fn render(template: &str, args: &str) -> String {
    if template.contains("$ARGUMENTS") {
        template.replace("$ARGUMENTS", args)
    } else if args.is_empty() {
        template.to_string()
    } else {
        format!("{template}\n\n{args}")
    }
}

/// Whether `name` can be typed as `/name`: letters, digits, `-`, `_`, `.` and `:`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_matter_description_and_usage() {
        let f = parse(
            "---\ndescription: \"Review the diff\"\nargument-hint: [focus]\nother: x\n---\n\nReview it. $ARGUMENTS\n",
        );
        assert_eq!(f.description, "Review the diff");
        assert_eq!(f.usage, "[focus]");
        assert_eq!(f.template, "Review it. $ARGUMENTS");
    }

    #[test]
    fn first_line_when_no_front_matter() {
        let f = parse("\n# Explain the code\n\nExplain $ARGUMENTS in detail.");
        assert_eq!(f.description, "Explain the code");
        assert_eq!(f.usage, "");
        assert!(f.template.starts_with("# Explain the code"));
    }

    #[test]
    fn front_matter_without_description_falls_back() {
        let f = parse("---\nargument-hint: <file>\n---\nSummarise $ARGUMENTS");
        assert_eq!(f.description, "Summarise $ARGUMENTS");
        assert_eq!(f.usage, "<file>");
    }

    #[test]
    fn unterminated_front_matter_is_body() {
        let f = parse("---\ndescription: x\nno end");
        assert_eq!(f.template, "---\ndescription: x\nno end");
        assert_eq!(f.description, "---");
    }

    #[test]
    fn long_first_lines_are_shortened() {
        let f = parse(&"word ".repeat(40));
        assert!(f.description.chars().count() <= MAX_DESCRIPTION);
        assert!(f.description.ends_with('…'));
    }

    #[test]
    fn render_replaces_or_appends() {
        assert_eq!(
            render("Fix $ARGUMENTS now. ($ARGUMENTS)", "it"),
            "Fix it now. (it)"
        );
        assert_eq!(render("Fix $ARGUMENTS", ""), "Fix ");
        assert_eq!(render("Do it", ""), "Do it");
        assert_eq!(render("Do it", "fast"), "Do it\n\nfast");
    }

    #[test]
    fn names() {
        assert!(valid_name("review-pr"));
        assert!(valid_name("git:commit"));
        assert!(!valid_name("two words"));
        assert!(!valid_name(""));
    }
}
