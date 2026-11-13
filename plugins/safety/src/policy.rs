//! The permission policy: modes, rules, the built-in bash lists and sensitive paths.

use std::path::{Component, Path};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::bash::{self, Segment};
use crate::path_guard::PathGuard;
use crate::rules::{Rule, parse_rules};

/// Tools from `tools-fs`; their rule globs match the workspace-relative `path`.
pub const FILE_TOOLS: &[&str] = &[
    "read_file",
    "edit_file",
    "write_file",
    "list_files",
    "glob_files",
    "grep_text",
];

/// File tools that may also read inside the session's directory (spilled output).
pub const SESSION_READERS: &[&str] = &["read_file", "grep_text"];

/// Tools that change files in the workspace.
pub const WRITE_TOOLS: &[&str] = &["edit_file", "write_file"];

/// Tools that are not read-only but only change ch's own state, so they never ask.
pub const STATE_ONLY_TOOLS: &[&str] = &["update_plan"];

/// How much runs without asking.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Read-only tools run; edits, writes, non-allowlisted bash and web fetches ask.
    #[default]
    Ask,
    /// Like `ask`, but edits and writes inside the workspace are allowed (sensitive
    /// paths still ask).
    AcceptEdits,
    /// Everything runs except the built-in deny list and `deny` / `ask` rules.
    Bypass,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Ask => "ask",
            Mode::AcceptEdits => "accept-edits",
            Mode::Bypass => "bypass",
        }
    }
}

/// `[plugins.safety]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SafetyConfig {
    pub mode: Mode,
    /// Rules that run without asking (`"edit_file"`, `"bash:npm test *"`).
    pub allow: Vec<String>,
    /// Rules that always ask, even if allowed.
    pub ask: Vec<String>,
    /// Rules that are always denied.
    pub deny: Vec<String>,
}

/// What the policy says about one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// `key` is an allow rule covering this call (for "always allow").
    Ask {
        reason: String,
        key: Option<String>,
    },
    Deny {
        reason: String,
    },
}

impl Decision {
    fn ask(reason: impl Into<String>, key: Option<String>) -> Self {
        Self::Ask {
            reason: reason.into(),
            key,
        }
    }

    fn deny(reason: impl Into<String>) -> Self {
        Self::Deny {
            reason: reason.into(),
        }
    }
}

/// The compiled policy. Precedence: built-in deny list > `deny` > `ask` > `allow` >
/// the mode's default.
#[derive(Debug, Clone)]
pub struct Policy {
    mode: Mode,
    allow: Vec<Rule>,
    ask: Vec<Rule>,
    deny: Vec<Rule>,
    guard: Arc<PathGuard>,
}

impl Policy {
    /// Compiles the rules in `config`; errors name the bad rule.
    pub fn new(config: &SafetyConfig, guard: Arc<PathGuard>) -> anyhow::Result<Self> {
        Ok(Self {
            mode: config.mode,
            allow: parse_rules(&config.allow, "allow")?,
            ask: parse_rules(&config.ask, "ask")?,
            deny: parse_rules(&config.deny, "deny")?,
            guard,
        })
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Decides one call. `read_only` is the tool's own flag.
    pub fn decide(&self, tool: &str, input: &Value, read_only: bool) -> Decision {
        self.decide_with(tool, input, read_only, None)
    }

    /// [`Policy::decide`] where [`SESSION_READERS`] may also read inside `session_dir`.
    pub fn decide_with(
        &self,
        tool: &str,
        input: &Value,
        read_only: bool,
        session_dir: Option<&Path>,
    ) -> Decision {
        // First, by the reserved prefix: an outside tool never reaches a built-in
        // tool's checks, and a built-in name never reaches these.
        if ch_kernel::tool_source::is_outside_tool(tool) {
            return self.decide_outside(tool, input, read_only);
        }
        if tool == "bash" {
            let command = input.get("command").and_then(Value::as_str).unwrap_or("");
            return self.decide_bash(command);
        }
        if FILE_TOOLS.contains(&tool) {
            return self.decide_file(tool, input, read_only, session_dir);
        }
        if tool == "web_fetch" {
            return self.decide_fetch(input);
        }
        self.decide_other(tool, input, read_only)
    }

    /// `deny` and `ask` rules, which apply before anything is allowed.
    fn deny_or_ask(&self, tool: &str, targets: &[&str]) -> Option<Decision> {
        if let Some(r) = self.deny.iter().find(|r| r.matches(tool, targets)) {
            return Some(Decision::deny(format!(
                "Denied by the rule `{}` in plugins.safety.deny. Don't retry this call; \
                 choose another approach or ask the user to change the rule.",
                r.source()
            )));
        }
        if let Some(r) = self.ask.iter().find(|r| r.matches(tool, targets)) {
            // An ask rule means "always ask", so there is nothing to remember.
            return Some(Decision::ask(
                format!("the rule `{}` in plugins.safety.ask", r.source()),
                None,
            ));
        }
        None
    }

    fn allowed(&self, tool: &str, targets: &[&str], with_pattern_only: bool) -> bool {
        self.allow
            .iter()
            .any(|r| (!with_pattern_only || r.has_pattern()) && r.matches(tool, targets))
    }

    fn decide_file(
        &self,
        tool: &str,
        input: &Value,
        read_only: bool,
        session_dir: Option<&Path>,
    ) -> Decision {
        let raw = input.get("path").and_then(Value::as_str);
        let writes = WRITE_TOOLS.contains(&tool);
        let readable = session_dir.filter(|_| read_only && SESSION_READERS.contains(&tool));
        let rel = match raw {
            Some(p) => match self.guard.resolve_readable(p, readable) {
                Ok(abs) => self.guard.relative(&abs),
                Err(msg) => return Decision::deny(msg),
            },
            None => ".".to_string(),
        };
        let mut targets = vec![rel.as_str()];
        if let Some(r) = raw {
            targets.push(r);
        }
        if let Some(d) = self.deny_or_ask(tool, &targets) {
            return d;
        }
        // Allow rules see only the resolved path: `src/**` matches the raw
        // `src/../.git/hooks/pre-commit`.
        let allow_targets = [rel.as_str()];
        if writes && self.mode != Mode::Bypass && is_sensitive(Path::new(&rel)) {
            // Only a rule naming the path (not a bare `edit_file`) skips the question.
            if self.allowed(tool, &allow_targets, true) {
                return Decision::Allow;
            }
            return Decision::ask(
                format!("`{rel}` is a sensitive path (git internals, secrets or ch config)"),
                Some(format!("{tool}:{}", globset::escape(&rel))),
            );
        }
        if self.allowed(tool, &allow_targets, false) {
            return Decision::Allow;
        }
        if read_only && !writes {
            return Decision::Allow;
        }
        match self.mode {
            Mode::AcceptEdits | Mode::Bypass => Decision::Allow,
            Mode::Ask => Decision::ask(
                format!("{tool} changes `{rel}` (mode `ask` asks before every edit)"),
                Some(tool.to_string()),
            ),
        }
    }

    fn decide_bash(&self, command: &str) -> Decision {
        let parsed = bash::parse(command);
        if let Some(rule) = bash::denied(command, &parsed) {
            return Decision::deny(format!(
                "`{}` is on the built-in deny list ({}). {}",
                ch_kernel::util::truncate_middle(command.trim(), 200),
                rule.name,
                rule.reason
            ));
        }
        // User deny/ask rules see the whole command and every part of it.
        let normalized: Vec<String> = parsed
            .all()
            .map(|s| bash::Cmd::of(s).normalized())
            .collect();
        let mut targets: Vec<&str> = vec![command.trim()];
        for (seg, norm) in parsed.all().zip(&normalized) {
            targets.push(&seg.raw);
            targets.push(norm);
        }
        if let Some(d) = self.deny_or_ask("bash", &targets) {
            return d;
        }
        if parsed.all().next().is_none() && self.mode != Mode::Bypass {
            return Decision::ask(
                format!(
                    "no command was found in `{}` to check",
                    ch_kernel::util::truncate_middle(command.trim(), 120)
                ),
                None,
            );
        }
        // Allow rules only count part by part, so `bash:git commit *` can't
        // smuggle in `git commit -m x && rm -rf src`.
        let unapproved: Vec<&Segment> = parsed
            .all()
            .zip(&normalized)
            .filter(|(seg, norm)| {
                !bash::segment_is_read_only(seg)
                    && !self.allowed("bash", &[seg.raw.as_str(), norm.as_str()], false)
            })
            .map(|(seg, _)| seg)
            .collect();
        if unapproved.is_empty() || self.mode == Mode::Bypass {
            return Decision::Allow;
        }
        let mut keys: Vec<String> = unapproved.iter().map(|s| bash::ask_key(s)).collect();
        keys.sort();
        keys.dedup();
        let key = if keys.len() == 1 { keys.pop() } else { None };
        let first = &unapproved[0].raw;
        let reason = if parsed.all().count() > 1 {
            format!(
                "`{}` (part of a compound command) is not on the read-only allowlist",
                ch_kernel::util::truncate_middle(first, 120)
            )
        } else {
            format!(
                "`{}` is not on the read-only allowlist",
                ch_kernel::util::truncate_middle(first, 120)
            )
        };
        Decision::ask(reason, key)
    }

    fn decide_fetch(&self, input: &Value) -> Decision {
        let raw = input.get("url").and_then(Value::as_str).unwrap_or("");
        let parsed = url::Url::parse(raw.trim()).ok();
        let normalized = parsed.as_ref().map(|u| u.as_str().to_string());
        let mut targets = vec![raw];
        if let Some(n) = &normalized {
            targets.push(n);
        }
        if let Some(d) = self.deny_or_ask("web_fetch", &targets) {
            return d;
        }
        let allow_targets: Vec<&str> = normalized.as_deref().into_iter().collect();
        if self.allowed("web_fetch", &allow_targets, false) || self.mode == Mode::Bypass {
            return Decision::Allow;
        }
        // Network access is not an edit, so accept-edits still asks: a fetched page
        // could carry instructions, and a URL can carry data out.
        match parsed.as_ref().and_then(|u| u.host_str().map(|h| (u, h))) {
            Some((u, host)) => {
                let origin = match u.port() {
                    Some(port) => format!("{}://{host}:{port}", u.scheme()),
                    None => format!("{}://{host}", u.scheme()),
                };
                Decision::ask(
                    format!("fetching from {origin} needs approval"),
                    Some(format!("web_fetch:{}/*", globset::escape(&origin))),
                )
            }
            None => Decision::ask(
                format!(
                    "`{}` has no host to approve",
                    ch_kernel::util::truncate_middle(raw.trim(), 120)
                ),
                None,
            ),
        }
    }

    fn decide_other(&self, tool: &str, input: &Value, read_only: bool) -> Decision {
        let target = ["path", "command", "url"]
            .iter()
            .find_map(|k| input.get(*k).and_then(Value::as_str))
            .unwrap_or("");
        let targets = [target];
        if let Some(d) = self.deny_or_ask(tool, &targets) {
            return d;
        }
        if read_only
            || STATE_ONLY_TOOLS.contains(&tool)
            || self.mode == Mode::Bypass
            || self.allowed(tool, &targets, false)
        {
            return Decision::Allow;
        }
        Decision::ask(
            format!("{tool} is not read-only"),
            Some(globset::escape(tool)),
        )
    }
}

/// Most string values of an outside tool's input checked against `deny` and `ask`
/// rules. Past that, the call asks.
pub const MAX_OUTSIDE_STRINGS: usize = 4096;

/// Deepest nesting of an outside tool's input that is searched for strings.
const MAX_OUTSIDE_DEPTH: usize = 64;

/// Every string in `input`, at any depth: values and object keys (a server may take a
/// map keyed by path). Returns `None`
/// when there are more than [`MAX_OUTSIDE_STRINGS`] or the input nests deeper than
/// [`MAX_OUTSIDE_DEPTH`]: an input too big to check can't be allowed unchecked.
pub fn string_values(input: &Value) -> Option<Vec<&str>> {
    fn walk<'a>(v: &'a Value, depth: usize, out: &mut Vec<&'a str>) -> bool {
        if depth > MAX_OUTSIDE_DEPTH {
            return false;
        }
        match v {
            Value::String(s) => {
                out.push(s);
                out.len() <= MAX_OUTSIDE_STRINGS
            }
            Value::Array(items) => items.iter().all(|i| walk(i, depth + 1, out)),
            Value::Object(map) => map.iter().all(|(k, v)| {
                out.push(k);
                out.len() <= MAX_OUTSIDE_STRINGS && walk(v, depth + 1, out)
            }),
            _ => true,
        }
    }
    let mut out = Vec::new();
    walk(input, 0, &mut out).then_some(out)
}

impl Policy {
    /// Tools from MCP servers and external plugins (ADR 0006 §8). `deny` and `ask` rules
    /// match against every string in the input; `allow` rules count only as bare names
    /// or name globs (`mcp__github__*`), since a pattern can't say what an unknown
    /// tool's input means. `read_only` is true only for tools the user's config lists
    /// in `read_only_tools`.
    fn decide_outside(&self, tool: &str, input: &Value, read_only: bool) -> Decision {
        let Some(strings) = string_values(input) else {
            if let Some(d) = self.deny_or_ask(tool, &[]) {
                return d;
            }
            return Decision::ask(
                format!("{tool}'s input is too large or too deeply nested to check"),
                None,
            );
        };
        // A rule without a pattern matches whatever the targets are; one with a pattern
        // matches if any string does. With no strings, `""` stands in, so `tool:*`
        // still covers the call.
        let targets: Vec<&str> = if strings.is_empty() {
            vec![""]
        } else {
            strings
        };
        if let Some(d) = self.deny_or_ask(tool, &targets) {
            return d;
        }
        let allowed = self
            .allow
            .iter()
            .any(|r| !r.has_pattern() && r.matches(tool, &[]));
        if read_only || allowed || self.mode == Mode::Bypass {
            return Decision::Allow;
        }
        Decision::ask(
            format!("{tool} comes from outside ch and isn't marked read-only"),
            Some(globset::escape(tool)),
        )
    }
}

/// Paths where a write always asks outside bypass mode: git internals, `.env*` files,
/// ch's project config, and what makes ch run other programs (`.mcp.json`, the
/// project's `.ch/plugins/`). Names compare case-insensitively, because on
/// case-insensitive file systems `.GIT/config` is `.git/config`.
pub fn is_sensitive(rel: &Path) -> bool {
    let names: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(n) => n.to_str().map(str::to_ascii_lowercase),
            _ => None,
        })
        .collect();
    if names.iter().any(|n| n == ".git") {
        return true;
    }
    if names
        .last()
        .is_some_and(|n| n.starts_with(".env") || n == ".mcp.json")
    {
        return true;
    }
    if names.windows(2).any(|w| w[0] == ".ch" && w[1] == "plugins") {
        return true;
    }
    names.ends_with(&[".ch".to_string(), "config.toml".to_string()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fixture {
        _dir: tempfile::TempDir,
        guard: Arc<PathGuard>,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        let guard = Arc::new(PathGuard::new(dir.path()).unwrap());
        Fixture { _dir: dir, guard }
    }

    fn policy(f: &Fixture, toml_cfg: &str) -> Policy {
        let cfg: SafetyConfig = toml::from_str(toml_cfg).unwrap();
        Policy::new(&cfg, f.guard.clone()).unwrap()
    }

    fn bash(p: &Policy, cmd: &str) -> Decision {
        p.decide("bash", &json!({ "command": cmd }), false)
    }

    fn edit(p: &Policy, path: &str) -> Decision {
        p.decide("edit_file", &json!({ "path": path }), false)
    }

    fn key(d: &Decision) -> Option<&str> {
        match d {
            Decision::Ask { key, .. } => key.as_deref(),
            _ => None,
        }
    }

    fn is_ask(d: &Decision) -> bool {
        matches!(d, Decision::Ask { .. })
    }

    fn is_deny(d: &Decision) -> bool {
        matches!(d, Decision::Deny { .. })
    }

    #[test]
    fn config_parses_modes() {
        let c: SafetyConfig = toml::from_str("mode = \"accept-edits\"").unwrap();
        assert_eq!(c.mode, Mode::AcceptEdits);
        let c: SafetyConfig = toml::from_str("").unwrap();
        assert_eq!(c.mode, Mode::Ask);
        assert!(toml::from_str::<SafetyConfig>("mode = \"yolo\"").is_err());
    }

    #[test]
    fn ask_mode() {
        let f = fixture();
        let p = policy(&f, "");
        assert_eq!(
            p.decide("read_file", &json!({"path": "src/main.rs"}), true),
            Decision::Allow
        );
        assert_eq!(
            p.decide("grep_text", &json!({"query": "x"}), true),
            Decision::Allow
        );
        let d = edit(&p, "src/main.rs");
        assert_eq!(key(&d), Some("edit_file"));
        let d = p.decide("write_file", &json!({"path": "new.txt"}), false);
        assert_eq!(key(&d), Some("write_file"));
        assert_eq!(bash(&p, "git status"), Decision::Allow);
        assert_eq!(key(&bash(&p, "npm test")), Some("bash:npm test *"));
        assert!(is_deny(&bash(&p, "sudo ls")));
        let d = p.decide("web_fetch", &json!({"url": "https://docs.rs/serde"}), true);
        assert_eq!(key(&d), Some("web_fetch:https://docs.rs/*"));
        assert_eq!(p.decide("update_plan", &json!({}), false), Decision::Allow);
        assert_eq!(
            key(&p.decide("mcp__x__y", &json!({}), false)),
            Some("mcp__x__y")
        );
    }

    #[test]
    fn accept_edits_mode() {
        let f = fixture();
        let p = policy(&f, "mode = \"accept-edits\"");
        assert_eq!(edit(&p, "src/main.rs"), Decision::Allow);
        assert_eq!(
            p.decide("write_file", &json!({"path": "new/file.txt"}), false),
            Decision::Allow
        );
        assert!(is_ask(&bash(&p, "npm test")));
        assert_eq!(bash(&p, "ls"), Decision::Allow);
        assert!(is_ask(&p.decide(
            "web_fetch",
            &json!({"url": "https://x.dev/"}),
            true
        )));
        // Sensitive paths still ask, with a path-specific key.
        let d = edit(&p, ".env");
        assert_eq!(key(&d), Some("edit_file:.env"));
        assert!(is_ask(&edit(&p, ".git/config")));
        // Spelled differently, as on a case-insensitive file system.
        assert!(is_ask(&p.decide(
            "write_file",
            &json!({"path": ".Git/hooks/pre-commit"}),
            false
        )));
        assert!(is_ask(&p.decide(
            "write_file",
            &json!({"path": ".ch/config.toml"}),
            false
        )));
        assert!(is_deny(&bash(&p, "git push")));
    }

    #[test]
    fn bypass_mode() {
        let f = fixture();
        let p = policy(&f, "mode = \"bypass\"");
        assert_eq!(bash(&p, "npm test && make"), Decision::Allow);
        assert_eq!(edit(&p, ".env"), Decision::Allow);
        assert_eq!(
            p.decide("web_fetch", &json!({"url": "https://x.dev/"}), true),
            Decision::Allow
        );
        assert!(is_deny(&bash(&p, "rm -rf /")));
        assert!(is_deny(&bash(&p, "git reset --hard")));
        // Paths outside the workspace are still denied.
        assert!(is_deny(&edit(&p, "../x")));
    }

    #[test]
    fn paths_outside_workspace_are_denied() {
        let f = fixture();
        let p = policy(&f, "");
        let d = p.decide("read_file", &json!({"path": "/etc/passwd"}), true);
        match d {
            Decision::Deny { reason } => assert!(reason.contains("must be inside")),
            other => panic!("{other:?}"),
        }
        assert!(is_deny(&p.decide(
            "list_files",
            &json!({"path": "../"}),
            true
        )));
    }

    #[test]
    fn the_session_dir_opens_reads_by_the_readers_only() {
        let f = fixture();
        let p = policy(&f, "");
        let outside = tempfile::tempdir().unwrap();
        let session = outside.path().join("s1");
        std::fs::create_dir_all(session.join("spill")).unwrap();
        std::fs::write(session.join("spill/out.txt"), "x").unwrap();
        let file = json!({"path": session.join("spill/out.txt").to_str().unwrap()});
        let dir = json!({"path": session.join("spill").to_str().unwrap()});
        let s = Some(session.as_path());

        for tool in ["read_file", "grep_text"] {
            assert_eq!(p.decide_with(tool, &file, true, s), Decision::Allow);
            assert!(
                is_deny(&p.decide(tool, &file, true)),
                "{tool} without a session"
            );
        }
        for tool in ["list_files", "glob_files"] {
            assert!(is_deny(&p.decide_with(tool, &dir, true, s)), "{tool}");
        }
        for tool in ["edit_file", "write_file"] {
            assert!(is_deny(&p.decide_with(tool, &file, false, s)), "{tool}");
        }
    }

    #[test]
    fn rule_precedence() {
        let f = fixture();
        let p = policy(
            &f,
            r#"
            allow = ["bash:npm *", "edit_file", "read_file"]
            ask = ["bash:npm publish *"]
            deny = ["bash:npm run nuke *", "read_file:secrets/**"]
            "#,
        );
        assert_eq!(bash(&p, "npm test"), Decision::Allow);
        // ask beats allow, and an ask rule can't be remembered.
        let d = bash(&p, "npm publish --dry-run");
        assert!(is_ask(&d));
        assert_eq!(key(&d), None);
        // deny beats ask and allow.
        assert!(is_deny(&bash(&p, "npm run nuke now")));
        assert!(is_deny(&p.decide(
            "read_file",
            &json!({"path": "secrets/k.pem"}),
            true
        )));
        assert_eq!(edit(&p, "src/main.rs"), Decision::Allow);
        // The built-in deny list beats allow rules.
        let p = policy(&f, r#"allow = ["bash", "bash:git push *"]"#);
        assert!(is_deny(&bash(&p, "git push")));
        assert_eq!(bash(&p, "anything goes"), Decision::Allow);
    }

    #[test]
    fn sensitive_paths_need_a_specific_rule() {
        let f = fixture();
        let p = policy(
            &f,
            r#"mode = "accept-edits"
allow = ["edit_file"]"#,
        );
        assert!(is_ask(&edit(&p, ".env.local")));
        let p = policy(&f, r#"allow = ["edit_file:.env"]"#);
        assert_eq!(edit(&p, ".env"), Decision::Allow);
    }

    #[test]
    fn allow_rules_match_the_resolved_path() {
        let f = fixture();
        let p = policy(&f, r#"allow = ["edit_file:src/**"]"#);
        assert_eq!(edit(&p, "src/main.rs"), Decision::Allow);
        assert_eq!(edit(&p, "src/../src/main.rs"), Decision::Allow);
        let d = edit(&p, "src/../.git/hooks/pre-commit");
        assert_eq!(key(&d), Some("edit_file:.git/hooks/pre-commit"));
        assert!(is_ask(&edit(&p, "src/../.env")));
        assert_eq!(key(&edit(&p, "src/../Cargo.toml")), Some("edit_file"));
        let p = policy(&f, r#"deny = ["edit_file:src/../**"]"#);
        assert!(is_deny(&edit(&p, "src/../Cargo.toml")));
    }

    #[test]
    fn compound_bash_commands() {
        let f = fixture();
        let p = policy(&f, r#"allow = ["bash:git commit *"]"#);
        assert_eq!(bash(&p, "git status && git diff | head"), Decision::Allow);
        assert_eq!(
            bash(&p, "git add -A; git commit -m 'x'"),
            Decision::Ask {
                reason:
                    "`git add -A` (part of a compound command) is not on the read-only allowlist"
                        .into(),
                key: Some("bash:git add *".into()),
            }
        );
        assert_eq!(bash(&p, "git commit -m x && ls"), Decision::Allow);
        // An allowed command can't carry an unapproved one.
        let d = bash(&p, "git commit -m x && rm -rf src");
        assert_eq!(key(&d), Some("bash:rm *"));
        let d = bash(&p, "git commit -m \"$(rm -rf src)\"");
        assert!(is_ask(&d));
        // Two different unapproved parts: no single key covers them.
        assert_eq!(key(&bash(&p, "npm test && make")), None);
        assert!(is_ask(&bash(&p, "npm test && make")));
        // Read-only parts only.
        assert_eq!(bash(&p, "ls > /dev/null; cat a | wc -l"), Decision::Allow);
        assert!(is_ask(&bash(&p, "ls > out.txt")));
        assert!(is_deny(&bash(&p, "ls; sudo rm -rf /")));
    }

    #[test]
    fn calls_with_nothing_to_check_ask() {
        let f = fixture();
        let p = policy(&f, r#"mode = "accept-edits""#);
        for cmd in ["", "   ", ";", "&&", "()"] {
            let d = bash(&p, cmd);
            assert!(is_ask(&d), "{cmd:?}: {d:?}");
            assert_eq!(key(&d), None, "{cmd:?}");
        }
        assert!(is_ask(&p.decide("bash", &json!({}), false)));
        for url in ["", "not a url", "file:///etc/passwd", "data:text/plain,x"] {
            let d = p.decide("web_fetch", &json!({ "url": url }), true);
            assert!(is_ask(&d), "{url:?}: {d:?}");
            assert_eq!(key(&d), None, "{url:?}");
        }
        let p = policy(&f, r#"mode = "bypass""#);
        assert_eq!(bash(&p, ""), Decision::Allow);
        assert_eq!(
            p.decide("web_fetch", &json!({"url": "not a url"}), true),
            Decision::Allow
        );
    }

    #[test]
    fn ask_keys_round_trip_as_allow_rules() {
        let f = fixture();
        let p = policy(&f, "");
        for (tool, input) in [
            ("bash", json!({"command": "git commit -m 'x'"})),
            ("bash", json!({"command": "cargo test --all"})),
            ("edit_file", json!({"path": "src/main.rs"})),
            ("web_fetch", json!({"url": "https://docs.rs/serde/1.0"})),
            ("web_fetch", json!({"url": "http://localhost:8080/x"})),
            ("mcp__srv__tool", json!({})),
        ] {
            let k = key(&p.decide(tool, &input, false)).unwrap().to_string();
            let p2 = policy(&f, &format!("allow = [{k:?}]"));
            assert_eq!(p2.decide(tool, &input, false), Decision::Allow, "{k}");
        }
        let p = policy(&f, r#"mode = "accept-edits""#);
        let input = json!({"path": ".git/hooks/pre-commit"});
        let k = key(&p.decide("write_file", &input, false))
            .unwrap()
            .to_string();
        assert_eq!(k, "write_file:.git/hooks/pre-commit");
        let p2 = policy(&f, &format!("mode = \"accept-edits\"\nallow = [{k:?}]"));
        assert_eq!(p2.decide("write_file", &input, false), Decision::Allow);
    }

    #[test]
    fn bad_rules_fail_to_compile() {
        let f = fixture();
        let cfg: SafetyConfig = toml::from_str(r#"deny = ["bash:["]"#).unwrap();
        let err = Policy::new(&cfg, f.guard.clone()).unwrap_err();
        assert!(
            format!("{err:#}").contains("plugins.safety.deny"),
            "{err:#}"
        );
    }

    #[test]
    fn sensitive() {
        assert!(is_sensitive(Path::new(".git/config")));
        assert!(is_sensitive(Path::new("sub/.git/HEAD")));
        assert!(is_sensitive(Path::new(".env")));
        assert!(is_sensitive(Path::new("app/.env.production")));
        assert!(is_sensitive(Path::new(".ch/config.toml")));
        assert!(is_sensitive(Path::new(".GIT/config")));
        assert!(is_sensitive(Path::new(".Env.local")));
        assert!(is_sensitive(Path::new(".CH/Config.toml")));
        assert!(!is_sensitive(Path::new(".ch/commands/x.md")));
        assert!(!is_sensitive(Path::new("src/env.rs")));
        assert!(is_sensitive(Path::new(".mcp.json")));
        assert!(is_sensitive(Path::new("sub/.MCP.json")));
        assert!(is_sensitive(Path::new(".ch/plugins/x/plugin.toml")));
        assert!(is_sensitive(Path::new(".ch/plugins")));
        assert!(!is_sensitive(Path::new("plugins/x.rs")));
    }

    fn outside(p: &Policy, tool: &str, input: Value) -> Decision {
        p.decide(tool, &input, false)
    }

    #[test]
    fn outside_tools_ask_unless_allowed_by_name() {
        let f = fixture();
        let p = policy(&f, "");
        let d = outside(&p, "mcp__gh__create_issue", json!({"title": "x"}));
        assert_eq!(key(&d), Some("mcp__gh__create_issue"));
        // Read-only only by the user's own list, which arrives as `read_only`.
        assert_eq!(p.decide("mcp__gh__list", &json!({}), true), Decision::Allow);
        // Bypass allows them like any other tool.
        let p = policy(&f, "mode = \"bypass\"");
        assert_eq!(outside(&p, "mcp__gh__x", json!({})), Decision::Allow);
        // accept-edits doesn't: they aren't edits.
        let p = policy(&f, "mode = \"accept-edits\"");
        assert!(is_ask(&outside(&p, "mcp__gh__x", json!({}))));

        let p = policy(&f, "allow = [\"mcp__gh__*\", \"ext__p__run:*\"]");
        assert_eq!(outside(&p, "mcp__gh__x", json!({})), Decision::Allow);
        assert!(is_ask(&outside(&p, "mcp__other__x", json!({}))));
        // An allow rule with a pattern doesn't count for an outside tool.
        assert!(is_ask(&outside(&p, "ext__p__run", json!({"cmd": "ls"}))));
    }

    #[test]
    fn outside_tools_deny_and_ask_on_any_string() {
        let f = fixture();
        let p = policy(
            &f,
            "allow = [\"mcp__*\"]\ndeny = [\"mcp__fs__*:/etc/*\"]\nask = [\"mcp__*:*prod*\"]",
        );
        let nested = json!({"ops": [{"args": {"path": "/etc/passwd"}}]});
        assert!(is_deny(&outside(&p, "mcp__fs__read", nested)));
        assert!(is_ask(&outside(
            &p,
            "mcp__db__q",
            json!({"x": [1, {"y": "use prod db"}]})
        )));
        let keyed = json!({"files": {"/etc/passwd": true}});
        assert!(is_deny(&outside(&p, "mcp__fs__read", keyed)));
        assert_eq!(
            outside(&p, "mcp__db__q", json!({"x": "dev"})),
            Decision::Allow
        );

        // Too many strings to check: it asks, even when allowed.
        let many: Vec<String> = (0..=MAX_OUTSIDE_STRINGS).map(|i| i.to_string()).collect();
        assert!(is_ask(&outside(&p, "mcp__db__q", json!({ "list": many }))));
        let mut deep = json!("x");
        for _ in 0..100 {
            deep = json!([deep]);
        }
        assert!(is_ask(&outside(&p, "mcp__db__q", deep)));
    }

    #[test]
    fn built_in_checks_never_see_outside_tools() {
        let f = fixture();
        // `bash` rules and the bash deny list don't apply to an MCP tool that happens to
        // take a `command`, and an outside allow rule can't cover `bash`.
        let p = policy(&f, "allow = [\"bash\", \"mcp__*\"]");
        let d = outside(&p, "mcp__sh__run", json!({"command": "rm -rf /"}));
        assert_eq!(d, Decision::Allow, "allowed by name, not judged as bash");
        let p = policy(&f, "allow = [\"mcp__*\"]");
        assert!(is_ask(&bash(&p, "make deploy")));
        // `update_plan` never asks; an outside tool named like it does.
        let p = policy(&f, "");
        assert!(is_ask(&outside(&p, "ext__x__update_plan", json!({}))));
    }
}
