//! The environment block: workspace, platform, date and git state.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How many `git status` entries to show.
const STATUS_ENTRIES: usize = 10;

/// The variables git gets from ch's environment; everything else, API keys included, is
/// left out.
const GIT_ENV: &[&str] = &["PATH", "HOME", "USER", "LANG", "LC_ALL", "TERM", "TMPDIR"];

/// Runs `git <args>` in `dir`, returning trimmed stdout on success. Returns `None` on
/// any failure, including a missing git binary, a nonzero exit or the timeout (the
/// process is killed).
///
/// The repository's own config can name commands git runs (`core.fsmonitor`, filter
/// drivers), and a workspace's `.git/config` may have been planted or written by the
/// model. So git gets a cleared environment, without the provider keys, and
/// fsmonitor is off.
pub fn git(dir: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new("git")
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .current_dir(dir)
        .env_clear()
        .envs(
            GIT_ENV
                .iter()
                .filter_map(|k| Some((k, std::env::var_os(k)?))),
        )
        // Don't take the index lock: the user may be running git at the same time.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Read on a thread so a large output can't fill the pipe and block the child.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let out = reader.join().ok()?;
    if !status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out).trim_end().to_string())
}

/// Git facts for the environment block.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GitInfo {
    pub branch: Option<String>,
    /// `git status --porcelain` lines, if the command worked.
    pub status: Option<Vec<String>>,
}

/// Collects branch and status. Each command that fails is left out.
pub fn git_info(dir: &Path, timeout: Duration) -> GitInfo {
    // `symbolic-ref` also works on a branch with no commits yet; fall back to the
    // commit hash when HEAD is detached.
    let branch = git(dir, &["symbolic-ref", "--short", "-q", "HEAD"], timeout)
        .filter(|b| !b.is_empty())
        .or_else(|| {
            git(dir, &["rev-parse", "--short", "HEAD"], timeout)
                .filter(|h| !h.is_empty())
                .map(|h| format!("(detached at {h})"))
        });
    let status = git(dir, &["status", "--porcelain"], timeout)
        .map(|s| s.lines().map(str::to_string).collect::<Vec<_>>());
    GitInfo { branch, status }
}

/// Renders the environment block.
pub fn render(workspace: &Path, git_root: Option<&Path>, git: Option<&GitInfo>) -> String {
    let date = chrono::Local::now().format("%Y-%m-%d (%A)");
    let mut out = String::from("# Environment\n\n");
    out.push_str(&format!("- Working directory: {}\n", workspace.display()));
    out.push_str(&format!(
        "- Platform: {} ({})\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    out.push_str(&format!("- Today's date: {date}\n"));
    match git_root {
        None => out.push_str("- Git repository: no\n"),
        Some(root) => {
            if root == workspace {
                out.push_str("- Git repository: yes\n");
            } else {
                out.push_str(&format!(
                    "- Git repository: yes (root: {})\n",
                    root.display()
                ));
            }
            if let Some(g) = git {
                if let Some(branch) = &g.branch {
                    out.push_str(&format!("- Current branch: {branch}\n"));
                }
                if let Some(status) = &g.status {
                    out.push_str(&status_summary(status));
                }
            }
        }
    }
    out.trim_end().to_string()
}

/// The `git status` part of the block. It is a snapshot from the start of the session.
fn status_summary(lines: &[String]) -> String {
    if lines.is_empty() {
        return "- Git status (at session start): clean\n".to_string();
    }
    let n = lines.len();
    let noun = if n == 1 { "entry" } else { "entries" };
    let mut s = format!("- Git status (at session start): {n} changed {noun}\n");
    for line in lines.iter().take(STATUS_ENTRIES) {
        s.push_str(&format!("  {line}\n"));
    }
    if n > STATUS_ENTRIES {
        s.push_str(&format!("  ... and {} more\n", n - STATUS_ENTRIES));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_summary_caps_entries() {
        let lines: Vec<String> = (0..13).map(|i| format!("?? f{i}")).collect();
        let s = status_summary(&lines);
        assert!(s.contains("13 changed entries"));
        assert!(s.contains("?? f9"));
        assert!(!s.contains("?? f10"));
        assert!(s.contains("... and 3 more"));
        assert!(status_summary(&[]).contains("clean"));
    }

    fn init(dir: &Path) {
        let ok = Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "git init failed");
    }

    #[test]
    fn git_gets_no_more_of_the_environment_than_it_needs() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path());
        // Cargo sets CARGO_MANIFEST_DIR for test runs; like an API key, it must not reach
        // git or what git runs.
        assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
        let env = git(
            dir.path(),
            &["-c", "alias.e=!env", "e"],
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(env.contains("PATH="), "{env}");
        assert!(!env.contains("CARGO_MANIFEST_DIR="), "{env}");
    }

    #[cfg(unix)]
    #[test]
    fn a_repository_fsmonitor_command_does_not_run() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        init(dir.path());
        let marker = dir.path().join("ran");
        let hook = dir.path().join("monitor.sh");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let set = Command::new("git")
            .args(["config", "core.fsmonitor"])
            .arg(&hook)
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(set.success());
        // Plain git runs it, so the check below means something.
        let _ = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(dir.path())
            .output();
        assert!(marker.exists(), "this git doesn't run fsmonitor hooks");
        std::fs::remove_file(&marker).unwrap();
        git(
            dir.path(),
            &["status", "--porcelain"],
            Duration::from_secs(5),
        );
        assert!(!marker.exists(), "the repository's fsmonitor command ran");
    }

    #[test]
    fn missing_git_binary_or_dir_is_none() {
        let dir = std::env::temp_dir().join("ch-definitely-missing-dir-xyz");
        assert_eq!(git(&dir, &["status"], Duration::from_secs(1)), None);
    }
}
