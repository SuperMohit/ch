//! Markdown rendering for the terminal.
//!
//! Assistant text is streamed raw as it arrives; when a line completes, it is restyled
//! in place with [`MarkdownLines`], which keeps the little state markdown needs across
//! lines (whether we're inside a code fence). Whole messages from slash commands are
//! rendered with termimad in one go ([`render_message`]).

use nu_ansi_term::{Color, Style};
use termimad::MadSkin;

use crate::format::dim;

/// Restyles complete lines of streamed markdown.
pub struct MarkdownLines {
    skin: MadSkin,
    in_fence: bool,
}

impl Default for MarkdownLines {
    fn default() -> Self {
        Self::new()
    }
}

impl MarkdownLines {
    pub fn new() -> Self {
        Self {
            skin: skin(),
            in_fence: false,
        }
    }

    /// Forgets fence state (at the end of an assistant message).
    pub fn reset(&mut self) {
        self.in_fence = false;
    }

    /// The styled form of a complete `line` (without its newline), or `None` when the
    /// raw text is already right and doesn't need to be redrawn.
    pub fn render(&mut self, line: &str) -> Option<String> {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            self.in_fence = !self.in_fence;
            return Some(dim(line));
        }
        if self.in_fence {
            if line.is_empty() {
                return None;
            }
            return Some(Color::Fixed(180).paint(line).to_string());
        }
        if trimmed.is_empty() {
            return None;
        }
        let indent = &line[..line.len() - trimmed.len()];

        // Headings: drop the hashes, bold the text.
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
            let text = self.inline(trimmed[hashes..].trim());
            let style = if hashes == 1 {
                Style::new().bold().underline()
            } else {
                Style::new().bold()
            };
            return Some(format!(
                "{indent}{}",
                style.paint(strip_inline_reset(&text))
            ));
        }
        // Horizontal rules.
        if is_rule(trimmed) {
            return Some(dim("────────────────────────────────"));
        }
        // Bullets.
        for marker in ["- ", "* ", "+ "] {
            if let Some(rest) = trimmed.strip_prefix(marker) {
                return Some(format!("{indent}• {}", self.inline(rest)));
            }
        }
        // Block quotes.
        if let Some(rest) = trimmed.strip_prefix('>') {
            return Some(format!(
                "{indent}{} {}",
                dim("│"),
                Style::new().italic().paint(rest.trim_start())
            ));
        }
        // Inline styles only.
        if has_inline_markup(line) {
            let styled = self.inline(line);
            if styled != line {
                return Some(styled);
            }
        }
        None
    }

    fn inline(&self, text: &str) -> String {
        if has_inline_markup(text) {
            self.skin.inline(text).to_string()
        } else {
            text.to_string()
        }
    }
}

/// Renders a whole markdown message (slash command output) for the terminal.
pub fn render_message(text: &str) -> String {
    skin().term_text(text).to_string()
}

fn skin() -> MadSkin {
    MadSkin::default()
}

fn has_inline_markup(s: &str) -> bool {
    s.contains('`') || s.contains("**") || s.contains('*') || s.contains("~~") || s.contains('_')
}

fn is_rule(s: &str) -> bool {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    s.len() >= 3
        && (s.chars().all(|c| c == '-')
            || s.chars().all(|c| c == '*')
            || s.chars().all(|c| c == '_'))
}

/// Inline rendering may end with a reset sequence; inside an outer style that would
/// cut the style short, so heading text is styled from its plain form instead.
fn strip_inline_reset(s: &str) -> String {
    crate::format::strip_ansi(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::strip_ansi;

    fn plain(md: &mut MarkdownLines, line: &str) -> Option<String> {
        md.render(line).map(|s| strip_ansi(&s))
    }

    #[test]
    fn plain_lines_are_left_alone() {
        let mut md = MarkdownLines::new();
        assert_eq!(md.render("Just some text."), None);
        assert_eq!(md.render(""), None);
    }

    #[test]
    fn inline_styles_are_rendered() {
        let mut md = MarkdownLines::new();
        let out = md.render("Use **bold** and `code`").unwrap();
        assert_ne!(out, "Use **bold** and `code`");
        assert_eq!(strip_ansi(&out), "Use bold and code");
    }

    #[test]
    fn headings_bullets_quotes_rules() {
        let mut md = MarkdownLines::new();
        assert_eq!(plain(&mut md, "## Plan").unwrap(), "Plan");
        assert_eq!(plain(&mut md, "- first").unwrap(), "• first");
        assert_eq!(plain(&mut md, "  * nested").unwrap(), "  • nested");
        assert_eq!(plain(&mut md, "> quoted").unwrap(), "│ quoted");
        assert!(plain(&mut md, "---").unwrap().starts_with("────"));
        assert_eq!(md.render("#hashtag"), None);
    }

    #[test]
    fn code_fences_keep_contents_verbatim() {
        let mut md = MarkdownLines::new();
        assert_eq!(plain(&mut md, "```rust").unwrap(), "```rust");
        // Inside a fence, markdown syntax is not interpreted.
        assert_eq!(plain(&mut md, "let x = **y**;").unwrap(), "let x = **y**;");
        assert_eq!(plain(&mut md, "- not a bullet").unwrap(), "- not a bullet");
        assert_eq!(plain(&mut md, "```").unwrap(), "```");
        assert_eq!(plain(&mut md, "- bullet").unwrap(), "• bullet");
    }

    #[test]
    fn reset_closes_open_fences() {
        let mut md = MarkdownLines::new();
        md.render("```");
        md.reset();
        assert_eq!(md.render("plain"), None);
    }

    #[test]
    fn messages_render() {
        let out = strip_ansi(&render_message("# Help\n\n- `/exit` quits"));
        assert!(out.contains("Help"), "{out}");
        assert!(out.contains("/exit quits"), "{out}");
    }
}
