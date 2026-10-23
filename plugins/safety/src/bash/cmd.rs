use super::Segment;

const KEYWORDS: &[&str] = &[
    "!", "{", "}", "if", "then", "else", "elif", "fi", "do", "done", "while", "until",
];

const WRAPPERS: &[&str] = &[
    "env", "nohup", "time", "nice", "command", "exec", "builtin", "timeout", "xargs", "stdbuf",
];

/// A segment seen as `program args...`, after skipping keywords, `VAR=value`
/// assignments and wrappers like `env`, `timeout` or `xargs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd<'a> {
    /// The program's base name (`/usr/bin/git` → `git`); empty for an empty segment.
    pub program: &'a str,
    pub args: &'a [String],
    /// Leading `VAR=value` assignments.
    pub assignments: Vec<&'a str>,
}

impl<'a> Cmd<'a> {
    pub fn of(seg: &'a Segment) -> Cmd<'a> {
        let words = &seg.words[..];
        let mut i = 0;
        let mut assignments = Vec::new();
        loop {
            while i < words.len() && KEYWORDS.contains(&words[i].as_str()) {
                i += 1;
            }
            while i < words.len() && is_assignment(&words[i]) {
                assignments.push(words[i].as_str());
                i += 1;
            }
            let Some(w) = words.get(i) else { break };
            if WRAPPERS.contains(&basename(w)) {
                let mut j = i + 1;
                while j < words.len()
                    && (words[j].starts_with('-')
                        || is_assignment(&words[j])
                        || words[j].chars().next().is_some_and(|c| c.is_ascii_digit()))
                {
                    if is_assignment(&words[j]) {
                        assignments.push(words[j].as_str());
                    }
                    j += 1;
                }
                if j >= words.len() {
                    break;
                }
                i = j;
                continue;
            }
            break;
        }
        match words.get(i) {
            Some(p) => Cmd {
                program: basename(p),
                args: &words[i + 1..],
                assignments,
            },
            None => Cmd {
                program: "",
                args: &[],
                assignments,
            },
        }
    }

    /// The command as words joined by single spaces, for rule matching.
    pub fn normalized(&self) -> String {
        let mut s = self.program.to_string();
        for a in self.args {
            s.push(' ');
            s.push_str(a);
        }
        s
    }

    pub(super) fn has_flag(&self, short: char, long: &str) -> bool {
        self.args.iter().any(|a| {
            a == long || (a.starts_with('-') && !a.starts_with("--") && a[1..].contains(short))
        })
    }

    pub(super) fn has_arg(&self, pred: impl Fn(&str) -> bool) -> bool {
        self.args.iter().any(|a| pred(a))
    }
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn is_assignment(word: &str) -> bool {
    match word.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !name.starts_with(|c: char| c.is_ascii_digit())
        }
        None => false,
    }
}

pub(super) fn git_subcommand<'a>(cmd: &Cmd<'a>) -> Option<(&'a str, &'a [String])> {
    let args = cmd.args;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "-C" || a == "-c" || a == "--git-dir" || a == "--work-tree" || a == "--namespace" {
            i += 2;
        } else if a.starts_with('-') {
            i += 1;
        } else {
            return Some((a, &args[i + 1..]));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bash::parse;

    #[test]
    fn wrappers_and_assignments_are_skipped() {
        let p = parse("FOO=1 timeout 10 env -i sudo ls");
        assert_eq!(Cmd::of(&p.segments[0]).program, "sudo");
        let p = parse("env");
        assert_eq!(Cmd::of(&p.segments[0]).program, "env");
        let p = parse("/usr/bin/git status");
        assert_eq!(Cmd::of(&p.segments[0]).program, "git");
    }
}
