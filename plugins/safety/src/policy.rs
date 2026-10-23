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
        if tool == "bash" {
            let command = input.get("command").and_then(Value::as_str).unwrap_or("");
            return self.decide_bash(command);
        }
        if FILE_TOOLS.contains(&tool) {
            return self.decide_file(tool, input, read_only);
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

    fn decide_file(&self, tool: &str, input: &Value, read_only: bool) -> Decision {
        let raw = input.get("path").and_then(Value::as_str);
        let rel = match raw {
            Some(p) => match self.guard.resolve(p) {
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
        let writes = WRITE_TOOLS.contains(&tool);
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

/// Paths where a write always asks outside bypass mode: git internals, `.env*` files
/// and ch's project config. Names compare case-insensitively, because on
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
    if names.last().is_some_and(|n| n.starts_with(".env")) {
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
    }
}
