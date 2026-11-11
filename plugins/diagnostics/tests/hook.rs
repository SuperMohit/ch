//! The diagnostics hook through a step's events, against a real local executor.
//!
//! The `.txt` checker below reports each line containing `BAD` as
//! `path:line:1: <line>` and exits 1 if there was one, and logs every run.

use std::path::PathBuf;
use std::sync::Arc;

use ch_diagnostics::{DiagnosticsConfig, DiagnosticsHook};
use ch_executor_local::{LocalExecutor, LocalExecutorConfig};
use ch_kernel::{
    AgentEvent, Cx, Hook, HookDecision, HookEvent, ModelRequest, TerminationReason, ToolCall,
    ToolOutput, ToolSpec,
};
use ch_safety::{PathGuard, Policy, SafetyConfig};
use ch_testkit::TestCx;
use serde_json::json;

struct Fixture {
    _tmp: tempfile::TempDir,
    ws: PathBuf,
    log: PathBuf,
    hook: DiagnosticsHook,
    cx: Cx,
}

fn bash_spec() -> ToolSpec {
    ToolSpec {
        name: "bash".into(),
        description: "Runs a command.".into(),
        input_schema: json!({"type": "object"}),
        read_only: false,
    }
}

/// A workspace with the `.txt` checker, `extra` config and the given tools and policy.
fn fixture_with(extra: &str, tools: Vec<ToolSpec>, safety: &str) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let ws = ws.canonicalize().unwrap();
    let log = tmp.path().join("runs.log");
    let command = format!(
        "echo run >> {log}; awk -v p={{path}} '/BAD/ {{ print p \":\" NR \":1: \" $0; bad = 1 }} \
         END {{ exit bad }}'",
        log = log.display()
    );
    let config: DiagnosticsConfig = toml::from_str(&format!(
        "{extra}\n[checkers.txt]\ncommand = {}\n",
        toml::Value::String(command)
    ))
    .unwrap();
    let guard = Arc::new(PathGuard::new(&ws).unwrap());
    let safety: SafetyConfig = toml::from_str(safety).unwrap();
    let policy = Arc::new(Policy::new(&safety, guard.clone()).unwrap());
    let hook = DiagnosticsHook::new(&config, guard, Some(policy)).unwrap();
    let cx = TestCx::new(&ws)
        .executor(Arc::new(LocalExecutor::new(LocalExecutorConfig::default())))
        .tools(tools)
        .build();
    Fixture {
        _tmp: tmp,
        ws,
        log,
        hook,
        cx,
    }
}

fn fixture() -> Fixture {
    fixture_with("", vec![bash_spec()], "")
}

impl Fixture {
    fn write(&self, rel: &str, text: &str) {
        let path = self.ws.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn runs(&self) -> usize {
        std::fs::read_to_string(&self.log)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    async fn on(&self, event: HookEvent) -> HookDecision {
        self.hook.on(&event, &self.cx).await
    }

    async fn prompt(&self) {
        let ev = HookEvent::UserPrompt { text: "go".into() };
        assert_eq!(self.on(ev).await, HookDecision::Continue);
    }

    /// One `edit_file` call that leaves `rel` holding `text`.
    async fn edit(&self, rel: &str, text: &str) {
        self.call(
            "edit_file",
            rel,
            |f| f.write(rel, text),
            ToolOutput::ok("Edited."),
            false,
        )
        .await;
    }

    async fn call(
        &self,
        tool: &str,
        rel: &str,
        run: impl FnOnce(&Self),
        output: ToolOutput,
        not_run: bool,
    ) {
        let call = ToolCall {
            id: "t1".into(),
            name: tool.into(),
            input: json!({"path": rel}),
        };
        let pre = HookEvent::PreTool {
            call: call.clone(),
            read_only: false,
        };
        // The kernel may run pre_tool twice for one call.
        assert_eq!(self.on(pre.clone()).await, HookDecision::Continue);
        assert_eq!(self.on(pre).await, HookDecision::Continue);
        if !not_run {
            run(self);
        }
        let post = HookEvent::PostTool {
            call,
            output: output.with_metadata(json!({"path": rel, "diff": ""})),
            not_run,
        };
        assert_eq!(self.on(post).await, HookDecision::Continue);
    }

    /// The reminder for the next model call, if any.
    async fn next_step(&self) -> Option<String> {
        let ev = HookEvent::BeforeModel {
            request: ModelRequest {
                model: "m".into(),
                system: Vec::new(),
                messages: Vec::new(),
                tools: Vec::new(),
                max_tokens: 1000,
                temperature: None,
                thinking_budget: None,
                purpose: "agent".into(),
                volatile_tail: 0,
            },
        };
        note(self.on(ev).await)
    }

    async fn done(&self) -> Option<String> {
        let ev = HookEvent::TurnEnd {
            reason: TerminationReason::Done,
            steps: 3,
            final_text: "Done.".into(),
        };
        note(self.on(ev).await)
    }
}

fn note(decision: HookDecision) -> Option<String> {
    match decision {
        HookDecision::Continue => None,
        HookDecision::AppendNote { text } => Some(text),
        other => panic!("unexpected decision {other:?}"),
    }
}

#[tokio::test]
async fn only_what_the_edit_introduced_is_reported() {
    let f = fixture();
    f.write("notes.txt", "a\nBAD old\n");
    f.prompt().await;
    // The old problem moves down two lines; a new one appears.
    f.edit("notes.txt", "x\ny\na\nBAD old\nBAD new\n").await;
    let text = f.next_step().await.expect("a reminder");
    assert!(
        text.starts_with("Your last edits introduced these problems:\n"),
        "{text}"
    );
    assert!(text.contains("notes.txt:5:1: BAD new"), "{text}");
    assert!(!text.contains("BAD old"), "{text}");
    // Reported once: nothing is pending for the next step.
    assert_eq!(f.next_step().await, None);
}

#[tokio::test]
async fn edits_in_one_step_are_checked_once_on_the_final_state() {
    let f = fixture();
    f.write("a.txt", "fine\n");
    f.prompt().await;
    f.edit("a.txt", "BAD halfway\n").await;
    f.edit("a.txt", "fixed\n").await;
    assert_eq!(f.next_step().await, None);
    // Before and after, once each.
    assert_eq!(f.runs(), 2);

    // The next step's "before" is this step's "after", already known.
    f.edit("a.txt", "fixed\nBAD\n").await;
    assert!(f.next_step().await.is_some());
    assert_eq!(f.runs(), 3);
}

#[tokio::test]
async fn a_new_file_reports_everything_in_it() {
    let f = fixture();
    f.prompt().await;
    f.call(
        "write_file",
        "dir/new.txt",
        |f| f.write("dir/new.txt", "BAD one\nok\nBAD two\n"),
        ToolOutput::ok("Created."),
        false,
    )
    .await;
    let text = f.next_step().await.expect("a reminder");
    assert!(text.contains("dir/new.txt:1:1: BAD one"), "{text}");
    assert!(text.contains("dir/new.txt:3:1: BAD two"), "{text}");
    assert_eq!(f.runs(), 1, "an absent file isn't checked");
}

#[tokio::test]
async fn calls_that_failed_or_never_ran_mark_nothing() {
    let f = fixture();
    f.write("a.txt", "fine\n");
    f.prompt().await;
    f.call(
        "edit_file",
        "a.txt",
        |f| f.write("a.txt", "BAD\n"),
        ToolOutput::error("old_text not found"),
        false,
    )
    .await;
    f.call(
        "edit_file",
        "a.txt",
        |_| {},
        ToolOutput::error("Denied"),
        true,
    )
    .await;
    assert_eq!(f.next_step().await, None);
    assert_eq!(f.runs(), 0);

    // A snapshot from a step whose edit never ran isn't used later: the file changed
    // in between (the user edited it), and only this step's edit counts.
    f.write("b.txt", "fine\n");
    f.call(
        "edit_file",
        "b.txt",
        |_| {},
        ToolOutput::error("Denied"),
        true,
    )
    .await;
    assert_eq!(f.next_step().await, None);
    f.write("b.txt", "BAD by the user\n");
    f.edit("b.txt", "BAD by the user\nok\n").await;
    assert_eq!(f.next_step().await, None);
}

#[tokio::test]
async fn files_without_a_checker_are_ignored() {
    let f = fixture();
    f.prompt().await;
    f.edit("a.md", "BAD\n").await;
    f.call("read_file", "a.txt", |_| {}, ToolOutput::ok("x"), false)
        .await;
    assert_eq!(f.next_step().await, None);
    assert_eq!(f.runs(), 0);
}

#[tokio::test]
async fn nothing_runs_while_the_model_has_no_bash() {
    for f in [
        fixture_with("", Vec::new(), ""),
        fixture_with("", vec![bash_spec()], r#"deny = ["bash"]"#),
    ] {
        f.write("a.txt", "fine\n");
        f.prompt().await;
        f.edit("a.txt", "BAD\n").await;
        assert_eq!(f.next_step().await, None);
        assert_eq!(f.done().await, None);
        assert_eq!(f.runs(), 0);
    }
    // A deny rule for some commands leaves bash available.
    let f = fixture_with("", vec![bash_spec()], r#"deny = ["bash:rm *"]"#);
    f.write("a.txt", "fine\n");
    f.edit("a.txt", "BAD\n").await;
    assert!(f.next_step().await.is_some());
}

#[tokio::test]
async fn the_end_of_a_turn_lists_problems_still_there_once() {
    let f = fixture();
    f.write("a.txt", "fine\nBAD before the turn\n");
    f.prompt().await;
    f.edit("a.txt", "fine\nBAD before the turn\nBAD 1\n").await;
    assert!(f.next_step().await.is_some());
    f.edit("a.txt", "fine\nBAD before the turn\nBAD 1\nok\n")
        .await;
    // Not new in this step, so not repeated.
    assert_eq!(f.next_step().await, None);

    let text = f.done().await.expect("a turn-end note");
    assert!(text.starts_with("Before you finish:"), "{text}");
    assert!(text.contains("a.txt:3:1: BAD 1"), "{text}");
    assert!(!text.contains("before the turn"), "{text}");
    assert_eq!(f.done().await, None, "at most once per turn");

    // A new turn starts over; a fixed file gives no note.
    f.prompt().await;
    f.edit("a.txt", "BAD 2\n").await;
    assert!(f.next_step().await.is_some());
    f.edit("a.txt", "fine\n").await;
    assert_eq!(f.next_step().await, None);
    assert_eq!(f.done().await, None);
}

#[tokio::test]
async fn the_turn_end_note_can_be_turned_off() {
    let f = fixture_with("nudge_at_turn_end = false", vec![bash_spec()], "");
    f.prompt().await;
    f.edit("a.txt", "BAD\n").await;
    assert!(f.next_step().await.is_some());
    assert_eq!(f.done().await, None);
}

#[tokio::test]
async fn too_many_files_in_one_step_are_counted_not_checked() {
    let f = fixture_with("max_files_per_step = 2", vec![bash_spec()], "");
    f.prompt().await;
    for name in ["a", "b", "c", "d"] {
        f.edit(&format!("{name}.txt"), "BAD\n").await;
    }
    let text = f.next_step().await.expect("a reminder");
    assert!(text.contains("a.txt:1:1"), "{text}");
    assert!(text.contains("b.txt:1:1"), "{text}");
    assert!(!text.contains("c.txt"), "{text}");
    assert!(
        text.ends_with("(2 more changed files were not checked in this step.)"),
        "{text}"
    );
}

fn warnings(rx: &mut tokio::sync::broadcast::Receiver<AgentEvent>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let AgentEvent::Warning { message } = ev {
            out.push(message);
        }
    }
    out
}

#[tokio::test]
async fn a_missing_checker_is_turned_off_with_one_warning() {
    let f = fixture_with(
        "[checkers.md]\ncommand = \"ch-no-such-checker-xyz {path}\"",
        vec![bash_spec()],
        "",
    );
    let mut rx = f.cx.events.subscribe();
    f.prompt().await;
    for _ in 0..2 {
        f.edit("a.md", "x\n").await;
        assert_eq!(f.next_step().await, None);
    }
    let warned = warnings(&mut rx);
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert!(
        warned[0].contains("the .md checker is off for this session")
            && warned[0].contains("was not found"),
        "{warned:?}"
    );
}

#[tokio::test]
async fn a_checker_that_keeps_failing_is_turned_off() {
    // Exit 2 with nothing to read: a failure, not a finding.
    let f = fixture_with(
        "[checkers.md]\ncommand = \"cat >/dev/null; echo broken config >&2; exit 2\"",
        vec![bash_spec()],
        "",
    );
    let mut rx = f.cx.events.subscribe();
    f.prompt().await;
    for i in 0..3 {
        f.edit("a.md", &format!("{i}\n")).await;
        assert_eq!(f.next_step().await, None);
    }
    let warned = warnings(&mut rx);
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert!(warned[0].contains("3 runs in a row failed"), "{warned:?}");
    assert!(warned[0].contains("broken config"), "{warned:?}");
}

/// Whether `command` runs here (the real-tool tests below skip otherwise).
fn available(command: &str) -> bool {
    std::process::Command::new("sh")
        .args(["-c", command])
        .output()
        .is_ok_and(|o| o.status.success())
}

#[tokio::test]
async fn rustfmt_reports_parse_errors_only_and_ignores_the_repo_toolchain() {
    if !available("RUSTUP_TOOLCHAIN=stable RUSTUP_AUTO_INSTALL=0 rustfmt --version") {
        eprintln!("skipped: no rustfmt on the stable toolchain");
        return;
    }
    let f = fixture();
    // A toolchain file that would make rustup run a program from the repository.
    f.write("rust-toolchain.toml", "[toolchain]\npath = \"evil\"\n");
    f.write(
        "Cargo.toml",
        "[package]\nname = \"x\"\nedition = \"2021\"\n",
    );
    f.write("src/main.rs", "mod missing;\nfn main() {}\n");
    f.prompt().await;
    // Badly formatted but valid, with a child module that doesn't exist: nothing.
    f.edit("src/main.rs", "mod missing;\nfn main(){let x=1;   }\n")
        .await;
    assert_eq!(f.next_step().await, None);
    f.edit("src/main.rs", "mod missing;\nfn main(){let x=;   }\n")
        .await;
    let text = f.next_step().await.expect("a parse error");
    assert!(
        text.contains("src/main.rs:2:17: error: expected expression"),
        "{text}"
    );
}

#[tokio::test]
async fn ruff_reports_syntax_errors_and_undefined_names() {
    if !available("ruff --version") {
        eprintln!("skipped: no ruff");
        return;
    }
    let f = fixture();
    // The project's own rules don't apply: style isn't reported.
    f.write("pyproject.toml", "[tool.ruff.lint]\nselect = [\"ALL\"]\n");
    f.write("app/a.py", "def f():\n    return 1\n");
    f.prompt().await;
    f.edit("app/a.py", "import os\ndef f( ):\n    return reponse\n")
        .await;
    let text = f.next_step().await.expect("an undefined name");
    assert!(
        text.contains("app/a.py:3:12: F821 Undefined name `reponse`"),
        "{text}"
    );
    assert!(!text.contains("F401"), "{text}");
    f.edit("app/a.py", "def f():\n    return = 1\n").await;
    let text = f.next_step().await.expect("a syntax error");
    assert!(
        text.contains("app/a.py:2:") && text.contains("invalid-syntax"),
        "{text}"
    );
}

/// Runs commands with `dir` first on `PATH`, as a shell set up by direnv might.
struct PathFirst {
    inner: LocalExecutor,
    dir: PathBuf,
}

#[ch_kernel::reexports::async_trait]
impl ch_kernel::Executor for PathFirst {
    fn id(&self) -> &str {
        "path-first"
    }

    async fn run(
        &self,
        mut request: ch_kernel::ExecRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<ch_kernel::ExecResult> {
        let path = format!(
            "{}:{}",
            self.dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        request.env.push(("PATH".into(), path));
        self.inner.run(request, cancel).await
    }
}

#[tokio::test]
async fn a_built_in_program_found_inside_the_workspace_never_runs() {
    let mut f = fixture();
    let bin = f.ws.join(".venv/bin");
    f.write(
        ".venv/bin/ruff",
        "#!/bin/sh\ntouch \"$(dirname \"$0\")/ran\"\nexit 0\n",
    );
    {
        use std::os::unix::fs::PermissionsExt;
        let script = bin.join("ruff");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    f.cx.executor = Some(Arc::new(PathFirst {
        inner: LocalExecutor::new(LocalExecutorConfig::default()),
        dir: bin.clone(),
    }));
    let mut rx = f.cx.events.subscribe();
    f.prompt().await;
    f.edit("a.py", "return reponse\n").await;
    assert_eq!(f.next_step().await, None);
    assert!(!bin.join("ran").exists(), "the workspace's ruff ran");
    let warned = warnings(&mut rx);
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert!(warned[0].contains("inside the workspace"), "{warned:?}");
}
