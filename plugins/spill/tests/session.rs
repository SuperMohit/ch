//! Inline and fetch through a real session: a long result is spilled, and the model
//! reads the rest back with the file tools, through the real permission check.

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

async fn run(spill: bool) -> (Session, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let sessions = tmp.path().join("sessions");
    // The first call has id `call_0_0`, so its spill file name is known in advance.
    let file = sessions.join("s1/spill/call_0_0-big.txt");
    let file = file.to_str().unwrap();
    let steps = vec![
        ScriptStep::tool("big", json!({})),
        ScriptStep::tool(
            "read_file",
            json!({"path": file, "offset": 2500, "limit": 2}),
        ),
        ScriptStep::tool("grep_text", json!({"query": "line 03333", "path": file})),
        ScriptStep::text("found it"),
    ];
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
    let registry = RegistryBuilder::build(plugins, &config, &ws).unwrap();
    let mut session = Session::builder(Arc::new(registry), config)
        .workspace(&ws)
        .id("s1")
        .store(SessionStore::new(&sessions))
        .build()
        .unwrap();
    assert_eq!(
        session.run_turn("find line 3333").await.reason,
        TerminationReason::Done
    );
    (session, tmp)
}

#[tokio::test]
async fn a_spilled_result_can_be_read_back_with_the_file_tools() {
    let (session, _tmp) = run(true).await;
    let results = results(&session);
    assert_eq!(results.len(), 3);

    let preview = &results[0];
    assert!(
        preview.chars().count() <= 24_000,
        "{}",
        preview.chars().count()
    );
    assert!(preview.starts_with("log line 00001\n"));
    assert!(preview.ends_with("log line 05000\n"));
    assert!(preview.contains("characters omitted. The full output is saved at"));
    assert!(
        !preview.contains("log line 02500"),
        "the middle is in the file only"
    );

    assert!(results[1].contains("log line 02500\n"), "{}", results[1]);
    assert!(results[1].contains("log line 02501"), "{}", results[1]);
    assert!(results[2].contains("log line 03333"), "{}", results[2]);
}

#[tokio::test]
async fn without_the_plugin_the_middle_is_cut_and_the_path_is_refused() {
    let (session, _tmp) = run(false).await;
    let results = results(&session);
    assert!(!results[0].contains("saved at"));
    assert!(!results[0].contains("log line 02500"));
    assert!(
        results[1].contains("outside the workspace"),
        "{}",
        results[1]
    );
}
