//! The host against a real plugin built with the SDK (`ch-ext-test-plugin`).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ch_kernel::reexports::CancellationToken;
use ch_kernel::*;
use futures::stream;
use serde_json::{Value, json};

const PLUGIN: &str = env!("CARGO_BIN_EXE_ch-ext-test-plugin");

#[derive(Clone, Default)]
struct Model {
    replies: Arc<Mutex<VecDeque<ModelResponse>>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

#[async_trait::async_trait]
impl ModelProvider for Model {
    fn id(&self) -> &str {
        "model"
    }
    async fn stream(
        &self,
        request: ModelRequest,
        _cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        self.requests.lock().unwrap().push(request);
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(ModelResponse {
                message: Message::assistant("done"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            });
        Ok(Box::pin(stream::iter(vec![Ok(ModelEvent::Completed(
            reply,
        ))])))
    }
    fn context_window(&self, _model: &str) -> Option<u64> {
        Some(1_000_000)
    }
}

struct ModelPlugin(Model);

impl Plugin for ModelPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new("model", "test model")
    }
    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        cx.add_provider(self.0.clone());
        Ok(())
    }
}

struct Harness {
    session: Session,
    model: Model,
    _dir: tempfile::TempDir,
    home: PathBuf,
}

/// A plugin directory `home/plugins/<name>` running the test plugin.
fn add_plugin(home: &Path, name: &str, args: &[&str], extra: &str) -> PathBuf {
    let dir = home.join("plugins").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let args: Vec<String> = args.iter().map(|a| format!("{a:?}")).collect();
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            "name = \"{name}\"\ncommand = {PLUGIN:?}\nargs = [{}]\n{extra}\n\
             [[commands]]\nname = \"hello\"\ndescription = \"Says hello\"\n",
            args.join(", ")
        ),
    )
    .unwrap();
    dir
}

async fn harness(setup: impl FnOnce(&Path), config: &str) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    setup(&home);
    let model = Model::default();
    let mut services = Services::default();
    services.provide(Arc::new(ChHome(home.clone())));
    let config = Config::parse(&format!("[model]\nid = \"m\"\n{config}")).unwrap();
    let plugins: Vec<Arc<dyn Plugin>> = vec![
        Arc::new(ModelPlugin(model.clone())),
        ch_external_host::plugin(),
    ];
    let registry = RegistryBuilder::build_with_services(plugins, &config, &ws, services).unwrap();
    let mut session = Session::builder(Arc::new(registry), Arc::new(config))
        .workspace(&ws)
        .build()
        .unwrap();
    session.start().await;
    Harness {
        session,
        model,
        _dir: dir,
        home,
    }
}

impl Harness {
    async fn call(&mut self, tool: &str, input: Value) -> (String, bool) {
        let id = format!("c{}", self.session.history().len());
        self.model.replies.lock().unwrap().push_back(ModelResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: id.clone(),
                    name: tool.into(),
                    input,
                }],
            },
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        });
        self.session.run_turn("go").await;
        self.session
            .history()
            .messages()
            .iter()
            .flat_map(|m| m.content.clone())
            .find_map(|b| match b {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } if tool_use_id == id => Some((content, is_error)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no result for {tool}"))
    }

    fn status(&self, name: &str) -> Option<SourceStatus> {
        let id = format!("ext:{name}");
        (self.session.tool_sources().into_iter())
            .find(|(i, _)| *i == id)
            .map(|(_, s)| s)
    }

    async fn wait_status(&self, name: &str, ok: impl Fn(&SourceStatus) -> bool) -> SourceStatus {
        for _ in 0..250 {
            if let Some(s) = self.status(name).filter(|s| ok(s)) {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{name}: {:?}", self.status(name));
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.model.requests.lock().unwrap().clone()
    }
}

fn alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
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
async fn tools_hooks_prompts_commands_and_events() {
    let mut h = harness(
        |home| {
            add_plugin(home, "t", &[], "");
        },
        "",
    )
    .await;
    assert_eq!(h.status("t"), Some(SourceStatus::Ready { tools: 10 }));

    let (out, err) = h.call("ext__t__echo", json!({"text": "hi"})).await;
    assert!(!err);
    assert!(
        out.starts_with("hi") && out.contains("checked by the test plugin"),
        "{out}"
    );

    // Conformance, and nothing is read-only on its own say.
    let tools = h.requests().last().unwrap().tools.clone();
    for spec in &tools {
        ch_testkit::assert_outside_spec_conforms(spec, false);
    }

    // The plugin's pre_tool hook.
    let (out, err) = h
        .call("ext__t__guarded_echo", json!({"text": "fine"}))
        .await;
    assert_eq!((out.as_str(), err), ("fine", false));
    let (out, err) = h
        .call("ext__t__guarded_echo", json!({"text": "forbidden"}))
        .await;
    assert!(err && out.contains("the input is forbidden"), "{out}");

    // Its system block is in the prompt, and its first-turn reminder in the history.
    let first = &h.requests()[0];
    assert!(
        first
            .system
            .iter()
            .any(|b| b.contains("TEST PLUGIN SYSTEM BLOCK"))
    );
    let history = format!("{:?}", h.session.history().messages());
    assert!(history.contains("first-turn reminder from the test plugin"));

    // Commands.
    match h.session.run_command("/hello world").await {
        Some(Ok(Outcome::Message(m))) => assert_eq!(m, "hello, world"),
        other => panic!("{other:?}"),
    }

    // Events it subscribed to.
    let log = h.home.join("plugins/t/events.log");
    let mut events = String::new();
    for _ in 0..100 {
        events = std::fs::read_to_string(&log).unwrap_or_default();
        if events.contains("ext__t__guarded_echo") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        events.contains("\"tool_call_end\" \"ext__t__echo\""),
        "{events}"
    );

    // Notifications reach the user.
    let mut rx = h.session.events().subscribe();
    h.call("ext__t__notify", json!({})).await;
    let mut noted = false;
    while let Ok(ev) = rx.try_recv() {
        if let AgentEvent::Note { source, text } = ev {
            noted |= source == "ext:t" && text == "hello from the plugin";
        }
    }
    assert!(noted);

    // A changed tool list.
    h.call("ext__t__add_tool", json!({})).await;
    h.wait_status("t", |s| *s == SourceStatus::Ready { tools: 2 })
        .await;
    let (out, _) = h.call("ext__t__extra", json!({})).await;
    assert_eq!(out, "extra");
    h.session.end().await;
}

#[tokio::test]
async fn the_environment_is_cleared_and_secrets_redacted() {
    let mut h = harness(
        |home| {
            add_plugin(
                home,
                "e",
                &[],
                "env = { SECRET_TOKEN = \"s3cr3t-token-value\" }",
            );
        },
        "",
    )
    .await;
    let (out, _) = h.call("ext__e__env", json!({})).await;
    let report: Value = serde_json::from_str(&out).unwrap();
    for name in report["env"].as_object().unwrap().keys() {
        assert!(
            ch_mcp_bridge::launch::INHERITED_ENV.contains(&name.as_str())
                || matches!(
                    name.as_str(),
                    "CH_WORKSPACE" | "CH_PLUGIN_DIR" | "SECRET_TOKEN" | "__CF_USER_TEXT_ENCODING"
                ),
            "{name} leaked"
        );
    }
    let cwd = Path::new(report["cwd"].as_str().unwrap())
        .canonicalize()
        .unwrap();
    assert_eq!(cwd, h.home.join("plugins/e").canonicalize().unwrap());
    let (out, _) = h.call("ext__e__leak", json!({})).await;
    assert_eq!(out, "secret=[redacted]");
    h.session.end().await;
}

#[tokio::test]
async fn a_crashed_guard_fails_closed_unless_told_otherwise() {
    let mut h = harness(
        |home| {
            add_plugin(home, "guard", &[], "");
            add_plugin(home, "lax", &[], "on_error = \"allow\"\nhook_priority = 5");
        },
        "",
    )
    .await;
    // Both plugins check `*guarded*` calls; crash the strict one.
    let (out, err) = h.call("ext__guard__crash", json!({})).await;
    assert!(err, "{out}");
    let status = h
        .wait_status("guard", |s| matches!(s, SourceStatus::Exited { .. }))
        .await;
    let SourceStatus::Exited { reason } = status else {
        unreachable!()
    };
    assert!(
        reason.contains("status 7") && reason.contains("crashing on purpose"),
        "{reason}"
    );

    let (out, err) = h
        .call("ext__lax__guarded_echo", json!({"text": "fine"}))
        .await;
    assert!(err, "{out}");
    assert!(out.contains("`guard` checks this call"), "{out}");

    match h.session.run_command("/hello x").await {
        Some(Ok(Outcome::Message(m))) => assert!(m.contains("isn't running"), "{m}"),
        other => panic!("{other:?}"),
    }
    h.session.end().await;
}

#[tokio::test]
async fn failed_slow_and_stopped_plugins() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(
        |home| {
            add_plugin(
                home,
                "broken",
                &["--fail-init"],
                "env = { SECRET_TOKEN = \"abcd1234efgh\" }",
            );
            add_plugin(home, "slow", &["--hang"], "startup_timeout_secs = 1");
            add_plugin(home, "timer", &["--linger"], "call_timeout_secs = 1");
            add_plugin(home, "off", &[], "");
        },
        "[plugins.external-host]\ndisable = [\"off\"]\n",
    )
    .await;
    drop(dir);
    let Some(SourceStatus::Failed { reason }) = h.status("broken") else {
        panic!("{:?}", h.status("broken"));
    };
    assert!(
        reason.contains("status 3") && !reason.contains("abcd1234efgh"),
        "{reason}"
    );
    assert_eq!(h.status("slow"), Some(SourceStatus::Starting));
    assert_eq!(h.status("off"), None);

    let (out, _) = h.call("ext__timer__env", json!({})).await;
    let pid = serde_json::from_str::<Value>(&out).unwrap()["pid"]
        .as_i64()
        .unwrap() as i32;
    let (out, _) = h.call("ext__timer__child", json!({})).await;
    let child: i32 = out.parse().unwrap();

    let (out, err) = h.call("ext__timer__slow", json!({"ms": 3000})).await;
    assert!(err && out.contains("no answer within 1 s"), "{out}");
    h.session.end().await;
    assert!(gone(pid).await, "the plugin survived the session");
    assert!(
        child > 0 && gone(child).await,
        "the plugin's child survived"
    );
}

#[tokio::test]
async fn the_python_example_blocks_secrets() {
    let python = std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !python {
        eprintln!("skipping: python3 not available");
        return;
    }
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/plugins/block-secrets/plugin.py")
        .canonicalize()
        .unwrap();
    let mut h = harness(
        |home| {
            add_plugin(home, "t", &[], "");
            let dir = home.join("plugins/block-secrets");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("plugin.toml"),
                format!(
                    "name = \"block-secrets\"\ncommand = \"python3\"\nargs = [{:?}]\n",
                    script.display().to_string()
                ),
            )
            .unwrap();
        },
        "",
    )
    .await;
    assert_eq!(
        h.status("block-secrets"),
        Some(SourceStatus::Ready { tools: 0 })
    );
    let (out, err) = h.call("ext__t__echo", json!({"text": "hello"})).await;
    assert!(!err && out.starts_with("hello"), "{out}");
    let (out, err) = h
        .call(
            "ext__t__echo",
            json!({"text": "key AKIAABCDEFGHIJKLMNOP here"}),
        )
        .await;
    assert!(err && out.contains("AWS access key id"), "{out}");
    let (_, err) = h
        .call(
            "ext__t__echo",
            json!({"nested": {"-----BEGIN RSA PRIVATE KEY-----": 1}}),
        )
        .await;
    assert!(err, "keys are checked too");
    h.session.end().await;
}

#[tokio::test]
async fn a_lax_guard_that_crashed_lets_calls_through() {
    let mut h = harness(
        |home| {
            add_plugin(home, "lax", &[], "on_error = \"allow\"");
            add_plugin(home, "tools", &[], "on_error = \"allow\"");
        },
        "",
    )
    .await;
    h.call("ext__lax__crash", json!({})).await;
    h.wait_status("lax", |s| matches!(s, SourceStatus::Exited { .. }))
        .await;
    // `lax` checked `*guarded*` calls; its crash doesn't stop them.
    let (out, err) = h
        .call("ext__tools__guarded_echo", json!({"text": "fine"}))
        .await;
    assert_eq!((out.as_str(), err), ("fine", false));
    h.session.end().await;
}

#[tokio::test]
async fn a_slow_plugin_keeps_starting_in_the_background() {
    let mut h = harness(
        |home| {
            add_plugin(
                home,
                "slow",
                &["--slow-init", "1500"],
                "startup_timeout_secs = 1",
            );
        },
        "",
    )
    .await;
    assert_eq!(h.status("slow"), Some(SourceStatus::Starting));
    h.wait_status("slow", |s| matches!(s, SourceStatus::Ready { .. }))
        .await;
    let (out, err) = h.call("ext__slow__echo", json!({"text": "late"})).await;
    assert!(!err && out.starts_with("late"), "{out}");
    h.session.end().await;
}

#[tokio::test]
async fn guards_refuse_calls_until_the_guard_runs() {
    let mut h = harness(
        |home| {
            add_plugin(home, "broken", &["--fail-init"], "guards = [\"*guarded*\"]");
            add_plugin(home, "tools", &[], "on_error = \"allow\"");
        },
        "",
    )
    .await;
    assert!(matches!(
        h.status("broken"),
        Some(SourceStatus::Failed { .. })
    ));
    let (out, err) = h
        .call("ext__tools__guarded_echo", json!({"text": "fine"}))
        .await;
    assert!(err && out.contains("`broken` checks this call"), "{out}");
    // Calls the guard doesn't cover still run.
    let (out, err) = h.call("ext__tools__echo", json!({"text": "ok"})).await;
    assert!(!err && out.starts_with("ok"), "{out}");
    h.session.end().await;
}

#[tokio::test]
async fn a_start_that_fails_while_the_session_ends_leaves_nothing_running() {
    let mut h = harness(
        |home| {
            add_plugin(
                home,
                "bad",
                &["--bad-protocol", "200"],
                "startup_timeout_secs = 1",
            );
        },
        "",
    )
    .await;
    let child: i32 = std::fs::read_to_string(h.home.join("plugins/bad/child_pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // The wrong answer came at 0.2 s, so the start is failing: stopping the group,
    // which waits its grace for the child that ignores SIGTERM. The session ends now and
    // aborts the start mid-stop.
    assert_eq!(h.status("bad"), Some(SourceStatus::Starting));
    h.session.end().await;
    assert!(
        gone(child).await,
        "the plugin's group survived an aborted start"
    );
}
