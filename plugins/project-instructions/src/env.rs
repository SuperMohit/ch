//! The environment block: workspace, platform, date and git state.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How many `git status` entries to show.
const STATUS_ENTRIES: usize = 10;

/// Runs `git <args>` in `dir`, returning trimmed stdout on success. Returns `None` on
/// any failure, including a missing git binary, a nonzero exit or the timeout (the
/// process is killed).
pub fn git(dir: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(dir)
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

    #[test]
    fn missing_git_binary_or_dir_is_none() {
        let dir = std::env::temp_dir().join("ch-definitely-missing-dir-xyz");
        assert_eq!(git(&dir, &["status"], Duration::from_secs(1)), None);
    }
}
