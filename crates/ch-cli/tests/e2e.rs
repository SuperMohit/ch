//! End-to-end runs of `ch -p` against the `calc-py` fixture repo with a scripted model:
//! read → edit → run the tests → done.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use ch_cli::Cli;
use ch_kernel::{ContentBlock, ModelRequest, Plugin};
use ch_provider_scripted::{ScriptedPlugin, load_script};
use clap::Parser;
use serde_json::Value;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn script_path() -> PathBuf {
    repo_root().join("tests/fixtures/calc-py.script.json")
}

/// Copies the fixture into a fresh temp dir and returns (guard, workspace path).
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("calc-py");
    std::fs::create_dir_all(&ws).unwrap();
    for name in ["calc.py", "test_calc.py", "CH.md"] {
        std::fs::copy(
            repo_root().join("tests/fixtures/calc-py").join(name),
            ws.join(name),
        )
        .unwrap();
    }
    (dir, ws)
}

fn has_python() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn common_args(ws: &Path) -> Vec<String> {
    [
        "--cwd",
        ws.to_str().unwrap(),
        "--permission-mode",
        "accept-edits",
        "--allow",
        "bash:python3 *",
        "--set",
        "model.provider=scripted",
        "--set",
        "plugins.enable=[\"provider-scripted\"]",
        "--set",
        "plugins.project-instructions.environment=false",
        "--set",
        "plugins.project-instructions.user_file=\"\"",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[test]
fn binary_fixes_the_fixture_repo() {
    if !has_python() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let (dir, ws) = fixture();
    let home = dir.path().join("home");
    let mut args = vec!["-p".to_string(), "fix the failing test".into()];
    args.extend(common_args(&ws));
    args.extend([
        "--output-format".into(),
        "json".into(),
        "--set".into(),
        format!(
            "plugins.provider-scripted.script=\"{}\"",
            script_path().display()
        ),
    ]);
    let out = Command::new(env!("CARGO_BIN_EXE_ch"))
        .args(&args)
        .env("CH_HOME", &home)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "exit {:?}\nstdout: {stdout}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    let result: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(result["reason"], "done", "{result}");
    assert_eq!(result["steps"], 4);
    assert!(
        result["final_text"]
            .as_str()
            .unwrap()
            .contains("tests pass")
    );

    // The bug is fixed and the fixture's own tests pass.
    let calc = std::fs::read_to_string(ws.join("calc.py")).unwrap();
    assert!(calc.contains("return a + b"), "{calc}");
    let tests = Command::new("python3")
        .args(["-m", "unittest", "-q"])
        .current_dir(&ws)
        .output()
        .unwrap();
    assert!(tests.status.success());

    // The session was saved.
    let sessions: Vec<_> = walk(&home.join("sessions"))
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect();
    assert_eq!(sessions.len(), 1, "{sessions:?}");
}

#[test]
fn edits_are_denied_without_permission_in_print_mode() {
    let (dir, ws) = fixture();
    let home = dir.path().join("home");
    let mut args = vec!["-p".to_string(), "fix it".into()];
    // Default permission mode ("ask") with the default auto-deny approver.
    args.extend(common_args(&ws).into_iter().skip(4));
    args.extend([
        "--cwd".into(),
        ws.to_str().unwrap().into(),
        "--output-format".into(),
        "stream-json".into(),
        "--set".into(),
        format!(
            "plugins.provider-scripted.script=\"{}\"",
            script_path().display()
        ),
    ]);
    let out = Command::new(env!("CARGO_BIN_EXE_ch"))
        .args(&args)
        .env("CH_HOME", &home)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let events: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let edit_end = events
        .iter()
        .find(|e| e["type"] == "tool_call_end" && e["name"] == "edit_file")
        .expect("edit_file call");
    assert_eq!(edit_end["is_error"], true);
    assert!(
        edit_end["content"].as_str().unwrap().contains("--allow"),
        "{edit_end}"
    );
    let calc = std::fs::read_to_string(ws.join("calc.py")).unwrap();
    assert!(calc.contains("return a - b"), "the file must be untouched");
}

/// Snapshot of what the model is sent: the assembled system prompt, the tools, and the
/// shape of every request. Changes to prompts or plugins show up here for review.
#[tokio::test]
async fn assembled_prompts_snapshot() {
    let (_dir, ws) = fixture();
    let home = ws.parent().unwrap().join("home");
    let scripted = ScriptedPlugin::from_steps(load_script(&script_path()).unwrap());
    let provider = scripted.provider().unwrap();
    let mut plugins: Vec<Arc<dyn Plugin>> = ch_cli::builtin_plugins()
        .into_iter()
        .filter(|p| p.manifest().id != "provider-scripted")
        .collect();
    plugins.push(Arc::new(scripted));

    let mut args = vec!["ch".to_string(), "-p".into(), "fix the failing test".into()];
    args.extend(common_args(&ws));
    if !has_python() {
        // Keep the run deterministic without python: the bash step then fails, which
        // the snapshot would show, so skip the snapshot entirely.
        eprintln!("skipping: python3 not available");
        return;
    }
    let cli = Cli::try_parse_from(args).unwrap();
    let code = ch_cli::run_with(cli, plugins, home, ch_cli::Stdin::None)
        .await
        .unwrap();
    assert_eq!(code, 0);

    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    let ws_str = ws.canonicalize().unwrap().to_string_lossy().to_string();
    let text = describe(&requests)
        .replace(&ws_str, "[WS]")
        .replace(&ws.to_string_lossy().to_string(), "[WS]");
    insta::assert_snapshot!(text);
}

fn describe(requests: &[ModelRequest]) -> String {
    let first = &requests[0];
    let mut out = String::new();
    out.push_str(&format!("model: {}\n", first.model));
    out.push_str(&format!(
        "tools: {}\n",
        first
            .tools
            .iter()
            .map(|t| format!("{}{}", t.name, if t.read_only { " (ro)" } else { "" }))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    for (i, block) in first.system.iter().enumerate() {
        out.push_str(&format!("\n===== system block {i} =====\n{block}\n"));
    }
    for (i, req) in requests.iter().enumerate() {
        out.push_str(&format!("\n===== request {i} messages =====\n"));
        for m in &req.messages {
            let blocks: Vec<String> = m
                .content
                .iter()
                .map(|b| match b {
                    ContentBlock::Text { text } => format!("text({})", first_line(text)),
                    ContentBlock::Thinking { .. } => "thinking".into(),
                    ContentBlock::RedactedThinking { .. } => "redacted_thinking".into(),
                    ContentBlock::ToolUse { name, input, .. } => {
                        format!("tool_use({name} {input})")
                    }
                    ContentBlock::ToolResult {
                        content, is_error, ..
                    } => format!(
                        "tool_result({}{})",
                        if *is_error { "error: " } else { "" },
                        first_line(content)
                    ),
                })
                .collect();
            out.push_str(&format!("{:?}: {}\n", m.role, blocks.join(" | ")));
        }
    }
    out
}

fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or("");
    let mut l: String = line.chars().take(70).collect();
    if s.lines().count() > 1 || line.chars().count() > 70 {
        l.push('…');
    }
    l
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}
