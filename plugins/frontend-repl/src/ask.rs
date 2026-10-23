//! The approval prompt: shows the request, reads the answer, and offers to save an
//! "always" rule.

use std::path::Path;

use ch_kernel::{Approval, ApprovalRequest};
use crossterm::event::{KeyCode, KeyModifiers};

use crate::RuleSaved;
use crate::approval::{ApprovalKey, is_yes_key, parse_approval_key};
use crate::format::{approval_prompt, dim, green, red, yellow};
use crate::rules::{PROJECT_CONFIG, project_config_path, save_allow_rule};
use crate::terminal::{KeyPoller, Term};

/// The user's answer at the approval prompt.
pub(crate) struct Answer {
    pub approval: Approval,
    /// Ctrl-C: cancel the turn. The kernel then answers the call itself, so
    /// `approval` is not used.
    pub cancel_turn: bool,
}

impl Answer {
    fn only(approval: Approval) -> Self {
        Self {
            approval,
            cancel_turn: false,
        }
    }
}

/// Shows the approval prompt and reads the answer.
pub(crate) async fn ask(
    req: &ApprovalRequest,
    keys: &mut Option<KeyPoller>,
    term: &mut Term,
    workspace: &Path,
    on_saved: Option<&RuleSaved>,
) -> Answer {
    term.write(&approval_prompt(req));
    let Some(poller) = keys.as_mut() else {
        term.line(&red("no terminal to ask on; denied"));
        return Answer::only(Approval::Deny {
            reason: Some("nobody could answer the approval prompt".into()),
        });
    };
    loop {
        let Some(key) = poller.next().await else {
            term.line(&red("denied"));
            return Answer::only(Approval::Deny { reason: None });
        };
        match parse_approval_key(&key) {
            None => continue,
            Some(ApprovalKey::Yes) => {
                term.line(&green("yes"));
                return Answer::only(Approval::Allow);
            }
            Some(ApprovalKey::NoQuiet) => {
                term.line(&red("no"));
                return Answer::only(Approval::Deny { reason: None });
            }
            Some(ApprovalKey::No) => {
                term.line(&red("no"));
                term.write(&dim("  tell the agent why (Enter to skip): "));
                let reason = read_text(poller, term).await;
                return Answer::only(Approval::Deny { reason });
            }
            Some(ApprovalKey::Cancel) => {
                term.line(&red("cancelled"));
                return Answer {
                    approval: Approval::Deny { reason: None },
                    cancel_turn: true,
                };
            }
            Some(ApprovalKey::Always) => {
                term.line(&green("always"));
                if let Some(rule) = &req.key {
                    offer_to_save(rule, poller, term, workspace, on_saved).await;
                }
                return Answer::only(Approval::AllowAlways);
            }
        }
    }
}

async fn offer_to_save(
    rule: &str,
    poller: &mut KeyPoller,
    term: &mut Term,
    workspace: &Path,
    on_saved: Option<&RuleSaved>,
) {
    let path = project_config_path(workspace);
    term.write(&format!(
        "  save `{rule}` to {} for future sessions? [y/N] ",
        dim(PROJECT_CONFIG)
    ));
    let yes = poller.next().await.is_some_and(|k| is_yes_key(&k));
    if !yes {
        term.line(&dim("not saved (allowed for this session only)"));
        return;
    }
    match save_allow_rule(&path, rule) {
        Ok(true) => {
            term.line(&green("saved"));
            if let Some(hook) = on_saved {
                (hook.0)(rule);
            }
        }
        Ok(false) => term.line(&dim("already in the allow list")),
        Err(e) => term.line(&yellow(&format!("could not save: {e:#}"))),
    }
}

/// Reads a short line of text in raw mode. Enter finishes, Esc or Ctrl-C give `None`.
async fn read_text(poller: &mut KeyPoller, term: &mut Term) -> Option<String> {
    let mut text = String::new();
    loop {
        let key = poller.next().await?;
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')) {
                term.line("");
                return None;
            }
            continue;
        }
        match key.code {
            KeyCode::Enter => {
                term.line("");
                let t = text.trim().to_string();
                return (!t.is_empty()).then_some(t);
            }
            KeyCode::Esc => {
                term.line("");
                return None;
            }
            KeyCode::Backspace => {
                if text.pop().is_some() {
                    term.write("\u{8} \u{8}");
                }
            }
            KeyCode::Char(c) => {
                text.push(c);
                term.write(&c.to_string());
            }
            _ => {}
        }
    }
}
