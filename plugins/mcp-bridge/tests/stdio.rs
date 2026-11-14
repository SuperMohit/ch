//! The bridge against a real stdio server (`ch-mcp-test-server`).

mod common;

use std::path::Path;
use std::time::Duration;

use ch_kernel::{Services, SourceStatus};
use common::{env_report, harness, server_toml};
use serde_json::json;

fn log_of(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("server.log")).unwrap_or_default()
}

#[tokio::test]
async fn tools_are_listed_named_and_called() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("server.log");
    let config = server_toml("test", &["--log", log.to_str().unwrap(), "--banner"], "");
    let mut h = harness(dir.path(), &config, Services::default()).await;
    // 19 tools, one with a non-object schema refused.
    assert_eq!(h.status("test"), Some(SourceStatus::Ready { tools: 18 }));

    let (out, err) = h.call("mcp__test__echo", json!({"text": "hi"})).await;
    assert_eq!((out.as_str(), err), ("hi", false));
    let tools = h.last_tools();
    assert!(tools.contains(&"mcp__test__echo".to_string()), "{tools:?}");
    assert!(!tools.iter().any(|t| t.contains("bad_schema")), "{tools:?}");
    let dotted = tools
        .iter()
        .find(|t| t.starts_with("mcp__test__dotted_name_"));
    let dotted = dotted.unwrap_or_else(|| panic!("{tools:?}")).clone();
    let (out, _) = h.call(&dotted, json!({})).await;
    assert!(out.contains("no tool") || out.contains("(no output)") || !out.is_empty());

    // Conformance: every tool passes the outside-tool profile, and none is read-only,
    // whatever the server says (`peek` claims readOnlyHint).
    let requests = h.model.requests.lock().unwrap().clone();
    for spec in &requests.last().unwrap().tools {
        ch_testkit::assert_outside_spec_conforms(spec, false);
    }

    // Only roots were declared, and the server's instructions went nowhere.
    let log = log_of(dir.path());
    assert!(log.contains(r#"initialize {"roots":{}}"#), "{log}");
    for r in &requests {
        assert!(!format!("{:?}", r.system).contains("IGNORE ALL"));
    }
    h.session.end().await;
}

#[tokio::test]
async fn results_become_observations_and_secrets_are_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let config = server_toml("t", &[], "env = { MCP_SECRET = \"literal-secret-123\" }");
    let mut h = harness(dir.path(), &config, Services::default()).await;

    let (out, err) = h.call("mcp__t__fail", json!({})).await;
    assert!(err && out == "bad input", "{out}");

    let (out, err) = h.call("mcp__t__rpc_error", json!({})).await;
    assert!(err, "{out}");
    assert!(out.contains("token [redacted] rejected"), "{out}");

    let (out, err) = h.call("mcp__t__leak", json!({})).await;
    assert!(!err);
    for secret in ["literal-secret-123", "abcdefghijkl", "pw12345"] {
        assert!(!out.contains(secret), "{secret} in {out}");
    }

    let (out, _) = h.call("mcp__t__image", json!({})).await;
    assert_eq!(out, "see image\n[image omitted: image/png, 3 KB]");

    let (out, _) = h.call("mcp__t__structured", json!({})).await;
    assert_eq!(out, r#"{"answer":42}"#);

    let (out, err) = h.call("mcp__t__noise", json!({})).await;
    assert_eq!((out.as_str(), err), ("after noise", false));

    let (out, err) = h.call("mcp__t__big", json!({"mb": 17})).await;
    assert!(
        err && out.contains("over 16 MB"),
        "{}",
        &out[..out.len().min(300)]
    );
    // The connection survives an oversized message.
    let (out, _) = h.call("mcp__t__echo", json!({"text": "still here"})).await;
    assert_eq!(out, "still here");
    h.session.end().await;
}

#[tokio::test]
async fn the_environment_is_cleared_and_the_workspace_is_a_root() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    let config = format!(
        "{}{}",
        server_toml("home", &[], "env = { EXTRA = \"1\" }"),
        server_toml("ws", &[], "cwd = \"workspace\"")
    );
    let mut h = harness(&ws, &config, Services::default()).await;

    let (out, _) = h.call("mcp__home__env", json!({})).await;
    let report = env_report(&out);
    let names: Vec<&str> = report["env"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    for name in &names {
        assert!(
            ch_mcp_bridge::launch::INHERITED_ENV.contains(name)
                || matches!(*name, "CH_WORKSPACE" | "EXTRA" | "__CF_USER_TEXT_ENCODING"),
            "{name} leaked into the server's environment: {names:?}"
        );
    }
    assert_eq!(report["env"]["CH_WORKSPACE"], ws.display().to_string());
    let home = std::env::var("HOME").unwrap();
    let home = Path::new(&home).canonicalize().unwrap();
    let cwd = Path::new(report["cwd"].as_str().unwrap())
        .canonicalize()
        .unwrap();
    assert_eq!(cwd, home, "user-level servers start in $HOME");

    let (out, _) = h.call("mcp__ws__env", json!({})).await;
    let cwd = env_report(&out)["cwd"].as_str().unwrap().to_string();
    assert_eq!(Path::new(&cwd).canonicalize().unwrap(), ws);

    let (out, _) = h.call("mcp__ws__roots", json!({})).await;
    let roots = env_report(&out);
    let uri = roots["roots"][0]["uri"].as_str().unwrap_or_default();
    assert_eq!(uri, format!("file://{}/", ws.display()), "{out}");
    h.session.end().await;
}

#[tokio::test]
async fn a_changed_tool_list_and_pages() {
    let dir = tempfile::tempdir().unwrap();
    let config = format!(
        "{}{}",
        server_toml("p", &["--page-size", "2"], ""),
        server_toml("many", &["--page-size", "1", "--extra-tools", "20"], "")
    );
    let mut h = harness(dir.path(), &config, Services::default()).await;
    assert_eq!(h.status("p"), Some(SourceStatus::Ready { tools: 18 }));
    // Ten pages of one tool each.
    assert_eq!(h.status("many"), Some(SourceStatus::Ready { tools: 10 }));

    h.call("mcp__p__add_tool", json!({})).await;
    h.wait_status("p", |s| *s == SourceStatus::Ready { tools: 19 })
        .await;
    let (out, _) = h.call("mcp__p__echo", json!({"text": "x"})).await;
    assert_eq!(out, "x");
    assert!(h.last_tools().contains(&"mcp__p__added".to_string()));
    h.session.end().await;
}

#[tokio::test]
async fn timeouts_cancel_and_exits_fail_calls() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("server.log");
    let config = server_toml(
        "s",
        &["--log", log.to_str().unwrap()],
        "call_timeout_secs = 1\nenv = { MCP_SECRET = \"abcd1234efgh\" }",
    );
    let mut h = harness(dir.path(), &config, Services::default()).await;

    let (out, err) = h.call("mcp__s__sleep", json!({"ms": 5000})).await;
    assert!(err && out.contains("no answer within 1 s"), "{out}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        log_of(dir.path()).contains("stopped"),
        "{}",
        log_of(dir.path())
    );

    // A turn cancelled while a call runs sends the cancel notification too.
    let stopped = |dir: &Path| log_of(dir).matches("stopped").count();
    let before = stopped(dir.path());
    let cancel = h.session.cancel_handle();
    let stop = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel();
    });
    let (out, err) = h.call("mcp__s__sleep", json!({"ms": 3000})).await;
    stop.await.unwrap();
    assert!(err, "{out}");
    for _ in 0..100 {
        if stopped(dir.path()) > before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(stopped(dir.path()), before + 1, "{}", log_of(dir.path()));

    let (out, err) = h.call("mcp__s__exit", json!({"code": 7})).await;
    assert!(err, "{out}");
    let status = h
        .wait_status("s", |s| matches!(s, SourceStatus::Exited { .. }))
        .await;
    let SourceStatus::Exited { reason } = status else {
        unreachable!()
    };
    assert!(reason.contains("status 7"), "{reason}");
    assert!(reason.contains("bye from the test server"), "{reason}");
    h.session.end().await;
}

#[tokio::test]
async fn failed_and_slow_starts() {
    let dir = tempfile::tempdir().unwrap();
    let config = format!(
        "{}{}{}",
        server_toml(
            "broken",
            &["--fail-init"],
            "env = { MCP_SECRET = \"abcd1234efgh\" }"
        ),
        server_toml(
            "slow",
            &[
                "--hang-init",
                "--linger",
                "--log",
                dir.path().join("slow.log").to_str().unwrap()
            ],
            "startup_timeout_secs = 1",
        ),
        "[[mcp_servers]]\nname = \"missing\"\ncommand = \"no-such-ch-test-binary\"\n",
    );
    let started = std::time::Instant::now();
    let mut h = harness(dir.path(), &config, Services::default()).await;
    assert!(started.elapsed() < Duration::from_secs(5));

    let Some(SourceStatus::Failed { reason }) = h.status("broken") else {
        panic!("{:?}", h.status("broken"));
    };
    assert!(reason.contains("status 3"), "{reason}");
    assert!(
        reason.contains("config error") && !reason.contains("abcd1234efgh"),
        "{reason}"
    );
    let Some(SourceStatus::Failed { reason }) = h.status("missing") else {
        panic!("{:?}", h.status("missing"));
    };
    assert!(reason.contains("isn't on PATH"), "{reason}");
    assert_eq!(h.status("slow"), Some(SourceStatus::Starting));

    // Other servers' failures don't stop /mcp from listing them.
    let listing = h.command("/mcp").await;
    assert!(
        listing.contains("`broken`") && listing.contains("failed"),
        "{listing}"
    );
    let slow = logged_pid(&dir.path().join("slow.log"));
    h.session.end().await;
    assert!(
        gone(slow).await,
        "a server still starting survived the session"
    );
}

#[tokio::test]
async fn stopping_kills_the_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path(), &server_toml("k", &[], ""), Services::default()).await;
    let (out, _) = h.call("mcp__k__env", json!({})).await;
    let pid = env_report(&out)["pid"].as_i64().unwrap() as i32;
    assert!(ch_mcp_bridge::procs::registered().contains(&pid));

    let reply = h.command("/mcp reconnect k").await;
    assert!(reply.contains("connected"), "{reply}");
    assert!(!ch_mcp_bridge::procs::registered().contains(&pid));
    assert!(!alive(pid), "the old server still runs after a reconnect");

    let (out, _) = h.call("mcp__k__env", json!({})).await;
    let pid = env_report(&out)["pid"].as_i64().unwrap() as i32;
    let listing = h.command("/mcp").await;
    assert!(listing.contains("ch-test-server 1.0.0"), "{listing}");
    h.session.end().await;
    assert!(!alive(pid));
    assert!(!ch_mcp_bridge::procs::registered().contains(&pid));
}

fn alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

#[tokio::test]
async fn project_servers_start_only_once_trusted() {
    use std::sync::{Arc, Mutex};

    use ch_mcp_bridge::{Origin, OriginKind, ProjectServers, ServerConfig};

    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    let json = format!(
        r#"{{"mcpServers": {{
            "proj": {{"command": {:?}, "args": ["marker.txt"]}},
            "user": {{"command": {:?}}}
        }}}}"#,
        common::SERVER,
        common::SERVER
    );
    std::fs::write(ws.join(".mcp.json"), json).unwrap();
    std::fs::write(ws.join("marker.txt"), "v1").unwrap();
    let found = ch_mcp_bridge::discover_project(&ws, None, &["user".to_string()]);
    let names: Vec<&str> = found.servers.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["proj"]);
    assert!(
        found.warnings[0].contains("yours wins"),
        "{:?}",
        found.warnings
    );
    assert_eq!(
        found.servers[0].origin,
        Origin {
            kind: OriginKind::McpJson,
            file: ws.join(".mcp.json")
        }
    );

    let saved: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let mut services = Services::default();
    services.provide(Arc::new(ProjectServers {
        servers: found.servers.into_iter().map(|s| (s, false)).collect(),
        trust: Some(Box::new({
            let saved = saved.clone();
            move |s: &ServerConfig, hash: &str| {
                saved
                    .lock()
                    .unwrap()
                    .push((s.name.clone(), hash.to_string()));
                Ok(())
            }
        })),
    }));
    let mut h = harness(&ws, "", services).await;
    assert_eq!(
        h.status("proj"),
        None,
        "an untrusted project server started"
    );
    let listing = h.command("/mcp").await;
    assert!(
        listing.contains("`proj`") && listing.contains("not trusted"),
        "{listing}"
    );

    // Trusting needs the entry shown first.
    let reply = h.command("/mcp trust proj yes").await;
    assert!(reply.contains("wasn't shown yet"), "{reply}");
    assert!(saved.lock().unwrap().is_empty());
    let shown = h.command("/mcp trust proj").await;
    assert!(
        shown.contains("runs:") && shown.contains("ch-mcp-test-server"),
        "{shown}"
    );
    let reply = h.command("/mcp trust proj yes").await;
    assert!(reply.contains("connected"), "{reply}");
    assert_eq!(saved.lock().unwrap().len(), 1);
    let (out, _) = h.call("mcp__proj__echo", json!({"text": "trusted"})).await;
    assert_eq!(out, "trusted");

    // A file the server's arguments name changes: it doesn't start again.
    std::fs::write(ws.join("marker.txt"), "v2").unwrap();
    let reply = h.command("/mcp reconnect proj").await;
    assert!(reply.contains("changed since you trusted it"), "{reply}");
    h.session.end().await;
}

/// The pid a test server logged as `pid N` in `file`.
fn logged_pid(file: &Path) -> i32 {
    for _ in 0..250 {
        let text = std::fs::read_to_string(file).unwrap_or_default();
        if let Some(pid) = text
            .lines()
            .find_map(|l| l.strip_prefix("pid ").and_then(|p| p.trim().parse().ok()))
        {
            return pid;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("no pid in {}", file.display());
}

async fn gone(pid: i32) -> bool {
    for _ in 0..250 {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn a_server_stopped_during_its_handshake_is_killed() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("server.log");
    let config = server_toml(
        "s",
        &["--hang-init", "--linger", "--log", log.to_str().unwrap()],
        "startup_timeout_secs = 1",
    );
    let mut h = harness(dir.path(), &config, Services::default()).await;
    assert_eq!(h.status("s"), Some(SourceStatus::Starting));
    let first = logged_pid(&log);
    std::fs::remove_file(&log).unwrap();

    // A reconnect while it is still in the handshake (the kernel keeps starting it in the
    // background) kills the first process.
    h.command("/mcp reconnect s").await;
    assert!(gone(first).await, "the first server survived a reconnect");
    let second = logged_pid(&log);
    assert!(ch_mcp_bridge::procs::registered().contains(&second));
    h.session.end().await;
    assert!(gone(second).await, "the second server survived the session");
    assert!(!ch_mcp_bridge::procs::registered().contains(&second));
}

#[tokio::test]
async fn the_whole_group_is_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(
        dir.path(),
        &server_toml("g", &["--linger"], ""),
        Services::default(),
    )
    .await;
    let (out, _) = h.call("mcp__g__spawn_child", json!({})).await;
    let child: i32 = out.trim().parse().unwrap();
    assert!(alive(child));
    h.command("/mcp reconnect g").await;
    assert!(gone(child).await, "the server's child survived");
    h.session.end().await;
}

#[tokio::test]
async fn an_exit_is_noticed_while_a_grandchild_holds_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("server.log");
    let mut h = harness(
        dir.path(),
        &server_toml("x", &["--log", log.to_str().unwrap()], ""),
        Services::default(),
    )
    .await;
    let (_, err) = h.call("mcp__x__exit_keep_stdout", json!({})).await;
    assert!(err);
    let status = h
        .wait_status("x", |s| matches!(s, SourceStatus::Exited { .. }))
        .await;
    assert!(format!("{status:?}").contains("status 9"), "{status:?}");
    let grandchild: i32 = log_of(dir.path())
        .lines()
        .find_map(|l| l.strip_prefix("grandchild ").and_then(|p| p.parse().ok()))
        .unwrap();
    assert!(
        gone(grandchild).await,
        "the group wasn't stopped after the exit"
    );
    h.session.end().await;
}

#[tokio::test]
async fn a_server_that_stops_reading_still_times_out_and_stops() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("server.log");
    let config = server_toml(
        "b",
        &["--linger", "--log", log.to_str().unwrap()],
        "call_timeout_secs = 1",
    );
    let mut h = harness(dir.path(), &config, Services::default()).await;
    let pid = logged_pid(&log);
    let (out, err) = h.call("mcp__b__block", json!({"ms": 30000})).await;
    assert!(err && out.contains("no answer within 1 s"), "{out}");
    // Bigger than the pipe: the write blocks while the server doesn't read.
    let started = std::time::Instant::now();
    let (out, err) = h
        .call("mcp__b__echo", json!({"text": "x".repeat(300_000)}))
        .await;
    assert!(err, "{}", &out[..out.len().min(200)]);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    let started = std::time::Instant::now();
    h.session.end().await;
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    assert!(gone(pid).await, "the blocked server survived the session");
}
