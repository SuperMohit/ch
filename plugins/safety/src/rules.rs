//! Permission rules: `"<tool>"` or `"<tool>:<glob>"`.

use anyhow::Context as _;
use globset::{GlobBuilder, GlobMatcher};

/// One `allow` / `ask` / `deny` rule.
///
/// The glob matches the bash `command` for `bash`, the workspace-relative `path` for
/// file tools and the `url` for `web_fetch`. For commands and URLs `*` also matches
/// `/`; for paths it doesn't (use `**`). A glob ending in ` *` also matches the
/// command without arguments, so `bash:make *` covers plain `make`.
#[derive(Debug, Clone)]
pub struct Rule {
    source: String,
    tool: GlobMatcher,
    pattern: Option<Vec<GlobMatcher>>,
}

/// Tools whose rule glob matches a path (so `*` stops at `/`).
fn is_path_tool(tool: &str) -> bool {
    crate::policy::FILE_TOOLS.contains(&tool)
}

fn compile(pattern: &str, literal_separator: bool) -> anyhow::Result<GlobMatcher> {
    Ok(GlobBuilder::new(pattern)
        .literal_separator(literal_separator)
        .build()
        .with_context(|| format!("invalid glob `{pattern}`"))?
        .compile_matcher())
}

impl Rule {
    /// Parses a rule. Errors on an empty tool name or an invalid glob.
    pub fn parse(rule: &str) -> anyhow::Result<Self> {
        let rule_str = rule.trim();
        let (tool, pattern) = match rule_str.split_once(':') {
            Some((t, p)) => (t.trim(), Some(p.trim())),
            None => (rule_str, None),
        };
        if tool.is_empty() {
            anyhow::bail!(
                "permission rule `{rule}` has no tool name (expected `tool` or `tool:glob`)"
            );
        }
        let tool_glob =
            compile(tool, false).with_context(|| format!("in permission rule `{rule}`"))?;
        let pattern = match pattern {
            None => None,
            Some(p) => {
                let sep = is_path_tool(tool);
                let mut globs =
                    vec![compile(p, sep).with_context(|| format!("in permission rule `{rule}`"))?];
                if let Some(bare) = p.strip_suffix(" *") {
                    globs.push(
                        compile(bare, sep)
                            .with_context(|| format!("in permission rule `{rule}`"))?,
                    );
                }
                Some(globs)
            }
        };
        Ok(Self {
            source: rule_str.to_string(),
            tool: tool_glob,
            pattern,
        })
    }

    /// The rule as written.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Whether the rule has a glob (`tool:glob`) rather than covering the whole tool.
    pub fn has_pattern(&self) -> bool {
        self.pattern.is_some()
    }

    /// Whether the rule covers a call to `tool` whose target is any of `targets`. A rule
    /// without a glob covers every call to the tool.
    pub fn matches(&self, tool: &str, targets: &[&str]) -> bool {
        if !self.tool.is_match(tool) {
            return false;
        }
        match &self.pattern {
            None => true,
            Some(globs) => targets.iter().any(|t| globs.iter().any(|g| g.is_match(t))),
        }
    }
}

/// Parses a list of rules from config.
pub fn parse_rules(list: &[String], field: &str) -> anyhow::Result<Vec<Rule>> {
    list.iter()
        .map(|r| Rule::parse(r).with_context(|| format!("plugins.safety.{field}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_tool_rule() {
        let r = Rule::parse("edit_file").unwrap();
        assert!(!r.has_pattern());
        assert!(r.matches("edit_file", &["anything"]));
        assert!(!r.matches("write_file", &["anything"]));
    }

    #[test]
    fn bash_glob_matches_slashes_and_bare_command() {
        let r = Rule::parse("bash:git commit *").unwrap();
        assert!(r.matches("bash", &["git commit -m 'x/y'"]));
        assert!(r.matches("bash", &["git commit"]));
        assert!(!r.matches("bash", &["git commitx"]));
        assert!(!r.matches("bash", &["git push"]));
    }

    #[test]
    fn path_glob_respects_separators() {
        let r = Rule::parse("edit_file:src/*.rs").unwrap();
        assert!(r.matches("edit_file", &["src/main.rs"]));
        assert!(!r.matches("edit_file", &["src/a/b.rs"]));
        let r = Rule::parse("edit_file:src/**").unwrap();
        assert!(r.matches("edit_file", &["src/a/b.rs"]));
    }

    #[test]
    fn url_rule_keeps_colons() {
        let r = Rule::parse("web_fetch:https://docs.rs/*").unwrap();
        assert!(r.matches("web_fetch", &["https://docs.rs/serde/latest"]));
        assert!(!r.matches("web_fetch", &["https://evil.example/docs.rs/"]));
    }

    #[test]
    fn tool_glob() {
        let r = Rule::parse("mcp__*").unwrap();
        assert!(r.matches("mcp__github__list", &[]));
    }

    #[test]
    fn invalid_rules() {
        assert!(Rule::parse(":foo").is_err());
        assert!(Rule::parse("bash:[").is_err());
    }
}
