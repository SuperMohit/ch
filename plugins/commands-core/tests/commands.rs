//! Runs the core commands against a real `Session` with a tiny fake provider.

use std::path::Path;
use std::sync::Arc;

use ch_kernel::reexports::CancellationToken;
use ch_kernel::*;
use futures::stream;
use serde_json::{Value, json};

/// Always answers "ok".
struct FakeProvider;

#[async_trait::async_trait]
impl ModelProvider for FakeProvider {
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
                input_tokens: 1200,
                output_tokens: 34,
                cache_read_tokens: 5,
                cache_write_tokens: 0,
            },
        };
        Ok(Box::pin(stream::iter(vec![Ok(ModelEvent::Completed(
            resp,
        ))])))
    }
    fn context_window(&self, _model: &str) -> Option<u64> {
        Some(200_000)
    }
}

struct ReadTool;

#[async_trait::async_trait]
impl Tool for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "peek".into(),
            description: "\nLooks at things.\nWhen: always".into(),
            input_schema: json!({"type": "object"}),
            read_only: true,
        }
    }
    async fn call(&self, _input: Value, _cx: &Cx) -> ToolOutput {
        ToolOutput::ok("")
    }
}

struct FnPlugin {
    manifest: PluginManifest,
    #[allow(clippy::type_complexity)]
    register: Box<dyn Fn(&mut RegisterCx<'_>) -> anyhow::Result<()> + Send + Sync>,
}

impl Plugin for FnPlugin {
    fn manifest(&self) -> PluginManifest {
        self.manifest.clone()
    }
    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        (self.register)(cx)
    }
}

fn fn_plugin(
    manifest: PluginManifest,
    f: impl Fn(&mut RegisterCx<'_>) -> anyhow::Result<()> + Send + Sync + 'static,
) -> Arc<dyn Plugin> {
    Arc::new(FnPlugin {
        manifest,
        register: Box::new(f),
    })
}

const CONFIG: &str = r#"
[model]
provider = "fake"
id = "fake-model"

[plugins.provider-fake]
api_key = "sk-very-secret"
base_url = "https://example.invalid"
"#;

fn session(cfg: &str, store: Option<SessionStore>) -> Session {
    let config = Config::parse(cfg).unwrap();
    let plugins = vec![
        fn_plugin(
            PluginManifest::new("provider-fake", "fake provider"),
            |cx| {
                cx.add_provider(FakeProvider);
                Ok(())
            },
        ),
        fn_plugin(PluginManifest::new("tools-fake", "fake tools"), |cx| {
            cx.add_tool(ReadTool);
            Ok(())
        }),
        fn_plugin(
            PluginManifest::new("telemetry-fake", "needs credentials").default_enabled(false),
            |_| Ok(()),
        ),
        ch_commands_core::plugin(),
    ];
    let registry = Arc::new(RegistryBuilder::build(plugins, &config, Path::new("/tmp")).unwrap());
    let mut b = Session::builder(registry, Arc::new(config)).workspace("/tmp");
    if let Some(s) = store {
        b = b.store(s);
    }
    b.build().unwrap()
}

async fn message(s: &mut Session, line: &str) -> String {
    match s.run_command(line).await {
        Some(Ok(Outcome::Message(m))) => m,
        other => panic!("{line}: expected a message, got {other:?}"),
    }
}

#[tokio::test]
async fn help_lists_every_command() {
    let mut s = session(CONFIG, None);
    let out = message(&mut s, "/help").await;
    for name in ch_commands_core::COMMAND_NAMES {
        assert!(
            out.contains(&format!("`/{name}")),
            "missing /{name} in:\n{out}"
        );
    }
    assert!(out.contains("`/model [model-id]`"));
    assert!(out.contains("`/resume [session-id]`"));
}

#[tokio::test]
async fn exit_and_quit() {
    let mut s = session(CONFIG, None);
    assert!(matches!(
        s.run_command("/exit").await,
        Some(Ok(Outcome::Exit))
    ));
    assert!(matches!(
        s.run_command("/quit").await,
        Some(Ok(Outcome::Exit))
    ));
}

#[tokio::test]
async fn clear_starts_a_new_session_but_keeps_usage() {
    let mut s = session(CONFIG, None);
    s.run_turn("hello").await;
    assert!(!s.history().is_empty());
    let old_id = s.id().to_string();
    let out = message(&mut s, "/clear").await;
    assert!(out.contains("new conversation"), "{out}");
    assert!(s.history().is_empty());
    assert_ne!(s.id(), old_id);
    assert_eq!(s.usage().input_tokens, 1200);
}

#[tokio::test]
async fn compact_reports_strategy_result() {
    let mut s = session(CONFIG, None);
    assert!(
        message(&mut s, "/compact")
            .await
            .contains("Nothing to compact")
    );
    s.run_turn("hello").await;
    let out = message(&mut s, "/compact").await;
    assert!(out.contains("does not support manual compaction"), "{out}");
}

#[tokio::test]
async fn model_show_and_set() {
    let mut s = session(CONFIG, None);
    let out = message(&mut s, "/model").await;
    assert!(
        out.contains("`fake-model`") && out.contains("`fake`"),
        "{out}"
    );
    assert!(out.contains("200,000"), "{out}");
    let out = message(&mut s, "/model claude-sonnet-5-5").await;
    assert!(out.contains("`claude-sonnet-5-5`"), "{out}");
    assert_eq!(s.model(), "claude-sonnet-5-5");
    let out = message(&mut s, "/model a b").await;
    assert!(out.contains("Usage"), "{out}");
    assert_eq!(s.model(), "claude-sonnet-5-5");
}

#[tokio::test]
async fn cost_without_and_with_pricing() {
    let mut s = session(CONFIG, None);
    s.run_turn("hello").await;
    let out = message(&mut s, "/cost").await;
    assert!(out.contains("Input: 1,200"), "{out}");
    assert!(out.contains("Output: 34"), "{out}");
    assert!(out.contains("Cache read: 5"), "{out}");
    assert!(out.contains("not tracked"), "{out}");

    let priced = format!("{CONFIG}\n[model.pricing]\ninput = 3.0\noutput = 15.0\n");
    let mut s = session(&priced, None);
    s.run_turn("hello").await;
    let out = message(&mut s, "/cost").await;
    assert!(out.contains("**Cost:** $"), "{out}");
    assert!(!out.contains("not tracked"), "{out}");
}

#[tokio::test]
async fn tools_lists_names_flags_and_first_lines() {
    let mut s = session(CONFIG, None);
    let out = message(&mut s, "/tools").await;
    assert!(
        out.contains("- `peek` (read-only): Looks at things."),
        "{out}"
    );
    assert!(!out.contains("When: always"), "{out}");
}

#[tokio::test]
async fn plugins_shows_status_and_reason() {
    let mut s = session(CONFIG, None);
    let out = message(&mut s, "/plugins").await;
    assert!(out.contains("3 of 4 enabled"), "{out}");
    assert!(out.contains("`commands-core`"), "{out}");
    assert!(
        out.contains("**enabled**: fake tools · _enabled by default_"),
        "{out}"
    );
    assert!(
        out.contains("`telemetry-fake` 0.1.0 **disabled**: needs credentials · _off by default"),
        "{out}"
    );
}

#[tokio::test]
async fn config_masks_secrets() {
    let mut s = session(CONFIG, None);
    let out = message(&mut s, "/config").await;
    assert!(!out.contains("sk-very-secret"), "{out}");
    assert!(out.contains("api_key = \"********\""), "{out}");
    assert!(out.contains("https://example.invalid"), "{out}");
    assert!(out.contains("```toml"), "{out}");
}

#[tokio::test]
async fn resume_lists_and_loads_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());

    let mut s = session(CONFIG, Some(store.clone()));
    assert!(
        message(&mut s, "/resume")
            .await
            .contains("No saved sessions")
    );
    s.run_turn("first task").await;
    s.save().unwrap();
    let old_id = s.id().to_string();
    // Closed, as when that ch exits; an open session can't be resumed elsewhere.
    drop(s);

    let mut s2 = session(CONFIG, Some(store));
    s2.run_turn("second task").await;
    s2.save().unwrap();

    let out = message(&mut s2, "/resume").await;
    assert!(out.contains(&format!("`{old_id}`: first task")), "{out}");
    assert!(
        out.contains("second task") && out.contains("(current)"),
        "{out}"
    );

    let out = message(&mut s2, "/resume nope-nope").await;
    assert!(out.contains("No saved session matches"), "{out}");

    let out = message(&mut s2, &format!("/resume {old_id}")).await;
    assert!(out.contains("Resumed session"), "{out}");
    assert_eq!(s2.id(), old_id);
    assert_eq!(
        s2.history().first_user_text().as_deref(),
        Some("first task")
    );
}

#[tokio::test]
async fn resume_without_store() {
    let mut s = session(CONFIG, None);
    assert!(
        message(&mut s, "/resume")
            .await
            .contains("aren't being saved")
    );
}
