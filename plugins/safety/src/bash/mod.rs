//! A small shell-command splitter plus the built-in bash deny list and read-only
//! allowlist.
//!
//! This is not a shell parser. It splits a command line on `&&`, `||`, `;`, `;;`, `|`,
//! `|&`, `&`, `(`, `)` and newlines (respecting quotes), notes command substitutions and
//! file-writing redirections, and looks into `$(...)`, backticks, `<(...)`, `>(...)`,
//! `sh -c '...'` strings and `eval` arguments so the deny list can't be dodged by
//! nesting. Where it reads a command differently from the shell, the command can be
//! misjudged in either direction; `bash_pin.rs` records the known cases.

mod allowlist;
mod cmd;
mod deny;
mod lexer;

pub use allowlist::{ask_key, segment_is_read_only};
pub use cmd::Cmd;
pub use deny::denied;

/// How a segment is joined to the one before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    And,
    Or,
    Seq,
    Pipe,
    Background,
}

/// One simple command of a command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Segment {
    /// The source text of the segment, trimmed.
    pub raw: String,
    /// Words with quotes removed (redirections excluded).
    pub words: Vec<String>,
    pub op_before: Option<Op>,
    /// Contains `$(...)`, backticks, `<(...)` or `>(...)`.
    pub has_substitution: bool,
    /// Redirects output to a file (`>`, `>>`, `>|`, `&>`, `<>` or `>&file`), unless the
    /// target is an fd, `/dev/null`, `/dev/stdout` or `/dev/stderr`.
    pub writes_file: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Parsed {
    /// Top-level segments, in order.
    pub segments: Vec<Segment>,
    /// Segments found inside substitutions, `sh -c` strings and `eval` arguments, up to
    /// [`MAX_DEPTH`] levels down.
    pub nested: Vec<Segment>,
    /// Something is nested deeper than [`MAX_DEPTH`], so not every part was seen.
    pub too_deep: bool,
}

impl Parsed {
    pub fn all(&self) -> impl Iterator<Item = &Segment> {
        self.segments.iter().chain(self.nested.iter())
    }
}

const MAX_DEPTH: usize = 4;

pub fn parse(command: &str) -> Parsed {
    let mut parsed = Parsed::default();
    parse_into(command, 0, &mut parsed, false);
    parsed
}

fn parse_into(command: &str, depth: usize, out: &mut Parsed, nested: bool) {
    let (segments, substitution_bodies) = lexer::split(command);
    if depth < MAX_DEPTH {
        for seg in &segments {
            if let Some(script) = shell_script_arg(seg) {
                parse_into(&script, depth + 1, out, true);
            }
        }
        for sub in substitution_bodies {
            parse_into(&sub, depth + 1, out, true);
        }
    } else if !substitution_bodies.is_empty()
        || segments.iter().any(|s| shell_script_arg(s).is_some())
    {
        out.too_deep = true;
    }
    if nested {
        out.nested.extend(segments);
    } else {
        out.segments.extend(segments);
    }
}

fn shell_script_arg(seg: &Segment) -> Option<String> {
    let cmd = Cmd::of(seg);
    if cmd.program == "eval" {
        return Some(cmd.args.join(" "));
    }
    if !SHELLS.contains(&cmd.program) {
        return None;
    }
    let mut it = cmd.args.iter();
    while let Some(a) = it.next() {
        if a.starts_with('-') && !a.starts_with("--") && a.contains('c') {
            return it.find(|x| !x.starts_with('-')).cloned();
        }
    }
    None
}

fn is_harmless_target(target: &str) -> bool {
    matches!(target, "/dev/null" | "/dev/stdout" | "/dev/stderr")
}

const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish", "ash"];

#[cfg(test)]
mod tests {
    use super::*;

    fn programs(cmd: &str) -> Vec<String> {
        parse(cmd)
            .segments
            .iter()
            .map(|s| Cmd::of(s).program.to_string())
            .collect()
    }

    #[test]
    fn splits_on_operators_respecting_quotes() {
        assert_eq!(
            programs("cd src && ls | wc -l; echo 'a && b' || true & pwd"),
            ["cd", "ls", "wc", "echo", "true", "pwd"]
        );
        let p = parse("echo \"x; y\" | grep 'a|b'");
        assert_eq!(p.segments.len(), 2);
        assert_eq!(p.segments[0].words, ["echo", "x; y"]);
        assert_eq!(p.segments[1].words, ["grep", "a|b"]);
        assert_eq!(p.segments[1].op_before, Some(Op::Pipe));
    }

    #[test]
    fn raw_text_is_kept() {
        let p = parse("git commit -m \"fix it\" && npm test");
        assert_eq!(p.segments[0].raw, "git commit -m \"fix it\"");
        assert_eq!(p.segments[1].raw, "npm test");
    }

    #[test]
    fn redirections() {
        let p = parse("ls > out.txt");
        assert!(p.segments[0].writes_file);
        assert_eq!(p.segments[0].words, ["ls"]);
        assert!(!parse("ls 2>&1 >/dev/null").segments[0].writes_file);
        assert!(!parse("cat < in.txt").segments[0].writes_file);
        assert!(parse("echo hi >> log").segments[0].writes_file);
        assert!(parse("echo hi &> log").segments[0].writes_file);
    }

    #[test]
    fn substitutions_are_nested() {
        let p = parse("echo $(rm -rf /tmp/x) `whoami`");
        assert!(p.segments[0].has_substitution);
        let nested: Vec<_> = p.nested.iter().map(|s| Cmd::of(s).program).collect();
        assert_eq!(nested, ["rm", "whoami"]);
        let p = parse("bash -c 'sudo ls'");
        assert_eq!(Cmd::of(&p.nested[0]).program, "sudo");
    }

    #[test]
    fn heredoc_body_is_skipped() {
        let p = parse("cat <<EOF\nrm -rf /\nEOF\nls");
        let progs: Vec<_> = p.segments.iter().map(|s| Cmd::of(s).program).collect();
        assert_eq!(progs, ["cat", "ls"]);
    }

    #[test]
    fn odd_input_never_panics() {
        for cmd in [
            "",
            " ",
            "\\",
            "'",
            "\"",
            "$(",
            "`",
            "echo \"$(",
            "echo 'é ü' | wc",
            "a\\",
            "cat <<EOF",
            "cat <<",
            ">",
            "2>",
            "&>",
            "(((",
            ")))",
            "$(echo `ls`)",
            "ä$(ö)ü",
            "echo \"\\",
            "x=1",
            "|||",
            ";;",
            "&&&",
            "echo #comment\nls",
            "<(",
            "$((1+2))",
        ] {
            let p = parse(cmd);
            let _ = denied(cmd, &p);
            let _ = p.all().all(segment_is_read_only);
            for s in p.all() {
                let _ = ask_key(s);
            }
        }
    }
}
