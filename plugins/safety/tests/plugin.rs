//! The safety plugin registered through the kernel registry.

use ch_kernel::{HookEvent, ToolCall, Verdict, run_hooks};
use ch_safety::{FileState, PathGuard};
use ch_testkit::{TestCx, build_registry};
use serde_json::{Value, json};

async fn verdict(config: &str, tool: &str, input: Value, read_only: bool) -> Verdict {
    let dir = tempfile::tempdir().unwrap();
    let registry = build_registry(vec![ch_safety::plugin()], dir.path(), config);
    let cx = TestCx::from_registry(&registry, dir.path()).build();
    let hooks: Vec<_> = registry.hooks.iter().map(|(_, h)| h.clone()).collect();
    let mut event = HookEvent::PreTool {
        call: ToolCall {
            id: "t1".into(),
            name: tool.into(),
            input,
        },
        read_only,
    };
    run_hooks(&hooks, &mut event, &cx).await.verdict
}

#[test]
fn provides_services_and_hook() {
    let dir = tempfile::tempdir().unwrap();
    let registry = build_registry(vec![ch_safety::plugin()], dir.path(), "");
    assert!(registry.services.get::<PathGuard>().is_some());
    assert!(registry.services.get::<FileState>().is_some());
    let (_, hook) = &registry.hooks[0];
    assert_eq!(hook.id(), "safety");
    assert_eq!(hook.priority(), -100);
}

#[tokio::test]
async fn hook_applies_the_policy() {
    let v = verdict("", "bash", json!({"command": "git push"}), false).await;
    assert!(
        matches!(&v, Verdict::Deny { reason, .. } if reason.contains("Commit locally")),
        "{v:?}"
    );
    let v = verdict("", "bash", json!({"command": "npm test"}), false).await;
    assert!(
        matches!(&v, Verdict::Ask { asks } if matches!(asks.as_slice(), [a] if a.key.as_deref() == Some("bash:npm test *"))),
        "{v:?}"
    );
    let v = verdict("", "bash", json!({"command": "git status"}), false).await;
    assert_eq!(v, Verdict::Continue);
    let cfg = "[plugins.safety]\nmode = \"accept-edits\"\n";
    let v = verdict(cfg, "edit_file", json!({"path": "src/a.rs"}), false).await;
    assert_eq!(v, Verdict::Continue);
    let cfg = "[plugins.safety]\nallow = [\"bash:npm test *\"]\n";
    let v = verdict(
        cfg,
        "bash",
        json!({"command": "npm test -- --watch"}),
        false,
    )
    .await;
    assert_eq!(v, Verdict::Continue);
}

#[test]
fn bad_config_fails_registration() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = ch_kernel::Config::parse("[plugins.safety]\ndeny = [\"bash:[\"]\n").unwrap();
    let err = ch_kernel::RegistryBuilder::build(vec![ch_safety::plugin()], &cfg, dir.path())
        .err()
        .expect("bad glob should fail");
    assert!(
        format!("{err:#}").contains("plugins.safety.deny"),
        "{err:#}"
    );
    let cfg = ch_kernel::Config::parse("[plugins.safety]\nmode = \"yolo\"\n").unwrap();
    assert!(
        ch_kernel::RegistryBuilder::build(vec![ch_safety::plugin()], &cfg, dir.path()).is_err()
    );
}
