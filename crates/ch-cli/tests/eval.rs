//! The eval pipeline end to end through this crate's `ch` binary, on the host, with
//! ch-eval's test-only host backend.
//!
//! - With the scripted model on a copy of the `calc-py` fixture: read, edit, test; the
//!   diff, the hidden check, `run.json`, the report, resuming and fingerprints.
//! - With the Anthropic provider through ch-eval's credential proxy to a fake upstream:
//!   the token, the proxy's usage and cost, `--max-cost`, and a refused key.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use ch_eval::args::Command as EvalCommand;
use ch_eval::backend::{HostBackend, ProxyRoute};
use ch_eval::ledger::StateDir;
use ch_eval::proxy::{ProxyConfig, Upstream};
use ch_eval::record::{RunRecord, load_all};
use ch_eval::run::{Matrix, Options, describe, run_matrix};
use clap::Parser;
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn has_python() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Writes a task: `task.toml`, `repo/` with `script.json` (the scripted model's replies)
/// and `files`, and `verify/` with `hidden`.
fn write_task(
    suite: &Path,
    id: &str,
    toml: &str,
    script: &str,
    files: &[(&str, &str)],
    hidden: &[(&str, &str)],
) {
    let dir = suite.join(id);
    std::fs::create_dir_all(dir.join("repo")).unwrap();
    std::fs::write(dir.join("task.toml"), toml).unwrap();
    std::fs::write(dir.join("repo/script.json"), script).unwrap();
    for (name, text) in files {
        std::fs::write(dir.join("repo").join(name), text).unwrap();
    }
    if !hidden.is_empty() {
        std::fs::create_dir_all(dir.join("verify")).unwrap();
    }
    for (name, text) in hidden {
        std::fs::write(dir.join("verify").join(name), text).unwrap();
    }
}

/// The scripted model reading `script.json` from the workspace, with pricing so `ch`
/// reports a cost of its own.
const SCRIPTED: &str = r#"
[model]
provider = "scripted"
[model.pricing]
input = 3.0
output = 15.0
[plugins]
enable = ["provider-scripted"]
[plugins.provider-scripted]
script = "script.json"
[plugins.project-instructions]
environment = false
user_file = ""
"#;

fn write_config(dir: &Path, text: &str) -> PathBuf {
    let path = dir.join("config.toml");
    std::fs::write(&path, text).unwrap();
    path
}

/// The matrix and options for `ch-eval run ARGS`, parsed as the command line would be.
fn prepare(args: &[&str]) -> (Matrix, Options) {
    let cli = ch_eval::Cli::try_parse_from(["ch-eval", "run"].iter().chain(args).copied()).unwrap();
    let EvalCommand::Run(run) = cli.command else {
        panic!("not a run");
    };
    let matrix = ch_eval::app::prepare(&run).unwrap();
    let opts = Options {
        out: std::path::absolute(&run.out).unwrap(),
        jobs: run.jobs as usize,
        max_cost: run.max_cost,
        max_attempts: run.max_attempts,
        backoff: std::time::Duration::from_millis(10),
    };
    (matrix, opts)
}

fn backend(proxy: Option<ProxyRoute>) -> Arc<HostBackend> {
    Arc::new(HostBackend {
        ch: PathBuf::from(env!("CARGO_BIN_EXE_ch")),
        proxy,
    })
}

fn record<'a>(records: &'a [RunRecord], task: &str, cell: &str) -> &'a RunRecord {
    records
        .iter()
        .find(|r| r.task == task && r.cell == cell)
        .unwrap_or_else(|| panic!("no run of {task} in {cell}: {records:#?}"))
}

#[tokio::test]
async fn runs_a_matrix_on_the_fixture_and_resumes() {
    if !has_python() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let suite = tmp.path().join("suite");
    let fixture = repo_root().join("tests/fixtures/calc-py");
    let read = |name: &str| std::fs::read_to_string(fixture.join(name)).unwrap();
    let script =
        std::fs::read_to_string(repo_root().join("tests/fixtures/calc-py.script.json")).unwrap();
    write_task(
        &suite,
        "calc",
        "instruction = \"fix the failing test\"\n\
         verify = \"python3 -m unittest -q test_hidden\"\n\
         tags = [\"bugfix\"]\n",
        &script,
        &[
            ("calc.py", &read("calc.py")),
            ("test_calc.py", &read("test_calc.py")),
            ("CH.md", &read("CH.md")),
        ],
        &[(
            "test_hidden.py",
            "import unittest\nfrom calc import add\n\n\
             class Hidden(unittest.TestCase):\n    def test_add(self):\n        \
             self.assertEqual(add(-2, 7), 5)\n",
        )],
    );
    let config = write_config(tmp.path(), SCRIPTED);
    let out = tmp.path().join("out");
    let args = [
        "--suite",
        suite.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
        "--vary",
        "planning=on,off",
        "-j",
        "2",
    ];
    let (matrix, opts) = prepare(&args);
    let matrix = Arc::new(matrix);

    let summary = run_matrix(matrix.clone(), backend(None), &opts)
        .await
        .unwrap();
    assert_eq!(
        (summary.planned, summary.ran, summary.done_before),
        (2, 2, 0)
    );
    assert!(
        !out.join("preflight").exists(),
        "the pre-flight cleans up after itself"
    );
    let records = load_all(&out).unwrap();
    assert_eq!(records.len(), 2);
    for cell in ["planning=on", "planning=off"] {
        let r = record(&records, "calc", cell);
        assert!(r.passed, "{r:#?}");
        assert_eq!(r.reason, "done");
        assert_eq!(r.exit_code, Some(0));
        assert_eq!(r.verify.exit_code, Some(0));
        assert_eq!(r.tags, ["bugfix"]);
        // No proxy: nothing trusted was counted. ch's own numbers are kept apart.
        assert_eq!((r.cost, r.model_calls, r.usage.input_tokens), (0.0, 0, 0));
        assert_eq!(r.reported.steps, 4);
        assert!(
            r.reported.cost > 0.0 && r.reported.usage.input_tokens > 0,
            "{r:#?}"
        );
        let calls: Vec<(&str, u64)> = r
            .signals
            .tool_calls
            .iter()
            .map(|(k, v)| (k.as_str(), *v))
            .collect();
        assert_eq!(calls, [("bash", 1), ("edit_file", 1), ("read_file", 1)]);
        assert!(r.signals.tool_errors.is_empty(), "{r:#?}");
        assert_eq!(r.git_entries_removed, 1);
        assert!(!r.fingerprint.ch.is_empty() && !r.fingerprint.repo.is_empty());
    }
    // The cells' settings reach ch, merged with the base config's plugin list.
    let on = record(&records, "calc", "planning=on");
    assert_eq!(
        on.settings,
        [r#"plugins.enable=["provider-scripted", "planning"]"#]
    );
    let off = record(&records, "calc", "planning=off");
    assert_eq!(off.settings, [r#"plugins.disable=["planning"]"#]);
    assert!(
        off.command
            .windows(2)
            .any(|w| w == ["--set", r#"plugins.disable=["planning"]"#])
    );
    assert!(
        off.command
            .windows(2)
            .any(|w| w == ["--permission-mode", "bypass"])
    );

    let dir = out.join("runs/calc/planning=off/1");
    let events = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
    assert!(
        events
            .lines()
            .last()
            .unwrap()
            .contains("\"type\":\"result\"")
    );
    let patch = std::fs::read_to_string(dir.join("diff.patch")).unwrap();
    assert!(
        patch.contains("-    return a - b\n+    return a + b"),
        "{patch}"
    );
    assert!(
        !patch.contains("test_hidden"),
        "the diff is taken before the check is copied in"
    );
    assert!(dir.join("work/test_hidden.py").is_file());
    assert!(
        !dir.join("work/.git").exists(),
        "the workspace's .git is removed"
    );
    assert!(
        dir.join("baseline.git/HEAD").is_file(),
        "the baseline is kept outside it"
    );
    assert!(!dir.join("diff.git").exists());
    assert!(dir.join("verify.log").is_file());
    assert!(
        dir.join("ch-home/sessions").is_dir(),
        "ch kept its files in the run's ch home"
    );
    // The agent's test run found only the visible tests: the hidden one wasn't there.
    assert!(events.contains("Ran 2 tests"), "{events}");

    let results = ch_eval::report::write(&out).unwrap();
    assert_eq!(results.cells.len(), 2);
    assert!(results.cells.iter().all(|c| c.pass_rate == 1.0));
    let pair = &results.pairs[0];
    assert_eq!((pair.a_wins, pair.b_wins, pair.ties), (0, 0, 1));
    let md = std::fs::read_to_string(out.join("report.md")).unwrap();
    assert!(md.contains("| planning=off | 1 | 1 | 100.0% |"), "{md}");

    // Running again resumes: both runs are done.
    let again = run_matrix(matrix.clone(), backend(None), &opts)
        .await
        .unwrap();
    assert_eq!((again.ran, again.done_before), (0, 2));
    // A run without its record is done over.
    std::fs::remove_file(dir.join("run.json")).unwrap();
    let again = run_matrix(matrix, backend(None), &opts).await.unwrap();
    assert_eq!((again.ran, again.done_before), (1, 1));
    assert_eq!(load_all(&out).unwrap().len(), 2);

    // A changed task can't be resumed onto the runs made from the old one.
    std::fs::write(suite.join("calc/repo/NOTES.md"), "new file").unwrap();
    let (matrix, opts) = prepare(&args);
    let err = run_matrix(Arc::new(matrix), backend(None), &opts)
        .await
        .unwrap_err();
    let err = format!("{err:#}");
    assert!(
        err.contains("2 recorded runs") && err.contains("repo/"),
        "{err}"
    );
}

#[tokio::test]
async fn records_failed_checks_timeouts_and_hung_checks() {
    let tmp = tempfile::tempdir().unwrap();
    let suite = tmp.path().join("suite");
    write_task(
        &suite,
        "check-fails",
        "instruction = \"write NOTES.md\"\nverify = \"test -f NOTES.md\"\n",
        r#"[{"text": "I won't."}]"#,
        &[("README.md", "hi\n")],
        &[],
    );
    // The agent is killed at the timeout, and the check still decides the outcome.
    write_task(
        &suite,
        "slow",
        "instruction = \"wait\"\nverify = \"test -f STARTED\"\ntimeout_secs = 5\n",
        r#"[{"tool_calls": [{"name": "bash", "input": {"command": "touch STARTED; sleep 20"}}]}]"#,
        &[("README.md", "hi\n")],
        &[],
    );
    // The check hangs once the agent has made DONE.
    write_task(
        &suite,
        "check-hangs",
        "instruction = \"make DONE\"\nverify = \"test -f DONE && sleep 30\"\nverify_timeout_secs = 1\n",
        r#"[{"tool_calls": [{"name": "bash", "input": {"command": "touch DONE"}}]}, {"text": "Done."}]"#,
        &[("README.md", "hi\n")],
        &[],
    );
    let config = write_config(tmp.path(), SCRIPTED);
    let out = tmp.path().join("out");
    let (matrix, opts) = prepare(&[
        "--suite",
        suite.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
        "-j",
        "3",
    ]);
    let summary = run_matrix(Arc::new(matrix), backend(None), &opts)
        .await
        .unwrap();
    assert_eq!(summary.ran, 3);
    let records = load_all(&out).unwrap();

    let failed = record(&records, "check-fails", "base");
    assert!(!failed.passed);
    assert_eq!(failed.reason, "done");
    assert_eq!(failed.verify.exit_code, Some(1));

    let slow = record(&records, "slow", "base");
    assert_eq!(slow.reason, "timeout");
    assert_eq!(slow.exit_code, None);
    assert_eq!(slow.detail.as_deref(), Some("killed after 5s"));
    assert!(slow.wall_ms < 20_000, "{slow:#?}");
    assert!(slow.passed);
    assert_eq!(slow.reported.steps, 1, "counted from the events");

    let hung = record(&records, "check-hangs", "base");
    assert!(!hung.passed);
    assert!(hung.verify.timed_out);
    assert_eq!(hung.verify.exit_code, None);

    let md = ch_eval::report::markdown(&ch_eval::report::build(&records));
    assert!(md.contains("| base | 1 | 1 | 1 | 0 |"), "{md}");
}

#[tokio::test]
async fn runs_that_say_nothing_about_the_agent_stop_the_matrix() {
    let tmp = tempfile::tempdir().unwrap();
    let suite = tmp.path().join("suite");
    write_task(
        &suite,
        "t",
        "instruction = \"write NOTES.md\"\nverify = \"test -f NOTES.md\"\n",
        r#"[{"text": "ok"}]"#,
        &[("README.md", "hi\n")],
        &[],
    );
    let config = write_config(tmp.path(), SCRIPTED);
    let base = |out: &Path, vary: &str| {
        prepare(&[
            "--suite",
            suite.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "--vary",
            vary,
        ])
    };

    // A config the kernel can't read is refused before anything runs.
    let out = tmp.path().join("invalid");
    let (matrix, opts) = base(&out, r#"model.temperature="hot""#);
    let err = run_matrix(Arc::new(matrix), backend(None), &opts)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("temperature"), "{err:#}");
    assert!(!out.join("runs").exists());

    // With the scripted model (no proxy), a run that ch rejects at startup is the
    // agent's own: there is nothing trustworthy to tell otherwise.
    let out = tmp.path().join("rejected");
    let (matrix, opts) = base(&out, r#"plugins.context.strategy="bogus""#);
    run_matrix(Arc::new(matrix), backend(None), &opts)
        .await
        .unwrap();
    assert_eq!(load_all(&out).unwrap()[0].reason, "no_result");

    // A setting for a plugin this ch doesn't have measured nothing.
    let out = tmp.path().join("no-plugin");
    let (matrix, opts) = base(&out, "plugins.executor-docker.image=\"x\"");
    let err = run_matrix(Arc::new(matrix), backend(None), &opts)
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("plugin `executor-docker` wasn't enabled"),
        "{err:#}"
    );

    // A check that passes on the untouched repo, or can't run there, stops everything
    // before any run.
    for (verify, expected) in [
        ("true", "passes on the starting repo"),
        ("exit 124", "couldn't run on the starting repo (exit 124)"),
        ("exit 125", "couldn't run on the starting repo (exit 125)"),
    ] {
        write_task(
            &suite,
            "t",
            &format!("instruction = \"x\"\nverify = \"{verify}\"\n"),
            r#"[{"text": "ok"}]"#,
            &[],
            &[],
        );
        let out = tmp
            .path()
            .join(format!("preflight-{}", verify.replace(' ', "-")));
        let (matrix, opts) = base(&out, "planning=on");
        let err = run_matrix(Arc::new(matrix), backend(None), &opts)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains(expected), "{err:#}");
        assert!(!out.join("runs").exists());
    }
}

// --- Through the proxy -------------------------------------------------------------

const REAL_KEY: &str = "sk-ant-real-test-key";

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

/// An Anthropic stream with `block` as its content and the given usage.
fn reply(block: Value, delta: Value, stop: &str, usage: [u64; 4]) -> String {
    let [input, output, read, write] = usage;
    sse(&[
        json!({"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],
            "usage":{"input_tokens":input,"output_tokens":1,"cache_read_input_tokens":read,"cache_creation_input_tokens":write}}}),
        json!({"type":"content_block_start","index":0,"content_block":block}),
        json!({"type":"content_block_delta","index":0,"delta":delta}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":stop},"usage":{"output_tokens":output}}),
        json!({"type":"message_stop"}),
    ])
}

fn bash_reply(command: &str, usage: [u64; 4]) -> String {
    let input = json!({ "command": command }).to_string();
    reply(
        json!({"type":"tool_use","id":"toolu_1","name":"bash","input":{}}),
        json!({"type":"input_json_delta","partial_json": input}),
        "tool_use",
        usage,
    )
}

fn text_reply(text: &str, usage: [u64; 4]) -> String {
    reply(
        json!({"type":"text","text":""}),
        json!({"type":"text_delta","text":text}),
        "end_turn",
        usage,
    )
}

/// Answers with the queued replies in order.
struct Replies(Mutex<VecDeque<String>>);

impl Respond for Replies {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let body = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| text_reply("(no more replies)", [1, 1, 0, 0]));
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(body)
    }
}

/// A fake upstream that wants the real key, and a proxy forwarding to it.
async fn proxy(
    dir: &Path,
    replies: Vec<String>,
    max_cost: Option<f64>,
) -> (MockServer, ProxyRoute) {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", REAL_KEY))
        .respond_with(Replies(Mutex::new(replies.into())))
        .mount(&upstream)
        .await;
    let state = StateDir(dir.join("proxy"));
    state.prepare().unwrap();
    let config = ProxyConfig {
        anthropic: Some(Upstream {
            base_url: upstream.uri(),
            key: REAL_KEY.into(),
        }),
        openai: None,
        max_cost,
        ..Default::default()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(ch_eval::proxy::serve(listener, state.clone(), config));
    (upstream, ProxyRoute { url, state })
}

/// The Anthropic provider, retrying fast, with the fake upstream's prices.
fn anthropic_config(dir: &Path, upstream: &str) -> PathBuf {
    write_config(
        dir,
        &format!(
            r#"
[model]
provider = "anthropic"
id = "claude-opus-5-5"
[model.pricing]
input = 3.0
output = 15.0
cache_read = 0.3
cache_write = 3.75
[plugins.provider-anthropic]
base_url = "{upstream}"
max_retries = 0
[plugins.project-instructions]
environment = false
user_file = ""
"#
        ),
    )
}

fn note_task(suite: &Path) {
    write_task(
        suite,
        "note",
        "instruction = \"write NOTES.md\"\nverify = \"test -f NOTES.md\"\n",
        "[]",
        &[("README.md", "hi\n")],
        &[],
    );
}

#[tokio::test]
async fn model_calls_go_through_the_proxy_and_are_counted_there() {
    let tmp = tempfile::tempdir().unwrap();
    let suite = tmp.path().join("suite");
    note_task(&suite);
    let replies = vec![
        bash_reply("echo hi > NOTES.md", [100, 20, 1000, 50]),
        text_reply("Wrote NOTES.md.", [130, 5, 1100, 0]),
    ];
    let (upstream, route) = proxy(tmp.path(), replies, None).await;
    let config = anthropic_config(tmp.path(), &upstream.uri());
    let out = tmp.path().join("out");
    let (matrix, opts) = prepare(&[
        "--suite",
        suite.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
    ]);
    let matrix = Arc::new(matrix);
    let plan = describe(&matrix, backend(Some(route.clone())).as_ref(), &opts.out).unwrap();
    assert!(
        plan.starts_with("1 task × 1 cell × 1 repeat = 1 run; 0 done already"),
        "{plan}"
    );
    assert!(
        plan.contains("provider-anthropic.base_url=\"http://127.0.0.1:"),
        "{plan}"
    );
    assert!(!out.exists(), "describing the plan writes nothing");

    let summary = run_matrix(matrix, backend(Some(route.clone())), &opts)
        .await
        .unwrap();
    assert_eq!(summary.ran, 1);
    let records = load_all(&out).unwrap();
    let r = &records[0];
    assert!(r.passed, "{r:#?}");
    assert_eq!(r.model_calls, 2);
    assert_eq!(
        (
            r.usage.input_tokens,
            r.usage.output_tokens,
            r.usage.cache_read_tokens,
            r.usage.cache_write_tokens
        ),
        (230, 25, 2100, 50)
    );
    let expected = (230.0 * 3.0 + 25.0 * 15.0 + 2100.0 * 0.3 + 50.0 * 3.75) / 1e6;
    assert!((r.cost - expected).abs() < 1e-12, "{}", r.cost);
    assert!((summary.recorded_cost - expected).abs() < 1e-12);
    // The key went upstream; the run only ever had its token.
    for request in upstream.received_requests().await.unwrap() {
        assert_eq!(request.headers["x-api-key"], REAL_KEY);
    }
    let dir = out.join("runs/note/base/1");
    for file in [
        "events.jsonl",
        "stderr.log",
        "run.json",
        "ch-home/config.toml",
    ] {
        let text = std::fs::read_to_string(dir.join(file)).unwrap();
        assert!(!text.contains(REAL_KEY), "{file} holds the key");
    }
    assert!(
        std::fs::read_dir(route.state.tokens())
            .unwrap()
            .next()
            .is_none(),
        "the token is revoked after the run"
    );
}

#[tokio::test]
async fn max_cost_cuts_runs_off_and_stops_new_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let suite = tmp.path().join("suite");
    note_task(&suite);
    // The first call costs more than the cap, so the second is refused: the run is cut
    // off, gets no record, and nothing else starts.
    let replies = vec![bash_reply("echo hi > NOTES.md", [1000, 20, 0, 0])];
    let (upstream, route) = proxy(tmp.path(), replies, Some(0.001)).await;
    let config = anthropic_config(tmp.path(), &upstream.uri());
    let out = tmp.path().join("out");
    let (matrix, opts) = prepare(&[
        "--suite",
        suite.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
        "--repeat",
        "3",
        "-j",
        "1",
        "--max-cost",
        "0.001",
    ]);
    let summary = run_matrix(Arc::new(matrix), backend(Some(route)), &opts)
        .await
        .unwrap();
    assert_eq!((summary.ran, summary.stopped_by_max_cost), (0, 3));
    assert!(summary.recorded_cost > 0.001);
    assert!(load_all(&out).unwrap().is_empty());
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_refused_key_stops_the_matrix() {
    let tmp = tempfile::tempdir().unwrap();
    let suite = tmp.path().join("suite");
    note_task(&suite);
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "type": "error", "error": {"type": "authentication_error", "message": "invalid x-api-key"}
        })))
        .mount(&upstream)
        .await;
    let state = StateDir(tmp.path().join("proxy"));
    state.prepare().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let config = ProxyConfig {
        anthropic: Some(Upstream {
            base_url: upstream.uri(),
            key: "sk-wrong".into(),
        }),
        ..Default::default()
    };
    tokio::spawn(ch_eval::proxy::serve(listener, state.clone(), config));
    let config = anthropic_config(tmp.path(), &upstream.uri());
    let out = tmp.path().join("out");
    let (matrix, opts) = prepare(&[
        "--suite",
        suite.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
    ]);
    let route = ProxyRoute { url, state };
    let err = run_matrix(Arc::new(matrix), backend(Some(route)), &opts)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("refused the key"), "{err:#}");
    assert!(load_all(&out).unwrap().is_empty());
}

#[tokio::test]
async fn a_run_that_never_reaches_the_model_stops_the_matrix() {
    let tmp = tempfile::tempdir().unwrap();
    let suite = tmp.path().join("suite");
    note_task(&suite);
    let (upstream, route) = proxy(tmp.path(), vec![], None).await;
    let config = anthropic_config(tmp.path(), &upstream.uri());
    let out = tmp.path().join("out");
    // ch rejects the plugin setting at startup, before any model call.
    let (matrix, opts) = prepare(&[
        "--suite",
        suite.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
        "--vary",
        r#"plugins.context.strategy="bogus""#,
    ]);
    let err = run_matrix(Arc::new(matrix), backend(Some(route)), &opts)
        .await
        .unwrap_err();
    let err = format!("{err:#}");
    assert!(err.contains("before its first model call"), "{err}");
    assert!(err.contains("bogus"), "ch's error is quoted: {err}");
    assert!(load_all(&out).unwrap().is_empty(), "no record is written");
    let work = out.join("runs/note/context.strategy=bogus/1/work");
    assert!(work.is_dir() && !work.join(".git").exists());
}

/// Answers 529 `count` times, then with `then`.
struct Overloaded(Mutex<(usize, VecDeque<String>)>);

impl Respond for Overloaded {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let mut state = self.0.lock().unwrap();
        if state.0 > 0 {
            state.0 -= 1;
            return ResponseTemplate::new(529).set_body_json(json!({
                "type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}
            }));
        }
        let body = state
            .1
            .pop_front()
            .unwrap_or_else(|| text_reply("(no more replies)", [1, 1, 0, 0]));
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(body)
    }
}

async fn overloaded_proxy(
    dir: &Path,
    failures: usize,
    replies: Vec<String>,
) -> (MockServer, ProxyRoute) {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(Overloaded(Mutex::new((failures, replies.into()))))
        .mount(&upstream)
        .await;
    let state = StateDir(dir.join("proxy"));
    state.prepare().unwrap();
    let config = ProxyConfig {
        anthropic: Some(Upstream {
            base_url: upstream.uri(),
            key: REAL_KEY.into(),
        }),
        ..Default::default()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(ch_eval::proxy::serve(listener, state.clone(), config));
    (upstream, ProxyRoute { url, state })
}

#[tokio::test]
async fn transient_api_failures_are_tried_again() {
    let tmp = tempfile::tempdir().unwrap();
    let suite = tmp.path().join("suite");
    note_task(&suite);
    // One overload, then a good run: the run is tried again and recorded once.
    let replies = vec![
        bash_reply("echo hi > NOTES.md", [100, 20, 0, 0]),
        text_reply("Done.", [100, 5, 0, 0]),
    ];
    let (upstream, route) = overloaded_proxy(tmp.path(), 1, replies).await;
    let config = anthropic_config(tmp.path(), &upstream.uri());
    let out = tmp.path().join("out");
    let args = [
        "--suite",
        suite.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
        "--max-attempts",
        "2",
    ];
    let (matrix, opts) = prepare(&args);
    let summary = run_matrix(Arc::new(matrix), backend(Some(route)), &opts)
        .await
        .unwrap();
    assert_eq!((summary.ran, summary.retried), (1, 1));
    let r = &load_all(&out).unwrap()[0];
    assert!(r.passed, "{r:#?}");

    // Overloaded every time: after two in a row the matrix stops, with no record.
    let tmp = tempfile::tempdir().unwrap();
    let suite = tmp.path().join("suite");
    note_task(&suite);
    let (upstream, route) = overloaded_proxy(tmp.path(), 100, vec![]).await;
    let config = anthropic_config(tmp.path(), &upstream.uri());
    let out = tmp.path().join("out");
    let args = [
        "--suite",
        suite.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
        "--max-attempts",
        "2",
    ];
    let (matrix, opts) = prepare(&args);
    let err = run_matrix(Arc::new(matrix), backend(Some(route)), &opts)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("on all 2 attempts"), "{err:#}");
    assert!(load_all(&out).unwrap().is_empty());
}

#[tokio::test]
async fn a_run_whose_model_calls_all_fail_stops_the_matrix() {
    // A misspelled model: the API answers 404, ch ends with an error.
    let tmp = tempfile::tempdir().unwrap();
    let suite = tmp.path().join("suite");
    note_task(&suite);
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "type": "error", "error": {"type": "not_found_error", "message": "model: claude-opus-5-5"}
        })))
        .mount(&upstream)
        .await;
    let state = StateDir(tmp.path().join("proxy"));
    state.prepare().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let config = ProxyConfig {
        anthropic: Some(Upstream {
            base_url: upstream.uri(),
            key: REAL_KEY.into(),
        }),
        ..Default::default()
    };
    tokio::spawn(ch_eval::proxy::serve(listener, state.clone(), config));
    let config = anthropic_config(tmp.path(), &upstream.uri());
    let out = tmp.path().join("out");
    let (matrix, opts) = prepare(&[
        "--suite",
        suite.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
    ]);
    let err = run_matrix(
        Arc::new(matrix),
        backend(Some(ProxyRoute { url, state })),
        &opts,
    )
    .await
    .unwrap_err();
    let err = format!("{err:#}");
    assert!(err.contains("no model call of the run succeeded"), "{err}");
    assert!(err.contains("HTTP 404: model: claude-opus-5-5"), "{err}");
    assert!(load_all(&out).unwrap().is_empty());
}
