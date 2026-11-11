//! Diagnostics through a real session: the reminder reaches the next request without
//! entering the history, and the turn-end note continues the turn once.

use std::sync::Arc;

use ch_kernel::reexports::async_trait;
use ch_kernel::{
    Config, ContentBlock, Cx, Message, Plugin, PluginManifest, RegisterCx, RegistryBuilder,
    Session, TerminationReason, Tool, ToolOutput, ToolSpec,
};
use ch_provider_scripted::{ScriptStep, ScriptedPlugin, ScriptedProvider};
use serde_json::{Value, json};

/// A `bash` the model never calls: diagnostics only needs it to be there.
struct Bash;

#[async_trait]
impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "When: tests. Protocol: -. Error modes: none. Side effects: none.".into(),
            input_schema: json!({"type": "object", "properties": {"command": {"type": "string"}}}),
            read_only: false,
        }
    }

    async fn call(&self, _input: Value, _cx: &Cx) -> ToolOutput {
        ToolOutput::error("not in this test")
    }
}

struct BashPlugin;

impl Plugin for BashPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new("fake-bash", "test tool")
    }
    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        cx.add_tool(Bash);
        Ok(())
    }
}

/// All the text a message carries, tool results included.
fn text(messages: &[Message]) -> String {
    messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn the_reminder_is_sent_once_and_the_turn_end_note_continues_the_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let provider = ScriptedProvider::new(vec![
        ScriptStep::tool(
            "write_file",
            json!({"path": "notes.txt", "content": "ok\nBAD one\n"}),
        ),
        ScriptStep::text("All done."),
        ScriptStep::text("I left the BAD line on purpose."),
    ]);
    let plugins: Vec<Arc<dyn Plugin>> = vec![
        Arc::new(ScriptedPlugin::from_provider(provider.clone())),
        ch_safety::plugin(),
        ch_tools_fs::plugin(),
        ch_executor_local::plugin(),
        Arc::new(BashPlugin),
        ch_diagnostics::plugin(),
    ];
    let config = Arc::new(
        Config::parse(
            r#"
            plugins.enable = ["provider-scripted"]
            [plugins.safety]
            mode = "bypass"
            [plugins.diagnostics.checkers.txt]
            command = "awk -v p={path} '/BAD/ { print p \":\" NR \":1: \" $0; bad = 1 } END { exit bad }'"
            "#,
        )
        .unwrap(),
    );
    let registry = RegistryBuilder::build(plugins, &config, ws).unwrap();
    let mut session = Session::builder(Arc::new(registry), config)
        .workspace(ws)
        .build()
        .unwrap();
    let result = session.run_turn("write the notes").await;
    assert_eq!(result.reason, TerminationReason::Done);

    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    let reminder = "Your last edits introduced these problems:\nnotes.txt:2:1: BAD one";
    let second = text(&requests[1].messages);
    assert!(second.contains(reminder), "{second}");
    let third = text(&requests[2].messages);
    assert!(
        !third.contains(reminder),
        "the reminder is for one request: {third}"
    );
    assert!(third.contains("Before you finish:"), "{third}");
    assert!(third.contains("notes.txt:2:1: BAD one"), "{third}");

    let history = text(&session.history().messages());
    assert!(!history.contains("Your last edits"), "{history}");
    assert_eq!(
        history.matches("Before you finish:").count(),
        1,
        "{history}"
    );
}
