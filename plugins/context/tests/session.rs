//! The plugin in real sessions with the scripted provider, whose window is small enough
//! to overflow: trimming keeps a long run going, stubs are read back with `read_file`
//! after a resume, and a copied marker is refused.

use std::path::Path;
use std::sync::Arc;

use ch_kernel::reexports::async_trait;
use ch_kernel::{
    AgentEvent, Config, ContentBlock, Cx, ModelRequest, Plugin, PluginManifest, RegisterCx,
    RegistryBuilder, Role, Session, SessionStore, TerminationReason, Tool, ToolOutput, ToolSpec,
};
use ch_provider_scripted::{ScriptStep, ScriptedPlugin, ScriptedProvider};
use serde_json::{Value, json};

/// The scripted provider rejects requests over this many tokens.
const WINDOW: u64 = 20_000;

/// `big`: a 3,000-character log, different for each `n`.
struct Big;

#[async_trait]
impl Tool for Big {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "big".into(),
            description: "When: tests. Protocol: -. Error modes: -. Side effects: none.".into(),
            input_schema: json!({"type": "object", "properties": {"n": {"type": "integer"}}}),
            read_only: true,
        }
    }

    async fn call(&self, input: Value, _cx: &Cx) -> ToolOutput {
        let n = input["n"].as_u64().unwrap_or(0);
        ToolOutput::ok(
            (1..=100)
                .map(|i| format!("big {n:02} line {i:03} of the log\n"))
                .collect::<String>(),
        )
    }
}

struct BigPlugin;

impl Plugin for BigPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new("big", "test tool")
    }
    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        cx.add_tool(Big);
        Ok(())
    }
}

/// `count` calls to `big`, then a reply.
fn calls(count: u64) -> Vec<ScriptStep> {
    let mut steps: Vec<ScriptStep> = (1..=count)
        .map(|n| ScriptStep::tool("big", json!({ "n": n })))
        .collect();
    steps.push(ScriptStep::text("done"));
    steps
}

/// A session over `provider` in `ws`, saved under `sessions` as `s1`, like one `ch`
/// process.
fn session(provider: ScriptedProvider, ws: &Path, sessions: &Path, toml: &str) -> Session {
    session_as("s1", provider, ws, sessions, toml)
}

fn session_as(
    id: &str,
    provider: ScriptedProvider,
    ws: &Path,
    sessions: &Path,
    toml: &str,
) -> Session {
    let plugins: Vec<Arc<dyn Plugin>> = vec![
        Arc::new(ScriptedPlugin::from_provider(provider)),
        Arc::new(BigPlugin),
        ch_safety::plugin(),
        ch_tools_fs::plugin(),
        ch_context::plugin(),
    ];
    let config = Arc::new(
        Config::parse(&format!(
            "plugins.enable = [\"provider-scripted\"]\n[model]\nmax_output_tokens = 2000\n{toml}"
        ))
        .unwrap(),
    );
    let registry = RegistryBuilder::build(plugins, &config, ws).unwrap();
    Session::builder(Arc::new(registry), config)
        .workspace(ws)
        .id(id)
        .store(SessionStore::new(sessions))
        .build()
        .unwrap()
}

struct Dirs {
    _tmp: tempfile::TempDir,
    ws: std::path::PathBuf,
    sessions: std::path::PathBuf,
}

fn dirs() -> Dirs {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let sessions = tmp.path().join("sessions");
    Dirs {
        ws,
        sessions,
        _tmp: tmp,
    }
}

fn compactions(events: &[AgentEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ContextCompacted { .. }))
        .count()
}

/// Requests that don't start with the whole request before them.
fn rewrites(requests: &[ModelRequest]) -> usize {
    requests
        .windows(2)
        .filter(|p| {
            let (a, b) = (&p[0].messages, &p[1].messages);
            b.len() < a.len() || b[..a.len()] != a[..]
        })
        .count()
}

fn received(rx: &mut tokio::sync::broadcast::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

#[tokio::test]
async fn a_long_run_fits_under_elide_and_overflows_under_none() {
    let d = dirs();
    let provider = ScriptedProvider::new(calls(60)).with_context_window(WINDOW);
    let mut s = session(provider.clone(), &d.ws, &d.sessions, "");
    let mut rx = s.events().subscribe();
    let r = s.run_turn("read the logs").await;
    assert_eq!(r.reason, TerminationReason::Done, "{r:?}");
    assert_eq!(r.steps, 61);
    // Each request repeats the one before it, except right after a trim, which comes
    // every few steps here because each result is a big share of the window.
    let trims = compactions(&received(&mut rx));
    assert!((1..=20).contains(&trims), "{trims}");
    assert_eq!(rewrites(&provider.requests()), trims);

    let d = dirs();
    let provider = ScriptedProvider::new(calls(60)).with_context_window(WINDOW);
    let mut s = session(
        provider,
        &d.ws,
        &d.sessions,
        "[plugins.context]\nstrategy = \"none\"",
    );
    let r = s.run_turn("read the logs").await;
    assert!(
        matches!(r.reason, TerminationReason::ContextOverflow { .. }),
        "{r:?}"
    );
}

#[tokio::test]
async fn an_overflow_is_trimmed_and_the_step_sent_again() {
    let d = dirs();
    let provider = ScriptedProvider::new(calls(25)).with_context_window(WINDOW);
    // The session thinks the window is far bigger, so only the provider's rejection
    // shows that it's full.
    let mut s = session(
        provider.clone(),
        &d.ws,
        &d.sessions,
        "context_window = 1000000",
    );
    let r = s.run_turn("read the logs").await;
    assert_eq!(r.reason, TerminationReason::Done, "{r:?}");
    let log = std::fs::read_to_string(d.sessions.join("s1/context/log.jsonl")).unwrap();
    assert!(log.contains("\"trigger\":\"overflow\""), "{log}");
    assert!(provider.requests().len() > 26, "some steps were sent twice");
}

/// The stubbed result in `request` whose stub names a saved copy: its call id and path.
fn a_stub(request: &ModelRequest) -> (String, String) {
    request
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } if content.starts_with("[Output elided") => {
                let path = content.split("saved at ").nth(1)?.split(';').next()?;
                Some((tool_use_id.clone(), path.to_string()))
            }
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn a_resumed_session_renders_the_same_view_and_reads_a_stub_back() {
    let d = dirs();
    let provider = ScriptedProvider::new(calls(25)).with_context_window(WINDOW);
    let mut first = session(provider.clone(), &d.ws, &d.sessions, "");
    assert_eq!(
        first.run_turn("read the logs").await.reason,
        TerminationReason::Done
    );
    let last = provider.requests().pop().unwrap();
    let (call, path) = a_stub(&last);
    first.end().await;
    drop(first);

    // Another process, with settings under which it wouldn't have trimmed: the earlier
    // trims still render as they were sent.
    let steps = vec![
        ScriptStep::tool("read_file", json!({"path": path, "limit": 3})),
        ScriptStep::text("found it"),
    ];
    let provider = ScriptedProvider::new(steps).with_context_window(WINDOW);
    let mut resumed = session_as(
        "new",
        provider.clone(),
        &d.ws,
        &d.sessions,
        "[plugins.context]\nsoft = 0.9\nelide_min_chars = 20000",
    );
    resumed
        .restore(SessionStore::new(&d.sessions).load("s1").unwrap())
        .unwrap();
    assert_eq!(
        resumed.run_turn("show one").await.reason,
        TerminationReason::Done
    );
    let again = &provider.requests()[0];
    assert_eq!(again.messages[..last.messages.len()], last.messages[..]);

    // The stub's file holds the original result, and read_file can open it.
    let original = resumed
        .history()
        .messages()
        .iter()
        .flat_map(|m| m.content.clone())
        .find_map(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == call => Some(content),
            _ => None,
        })
        .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    let read = resumed
        .history()
        .messages()
        .iter()
        .rev()
        .filter(|m| m.role == Role::User)
        .flat_map(|m| m.content.clone())
        .find_map(|b| match b {
            ContentBlock::ToolResult { content, .. } => Some(content),
            _ => None,
        })
        .unwrap();
    let first_line = original.lines().next().unwrap();
    assert!(read.contains(first_line), "{read}");
}

#[tokio::test]
async fn a_call_that_copies_a_marker_is_refused() {
    let d = dirs();
    let marker = "[elided from an earlier call to save context: 5,000 characters]";
    let steps = vec![
        ScriptStep::tool("write_file", json!({"path": "a.txt", "content": marker})),
        ScriptStep::text("ok"),
    ];
    let mut s = session(ScriptedProvider::new(steps), &d.ws, &d.sessions, "");
    s.run_turn("write it").await;
    let result = &s.history().events()[2].message.content[0];
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = result
    else {
        panic!("{result:?}");
    };
    assert!(is_error);
    assert!(content.contains(ch_context::REFUSED), "{content}");
    assert!(!d.ws.join("a.txt").exists());
}
