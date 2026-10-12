//! Markdown commands registered next to the core commands in a real session.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use ch_kernel::reexports::CancellationToken;
use ch_kernel::*;
use futures::stream;

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
            usage: Usage::default(),
        };
        Ok(Box::pin(stream::iter(vec![Ok(ModelEvent::Completed(
            resp,
        ))])))
    }
}

struct ProviderPlugin;

impl Plugin for ProviderPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new("provider-fake", "fake provider")
    }
    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        cx.add_provider(FakeProvider);
        Ok(())
    }
}

fn write(dir: &Path, name: &str, text: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join(name), text).unwrap();
}

fn session(workspace: &Path, user_dir: &Path) -> Session {
    let cfg = Config::parse(&format!(
        "[model]\nprovider = \"fake\"\nid = \"m\"\n\n[plugins.commands-markdown]\nuser_dir = {:?}\n",
        user_dir.to_string_lossy()
    ))
    .unwrap();
    let plugins: Vec<Arc<dyn Plugin>> = vec![
        Arc::new(ProviderPlugin),
        ch_commands_core::plugin(),
        ch_commands_markdown::plugin(),
    ];
    let registry = Arc::new(RegistryBuilder::build(plugins, &cfg, workspace).unwrap());
    Session::builder(registry, Arc::new(cfg))
        .workspace(workspace)
        .build()
        .unwrap()
}

#[tokio::test]
async fn markdown_commands_run_as_prompts_beside_core_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    let user = tmp.path().join("user-commands");
    let project = ws.join(".ch/commands");
    write(
        &project,
        "fix.md",
        "---\ndescription: Fix an issue\nargument-hint: <issue>\n---\nFix this issue: $ARGUMENTS\nThen run the tests.",
    );
    write(&project, "help.md", "This must not replace /help");
    write(&project, "exit.md", "Nor this");
    write(&user, "fix.md", "User version of fix");
    write(
        &user,
        "standup.md",
        "# Summarise my work today\nUse git log.",
    );

    let mut s = session(&ws, &user);

    // The registry built even though help.md and exit.md exist.
    match s.run_command("/fix the flaky login test").await {
        Some(Ok(Outcome::Prompt(p))) => {
            assert_eq!(
                p,
                "Fix this issue: the flaky login test\nThen run the tests."
            )
        }
        other => panic!("unexpected {other:?}"),
    }
    match s.run_command("/standup").await {
        Some(Ok(Outcome::Prompt(p))) => assert!(p.starts_with("# Summarise my work today")),
        other => panic!("unexpected {other:?}"),
    }
    assert!(matches!(
        s.run_command("/exit").await,
        Some(Ok(Outcome::Exit))
    ));

    let help = match s.run_command("/help").await {
        Some(Ok(Outcome::Message(m))) => m,
        other => panic!("unexpected {other:?}"),
    };
    assert!(
        help.contains("`/fix <issue>`: Fix an issue _(commands-markdown)_"),
        "{help}"
    );
    assert!(
        help.contains("`/standup`: Summarise my work today"),
        "{help}"
    );
    assert!(!help.contains("must not replace"), "{help}");
}

#[tokio::test]
async fn edits_apply_without_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    let project = ws.join(".ch/commands");
    write(&project, "go.md", "Version one");
    let mut s = session(&ws, &tmp.path().join("none"));
    write(&project, "go.md", "Version two $ARGUMENTS");
    match s.run_command("/go now").await {
        Some(Ok(Outcome::Prompt(p))) => assert_eq!(p, "Version two now"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn reserved_names_match_the_core_commands() {
    let mut reserved = ch_commands_markdown::RESERVED.to_vec();
    let mut core = ch_commands_core::COMMAND_NAMES.to_vec();
    reserved.sort_unstable();
    core.sort_unstable();
    assert_eq!(reserved, core);
}
