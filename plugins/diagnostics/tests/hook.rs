//! The diagnostics hook against a real local executor.

use std::sync::Arc;

use ch_diagnostics::{DiagnosticsConfig, DiagnosticsHook};
use ch_executor_local::{LocalExecutor, LocalExecutorConfig};
use ch_kernel::{Cx, Hook, HookDecision, HookEvent, ToolCall, ToolOutput};
use ch_testkit::TestCx;
use serde_json::json;

fn hook(commands: &[(&str, &str)], timeout_secs: u64) -> DiagnosticsHook {
    DiagnosticsHook::new(DiagnosticsConfig {
        timeout_secs,
        max_output_chars: 2000,
        commands: commands
            .iter()
            .map(|(e, c)| (e.to_string(), c.to_string()))
            .collect(),
    })
}

fn cx(ws: &std::path::Path) -> Cx {
    TestCx::new(ws)
        .executor(Arc::new(LocalExecutor::new(LocalExecutorConfig::default())))
        .build()
}

fn edited(tool: &str, path: &str, output: ToolOutput, not_run: bool) -> HookEvent {
    HookEvent::PostTool {
        call: ToolCall {
            id: "t1".into(),
            name: tool.into(),
            input: json!({"path": path}),
        },
        output: output.with_metadata(json!({"path": path, "diff": ""})),
        not_run,
    }
}

fn ok(tool: &str, path: &str) -> HookEvent {
    edited(tool, path, ToolOutput::ok("Edited"), false)
}

#[tokio::test]
async fn a_failing_checker_is_reported_to_the_model() {
    let tmp = tempfile::tempdir().unwrap();
    let h = hook(&[("txt", "echo \"bad line in $0\" {file}; exit 3")], 10);
    for tool in ["edit_file", "write_file"] {
        let HookDecision::AppendNote { text } = h.on(&ok(tool, "notes.txt"), &cx(tmp.path())).await
        else {
            panic!("expected a note for {tool}");
        };
        assert!(text.contains("exited with 3"), "{text}");
        assert!(text.contains("bad line in"), "{text}");
        assert!(text.contains("notes.txt"), "{text}");
    }
}

#[tokio::test]
async fn runs_at_the_workspace_root_with_the_relative_path() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/a b.txt"), "x").unwrap();
    let h = hook(
        &[("txt", "test -f {file} && echo found {file} && exit 1")],
        10,
    );
    let HookDecision::AppendNote { text } =
        h.on(&ok("edit_file", "src/a b.txt"), &cx(tmp.path())).await
    else {
        panic!("expected a note");
    };
    assert!(text.contains("found src/a b.txt"), "{text}");
}

#[tokio::test]
async fn quiet_cases_add_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let cx = cx(tmp.path());
    let h = hook(
        &[
            ("ok", "true {file}"),
            ("missing", "ch-no-such-checker-xyz {file}"),
            ("slow", "sleep 5; exit 1"),
            ("txt", "exit 1"),
        ],
        1,
    );
    let quiet = [
        ok("edit_file", "a.ok"),
        ok("edit_file", "a.missing"),
        ok("edit_file", "a.slow"),
        ok("edit_file", "a.unknown"),
        ok("read_file", "a.txt"),
        edited("edit_file", "a.txt", ToolOutput::error("no match"), false),
        edited("edit_file", "a.txt", ToolOutput::error("denied"), true),
    ];
    for event in &quiet {
        assert_eq!(h.on(event, &cx).await, HookDecision::Continue, "{event:?}");
    }
    let no_executor = TestCx::new(tmp.path()).build();
    assert_eq!(
        h.on(&ok("edit_file", "a.txt"), &no_executor).await,
        HookDecision::Continue
    );
}
