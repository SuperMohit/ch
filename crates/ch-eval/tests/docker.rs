//! Smoke tests of the Docker backend, in real containers. Ignored by default, since CI
//! has no Docker:
//!
//! ```sh
//! cargo test -p ch-eval --test docker -- --ignored --nocapture --test-threads 1
//! ```
//!
//! The first run builds the `ch-eval-base` image and static `ch` and `ch-eval`, which
//! takes a few minutes.
//!
//! The main test runs the Anthropic provider through the credential proxy container to
//! a fake model API on the host (reached as `host.docker.internal`, as Docker Desktop
//! provides it). The fake model fixes `calc.py`, runs the tests, and probes the agent's
//! container: the proxy is reachable, the host and the internet aren't, and no process
//! in it has the API key.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use ch_eval::app::{needs, prepare, source_root};
use ch_eval::args::Command as EvalCommand;
use ch_eval::docker::{
    DockerBackend, Setup, arch, check_docker, ensure_base_image, ensure_binaries,
};
use ch_eval::ledger::StateDir;
use ch_eval::proxy::Keys;
use ch_eval::record::load_all;
use ch_eval::run::{Options, run_matrix};
use clap::Parser;
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const REAL_KEY: &str = "sk-ant-smoke-6f1c2d9e8b7a4f30a1b2c3d4e5f60718";

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

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

fn tool(id: &str, name: &str, input: Value, usage: [u64; 4]) -> String {
    reply(
        json!({"type":"tool_use","id":id,"name":name,"input":{}}),
        json!({"type":"input_json_delta","partial_json": input.to_string()}),
        "tool_use",
        usage,
    )
}

fn text(text: &str, usage: [u64; 4]) -> String {
    reply(
        json!({"type":"text","text":""}),
        json!({"type":"text_delta","text":text}),
        "end_turn",
        usage,
    )
}

struct Replies(Mutex<VecDeque<String>>);

impl Respond for Replies {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let body = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| text("(no more replies)", [1, 1, 0, 0]));
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(body)
    }
}

/// Probes the agent's container and prints one line per finding.
fn probe(host_port: u16) -> String {
    format!(
        r#"python3 - <<'EOF'
import os, socket
def reach(host, port):
    try:
        socket.create_connection((host, port), 3).close()
        return "reached"
    except Exception:
        return "blocked"
print("probe proxy:", reach("ch-eval-proxy", 8080))
print("probe host:", reach("host.docker.internal", {host_port}))
print("probe internet:", reach("1.1.1.1", 443))
print("probe dns:", reach("example.com", 443))
found = False
for pid in os.listdir("/proc"):
    if pid.isdigit():
        try:
            env = open(f"/proc/{{pid}}/environ", "rb").read().split(b"\0")
        except OSError:
            continue
        found = found or any(b"=sk-" + b"ant" in v for v in env)
print("probe key in any environment:", "yes" if found else "no")
print("probe token in commands' environment:", "yes" if "CH_EVAL_API_KEY" in os.environ else "no")
EOF"#
    )
}

fn containers(label: &str) -> String {
    let out = Command::new("docker")
        .args([
            "ps",
            "--all",
            "--quiet",
            "--filter",
            &format!("label={label}"),
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The running proxy container's name.
fn proxy_container() -> String {
    containers_named("label=ch-eval")
        .into_iter()
        .find(|n| n.contains("-proxy-"))
        .expect("a proxy container")
}

fn containers_named(filter: &str) -> Vec<String> {
    let out = Command::new("docker")
        .args(["ps", "--format", "{{.Names}}", "--filter", filter])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(String::from)
        .collect()
}

fn inspect(name: &str, format: &str) -> String {
    let out = Command::new("docker")
        .args(["inspect", "--format", format, name])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn remove(name: &str) {
    Command::new("docker")
        .args(["rm", "--force", name])
        .output()
        .unwrap();
}

fn networks() -> String {
    let out = Command::new("docker")
        .args(["network", "ls", "--quiet", "--filter", "label=ch-eval"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Builds what the tests need; returns the binaries.
fn setup() -> ch_eval::docker::Binaries {
    check_docker().expect("Docker must be running");
    let source = source_root();
    let binaries = ensure_binaries(&source, arch()).unwrap();
    ensure_base_image(&source).unwrap();
    binaries
}

fn calc_task(task: &Path) {
    let fixture = source_root().join("tests/fixtures/calc-py");
    for name in ["calc.py", "test_calc.py", "CH.md"] {
        let text = std::fs::read_to_string(fixture.join(name)).unwrap();
        write(&task.join("repo").join(name), &text);
    }
    write(&task.join("repo/.gitignore"), "__pycache__/\n");
    write(
        &task.join("verify/test_hidden.py"),
        "import unittest\nfrom calc import add\n\n\
         class Hidden(unittest.TestCase):\n    def test_add(self):\n        \
         self.assertEqual(add(-2, 7), 5)\n",
    );
    // The check passes only without a network: the container has no eth0.
    write(
        &task.join("task.toml"),
        "instruction = \"fix the failing test\"\n\
         verify = \"python3 -m unittest -q test_hidden && ! test -e /sys/class/net/eth0\"\n\
         timeout_secs = 120\n",
    );
}

fn run_args(suite: &Path, out: &Path, config: &Path) -> ch_eval::args::RunArgs {
    let cli = ch_eval::Cli::try_parse_from([
        "ch-eval",
        "run",
        "--suite",
        suite.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
    ])
    .unwrap();
    let EvalCommand::Run(args) = cli.command else {
        panic!("not a run");
    };
    *args
}

#[tokio::test]
#[ignore = "needs Docker; builds the image and static binaries on first use"]
async fn a_run_through_the_proxy_in_real_containers() {
    let binaries = setup();
    let upstream = MockServer::start().await;
    let port = upstream.address().port();
    let usage = [
        [900, 30, 0, 800],
        [1000, 40, 800, 0],
        [1100, 20, 800, 0],
        [1200, 60, 800, 0],
        [1300, 10, 800, 0],
    ];
    let replies = vec![
        tool("toolu_1", "read_file", json!({"path": "calc.py"}), usage[0]),
        tool(
            "toolu_2",
            "edit_file",
            json!({"path": "calc.py", "old_text": "    return a - b", "new_text": "    return a + b"}),
            usage[1],
        ),
        tool(
            "toolu_3",
            "bash",
            json!({"command": "python3 -m unittest -q"}),
            usage[2],
        ),
        tool("toolu_4", "bash", json!({"command": probe(port)}), usage[3]),
        text("Fixed add; the tests pass.", usage[4]),
    ];
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", REAL_KEY))
        .respond_with(Replies(Mutex::new(replies.into())))
        .expect(5)
        .mount(&upstream)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    calc_task(&root.join("suite/calc"));
    let config = root.join("config.toml");
    write(
        &config,
        &format!(
            "[model]\nprovider = \"anthropic\"\nid = \"claude-opus-5-5\"\n\
             [model.pricing]\ninput = 3.0\noutput = 15.0\ncache_read = 0.3\ncache_write = 3.75\n\
             [plugins.provider-anthropic]\nbase_url = \"http://host.docker.internal:{port}\"\n\
             max_retries = 0\n"
        ),
    );
    let out = root.join("out");
    let args = run_args(&root.join("suite"), &out, &config);
    let matrix = prepare(&args).unwrap();
    let needs = needs(&matrix, None).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    let state = StateDir(out.join("proxy"));
    let (backend, cleanup) = DockerBackend::start(Setup {
        binaries,
        images: vec!["ch-eval-base".into()],
        state: state.clone(),
        spec: needs.spec,
        keys: Keys {
            anthropic: Some(REAL_KEY.into()),
            openai: None,
        },
    })
    .await
    .unwrap();
    // The key went to the proxy on stdin: its container's config doesn't hold it.
    let proxy = proxy_container();
    let config = inspect(&proxy, "{{json .Config}}");
    assert!(!config.contains(REAL_KEY), "{config}");
    // A proxy that stops is started again.
    remove(&proxy);
    assert!(backend.supervise().await.unwrap());
    assert_ne!(proxy_container(), proxy);
    let opts = Options {
        out: out.clone(),
        jobs: 1,
        max_cost: None,
        max_attempts: 3,
        backoff: std::time::Duration::from_secs(1),
    };
    let summary = run_matrix(Arc::new(matrix), Arc::new(backend), &opts).await;
    cleanup.run();
    let summary = summary.unwrap();
    assert_eq!(summary.ran, 1);

    let records = load_all(&out).unwrap();
    let r = &records[0];
    let dir = out.join("runs/calc/base/1");
    let events = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
    assert!(
        r.passed,
        "{r:#?}\nverify.log:\n{}\nstderr.log:\n{}",
        std::fs::read_to_string(dir.join("verify.log")).unwrap_or_default(),
        std::fs::read_to_string(dir.join("stderr.log")).unwrap_or_default()
    );
    assert_eq!(r.reason, "done");
    assert_eq!(r.model_calls, 5);
    // Usage and cost are the proxy's, from the fake API's answers.
    let sum = |i: usize| usage.iter().map(|u| u[i]).sum::<u64>();
    assert_eq!(
        (
            r.usage.input_tokens,
            r.usage.output_tokens,
            r.usage.cache_read_tokens,
            r.usage.cache_write_tokens
        ),
        (sum(0), sum(1), sum(2), sum(3))
    );
    let expected =
        (sum(0) as f64 * 3.0 + sum(1) as f64 * 15.0 + sum(2) as f64 * 0.3 + sum(3) as f64 * 3.75)
            / 1e6;
    assert!((r.cost - expected).abs() < 1e-12, "{}", r.cost);
    let patch = std::fs::read_to_string(dir.join("diff.patch")).unwrap();
    assert!(patch.contains("+    return a + b"), "{patch}");
    assert!(!patch.contains("__pycache__"), "{patch}");
    assert!(!dir.join("work/.git").exists());

    // Inside the agent's container.
    for expected in [
        "Ran 2 tests",
        "probe proxy: reached",
        "probe host: blocked",
        "probe internet: blocked",
        "probe dns: blocked",
        "probe key in any environment: no",
        // ch's bash hides the variable its provider reads the key from.
        "probe token in commands' environment: no",
    ] {
        assert!(events.contains(expected), "{expected}\n{events}");
    }
    // The key is in no file under the output directory.
    let mut stack = vec![out.clone()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap().flatten() {
            let p = entry.path();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                stack.push(p);
            } else if kind.is_file() {
                let bytes = std::fs::read(&p).unwrap_or_default();
                let text = String::from_utf8_lossy(&bytes);
                assert!(!text.contains(REAL_KEY), "{} holds the key", p.display());
            }
        }
    }
    // Every container and the network are gone.
    assert_eq!(containers("ch-eval"), "", "containers left behind");
    assert_eq!(networks(), "", "networks left behind");

    let results = ch_eval::report::write(&out).unwrap();
    assert_eq!(results.cells[0].passed, 1);
    println!(
        "{}",
        std::fs::read_to_string(out.join("report.md")).unwrap()
    );
}

#[tokio::test]
#[ignore = "needs Docker; builds the image and static binaries on first use"]
async fn a_timed_out_agent_container_is_stopped() {
    let binaries = setup();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let task = root.join("suite/slow");
    write(
        &task.join("repo/script.json"),
        r#"[{"tool_calls": [{"name": "bash", "input": {"command": "touch STARTED; sleep 600"}}]}]"#,
    );
    write(
        &task.join("task.toml"),
        "instruction = \"wait\"\nverify = \"test -f STARTED\"\ntimeout_secs = 5\n",
    );
    let config = root.join("config.toml");
    write(
        &config,
        "[model]\nprovider = \"scripted\"\n[plugins]\nenable = [\"provider-scripted\"]\n\
         [plugins.provider-scripted]\nscript = \"script.json\"\n",
    );
    let out = root.join("out");
    let matrix = prepare(&run_args(&root.join("suite"), &out, &config)).unwrap();
    let needs = needs(&matrix, None).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    let (backend, cleanup) = DockerBackend::start(Setup {
        binaries,
        images: vec!["ch-eval-base".into()],
        state: StateDir(out.join("proxy")),
        spec: needs.spec,
        keys: Keys::default(),
    })
    .await
    .unwrap();
    let opts = Options {
        out: out.clone(),
        jobs: 1,
        max_cost: None,
        max_attempts: 3,
        backoff: std::time::Duration::from_secs(1),
    };
    let started = std::time::Instant::now();
    let summary = run_matrix(Arc::new(matrix), Arc::new(backend), &opts).await;
    // The agent's container is gone before the cleanup runs.
    let left = containers("ch-eval");
    cleanup.run();
    summary.unwrap();
    let r = &load_all(&out).unwrap()[0];
    assert_eq!(r.reason, "timeout");
    assert_eq!(r.exit_code, None);
    assert!(r.passed, "the check still ran: {r:#?}");
    assert!(started.elapsed().as_secs() < 60, "{:?}", started.elapsed());
    // Only the proxy was still there.
    assert_eq!(left.lines().count(), 1, "{left}");
    assert_eq!(containers("ch-eval"), "");
    assert_eq!(networks(), "");
}

/// The scripted model, reading `script.json` from the workspace.
const SCRIPTED: &str = "[model]\nprovider = \"scripted\"\n[plugins]\nenable = [\"provider-scripted\"]\n\
                        [plugins.provider-scripted]\nscript = \"script.json\"\n";

#[tokio::test]
#[ignore = "needs Docker; builds the image and static binaries on first use"]
async fn runs_cannot_reach_each_other_and_dead_invocations_are_swept() {
    let binaries = setup();
    // What a killed ch-eval left: a container and a network labelled with a pid that
    // isn't running.
    let dead = [
        "--label",
        "ch-eval",
        "--label",
        "ch-eval.invocation=deadbeef0000",
        "--label",
        "ch-eval.pid=999999",
        "--label",
        "ch-eval.started=never",
    ];
    let net = Command::new("docker")
        .args(["network", "create", "--internal"])
        .args(dead)
        .arg("ch-eval-deadbeef0000-1-net")
        .output()
        .unwrap();
    assert!(
        net.status.success(),
        "{}",
        String::from_utf8_lossy(&net.stderr)
    );
    let leftover = Command::new("docker")
        .args(["create", "--name", "ch-eval-deadbeef0000-1-agent"])
        .args(dead)
        .args(["ch-eval-base", "true"])
        .output()
        .unwrap();
    assert!(
        leftover.status.success(),
        "{}",
        String::from_utf8_lossy(&leftover.stderr)
    );

    // Two runs at once, each listening on a port and looking for the other's.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let task = root.join("suite/peer");
    let scan = r#"python3 - <<'EOF'
import socket, threading, time, http.server
srv = http.server.HTTPServer(("0.0.0.0", 9999), http.server.SimpleHTTPRequestHandler)
threading.Thread(target=srv.serve_forever, daemon=True).start()
time.sleep(3)
me = socket.gethostbyname(socket.gethostname())
base = me.rsplit(".", 1)[0]
found = []
for i in range(1, 20):
    ip = f"{base}.{i}"
    if ip == me:
        continue
    try:
        socket.create_connection((ip, 9999), 0.3).close()
        found.append(ip)
    except Exception:
        pass
print("peers on 9999:", found or "none")
try:
    socket.create_connection((me, 9999), 1).close()
    print("self on 9999: reached")
except Exception:
    print("self on 9999: blocked")
open("SCANNED", "w").write("1")
EOF"#;
    let script = serde_json::json!([
        {"tool_calls": [{"name": "bash", "input": {"command": scan}}]},
        {"text": "done"}
    ]);
    write(&task.join("repo/script.json"), &script.to_string());
    write(
        &task.join("task.toml"),
        "instruction = \"scan\"\nverify = \"test -f SCANNED\"\ntimeout_secs = 120\n",
    );
    let config = root.join("config.toml");
    write(&config, SCRIPTED);
    let out = root.join("out");
    let mut args = run_args(&root.join("suite"), &out, &config);
    args.repeat = 2;
    let matrix = prepare(&args).unwrap();
    let needs = needs(&matrix, None).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    let (backend, cleanup) = DockerBackend::start(Setup {
        binaries,
        images: vec!["ch-eval-base".into()],
        state: StateDir(out.join("proxy")),
        spec: needs.spec,
        keys: Keys::default(),
    })
    .await
    .unwrap();
    // Starting swept the dead invocation's leftovers.
    assert!(containers_named("label=ch-eval.invocation=deadbeef0000").is_empty());
    assert_eq!(
        inspect("ch-eval-deadbeef0000-1-agent", "{{.Id}}").trim(),
        ""
    );
    let nets = Command::new("docker")
        .args([
            "network",
            "ls",
            "--quiet",
            "--filter",
            "name=ch-eval-deadbeef0000",
        ])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&nets.stdout).trim(), "");
    let opts = Options {
        out: out.clone(),
        jobs: 2,
        max_cost: None,
        max_attempts: 3,
        backoff: std::time::Duration::from_secs(1),
    };
    let summary = run_matrix(Arc::new(matrix), Arc::new(backend), &opts).await;
    cleanup.run();
    assert_eq!(summary.unwrap().ran, 2);
    for repeat in [1, 2] {
        let events =
            std::fs::read_to_string(out.join(format!("runs/peer/base/{repeat}/events.jsonl")))
                .unwrap();
        assert!(events.contains("peers on 9999: none"), "{events}");
        // The probe works: each run reaches its own listener.
        assert!(events.contains("self on 9999: reached"), "{events}");
    }
    let records = load_all(&out).unwrap();
    assert!(records.iter().all(|r| r.passed), "{records:#?}");
    assert_eq!(containers("ch-eval"), "");
    assert_eq!(networks(), "");
}

/// The agent builds a tree too deep for path-based tools, with a `.git` and an
/// fsmonitor command at the top and the bottom and a locked directory between, then
/// probes `ch`'s own `/proc` entries.
fn hardening_script(marker: &str) -> String {
    let deep = format!(
        r#"cd /work && git init -q && git config core.fsmonitor 'touch {marker}' && \
n=$(printf 'd%.0s' $(seq 200)) && p=zz && for i in 1 2 3 4 5 6; do p="$p/$n"; done && \
mkdir -p "$p/.git" && printf '[core]\n\tfsmonitor = touch {marker}\n' > "$p/.git/config" && \
mkdir -p /ch-home/.Git && chmod 000 "zz/$n" && echo deep tree made"#
    );
    let probe = r#"python3 - <<'EOF'
import os
pids = [p for p in os.listdir("/proc") if p.isdigit() and open(f"/proc/{p}/comm").read().strip() == "ch"]
print("probe ch found:", "yes" if pids else "no")
for pid in pids:
    try:
        open(f"/proc/{pid}/environ", "rb").read()
        print("probe environ: readable")
    except OSError:
        print("probe environ: blocked")
    try:
        open(f"/proc/{pid}/fd/1", "a").close()
        print("probe stdout: writable")
    except OSError:
        print("probe stdout: blocked")
EOF"#;
    serde_json::json!([
        {"tool_calls": [{"name": "bash", "input": {"command": deep}}]},
        {"tool_calls": [{"name": "bash", "input": {"command": probe}}]},
        {"text": "done"}
    ])
    .to_string()
}

/// Whether `find` sees any `.git`, in any case, under `dir` (GNU find in a container
/// copes with the depth).
fn git_entries_under(dir: &Path) -> String {
    let out = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--label",
            "ch-eval",
            "--network",
            "none",
            "--user",
        ])
        .arg(format!(
            "{}:{}",
            String::from_utf8_lossy(&Command::new("id").arg("-u").output().unwrap().stdout).trim(),
            String::from_utf8_lossy(&Command::new("id").arg("-g").output().unwrap().stdout).trim()
        ))
        .args(["-v", &format!("{}:/run:ro", dir.display())])
        .args(["ch-eval-base", "find", "/run", "-iname", ".git"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test]
#[ignore = "needs Docker; builds the image and static binaries on first use"]
async fn deep_trees_and_proc_are_handled_in_real_containers() {
    let binaries = setup();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let marker = format!("/tmp/ch-eval-pwned-{}", std::process::id());
    let task = root.join("suite/deep");
    write(&task.join("repo/script.json"), &hardening_script(&marker));
    write(&task.join("repo/README.md"), "hi\n");
    write(
        &task.join("task.toml"),
        "instruction = \"build\"\nverify = \"test -d zz\"\ntimeout_secs = 120\n",
    );
    let config = root.join("config.toml");
    write(&config, SCRIPTED);
    let out = root.join("out");
    let run = |out: PathBuf, binaries: ch_eval::docker::Binaries| {
        let args = run_args(&root.join("suite"), &out, &config);
        async move {
            let matrix = prepare(&args).unwrap();
            let needs = needs(&matrix, None).unwrap();
            std::fs::create_dir_all(&out).unwrap();
            let (backend, cleanup) = DockerBackend::start(Setup {
                binaries,
                images: vec!["ch-eval-base".into()],
                state: StateDir(out.join("proxy")),
                spec: needs.spec,
                keys: Keys::default(),
            })
            .await
            .unwrap();
            let opts = Options {
                out: out.clone(),
                jobs: 1,
                max_cost: None,
                max_attempts: 3,
                backoff: std::time::Duration::from_secs(1),
            };
            let summary = run_matrix(Arc::new(matrix), Arc::new(backend), &opts).await;
            cleanup.run();
            summary
        }
    };
    assert_eq!(run(out.clone(), binaries.clone()).await.unwrap().ran, 1);
    let dir = out.join("runs/deep/base/1");
    let events = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
    for expected in [
        "deep tree made",
        "probe ch found: yes",
        "probe environ: blocked",
        "probe stdout: blocked",
    ] {
        assert!(events.contains(expected), "{expected}\n{events}");
    }
    let r = &load_all(&out).unwrap()[0];
    assert!(r.passed, "{r:#?}");
    assert_eq!(r.git_entries_removed, 3, "{r:#?}");
    assert_eq!(git_entries_under(&dir), "", "a .git is left");
    // The host's git finds no repository there, and runs nothing.
    let status = Command::new("git")
        .args(["status"])
        .current_dir(dir.join("work"))
        .env("GIT_CEILING_DIRECTORIES", &root)
        .output()
        .unwrap();
    assert!(!status.status.success());
    assert!(
        !Path::new(&marker).exists(),
        "the agent's fsmonitor ran on the host"
    );

    // Resuming removes the deep, locked run directory and makes the run again.
    std::fs::remove_file(dir.join("run.json")).unwrap();
    assert_eq!(run(out.clone(), binaries).await.unwrap().ran, 1);
    assert_eq!(containers("ch-eval"), "");
    assert_eq!(networks(), "");
    // The tempdir holds the deep tree again; remove it the way ch-eval does.
    ch_eval::workspace::remove_dir(&root.join("out")).unwrap();
}

/// Answers each run from its own queue, told apart by a word in the request.
struct ByRun(Mutex<Vec<(String, VecDeque<String>)>>);

impl Respond for ByRun {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body = String::from_utf8_lossy(&request.body);
        let mut runs = self.0.lock().unwrap();
        let reply = runs
            .iter_mut()
            .find(|(word, _)| body.contains(word.as_str()))
            .and_then(|(_, replies)| replies.pop_front())
            .unwrap_or_else(|| text("(no more replies)", [1, 1, 0, 0]));
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(reply)
    }
}

#[tokio::test]
#[ignore = "needs Docker; builds the image and static binaries on first use"]
async fn one_run_flooding_the_proxy_doesnt_hold_up_another() {
    let binaries = setup();
    let upstream = MockServer::start().await;
    let port = upstream.address().port();
    let flood = r#"python3 - <<'EOF'
import socket, time
held = []
for i in range(300):
    try:
        held.append(socket.create_connection(("ch-eval-proxy", 8080), 2))
    except OSError:
        pass
time.sleep(1)
alive = 0
for c in held:
    c.setblocking(False)
    try:
        alive += c.recv(1) != b""
    except BlockingIOError:
        alive += 1
    except OSError:
        pass
print("probe connections kept open:", alive)
time.sleep(20)
EOF"#;
    let u = [100, 10, 0, 0];
    let replies = vec![
        (
            "FLOOD-TASK".to_string(),
            VecDeque::from([
                tool("toolu_f1", "bash", json!({"command": flood}), u),
                text("done", u),
            ]),
        ),
        (
            "fix the failing test".to_string(),
            VecDeque::from([
                tool("toolu_c0", "bash", json!({"command": "sleep 4"}), u),
                tool("toolu_c1", "read_file", json!({"path": "calc.py"}), u),
                tool(
                    "toolu_c2",
                    "edit_file",
                    json!({"path": "calc.py", "old_text": "    return a - b", "new_text": "    return a + b"}),
                    u,
                ),
                text("Fixed.", u),
            ]),
        ),
    ];
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ByRun(Mutex::new(replies)))
        .mount(&upstream)
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    calc_task(&root.join("suite/calc"));
    let flood_task = root.join("suite/flood");
    write(&flood_task.join("repo/README.md"), "hi\n");
    write(
        &flood_task.join("task.toml"),
        "instruction = \"FLOOD-TASK\"\nverify = \"test -f FLOODED\"\ntimeout_secs = 120\n",
    );
    let config = root.join("config.toml");
    write(
        &config,
        &format!(
            "[model]\nprovider = \"anthropic\"\nid = \"claude-opus-5-5\"\n\
             [plugins.provider-anthropic]\nbase_url = \"http://host.docker.internal:{port}\"\n\
             max_retries = 0\n"
        ),
    );
    let out = root.join("out");
    let mut args = run_args(&root.join("suite"), &out, &config);
    args.jobs = 2;
    let matrix = prepare(&args).unwrap();
    let needs = needs(&matrix, None).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    let (backend, cleanup) = DockerBackend::start(Setup {
        binaries,
        images: vec!["ch-eval-base".into()],
        state: StateDir(out.join("proxy")),
        spec: needs.spec,
        keys: Keys {
            anthropic: Some(REAL_KEY.into()),
            openai: None,
        },
    })
    .await
    .unwrap();
    let opts = Options {
        out: out.clone(),
        jobs: 2,
        max_cost: None,
        max_attempts: 3,
        backoff: std::time::Duration::from_secs(1),
    };
    let summary = run_matrix(Arc::new(matrix), Arc::new(backend), &opts).await;
    cleanup.run();
    summary.unwrap();
    let records = load_all(&out).unwrap();
    let calc = records.iter().find(|r| r.task == "calc").unwrap();
    assert!(calc.passed, "{calc:#?}");
    // Its calls weren't held up by the other run's connections (a header timeout is
    // 30 seconds; the run itself sleeps 4).
    assert!(calc.wall_ms < 25_000, "{calc:#?}");
    let events = std::fs::read_to_string(out.join("runs/flood/base/1/events.jsonl")).unwrap();
    let kept: usize = events
        .split("probe connections kept open: ")
        .nth(1)
        .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.parse().ok())
        .expect("the flood ran");
    assert!(kept <= 8, "{kept} connections kept");
    assert_eq!(containers("ch-eval"), "");
}

#[tokio::test]
#[ignore = "needs Docker; builds the image and static binaries on first use"]
async fn another_invocation_in_another_time_zone_leaves_a_live_one_alone() {
    let binaries = setup();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    // A live invocation: this process, with its proxy.
    let live = root.join("live");
    std::fs::create_dir_all(&live).unwrap();
    let state = StateDir(live.join("proxy"));
    let (_backend, cleanup) = DockerBackend::start(Setup {
        binaries,
        images: vec!["ch-eval-base".into()],
        state: state.clone(),
        spec: Default::default(),
        keys: Keys::default(),
    })
    .await
    .unwrap();
    let proxy = proxy_container();
    // Without its heartbeat, only the pid check can tell it is alive.
    std::fs::remove_file(state.heartbeat()).unwrap();

    // Another ch-eval, in another time zone and locale, runs a scripted task.
    let task = root.join("suite/t");
    write(
        &task.join("repo/script.json"),
        r#"[{"tool_calls": [{"name": "bash", "input": {"command": "touch DONE"}}]}, {"text": "ok"}]"#,
    );
    write(
        &task.join("task.toml"),
        "instruction = \"x\"\nverify = \"test -f DONE\"\n",
    );
    let config = root.join("config.toml");
    write(&config, SCRIPTED);
    let other = Command::new(env!("CARGO_BIN_EXE_ch-eval"))
        .args(["run", "--suite"])
        .arg(root.join("suite"))
        .arg("--out")
        .arg(root.join("other"))
        .arg("--config")
        .arg(&config)
        .env("TZ", "Asia/Kolkata")
        .env("LC_ALL", "fr_FR.UTF-8")
        .output()
        .unwrap();
    assert!(
        other.status.success(),
        "{}",
        String::from_utf8_lossy(&other.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&other.stderr).contains("removing what a stopped"),
        "{}",
        String::from_utf8_lossy(&other.stderr)
    );
    assert_eq!(
        proxy_container(),
        proxy,
        "the live invocation's proxy was removed"
    );
    cleanup.run();
    assert_eq!(containers("ch-eval"), "");
    assert_eq!(networks(), "");
}
