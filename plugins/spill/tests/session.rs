//! Inline and fetch through real sessions: a long result is spilled, and the model reads
//! the rest back with the file tools, through the real permission check, also after the
//! session is resumed in a fresh registry and session, as `--continue` builds them.

use std::path::Path;
use std::sync::Arc;

use ch_kernel::reexports::async_trait;
use ch_kernel::{
    Config, ContentBlock, Cx, Plugin, PluginManifest, RegisterCx, RegistryBuilder, Role, Session,
    SessionStore, TerminationReason, Tool, ToolOutput, ToolSpec,
};
use ch_provider_scripted::{ScriptStep, ScriptedPlugin};
use serde_json::{Value, json};

/// `big`: a 60k-character numbered log.
struct Big;

#[async_trait]
impl Tool for Big {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "big".into(),
            description: "When: tests. Protocol: -. Error modes: -. Side effects: none.".into(),
            input_schema: json!({"type": "object", "properties": {}}),
            read_only: true,
        }
    }

    async fn call(&self, _input: Value, _cx: &Cx) -> ToolOutput {
        ToolOutput::ok(
            (1..=5000)
                .map(|i| format!("log line {i:05}\n"))
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

fn results(session: &Session) -> Vec<String> {
    session
        .history()
        .messages()
        .iter()
        .filter(|m| m.role == Role::User)
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect()
}

/// A session over `steps` in `ws`, saved under `sessions`, like one `ch` process.
fn session(steps: Vec<ScriptStep>, ws: &Path, sessions: &Path, spill: bool) -> Session {
    let mut plugins: Vec<Arc<dyn Plugin>> = vec![
        Arc::new(ScriptedPlugin::from_steps(steps)),
        Arc::new(BigPlugin),
        ch_safety::plugin(),
        ch_tools_fs::plugin(),
    ];
    if spill {
        plugins.push(ch_spill::plugin());
    }
    let config = Arc::new(Config::parse("plugins.enable = [\"provider-scripted\"]").unwrap());
    let registry = RegistryBuilder::build(plugins, &config, ws).unwrap();
    Session::builder(Arc::new(registry), config)
        .workspace(ws)
        .store(SessionStore::new(sessions))
        .build()
        .unwrap()
}

/// The path and the first omitted line named in a spill notice.
fn notice_target(preview: &str) -> (String, usize) {
    let path = preview.split("saved at ").nth(1).unwrap();
    let path = path.split(".\n").next().unwrap().to_string();
    let offset = preview.split("offset=").nth(1).unwrap();
    let offset = offset.split(|c: char| !c.is_ascii_digit()).next().unwrap();
    (path, offset.parse().unwrap())
}

#[tokio::test]
async fn a_spilled_result_can_be_read_back_also_after_resuming() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let sessions = tmp.path().join("sessions");

    // First process: the long result is spilled.
    let steps = vec![
        ScriptStep::tool("big", json!({})),
        ScriptStep::text("long log"),
    ];
    let mut first = session(steps, &ws, &sessions, true);
    assert_eq!(
        first.run_turn("show the log").await.reason,
        TerminationReason::Done
    );
    let preview = results(&first)[0].clone();
    assert!(
        preview.chars().count() <= 24_000,
        "{}",
        preview.chars().count()
    );
    assert!(preview.starts_with("log line 00001\n"));
    assert!(preview.ends_with("log line 05000\n"));
    assert!(
        !preview.contains("log line 02500"),
        "the middle is in the file only"
    );
    let (path, offset) = notice_target(&preview);
    assert!(preview.contains(&format!("log line {:05}\n\n[… lines {offset}–", offset - 1)));
    let id = first.id().to_string();
    first.end().await;
    drop(first);

    // Second process: resume and follow the notice, as the model would.
    let steps = vec![
        ScriptStep::tool(
            "read_file",
            json!({"path": path, "offset": offset, "limit": 2}),
        ),
        ScriptStep::tool("grep_text", json!({"query": "line 02500", "path": path})),
        ScriptStep::text("found it"),
    ];
    let mut resumed = session(steps, &ws, &sessions, true);
    resumed
        .restore(SessionStore::new(&sessions).load(&id).unwrap())
        .unwrap();
    assert_eq!(
        resumed.run_turn("go on").await.reason,
        TerminationReason::Done
    );
    let results = results(&resumed);
    let read = &results[1];
    assert!(read.contains(&format!("log line {offset:05}\n")), "{read}");
    assert!(results[2].contains("log line 02500"), "{}", results[2]);
}

#[tokio::test]
async fn without_the_plugin_the_middle_is_cut_and_the_session_dir_holds_no_output() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let sessions = tmp.path().join("sessions");
    let steps = vec![
        ScriptStep::tool("big", json!({})),
        ScriptStep::text("long log"),
    ];
    let mut s = session(steps, &ws, &sessions, false);
    s.run_turn("show the log").await;
    let preview = &results(&s)[0];
    assert!(!preview.contains("saved at"));
    assert!(!preview.contains("log line 02500"));
    assert!(!sessions.join(s.id()).join("spill").exists());
}
