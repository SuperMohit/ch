//! The interactive `repl` approver.
//!
//! Approval requests arrive while a turn is running and the REPL is rendering events,
//! so the approver doesn't touch the terminal itself. It sends the request over a
//! channel to the REPL's turn loop (the single owner of the terminal), which pauses the
//! spinner, prints the prompt, reads one key and sends the answer back.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, bail};
use async_trait::async_trait;
use ch_kernel::{Approval, ApprovalRequest, Approver};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::{mpsc, oneshot};
use toml::{Table, Value};

/// An approval request waiting for the user.
pub struct Pending {
    pub request: ApprovalRequest,
    pub reply: oneshot::Sender<Approval>,
}

/// Connects the approver (called from the turn pipeline) to the REPL loop.
pub struct ApprovalBridge {
    tx: mpsc::UnboundedSender<Pending>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<Pending>>>,
    /// True while the REPL frontend is running and can answer.
    active: AtomicBool,
}

impl Default for ApprovalBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalBridge {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            tx,
            rx: Mutex::new(Some(rx)),
            active: AtomicBool::new(false),
        }
    }

    /// Takes the receiving end; the frontend calls this when it starts.
    pub fn take_receiver(&self) -> Option<mpsc::UnboundedReceiver<Pending>> {
        self.rx.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Gives the receiving end back when the frontend stops.
    pub fn return_receiver(&self, rx: mpsc::UnboundedReceiver<Pending>) {
        *self.rx.lock().unwrap_or_else(|e| e.into_inner()) = Some(rx);
    }

    pub fn set_active(&self, on: bool) {
        self.active.store(on, Ordering::SeqCst);
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
}

/// The `repl` approver: asks the user at the REPL.
pub struct ReplApprover {
    pub(crate) bridge: std::sync::Arc<ApprovalBridge>,
}

#[async_trait]
impl Approver for ReplApprover {
    fn id(&self) -> &str {
        "repl"
    }

    async fn ask(&self, request: &ApprovalRequest) -> Approval {
        let not_available = || Approval::Deny {
            reason: Some(
                "nobody is at the REPL to approve this (the `repl` approver only works with \
                 the `repl` frontend)"
                    .into(),
            ),
        };
        if !self.bridge.is_active() {
            return not_available();
        }
        let (reply, answer) = oneshot::channel();
        let pending = Pending {
            request: request.clone(),
            reply,
        };
        if self.bridge.tx.send(pending).is_err() {
            return not_available();
        }
        answer.await.unwrap_or_else(|_| not_available())
    }
}

/// What a key press means at the approval prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalKey {
    /// `y` or Enter-free yes.
    Yes,
    /// `n`: deny, then optionally type a reason.
    No,
    /// Esc: deny without a reason.
    NoQuiet,
    /// `a`: allow for the rest of the session.
    Always,
    /// Ctrl-C: deny and cancel the whole turn.
    Cancel,
}

/// Maps a key press at the approval prompt; `None` means "ignore it and keep waiting".
pub fn parse_approval_key(key: &KeyEvent) -> Option<ApprovalKey> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') | KeyCode::Char('C') => Some(ApprovalKey::Cancel),
            _ => None,
        };
    }
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => Some(ApprovalKey::Yes),
        KeyCode::Char('n') | KeyCode::Char('N') => Some(ApprovalKey::No),
        KeyCode::Char('a') | KeyCode::Char('A') => Some(ApprovalKey::Always),
        KeyCode::Esc => Some(ApprovalKey::NoQuiet),
        _ => None,
    }
}

/// Whether a key pressed during a turn should cancel it (Esc or Ctrl-C).
pub fn is_interrupt_key(key: &KeyEvent) -> bool {
    key.code == KeyCode::Esc
        || (key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')))
}

/// Whether a key answers "yes" to a `[y/N]` question.
pub fn is_yes_key(key: &KeyEvent) -> bool {
    !key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'))
}

/// Adds `rule` to `plugins.safety.allow` in the TOML document `text`. Returns the new
/// document, or `None` when the rule is already there.
///
/// Comments and formatting in the file are not preserved.
pub fn add_allow_rule(text: &str, rule: &str) -> anyhow::Result<Option<String>> {
    let mut root: Table = text
        .parse()
        .context("the config file is not valid TOML; fix it or add the rule by hand")?;
    let plugins = child_table(&mut root, "plugins")?;
    let safety = child_table(plugins, "safety")?;
    let allow = safety
        .entry("allow")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Value::Array(list) = allow else {
        bail!("`plugins.safety.allow` is not a list; fix the config file or add the rule by hand");
    };
    if list.iter().any(|v| v.as_str() == Some(rule)) {
        return Ok(None);
    }
    list.push(Value::String(rule.to_string()));
    Ok(Some(toml::to_string(&root)?))
}

fn child_table<'a>(table: &'a mut Table, key: &str) -> anyhow::Result<&'a mut Table> {
    match table
        .entry(key)
        .or_insert_with(|| Value::Table(Table::new()))
    {
        Value::Table(t) => Ok(t),
        _ => bail!("`{key}` in the config file is not a table; fix it or add the rule by hand"),
    }
}

/// Adds `rule` to the allow list in the config file at `path`, creating the file (and
/// its directory) if needed. Returns `false` if the rule was already there.
pub fn save_allow_rule(path: &Path, rule: &str) -> anyhow::Result<bool> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let Some(updated) =
        add_allow_rule(&text, rule).with_context(|| format!("updating {}", path.display()))?
    else {
        return Ok(false);
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(path, updated).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn approval_keys() {
        assert_eq!(
            parse_approval_key(&key(KeyCode::Char('y'))),
            Some(ApprovalKey::Yes)
        );
        assert_eq!(
            parse_approval_key(&key(KeyCode::Char('Y'))),
            Some(ApprovalKey::Yes)
        );
        assert_eq!(
            parse_approval_key(&key(KeyCode::Char('n'))),
            Some(ApprovalKey::No)
        );
        assert_eq!(
            parse_approval_key(&key(KeyCode::Char('a'))),
            Some(ApprovalKey::Always)
        );
        assert_eq!(
            parse_approval_key(&key(KeyCode::Esc)),
            Some(ApprovalKey::NoQuiet)
        );
        assert_eq!(parse_approval_key(&ctrl('c')), Some(ApprovalKey::Cancel));
        assert_eq!(parse_approval_key(&key(KeyCode::Char('x'))), None);
        assert_eq!(parse_approval_key(&key(KeyCode::Enter)), None);
        // Ctrl-Y is not "yes".
        assert_eq!(parse_approval_key(&ctrl('y')), None);
    }

    #[test]
    fn interrupt_and_yes_keys() {
        assert!(is_interrupt_key(&key(KeyCode::Esc)));
        assert!(is_interrupt_key(&ctrl('c')));
        assert!(!is_interrupt_key(&key(KeyCode::Char('c'))));
        assert!(is_yes_key(&key(KeyCode::Char('y'))));
        assert!(!is_yes_key(&key(KeyCode::Enter)));
    }

    #[test]
    fn adds_rule_to_empty_and_existing_config() {
        let out = add_allow_rule("", "bash:git status").unwrap().unwrap();
        let t: Table = out.parse().unwrap();
        assert_eq!(
            t["plugins"]["safety"]["allow"].as_array().unwrap()[0].as_str(),
            Some("bash:git status")
        );

        let existing =
            "[model]\nid = \"m\"\n\n[plugins.safety]\nmode = \"ask\"\nallow = [\"read_file\"]\n";
        let out = add_allow_rule(existing, "edit_file").unwrap().unwrap();
        let t: Table = out.parse().unwrap();
        assert_eq!(t["model"]["id"].as_str(), Some("m"));
        assert_eq!(t["plugins"]["safety"]["mode"].as_str(), Some("ask"));
        let allow: Vec<&str> = t["plugins"]["safety"]["allow"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(allow, vec!["read_file", "edit_file"]);

        assert!(add_allow_rule(existing, "read_file").unwrap().is_none());
    }

    #[test]
    fn rejects_bad_shapes() {
        assert!(add_allow_rule("plugins = 3", "x").is_err());
        assert!(add_allow_rule("[plugins.safety]\nallow = \"x\"", "x").is_err());
        assert!(add_allow_rule("not toml [", "x").is_err());
    }

    #[test]
    fn saves_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".ch/config.toml");
        assert!(save_allow_rule(&path, "bash:ls *").unwrap());
        assert!(!save_allow_rule(&path, "bash:ls *").unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("bash:ls *"));
    }

    #[tokio::test]
    async fn approver_round_trip_through_the_bridge() {
        let bridge = Arc::new(ApprovalBridge::new());
        let approver = ReplApprover {
            bridge: bridge.clone(),
        };
        let req = ApprovalRequest {
            tool: "bash".into(),
            input: serde_json::json!({}),
            summary: "x".into(),
            reason: "r".into(),
            key: None,
        };
        // Not active: denies without asking.
        assert!(matches!(approver.ask(&req).await, Approval::Deny { .. }));

        bridge.set_active(true);
        let mut rx = bridge.take_receiver().unwrap();
        let answer = tokio::spawn(async move {
            let p = rx.recv().await.unwrap();
            assert_eq!(p.request.tool, "bash");
            p.reply.send(Approval::AllowAlways).unwrap();
        });
        assert_eq!(approver.ask(&req).await, Approval::AllowAlways);
        answer.await.unwrap();
    }
}
