//! What a finished run means, decided from what the runner and the proxy saw.

use std::time::Duration;

use crate::backend::Exit;
use crate::events;
use crate::ledger::RunCalls;
use crate::matrix::Cell;
use crate::record::{NO_RESULT, TIMEOUT};

/// How much of a self-reported detail is kept.
const MAX_DETAIL: usize = 500;

/// What to do with a run once the agent is done.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// It measured the agent: check and record it.
    Record,
    /// `--max-cost` cut it off: no record, so it runs again on resume.
    CutByMaxCost,
    /// The model API failed in a way that may pass: no record; try it again later.
    Transient(String),
    /// It says nothing about the agent, and trying again won't help: stop the matrix.
    Infra(String),
}

/// Decides what a finished agent step means, from what the runner and the proxy saw:
/// the container's exit, and the ledger's `calls` (none without a proxy). `proxied`
/// says whether the cell's model goes through the proxy. `plugins` is the first
/// `session_start` event's list, written before the agent's first command.
///
/// `ch` ended normally when it exited with a code of its own other than 1 (done, step
/// limit, context overflow, a hook stop, a denied prompt, a refusal, cancelled), or was killed at
/// the timeout. A run through the proxy with no successful model call says nothing about
/// the agent, however it ended.
pub fn classify(
    exit: &Exit,
    calls: Option<&RunCalls>,
    proxied: bool,
    plugins: Option<&[String]>,
    cell: &Cell,
) -> Verdict {
    if let Some(e) = &exit.start_error {
        return Verdict::Infra(format!("the agent's container didn't start: {e}"));
    }
    let ended_normally = exit.timed_out || matches!(exit.code, Some(0 | 2 | 3 | 4 | 5 | 6 | 130));
    if let Some(calls) = calls {
        let last = || {
            let status = match calls.last_status {
                Some(0) | None => "no answer".to_string(),
                Some(s) => format!("HTTP {s}"),
            };
            match &calls.last_error {
                Some(message) => format!("{status}: {message}"),
                None => status,
            }
        };
        if calls.cut_by_max_cost {
            return Verdict::CutByMaxCost;
        }
        if calls.auth_failed {
            return Verdict::Infra(format!(
                "the model API refused the key ({}); check the key the proxy was given",
                last()
            ));
        }
        if !ended_normally && calls.last_transient {
            return Verdict::Transient(format!("the model API failed (last answer: {})", last()));
        }
        if proxied && calls.succeeded == 0 {
            return Verdict::Infra(if calls.forwarded == 0 {
                format!(
                    "the run ended ({}) before its first model call",
                    code_text(exit.code)
                )
            } else {
                format!(
                    "no model call of the run succeeded (last answer: {}); check the \
                     model, the account and the request size",
                    last()
                )
            });
        }
    }
    match plugins.map(|p| cell.check_plugins(p)) {
        Some(Err(e)) => Verdict::Infra(e),
        _ => Verdict::Record,
    }
}

/// The termination reason and its detail: `timeout` when the agent was killed (even if
/// it printed a result), else the result line's, else `no_result`. Self-reported, so
/// only recorded.
pub fn reason_of(
    exit: &Exit,
    stream: &events::Stream,
    timeout: Duration,
) -> (String, Option<String>) {
    if exit.timed_out {
        return (
            TIMEOUT.to_string(),
            Some(format!("killed after {}s", timeout.as_secs())),
        );
    }
    match stream.reason() {
        Some(reason) => {
            let detail = stream
                .result
                .as_ref()
                .and_then(|r| r.detail.as_deref())
                .map(|d| d.chars().take(MAX_DETAIL).collect());
            (reason.to_string(), detail)
        }
        None => (
            NO_RESULT.to_string(),
            Some(format!(
                "ch exited with {} and no result line; see stderr.log",
                code_text(exit.code)
            )),
        ),
    }
}

/// Why a pre-flight's exit disqualifies the task's check, if it does.
pub fn preflight_problem(exit: &Exit) -> Option<String> {
    if let Some(e) = &exit.start_error {
        return Some(format!("its check's container didn't start: {e}"));
    }
    if exit.timed_out {
        return Some("its check timed out on the starting repo".into());
    }
    match exit.code {
        Some(0) => {
            Some("its check passes on the starting repo, so it can't tell a fix from no fix".into())
        }
        Some(c @ 124..=127) => Some(format!(
            "its check couldn't run on the starting repo (exit {c})"
        )),
        None => Some("its check was killed by a signal".into()),
        _ => None,
    }
}

pub(super) fn code_text(code: Option<i32>) -> String {
    code.map_or("a signal".into(), |c| format!("code {c}"))
}
