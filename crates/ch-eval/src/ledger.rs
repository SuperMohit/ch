//! The credential proxy's state directory: the tokens that identify runs, and the
//! ledger of every model call each run made. The proxy mounts it; agent containers
//! never see it, so usage and cost read from here can't be forged by the agent.
//!
//! ```text
//! <state>/proxy.json         # what to forward where (no keys: they come on stdin)
//! <state>/tokens/<token>     # one per running run: TokenInfo as JSON
//! <state>/ledger/<run>.jsonl # one Call per model request, appended by the proxy
//! <state>/ready              # written by the proxy once it listens
//! <state>/heartbeat          # touched by the runner; the proxy exits when it goes stale
//! <state>/proxy.log          # the proxy's output; it never prints keys or headers
//! ```
//!
//! The ledger outlives runs and invocations: everything spent in an output directory
//! is in it, including runs that were interrupted or cut off by `--max-cost`.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use ch_kernel::Pricing;
use serde::{Deserialize, Serialize};

use crate::events::Usage;

/// The environment variable that holds a run's token inside the agent's container. The
/// provider's `api_key_env` points at it, so the token travels as the API key.
pub const TOKEN_ENV: &str = "CH_EVAL_API_KEY";

/// The proxy's state directory.
#[derive(Debug, Clone, PartialEq)]
pub struct StateDir(pub PathBuf);

impl StateDir {
    pub fn config(&self) -> PathBuf {
        self.0.join("proxy.json")
    }
    pub fn tokens(&self) -> PathBuf {
        self.0.join("tokens")
    }
    pub fn ledger(&self) -> PathBuf {
        self.0.join("ledger")
    }
    pub fn ready(&self) -> PathBuf {
        self.0.join("ready")
    }
    pub fn heartbeat(&self) -> PathBuf {
        self.0.join("heartbeat")
    }
    pub fn log(&self) -> PathBuf {
        self.0.join("proxy.log")
    }

    /// Creates the directories. Tokens left by an earlier invocation are removed.
    pub fn prepare(&self) -> anyhow::Result<()> {
        let tokens = self.tokens();
        if tokens.exists() {
            std::fs::remove_dir_all(&tokens)
                .with_context(|| format!("removing {}", tokens.display()))?;
        }
        for dir in [tokens, self.ledger()] {
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let _ = std::fs::remove_file(self.ready());
        Ok(())
    }
}

/// What a token stands for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenInfo {
    /// The run's ledger name.
    pub run: String,
    /// The models the run may call (its cell's agent and summarizer models), each with
    /// its pricing, if any.
    pub models: BTreeMap<String, Option<Pricing>>,
}

/// Tokens and run names are used as file names, so they are kept to a safe set.
fn safe_name(s: &str) -> bool {
    (8..=96).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Makes a token for `run`, which may call `models`, valid until [`revoke_token`].
pub fn issue_token(
    state: &StateDir,
    run: &str,
    models: BTreeMap<String, Option<Pricing>>,
) -> anyhow::Result<String> {
    if !safe_name(run) {
        bail!("run name `{run}` can't name a ledger file");
    }
    let token = format!("cheval-{}", uuid::Uuid::new_v4().simple());
    let info = TokenInfo {
        run: run.to_string(),
        models,
    };
    let path = state.tokens().join(&token);
    std::fs::write(&path, serde_json::to_string(&info)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(token)
}

/// Stops `token` from working.
pub fn revoke_token(state: &StateDir, token: &str) {
    if safe_name(token) {
        let _ = std::fs::remove_file(state.tokens().join(token));
    }
}

/// The run a token stands for, if it is current.
pub fn lookup_token(state: &StateDir, token: &str) -> Option<TokenInfo> {
    if !safe_name(token) {
        return None;
    }
    let text = std::fs::read_to_string(state.tokens().join(token)).ok()?;
    serde_json::from_str(&text).ok()
}

/// One model request the proxy handled.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Call {
    /// `anthropic` or `openai`.
    pub provider: String,
    /// The `model` field of the request.
    pub model: String,
    /// The model the answer names, which a fallback can make differ from `model`.
    pub response_model: String,
    /// `response_model` isn't one the run may call; the call is priced as `model`.
    pub unlisted_model: bool,
    /// The upstream's HTTP status; 0 when the request got no answer.
    pub status: u16,
    /// The type of an error event in a stream that began with 200, such as
    /// `overloaded_error`.
    pub stream_error: Option<String>,
    /// A successful answer carried no usage, so its cost is unknown.
    pub usage_missing: bool,
    pub usage: Usage,
    /// USD, from the pricing of the model that answered (or else the one asked for);
    /// 0 without one.
    pub cost: f64,
    /// Why the proxy didn't forward it: `max_cost`, or `model` (not one the run may
    /// call).
    pub refused: Option<String>,
    /// What went wrong on the way, such as an unreachable upstream.
    pub note: Option<String>,
    /// The upstream's error message, for an answer that wasn't a success.
    pub error_message: Option<String>,
}

impl Call {
    /// It failed in a way that says nothing about the agent, and trying later may
    /// work: a rate limit, a server error, no answer, or an overload or API error
    /// inside a stream.
    pub fn is_transient(&self) -> bool {
        let stream = self.stream_error.as_deref().is_some_and(|e| {
            matches!(
                e,
                "overloaded_error"
                    | "api_error"
                    | "rate_limit_error"
                    | "timeout_error"
                    | "connection_error"
            )
        });
        self.status == 0 || self.status == 429 || self.status >= 500 || stream
    }
}

/// Appends `call` to `run`'s ledger.
pub fn append(state: &StateDir, run: &str, call: &Call) -> anyhow::Result<()> {
    if !safe_name(run) {
        bail!("run name `{run}` can't name a ledger file");
    }
    let path = state.ledger().join(format!("{run}.jsonl"));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    let call = Call {
        cost: clean_cost(call.cost),
        ..call.clone()
    };
    // One write per line, so concurrent appends don't interleave.
    let line = serde_json::to_string(&call)? + "\n";
    file.write_all(line.as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

/// Every call recorded for `run`.
pub fn read_run(state: &StateDir, run: &str) -> Vec<Call> {
    read_file(&state.ledger().join(format!("{run}.jsonl")))
}

fn read_file(path: &Path) -> Vec<Call> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// The cost of every call in the ledger, from every run and invocation.
pub fn total_cost(state: &StateDir) -> f64 {
    let Ok(entries) = std::fs::read_dir(state.ledger()) else {
        return 0.0;
    };
    entries
        .flatten()
        .flat_map(|e| read_file(&e.path()))
        .map(|c| clean_cost(c.cost))
        .fold(0.0, |a, b| (a + b).min(f64::MAX))
}

/// A cost that can be added up: finite and not negative, or 0.
pub fn clean_cost(cost: f64) -> f64 {
    if cost.is_finite() && cost >= 0.0 {
        cost
    } else {
        0.0
    }
}

/// A run's calls, summed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunCalls {
    /// Requests forwarded upstream.
    pub forwarded: u32,
    /// Forwarded requests answered with a 2xx status and no error in the stream.
    pub succeeded: u32,
    pub usage: Usage,
    pub cost: f64,
    /// The upstream refused the key (401 or 403).
    pub auth_failed: bool,
    /// The proxy refused a request because `--max-cost` was reached.
    pub cut_by_max_cost: bool,
    /// The proxy refused a request for this model, which the run may not call.
    pub refused_model: Option<String>,
    /// The last forwarded request failed transiently (see [`Call::is_transient`]).
    pub last_transient: bool,
    /// The last forwarded request's status, for messages.
    pub last_status: Option<u16>,
    /// What the upstream said about the last request that failed.
    pub last_error: Option<String>,
    /// Some successful answer carried no usage.
    pub usage_missing: bool,
    /// A model that answered although the run may not call it.
    pub unlisted_model: Option<String>,
}

/// Sums `calls`.
pub fn summarize(calls: &[Call]) -> RunCalls {
    let mut out = RunCalls::default();
    for c in calls {
        match c.refused.as_deref() {
            Some("max_cost") => {
                out.cut_by_max_cost = true;
                continue;
            }
            Some("model") => {
                out.refused_model = Some(c.model.clone());
                continue;
            }
            Some(_) => continue,
            None => {}
        }
        out.forwarded = out.forwarded.saturating_add(1);
        out.usage.add(&c.usage);
        out.cost = (out.cost + clean_cost(c.cost)).min(f64::MAX);
        out.auth_failed |= matches!(c.status, 401 | 403);
        out.usage_missing |= c.usage_missing;
        if c.unlisted_model {
            out.unlisted_model = Some(c.response_model.clone());
        }
        out.last_transient = c.is_transient();
        out.last_status = Some(c.status);
        if (200..300).contains(&c.status) && c.stream_error.is_none() {
            out.succeeded = out.succeeded.saturating_add(1);
        } else {
            out.last_error = c
                .error_message
                .clone()
                .or_else(|| c.stream_error.clone())
                .or_else(|| c.note.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> (tempfile::TempDir, StateDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir(dir.path().join("state"));
        state.prepare().unwrap();
        (dir, state)
    }

    #[test]
    fn tokens_are_issued_looked_up_and_revoked() {
        let (_dir, state) = state();
        let pricing = Pricing {
            input: 3.0,
            ..Default::default()
        };
        let models = BTreeMap::from([("m".to_string(), Some(pricing))]);
        let token = issue_token(&state, "abc12345-1", models.clone()).unwrap();
        assert!(token.starts_with("cheval-"));
        let info = lookup_token(&state, &token).unwrap();
        assert_eq!(info.run, "abc12345-1");
        assert_eq!(info.models, models);
        assert_eq!(lookup_token(&state, "cheval-unknown0000"), None);
        assert_eq!(lookup_token(&state, "../../etc/passwd"), None);
        revoke_token(&state, &token);
        assert_eq!(lookup_token(&state, &token), None);
        assert!(issue_token(&state, "../x", BTreeMap::new()).is_err());
        // A new invocation starts without the old tokens.
        let token = issue_token(&state, "abc12345-2", BTreeMap::new()).unwrap();
        state.prepare().unwrap();
        assert_eq!(lookup_token(&state, &token), None);
    }

    #[test]
    fn calls_are_summed_per_run_and_overall() {
        let (_dir, state) = state();
        let call = |status, input, cost, refused: Option<&str>| Call {
            provider: "anthropic".into(),
            model: "m".into(),
            status,
            usage: Usage {
                input_tokens: input,
                cache_read_tokens: 7,
                ..Default::default()
            },
            cost,
            refused: refused.map(String::from),
            ..Default::default()
        };
        append(&state, "run-0001", &call(200, 10, 0.5, None)).unwrap();
        append(&state, "run-0001", &call(529, 0, f64::NAN, None)).unwrap();
        append(&state, "run-0002", &call(200, 5, 0.25, None)).unwrap();
        append(&state, "run-0002", &call(0, 0, 0.0, Some("max_cost"))).unwrap();
        append(&state, "run-0003", &call(401, 0, 0.0, None)).unwrap();

        let one = summarize(&read_run(&state, "run-0001"));
        assert_eq!(one.forwarded, 2);
        assert_eq!(one.usage.input_tokens, 10);
        assert_eq!(one.usage.cache_read_tokens, 14);
        assert_eq!(one.cost, 0.5);
        assert_eq!(one.last_status, Some(529));
        assert!(one.last_transient);
        assert!(!one.auth_failed && !one.cut_by_max_cost);
        let two = summarize(&read_run(&state, "run-0002"));
        assert!(two.cut_by_max_cost);
        assert_eq!(two.forwarded, 1);
        let three = summarize(&read_run(&state, "run-0003"));
        assert!(three.auth_failed && !three.last_transient);
        assert_eq!(total_cost(&state), 0.75);
        assert!(read_run(&state, "run-9999").is_empty());
    }

    #[test]
    fn transient_failures() {
        let call = |status, stream_error: Option<&str>| Call {
            status,
            stream_error: stream_error.map(String::from),
            ..Default::default()
        };
        for transient in [
            call(0, None),
            call(429, None),
            call(500, None),
            call(529, None),
            call(200, Some("overloaded_error")),
            call(200, Some("api_error")),
        ] {
            assert!(transient.is_transient(), "{transient:?}");
        }
        for not in [
            call(200, None),
            call(400, None),
            call(401, None),
            call(200, Some("invalid_request_error")),
        ] {
            assert!(!not.is_transient(), "{not:?}");
        }
        // A later success clears it; a refused model is remembered.
        let calls = [
            call(529, None),
            call(200, None),
            Call {
                model: "other".into(),
                refused: Some("model".into()),
                ..Default::default()
            },
        ];
        let sum = summarize(&calls);
        assert!(!sum.last_transient);
        assert_eq!(sum.refused_model.as_deref(), Some("other"));
        assert_eq!(sum.forwarded, 2);
    }
}
