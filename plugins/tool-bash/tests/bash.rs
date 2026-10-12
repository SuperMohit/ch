//! `bash` tool tests with the real local executor.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ch_executor_local::LocalExecutor;
use ch_kernel::{Config, Cx, Plugin, PluginManifest, RegisterCx, RegistryBuilder, Tool};
use ch_tool_bash::{BashConfig, BashTool, DescriptionVariant};
use serde_json::{Value, json};

fn tool() -> BashTool {
    BashTool::new(BashConfig::default(), DescriptionVariant::WithFileTools)
}

fn cx(workspace: &Path) -> Cx {
    let mut cx = Cx::detached(workspace);
    cx.executor = Some(Arc::new(LocalExecutor::default()));
    cx
}

#[tokio::test]
async fn success_returns_output_and_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let out = tool()
        .call(json!({"command": "echo hello"}), &cx(dir.path()))
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.content, "hello\n[exit code: 0]");
    assert_eq!(out.metadata["exit_code"], json!(0));
    assert_eq!(out.metadata["timed_out"], json!(false));
}

#[tokio::test]
async fn nonzero_exit_is_an_error_with_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let out = tool()
        .call(
            json!({"command": "echo partial; echo broken >&2; exit 4"}),
            &cx(dir.path()),
        )
        .await;
    assert!(out.is_error);
    assert_eq!(out.content, "partial\n\n[stderr]\nbroken\n[exit code: 4]");
    assert_eq!(out.metadata["exit_code"], json!(4));
}

#[tokio::test]
async fn empty_output_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let out = tool()
        .call(json!({"command": "true"}), &cx(dir.path()))
        .await;
    assert_eq!(out.content, "(no output)\n[exit code: 0]");
}

#[tokio::test]
async fn runs_at_the_workspace_root_and_in_cwd() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/inner.txt"), "x").unwrap();
    let c = cx(dir.path());
    let root = tool().call(json!({"command": "ls"}), &c).await;
    assert!(root.content.contains("sub"), "{}", root.content);
    let sub = tool()
        .call(json!({"command": "ls", "cwd": "sub"}), &c)
        .await;
    assert!(sub.content.contains("inner.txt"), "{}", sub.content);
}

#[tokio::test]
async fn timeout_kills_and_reports() {
    let dir = tempfile::tempdir().unwrap();
    let start = Instant::now();
    let out = tool()
        .call(
            json!({"command": "echo before; sleep 30", "timeout_seconds": 1}),
            &cx(dir.path()),
        )
        .await;
    assert!(
        start.elapsed() < Duration::from_secs(6),
        "{:?}",
        start.elapsed()
    );
    assert!(out.is_error);
    assert!(out.content.starts_with("before\n"), "{}", out.content);
    assert!(
        out.content
            .ends_with("[timed out after 1s — the process was killed]"),
        "{}",
        out.content
    );
    assert_eq!(out.metadata["timed_out"], json!(true));
    assert_eq!(out.metadata["exit_code"], Value::Null);
}

#[tokio::test]
async fn cancellation_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let c = cx(dir.path());
    let cancel = c.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel();
    });
    let out = tool().call(json!({"command": "sleep 30"}), &c).await;
    assert!(out.is_error);
    assert!(out.content.ends_with("[cancelled]"), "{}", out.content);
}

#[tokio::test]
async fn oversized_timeout_is_clamped_with_a_note() {
    let dir = tempfile::tempdir().unwrap();
    let out = tool()
        .call(
            json!({"command": "echo ok", "timeout_seconds": 5000}),
            &cx(dir.path()),
        )
        .await;
    assert!(!out.is_error);
    assert!(
        out.content.starts_with("[note: timeout_seconds 5000"),
        "{}",
        out.content
    );
    assert!(out.content.ends_with("ok\n[exit code: 0]"));
}

#[tokio::test]
async fn cwd_escape_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    std::os::unix::fs::symlink(dir.path(), ws.join("out")).unwrap();
    for cwd in ["..", "/tmp", "out", "../ws/../.."] {
        let out = tool()
            .call(json!({"command": "pwd", "cwd": cwd}), &cx(&ws))
            .await;
        assert!(out.is_error, "{cwd}: {}", out.content);
        assert!(
            out.content.contains("outside the workspace"),
            "{cwd}: {}",
            out.content
        );
    }
    let out = tool()
        .call(json!({"command": "pwd", "cwd": "missing"}), &cx(&ws))
        .await;
    assert!(out.is_error && out.content.contains("does not exist"));
}

#[tokio::test]
async fn missing_command_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    for input in [json!({}), json!({"command": "  "}), json!({"command": 3})] {
        let out = tool().call(input, &cx(dir.path())).await;
        assert!(out.is_error && out.content.contains("Missing `command`"));
    }
}

#[tokio::test]
async fn no_executor_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let out = tool()
        .call(json!({"command": "echo hi"}), &Cx::detached(dir.path()))
        .await;
    assert!(out.is_error);
    assert!(
        out.content.contains("No executor is configured"),
        "{}",
        out.content
    );
}

// ---------- registration and description variants ----------

/// Stand-in for the real `tools-fs` plugin, which this crate must not depend on.
struct FakeToolsFs;

impl Plugin for FakeToolsFs {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new("tools-fs", "fake")
    }
    fn register(&self, _cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        Ok(())
    }
}

fn bash_description(with_tools_fs: bool) -> String {
    let mut plugins: Vec<Arc<dyn Plugin>> = vec![ch_tool_bash::plugin()];
    if with_tools_fs {
        plugins.push(Arc::new(FakeToolsFs));
    }
    let registry = RegistryBuilder::build(plugins, &Config::default(), Path::new("/tmp")).unwrap();
    let spec = registry.tool("bash").unwrap().spec();
    assert!(!spec.read_only);
    assert_eq!(spec.input_schema["required"], json!(["command"]));
    spec.description
}

fn assert_sections_in_order(description: &str) {
    let positions: Vec<usize> = ["When:", "Protocol:", "Error modes:", "Side effects:"]
        .iter()
        .map(|s| description.find(s).unwrap_or_else(|| panic!("missing {s}")))
        .collect();
    assert!(positions.windows(2).all(|w| w[0] < w[1]), "{positions:?}");
}

#[test]
fn description_steers_to_file_tools_when_tools_fs_is_enabled() {
    let d = bash_description(true);
    assert_sections_in_order(&d);
    assert!(d.contains("Do NOT use it to read, search or edit files"));
    assert!(d.contains("edit_file"));
    assert!(!d.contains("Working with only a shell"));
}

#[test]
fn bash_only_description_when_tools_fs_is_disabled() {
    let d = bash_description(false);
    assert_sections_in_order(&d);
    assert!(d.contains("Working with only a shell"));
    assert!(d.contains("Nothing validates your edits"));
    assert!(d.contains("read the changed region back"));
    assert!(d.contains("python3 - <<'EOF'"));
    assert!(d.contains("sed -i"));
    assert!(!d.contains("edit_file"));
}

#[test]
fn config_changes_timeouts() {
    let mut cfg = Config::default();
    cfg.set_override("plugins.tool-bash.max_timeout_secs=60")
        .unwrap();
    let registry =
        RegistryBuilder::build(vec![ch_tool_bash::plugin()], &cfg, Path::new("/tmp")).unwrap();
    let spec = registry.tool("bash").unwrap().spec();
    assert_eq!(
        spec.input_schema["properties"]["timeout_seconds"]["maximum"],
        json!(60)
    );
    assert!(spec.description.contains("capped at 60"));
}
