//! Argument parsing, prompt resolution and frontend/approver selection.

use ch_cli::Cli;
use ch_cli::app::{Selection, resolve_print_prompt, select};
use ch_cli::args::{ApproverArg, OutputFormatArg, PermissionMode};
use ch_cli::config::Overrides;
use ch_cli::sessions::{Resume, parse_choice};
use ch_kernel::Config;
use clap::Parser;

fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(std::iter::once("ch").chain(args.iter().copied())).unwrap()
}

#[test]
fn bare_invocation_is_the_repl() {
    let cli = parse(&[]);
    assert!(!cli.is_print());
    assert_eq!(cli.prompt, None);
    assert_eq!(cli.output_format, OutputFormatArg::Text);
    assert_eq!(cli.approver, ApproverArg::AutoDeny);
    let cli = parse(&["fix the tests"]);
    assert_eq!(cli.prompt.as_deref(), Some("fix the tests"));
}

#[test]
fn print_flag_forms() {
    let cli = parse(&["-p", "do it"]);
    assert_eq!(cli.print, Some(Some("do it".into())));
    let cli = parse(&["--print"]);
    assert_eq!(cli.print, Some(None));
    let cli = parse(&["-p", "-"]);
    assert_eq!(cli.print, Some(Some("-".into())));
    let cli = parse(&[
        "-p",
        "x",
        "--output-format",
        "stream-json",
        "--verbose",
        "--approver",
        "auto-allow",
    ]);
    assert_eq!(cli.output_format, OutputFormatArg::StreamJson);
    assert!(cli.verbose);
    assert_eq!(cli.approver.as_str(), "auto-allow");
    assert!(Cli::try_parse_from(["ch", "--output-format", "yaml"]).is_err());
}

#[test]
fn config_flags_repeat_and_map_to_overrides() {
    let cli = parse(&[
        "--model",
        "claude-haiku-5-5",
        "--profile",
        "fast",
        "--set",
        "a.b=1",
        "--set",
        "c=2",
        "--allow",
        "edit_file",
        "--allow",
        "bash:cargo *",
        "--permission-mode",
        "accept-edits",
        "--trust-project",
    ]);
    assert_eq!(cli.permission_mode, Some(PermissionMode::AcceptEdits));
    let o = Overrides::from_cli(&cli);
    assert_eq!(
        o,
        Overrides {
            profile: Some("fast".into()),
            model: Some("claude-haiku-5-5".into()),
            set: vec!["a.b=1".into(), "c=2".into()],
            allow: vec!["edit_file".into(), "bash:cargo *".into()],
            permission_mode: Some("accept-edits".into()),
            trust_project: true,
        }
    );
    assert!(Cli::try_parse_from(["ch", "--permission-mode", "yolo"]).is_err());
}

#[test]
fn session_flags() {
    assert_eq!(
        Resume::from_flags(parse(&[]).continue_last, parse(&[]).resume),
        Resume::New
    );
    let cli = parse(&["-c"]);
    assert_eq!(
        Resume::from_flags(cli.continue_last, cli.resume),
        Resume::Latest
    );
    let cli = parse(&["--resume"]);
    assert_eq!(
        Resume::from_flags(cli.continue_last, cli.resume),
        Resume::Pick
    );
    let cli = parse(&["-r", "20260101-abc"]);
    assert_eq!(
        Resume::from_flags(cli.continue_last, cli.resume),
        Resume::Id("20260101-abc".into())
    );
    assert!(Cli::try_parse_from(["ch", "-c", "-r", "x"]).is_err());
}

#[test]
fn other_flags() {
    let cli = parse(&["--list-plugins", "--config-path", "--cwd", "/tmp"]);
    assert!(cli.list_plugins);
    assert!(cli.config_path);
    assert_eq!(cli.cwd.as_deref(), Some(std::path::Path::new("/tmp")));
}

#[test]
fn choosing_a_session_number() {
    assert_eq!(parse_choice("2\n", 3).unwrap(), 1);
    assert!(parse_choice("0", 3).is_err());
    assert!(parse_choice("4", 3).is_err());
    assert!(parse_choice("x", 3).is_err());
}

#[test]
fn print_prompt_sources() {
    let no_stdin = || -> anyhow::Result<String> { panic!("stdin must not be read") };
    let stdin = |s: &'static str| move || -> anyhow::Result<String> { Ok(s.to_string()) };

    let cli = parse(&["-p", "task"]);
    assert_eq!(resolve_print_prompt(&cli, false, no_stdin).unwrap(), "task");
    // The positional prompt works with a bare -p.
    let cli = parse(&["-p", "--", "positional task"]);
    assert_eq!(cli.print, Some(None));
    assert_eq!(
        resolve_print_prompt(&cli, false, no_stdin).unwrap(),
        "positional task"
    );
    // `-` reads stdin only.
    let cli = parse(&["-p", "-"]);
    assert_eq!(
        resolve_print_prompt(&cli, false, stdin("from stdin\n")).unwrap(),
        "from stdin\n"
    );
    // Piped stdin is appended to a given prompt, or used alone.
    let cli = parse(&["-p", "review this"]);
    assert_eq!(
        resolve_print_prompt(&cli, true, stdin("diff text\n")).unwrap(),
        "review this\n\ndiff text"
    );
    let cli = parse(&["-p"]);
    assert_eq!(
        resolve_print_prompt(&cli, true, stdin("only stdin")).unwrap(),
        "only stdin"
    );
    // Nothing at all.
    let e = resolve_print_prompt(&cli, false, no_stdin).unwrap_err();
    assert!(e.to_string().contains("no task given"));
    assert!(resolve_print_prompt(&cli, true, stdin("  \n")).is_err());
}

#[test]
fn frontend_and_approver_selection() {
    let kcfg = Config::default().kernel().unwrap();
    assert_eq!(
        select(&parse(&[]), &kcfg),
        Selection {
            frontend: "repl".into(),
            approver: Some("repl".into())
        }
    );
    assert_eq!(
        select(&parse(&["-p", "x"]), &kcfg),
        Selection {
            frontend: "print".into(),
            approver: Some("auto-deny".into())
        }
    );
    assert_eq!(
        select(&parse(&["-p", "x", "--approver", "auto-allow"]), &kcfg),
        Selection {
            frontend: "print".into(),
            approver: Some("auto-allow".into())
        }
    );
    // select.frontend picks another interactive frontend; its approver comes from config.
    let kcfg = Config::parse("[select]\nfrontend = \"tui\"\n")
        .unwrap()
        .kernel()
        .unwrap();
    assert_eq!(
        select(&parse(&[]), &kcfg),
        Selection {
            frontend: "tui".into(),
            approver: None
        }
    );
}
