//! Project MCP servers through the `ch` binary: they start only once trusted, `-p`
//! never asks, and a user-level server with the same name wins.
//!
//! The "server" is `touch <marker>`: it fails the handshake, but the marker shows that
//! ch started it.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Fixture {
    _dir: tempfile::TempDir,
    ws: PathBuf,
    home: PathBuf,
    script: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("proj").canonicalize_or_create();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let script = dir.path().join("script.json");
    std::fs::write(&script, r#"[{"text": "done"}]"#).unwrap();
    std::fs::write(
        ws.join(".mcp.json"),
        r#"{"mcpServers": {"proj": {"command": "/usr/bin/touch", "args": ["proj-started"]}}}"#,
    )
    .unwrap();
    Fixture {
        _dir: dir,
        ws,
        home,
        script,
    }
}

trait CreateDir {
    fn canonicalize_or_create(self) -> PathBuf;
}

impl CreateDir for PathBuf {
    fn canonicalize_or_create(self) -> PathBuf {
        std::fs::create_dir_all(self.join(".ch")).unwrap();
        self.canonicalize().unwrap()
    }
}

fn run(f: &Fixture, extra: &[&str]) -> Output {
    let mut args: Vec<String> = vec![
        "-p".into(),
        "hi".into(),
        "--cwd".into(),
        f.ws.display().to_string(),
        "--set".into(),
        "model.provider=\"scripted\"".into(),
        "--set".into(),
        "plugins.enable=[\"provider-scripted\"]".into(),
        "--set".into(),
        format!(
            "plugins.provider-scripted.script=\"{}\"",
            f.script.display()
        ),
    ];
    args.extend(extra.iter().map(|s| s.to_string()));
    Command::new(env!("CARGO_BIN_EXE_ch"))
        .args(&args)
        .env("CH_HOME", &f.home)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn started(ws: &Path, marker: &str) -> bool {
    ws.join(marker).exists()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn print_mode_never_starts_an_untrusted_project_server() {
    let f = fixture();
    let out = run(&f, &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!started(&f.ws, "proj-started"));
    let err = stderr(&out);
    assert!(
        err.contains("not starting the project's MCP servers `proj`"),
        "{err}"
    );

    // `--trust-project` covers settings, not servers.
    let out = run(&f, &["--trust-project"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!started(&f.ws, "proj-started"));

    // `--trust-project-servers` starts them for this run and records nothing.
    let out = run(&f, &["--trust-project-servers"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(started(&f.ws, "proj-started"), "{}", stderr(&out));
    let store = std::fs::read_to_string(f.home.join("trusted-projects.toml")).unwrap_or_default();
    assert!(!store.contains("[[server]]"), "{store}");
}

#[test]
fn servers_in_the_project_config_need_their_own_trust() {
    let f = fixture();
    std::fs::write(
        f.ws.join(".ch/config.toml"),
        "[[mcp_servers]]\nname = \"cfg\"\ncommand = \"/usr/bin/touch\"\nargs = [\"cfg-started\"]\n",
    )
    .unwrap();
    // No restricted settings: the servers aren't part of the project's settings.
    let out = run(&f, &[]);
    let err = stderr(&out);
    assert!(!err.contains("need your trust:"), "{err}");
    assert!(err.contains("`cfg`") && err.contains("`proj`"), "{err}");
    assert!(!started(&f.ws, "cfg-started"));

    let out = run(&f, &["--trust-project-servers"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(started(&f.ws, "cfg-started"));
}

#[test]
fn a_user_server_with_the_same_name_wins() {
    let f = fixture();
    std::fs::write(
        f.home.join("config.toml"),
        "[[mcp_servers]]\nname = \"proj\"\ncommand = \"/usr/bin/touch\"\nargs = [\"user-started\"]\ncwd = \"workspace\"\n",
    )
    .unwrap();
    let out = run(&f, &["--trust-project-servers"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(started(&f.ws, "user-started"));
    assert!(!started(&f.ws, "proj-started"));
    assert!(stderr(&out).contains("yours wins"), "{}", stderr(&out));
}
