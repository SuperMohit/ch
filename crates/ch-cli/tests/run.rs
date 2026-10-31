//! Whole runs of `ch -p` with a scripted provider, and `--list-plugins` output.

use std::sync::Arc;

use ch_cli::Cli;
use ch_cli::list::format_plugin_list;
use ch_kernel::reexports::CancellationToken;
use ch_kernel::*;
use clap::Parser;
use serde_json::{Value, json};

/// Replies "ok" to every request.
struct Ok200;

#[async_trait::async_trait]
impl ModelProvider for Ok200 {
    fn id(&self) -> &str {
        "fake"
    }
    async fn stream(
        &self,
        _request: ModelRequest,
        _cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let resp = ModelResponse {
            message: Message::assistant("ok"),
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 1,
                ..Default::default()
            },
        };
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(ModelEvent::TextDelta("ok".into())),
            Ok(ModelEvent::Completed(resp)),
        ])))
    }
}

struct Noop;

#[async_trait::async_trait]
impl Tool for Noop {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "noop".into(),
            description: "does nothing".into(),
            input_schema: json!({"type": "object"}),
            read_only: true,
        }
    }
    async fn call(&self, _input: Value, _cx: &Cx) -> ToolOutput {
        ToolOutput::ok("")
    }
}

struct Watch;

#[async_trait::async_trait]
impl Hook for Watch {
    fn id(&self) -> &str {
        "watch"
    }
    fn points(&self) -> &[HookPoint] {
        &[HookPoint::PreTool, HookPoint::PostTool]
    }
    async fn on(&self, _event: &HookEvent, _cx: &Cx) -> HookDecision {
        HookDecision::Continue
    }
}

struct Hello;

#[async_trait::async_trait]
impl Command for Hello {
    fn name(&self) -> &str {
        "hello"
    }
    fn description(&self) -> &str {
        "say hello"
    }
    async fn run(&self, _args: &str, _cx: &mut CmdCx<'_>) -> anyhow::Result<Outcome> {
        Ok(Outcome::Message("hello".into()))
    }
}

struct Fake;

impl Plugin for Fake {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new("fake", "A fake provider for tests").requires(&["frontend-print"])
    }
    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        cx.add_provider(Ok200);
        cx.add_tool(Noop);
        cx.add_hook(Watch);
        cx.add_command(Hello);
        Ok(())
    }
}

struct Off;

impl Plugin for Off {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new("off", "Off unless enabled").default_enabled(false)
    }
    fn register(&self, _cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        Ok(())
    }
}

fn plugins() -> Vec<Arc<dyn Plugin>> {
    let mut p = ch_cli::builtin_plugins();
    p.push(Arc::new(Fake));
    p.push(Arc::new(Off));
    p
}

/// `ch_cli::run_with` hands the print frontend its task through process-wide options, so
/// runs in this file must not overlap.
static RUN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn cli(args: &[&str]) -> Cli {
    Cli::try_parse_from(std::iter::once("ch").chain(args.iter().copied())).unwrap()
}

#[tokio::test]
async fn print_runs_save_and_continue_sessions() {
    let _guard = RUN_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let ws_arg = ws.to_str().unwrap();
    let common = [
        "--cwd",
        ws_arg,
        "--set",
        "model.provider=fake",
        "--model",
        "fake-model",
        "--output-format",
        "json",
    ];

    let mut args = vec!["-p", "first task"];
    args.extend(common);
    let code = ch_cli::run_with(cli(&args), plugins(), home.clone(), ch_cli::Stdin::None)
        .await
        .unwrap();
    assert_eq!(code, 0);

    let workspace = ws.canonicalize().unwrap();
    let store = SessionStore::for_workspace(&home.join("sessions"), &workspace);
    let sessions = store.list().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].turns, 1);
    assert_eq!(sessions[0].title, "first task");

    let mut args = vec!["-p", "second task", "-c"];
    args.extend(common);
    let code = ch_cli::run_with(cli(&args), plugins(), home.clone(), ch_cli::Stdin::None)
        .await
        .unwrap();
    assert_eq!(code, 0);
    let sessions = store.list().unwrap();
    assert_eq!(sessions.len(), 1, "--continue must reuse the session");
    assert_eq!(sessions[0].turns, 2);

    // --resume with an unknown id fails with a helpful message.
    let mut args = vec!["-p", "x", "-r", "nope"];
    args.extend(common);
    let e = ch_cli::run_with(cli(&args), plugins(), home.clone(), ch_cli::Stdin::None)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("no session `nope`"), "{e:#}");

    // An id that isn't one can't name a file outside the sessions directory.
    let mut args = vec!["-p", "x", "-r", "../../elsewhere/s"];
    args.extend(common);
    let e = ch_cli::run_with(cli(&args), plugins(), home.clone(), ch_cli::Stdin::None)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("is not a session id"), "{e:#}");
}

#[tokio::test]
async fn unknown_frontend_or_provider_is_reported() {
    let _guard = RUN_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().to_str().unwrap().to_string();
    let home = dir.path().join("home");
    let e = ch_cli::run_with(
        cli(&["-p", "x", "--cwd", &ws, "--set", "model.provider=nope"]),
        plugins(),
        home.clone(),
        ch_cli::Stdin::None,
    )
    .await
    .unwrap_err();
    assert!(
        format!("{e:#}").contains("no model provider `nope`"),
        "{e:#}"
    );

    let e = ch_cli::run_with(
        cli(&["-p", "x", "--cwd", &ws, "--set", "model.provider=fake"]),
        plugins(),
        home.clone(),
        ch_cli::Stdin::None,
    )
    .await
    .unwrap_err();
    assert!(
        format!("{e:#}").contains("`fake` has no default model; set model.id"),
        "{e:#}"
    );

    let e = ch_cli::run_with(
        cli(&["--cwd", &ws, "--set", "plugins.disable=[\"frontend-repl\"]"]),
        plugins(),
        home,
        ch_cli::Stdin::None,
    )
    .await
    .unwrap_err();
    assert!(
        format!("{e:#}").contains("no frontend named `repl`"),
        "{e:#}"
    );
}

#[test]
fn plugin_list_shows_status_and_registrations() {
    let cfg = Config::parse("[plugins]\ndisable = [\"frontend-repl\"]\n").unwrap();
    let registry = RegistryBuilder::build(plugins(), &cfg, std::path::Path::new("/tmp")).unwrap();
    let out = format_plugin_list(&registry);
    assert!(
        out.contains("frontend-repl 0.1.0  disabled (disabled in config)"),
        "{out}"
    );
    assert!(
        out.contains("frontend-print 0.1.0  enabled (enabled by default)"),
        "{out}"
    );
    assert!(out.contains("    frontends: print\n"), "{out}");
    assert!(
        out.contains("    approvers: auto-deny, auto-allow\n"),
        "{out}"
    );
    assert!(out.contains("fake 0.1.0  enabled"), "{out}");
    assert!(out.contains("    requires: frontend-print\n"), "{out}");
    assert!(out.contains("    providers: fake\n"), "{out}");
    assert!(out.contains("    tools: noop\n"), "{out}");
    assert!(
        out.contains("    hooks: watch [pre_tool post_tool]\n"),
        "{out}"
    );
    assert!(out.contains("    commands: /hello\n"), "{out}");
    assert!(
        out.contains("off 0.1.0  disabled (off by default (add to plugins.enable))"),
        "{out}"
    );
}
