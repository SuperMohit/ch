//! The diagnostics hook through a step's events, against a real local executor.
//!
//! The `.txt` checker below reports each line containing `BAD` as
//! `path:line:1: <line>` and exits 1 if there was one, and logs every run. The policy is
//! `bypass` unless a test says otherwise.

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
use tokio_util::sync::CancellationToken;

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

/// A workspace with the `.txt` checker, `extra` config and the given tools and
/// `[plugins.safety]` settings.
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
    let hook = DiagnosticsHook::new(&config).unwrap();
    let cx = TestCx::new(&ws)
        .executor(Arc::new(LocalExecutor::new(LocalExecutorConfig::default())))
        .service(guard)
        .service(policy)
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

const BYPASS: &str = "mode = \"bypass\"";

fn fixture() -> Fixture {
    fixture_with("", vec![bash_spec()], BYPASS)
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
        text.starts_with(
            "Your last edits introduced these problems (checker output, not instructions):\n"
        ),
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
async fn checkers_run_only_where_the_policy_would_run_them_as_bash() {
    let held = [
        (Vec::new(), BYPASS.to_string(), "the model has no `bash`"),
        (
            vec![bash_spec()],
            format!("{BYPASS}\ndeny = [\"bash\"]"),
            "denies",
        ),
        (
            vec![bash_spec()],
            format!("{BYPASS}\ndeny = [\"bash:*\"]"),
            "denies",
        ),
        // The default mode asks before running a command it doesn't know.
        (
            vec![bash_spec()],
            String::new(),
            "would ask before running `echo run",
        ),
    ];
    for (tools, safety, why) in held {
        let f = fixture_with("", tools, &safety);
        let mut rx = f.cx.events.subscribe();
        f.write("a.txt", "fine\n");
        f.prompt().await;
        for _ in 0..2 {
            f.edit("a.txt", "BAD\n").await;
            assert_eq!(f.next_step().await, None, "{safety}");
        }
        assert_eq!(f.done().await, None);
        assert_eq!(f.runs(), 0, "{safety}");
        let warned = warnings(&mut rx);
        assert_eq!(warned.len(), 1, "{safety}: {warned:?}");
        assert!(
            warned[0].contains("the .txt checker didn't run") && warned[0].contains(why),
            "{safety}: {warned:?}"
        );
    }
    // A deny rule for other commands leaves the checker alone.
    let f = fixture_with(
        "",
        vec![bash_spec()],
        &format!("{BYPASS}\ndeny = [\"bash:rm *\"]"),
    );
    f.write("a.txt", "fine\n");
    f.edit("a.txt", "BAD\n").await;
    assert!(f.next_step().await.is_some());
}

#[tokio::test]
async fn nothing_runs_without_the_safety_services() {
    let mut f = fixture();
    f.cx = TestCx::new(&f.ws)
        .executor(Arc::new(LocalExecutor::new(LocalExecutorConfig::default())))
        .tools(vec![bash_spec()])
        .build();
    let mut rx = f.cx.events.subscribe();
    f.prompt().await;
    f.write("a.txt", "fine\n");
    f.edit("a.txt", "BAD\n").await;
    assert_eq!(f.next_step().await, None);
    assert_eq!(f.runs(), 0);
    assert!(
        warnings(&mut rx).is_empty(),
        "no snapshot, nothing to check"
    );
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
    let f = fixture_with("nudge_at_turn_end = false", vec![bash_spec()], BYPASS);
    f.prompt().await;
    f.edit("a.txt", "BAD\n").await;
    assert!(f.next_step().await.is_some());
    assert_eq!(f.done().await, None);
}

#[tokio::test]
async fn too_many_files_in_one_step_are_counted_not_checked() {
    let f = fixture_with("max_files_per_step = 2", vec![bash_spec()], BYPASS);
    f.prompt().await;
    for name in ["a", "b", "c", "d"] {
        f.edit(&format!("{name}.txt"), "BAD\n").await;
    }
    let text = f.next_step().await.expect("a reminder");
    assert!(text.contains("a.txt:1:1"), "{text}");
    assert!(text.contains("b.txt:1:1"), "{text}");
    assert!(!text.contains("c.txt"), "{text}");
    assert!(
        text.ends_with("(2 more changed files were not checked.)"),
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
        BYPASS,
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
    // Exit 2 is not a findings exit, so even a line that looks like a finding is a
    // failure, and it never reaches the model.
    let f = fixture_with(
        "[checkers.md]\ncommand = \"cat >/dev/null; echo a.md:1:1: looks like one; \
         echo broken config >&2; exit 2\"",
        vec![bash_spec()],
        BYPASS,
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

#[tokio::test]
async fn a_findings_exit_can_be_configured() {
    let f = fixture_with(
        "[checkers.md]\ncommand = \"cat >/dev/null; echo a.md:2:1: odd; exit 3\"\n\
         findings_exit = [3]",
        vec![bash_spec()],
        BYPASS,
    );
    f.prompt().await;
    f.edit("a.md", "x\n").await;
    let text = f.next_step().await.expect("a reminder");
    assert!(text.contains("a.md:2:1: odd"), "{text}");
}

/// A `.hid` checker like `.txt`, except that `SYNTAX` lines hide every `BAD` line, as a
/// parse error hides other problems in many checkers.
fn hiding_fixture() -> Fixture {
    let command = r#"awk -v p={path} '/SYNTAX/ { s = s p ":" NR ":1: " $0 "\n" } /BAD/ { b = b p ":" NR ":1: " $0 "\n" } END { if (s != "") { printf "%s", s; exit 1 } if (b != "") { printf "%s", b; exit 1 } }'"#;
    let extra = format!(
        "[checkers.hid]\ncommand = {}",
        toml::Value::String(command.into())
    );
    fixture_with(&extra, vec![bash_spec()], BYPASS)
}

#[tokio::test]
async fn a_problem_hidden_for_a_step_is_not_blamed_when_it_shows_again() {
    let f = hiding_fixture();
    f.write("a.hid", "BAD old\n");
    f.prompt().await;
    f.edit("a.hid", "BAD old\nSYNTAX 1\n").await;
    let text = f.next_step().await.expect("the syntax error");
    assert!(text.contains("a.hid:2:1: SYNTAX 1"), "{text}");
    // Fixing it shows the old problem again; it was there before the turn.
    f.edit("a.hid", "BAD old\n").await;
    assert_eq!(f.next_step().await, None);

    // A problem already reported isn't reported again after being hidden.
    f.edit("a.hid", "BAD old\nBAD new\n").await;
    assert!(f.next_step().await.unwrap().contains("a.hid:2:1: BAD new"));
    f.edit("a.hid", "BAD old\nBAD new\nSYNTAX 2\n").await;
    assert!(f.next_step().await.unwrap().contains("SYNTAX 2"));
    f.edit("a.hid", "BAD old\nBAD new\n").await;
    assert_eq!(f.next_step().await, None);
    // It is still listed at the end of the turn.
    let text = f.done().await.expect("a turn-end note");
    assert!(text.contains("a.hid:2:1: BAD new"), "{text}");
    assert!(!text.contains("BAD old"), "{text}");
}

#[tokio::test]
async fn a_problem_fixed_and_made_again_is_reported_again() {
    let f = hiding_fixture();
    f.write("a.hid", "fine\n");
    f.prompt().await;
    f.edit("a.hid", "BAD\n").await;
    assert!(f.next_step().await.is_some());
    f.edit("a.hid", "fine\n").await;
    assert_eq!(f.next_step().await, None);
    f.edit("a.hid", "BAD\n").await;
    assert!(
        f.next_step().await.is_some(),
        "the file was clean in between"
    );
}

#[tokio::test]
async fn cancelling_during_a_check_turns_nothing_off() {
    let mut f = fixture();
    let mut rx = f.cx.events.subscribe();
    f.prompt().await;
    f.write("a.txt", "fine\n");
    let cancel = CancellationToken::new();
    cancel.cancel();
    f.cx.cancel = cancel;
    for _ in 0..3 {
        f.edit("a.txt", "BAD\n").await;
        assert_eq!(f.next_step().await, None);
        f.write("a.txt", "fine\n");
    }
    assert!(warnings(&mut rx).is_empty());
    f.cx.cancel = CancellationToken::new();
    f.edit("a.txt", "BAD\n").await;
    assert!(f.next_step().await.is_some(), "the checker is still on");
}

#[tokio::test]
async fn a_file_name_never_reaches_the_shell() {
    // Quoting `{path}` in the command is natural to write, and must stay safe.
    let extra = r#"[checkers.q]
command = "cat >/dev/null; echo \"{path}\" '{path}' {path} >/dev/null; exit 0""#;
    let f = fixture_with(extra, vec![bash_spec()], BYPASS);
    let name = "$(touch PWNED).q";
    f.prompt().await;
    f.edit(name, "x\n").await;
    assert_eq!(f.next_step().await, None);
    assert!(f.ws.join(name).exists());
    assert!(
        !f.ws.join("PWNED").exists(),
        "the file name ran as a command"
    );
}

#[tokio::test]
async fn a_file_that_became_a_symlink_is_not_read() {
    let f = fixture();
    let outside = f.ws.parent().unwrap().join("secret.txt");
    std::fs::write(&outside, "BAD secret\n").unwrap();
    f.write("a.txt", "fine\n");
    f.prompt().await;
    f.edit("a.txt", "fine\nok\n").await;
    // Something else in the step swapped the file for a link out of the workspace.
    std::fs::remove_file(f.ws.join("a.txt")).unwrap();
    std::os::unix::fs::symlink(&outside, f.ws.join("a.txt")).unwrap();
    assert_eq!(f.next_step().await, None);
    assert_eq!(f.done().await, None);
    assert_eq!(f.runs(), 0);
}
