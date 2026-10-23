//! The built-in read-only allowlist, and the rule "always allow" offers for the rest.

use super::cmd::git_subcommand;
use super::{Cmd, Segment};

/// Programs that only read, whatever their arguments (output redirection and
/// substitutions are checked separately).
const READ_ONLY_PROGRAMS: &[&str] = &[
    "ls", "pwd", "cat", "head", "tail", "wc", "grep", "egrep", "fgrep", "echo", "which", "file",
    "stat", "du", "df", "whoami", "uname", "basename", "dirname", "realpath", "readlink", "nl",
    "true", "false", "cmp", "diff", "id", "cd", "type", "tac", "cut", "jq",
];

/// `git` subcommands that only read.
const READ_ONLY_GIT: &[&str] = &[
    "status",
    "diff",
    "log",
    "show",
    "rev-parse",
    "ls-files",
    "blame",
    "shortlog",
    "describe",
];

/// Whether one segment is on the built-in read-only allowlist.
pub fn segment_is_read_only(seg: &Segment) -> bool {
    if seg.has_substitution || seg.writes_file {
        return false;
    }
    let cmd = Cmd::of(seg);
    // Environment variables can make an allowlisted program run another one
    // (`GIT_CONFIG_*` setting `core.fsmonitor`, `HOME` pointing git at another
    // `.gitconfig`, `LD_PRELOAD`, `RUSTC_WRAPPER`), so only locale and display
    // settings keep a command read-only.
    if !cmd.assignments.iter().all(|a| is_display_assignment(a)) {
        return false;
    }
    let p = cmd.program;
    if p.is_empty() {
        // An empty segment (e.g. between `(` and `)`) does nothing.
        return cmd.assignments.is_empty();
    }
    if READ_ONLY_PROGRAMS.contains(&p) {
        return true;
    }
    match p {
        "rg" => !cmd.has_arg(|a| a.starts_with("--pre")),
        "find" => !cmd.has_arg(|a| {
            matches!(
                a,
                "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir" | "-fls" | "-fprint"
            ) || a.starts_with("-fprint")
        }),
        "tree" => !cmd.has_arg(|a| a == "-o"),
        "sort" => !cmd.has_arg(|a| a == "-o" || a.starts_with("--output")),
        "date" => !cmd.has_arg(|a| a == "-s" || a.starts_with("--set")),
        "git" => git_is_read_only(&cmd),
        "cargo" => matches!(
            cmd.args.first().map(String::as_str),
            Some("check" | "metadata" | "tree" | "--version" | "-V")
        ),
        _ => false,
    }
}

fn is_display_assignment(assignment: &str) -> bool {
    let name = assignment.split_once('=').map_or(assignment, |(n, _)| n);
    name.starts_with("LC_")
        || matches!(
            name,
            "LANG" | "LANGUAGE" | "TZ" | "NO_COLOR" | "TERM" | "COLUMNS" | "LINES"
        )
}

fn git_is_read_only(cmd: &Cmd<'_>) -> bool {
    // `-c key=value` can set a pager or hook that runs arbitrary code.
    if cmd.args.iter().any(|a| a == "-c") {
        return false;
    }
    let Some((sub, rest)) = git_subcommand(cmd) else {
        // `git --version` and friends.
        return true;
    };
    if rest
        .iter()
        .any(|a| a.starts_with("--output") || a == "--ext-diff")
    {
        return false;
    }
    match sub {
        s if READ_ONLY_GIT.contains(&s) => true,
        "branch" => rest.iter().all(|a| {
            matches!(
                a.as_str(),
                "-a" | "-r" | "-v" | "-vv" | "--all" | "--remotes" | "--list" | "--show-current"
            ) || a.starts_with("--contains")
                || a.starts_with("--merged")
                || a.starts_with("--no-merged")
                || a.starts_with("--sort")
                || a.starts_with("--format")
        }),
        "remote" => rest.iter().all(|a| a == "-v" || a == "--verbose"),
        "stash" => matches!(rest.first().map(String::as_str), Some("list" | "show")),
        _ => false,
    }
}

/// The allow rule that "always allow" for this segment would add, e.g.
/// `bash:git commit *` for `git commit -m x`, `bash:make *` for `make`.
pub fn ask_key(seg: &Segment) -> String {
    let cmd = Cmd::of(seg);
    let mut key = format!("bash:{}", globset::escape(cmd.program));
    if let Some(second) = cmd.args.first()
        && !second.starts_with('-')
    {
        key.push(' ');
        key.push_str(&globset::escape(second));
    }
    key.push_str(" *");
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bash::parse;

    fn ro(cmd: &str) -> bool {
        parse(cmd).all().all(segment_is_read_only)
    }

    #[test]
    fn read_only_allowlist() {
        for cmd in [
            "ls -la",
            "git status",
            "git diff HEAD~1 -- src",
            "git log --oneline -5",
            "git branch",
            "git branch -a",
            "rg foo src",
            "find . -name '*.rs'",
            "cat a.txt | grep x | wc -l",
            "cd src && ls",
            "cargo check",
            "echo hi 2>&1",
            "ls >/dev/null",
            "head -n 5 a; tail b",
            "LANG=C ls",
            "LC_ALL=C sort a",
            "NO_COLOR=1 git log",
        ] {
            assert!(ro(cmd), "should be read-only: {cmd}");
        }
        for cmd in [
            "rm a",
            "ls && rm a",
            "echo hi > a.txt",
            "find . -delete",
            "find . -exec rm {} ;",
            "git commit -m x",
            "git branch new-feature",
            "git -c core.pager=evil log",
            "rg --pre ./x foo",
            "env FOO=1 make",
            "echo $(rm a)",
            "cargo build",
            "npm test",
            "sort -o out in",
            "LD_PRELOAD=x.so ls",
            "env LD_PRELOAD=x.so cat a",
            "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.fsmonitor GIT_CONFIG_VALUE_0=x git status",
            "env GIT_CONFIG_GLOBAL=./x git status",
            "HOME=. git status",
            "RUSTC_WRAPPER=./x cargo check",
            "env",
            "printenv",
            "printenv ANTHROPIC_API_KEY",
        ] {
            assert!(!ro(cmd), "should not be read-only: {cmd}");
        }
    }

    #[test]
    fn ask_keys() {
        let key = |c: &str| ask_key(&parse(c).segments[0]);
        assert_eq!(key("git commit -m x"), "bash:git commit *");
        assert_eq!(key("npm test"), "bash:npm test *");
        assert_eq!(key("make"), "bash:make *");
        assert_eq!(key("pytest -x tests"), "bash:pytest *");
        assert_eq!(key("FOO=1 npm run build"), "bash:npm run *");
        assert_eq!(key("ls *.rs"), "bash:ls [*].rs *");
    }
}
