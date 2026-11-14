//! The example plugin through ch's external host: it starts, its tool passes the
//! outside-tool conformance profile (roadmap 7.7), and it finds TODOs.

use std::sync::{Arc, Mutex};

use ch_kernel::reexports::CancellationToken;
use ch_kernel::*;
use serde_json::json;

#[derive(Clone, Default)]
struct Model(
    Arc<Mutex<Vec<ModelResponse>>>,
    Arc<Mutex<Vec<ModelRequest>>>,
);

#[async_trait::async_trait]
impl ModelProvider for Model {
    fn id(&self) -> &str {
        "model"
    }
    async fn stream(
        &self,
        r: ModelRequest,
        _c: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        self.1.lock().unwrap().push(r);
        let mut replies = self.0.lock().unwrap();
        let reply = if replies.is_empty() {
            ModelResponse {
                message: Message::assistant("done"),
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            }
        } else {
            replies.remove(0)
        };
        Ok(Box::pin(futures::stream::iter(vec![Ok(
            ModelEvent::Completed(reply),
        )])))
    }
    fn context_window(&self, _m: &str) -> Option<u64> {
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

#[tokio::test]
async fn the_example_conforms_and_scans() {
    let dir = tempfile::tempdir().unwrap();
    let (home, ws) = (dir.path().join("home"), dir.path().join("ws"));
    let plugin_dir = home.join("plugins/todo-scanner");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::create_dir_all(ws.join("target")).unwrap();
    std::fs::write(ws.join("src/a.rs"), "fn a() {}\n// TODO: handle errors\n").unwrap();
    std::fs::write(ws.join("target/b.rs"), "// TODO: build output\n").unwrap();
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/plugin.toml"))
        .unwrap()
        .replace(
            "command = \"todo-scanner\"",
            &format!("command = {:?}", env!("CARGO_BIN_EXE_todo-scanner")),
        );
    std::fs::write(plugin_dir.join("plugin.toml"), manifest).unwrap();

    let model = Model::default();
    model.0.lock().unwrap().push(ModelResponse {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "1".into(),
                name: "ext__todo-scanner__scan".into(),
                input: json!({}),
            }],
        },
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
    });
    let mut services = Services::default();
    services.provide(Arc::new(ChHome(home)));
    let config = Config::parse("[model]\nid = \"m\"").unwrap();
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
    session.run_turn("find todos").await;

    let requests = model.1.lock().unwrap().clone();
    let tools = &requests[0].tools;
    assert_eq!(tools.len(), 1);
    ch_testkit::assert_outside_spec_conforms(&tools[0], false);
    assert!(
        requests[0]
            .system
            .iter()
            .any(|b| b.contains("ext__todo-scanner__scan"))
    );
    let result = session
        .history()
        .messages()
        .iter()
        .flat_map(|m| m.content.clone())
        .find_map(|b| match b {
            ContentBlock::ToolResult { content, .. } => Some(content),
            _ => None,
        })
        .unwrap();
    assert_eq!(result, "src/a.rs:2: // TODO: handle errors");

    match session.run_command("/todos").await {
        Some(Ok(Outcome::Prompt(p))) => assert!(p.contains("scan tool"), "{p}"),
        other => panic!("{other:?}"),
    }
    session.end().await;
}
