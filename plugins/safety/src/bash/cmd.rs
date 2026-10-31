use super::Segment;

const KEYWORDS: &[&str] = &[
    "!", "{", "}", "if", "then", "else", "elif", "fi", "do", "done", "while", "until",
];

const WRAPPERS: &[&str] = &[
    "env", "nohup", "time", "nice", "command", "exec", "builtin", "timeout", "xargs", "stdbuf",
];

/// Wrapper options whose value is the next word unless it is in the same word
/// (`-sKILL`, `--signal=KILL`): the short options, then the long ones.
const WRAPPER_VALUE_OPTIONS: &[(&str, &str, &[&str])] = &[
    ("env", "uCSP", &["--unset", "--chdir", "--split-string"]),
    ("timeout", "sk", &["--signal", "--kill-after"]),
    ("nice", "n", &["--adjustment"]),
    (
        "xargs",
        "adEILnPs",
        &[
            "--arg-file",
            "--delimiter",
            "--eof",
            "--max-lines",
            "--max-args",
            "--max-procs",
            "--max-chars",
        ],
    ),
    ("stdbuf", "ioe", &["--input", "--output", "--error"]),
    ("time", "fo", &["--format", "--output"]),
    ("exec", "a", &[]),
];

/// The `env` short options that take a value.
const ENV_VALUE_OPTIONS: &str = "uCSP";

/// A segment seen as `program args...`, after skipping keywords, `VAR=value`
/// assignments and wrappers like `env`, `timeout` or `xargs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd<'a> {
    /// The program's base name (`/usr/bin/git` → `git`); empty when the segment has
    /// none (`fi`, `''`, a lone redirection).
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
        'words: loop {
            while i < words.len() && KEYWORDS.contains(&words[i].as_str()) {
                i += 1;
            }
            while i < words.len() && is_assignment(&words[i]) {
                assignments.push(words[i].as_str());
                i += 1;
            }
            let Some(w) = words.get(i) else { break };
            let wrapper = basename(w);
            if WRAPPERS.contains(&wrapper) {
                let mut j = i + 1;
                while j < words.len()
                    && (words[j].starts_with('-')
                        || is_assignment(&words[j])
                        || words[j].chars().next().is_some_and(|c| c.is_ascii_digit()))
                {
                    if is_assignment(&words[j]) {
                        assignments.push(words[j].as_str());
                    }
                    if !words[j].starts_with('-') {
                        j += 1;
                    } else if wrapper == "env" && is_split_string(&words[j]) {
                        // `env -S '<command line>'` is the command; its script is nested.
                        break 'words;
                    } else {
                        j += option_words(wrapper, &words[j]);
                    }
                }
                // A bare wrapper (`env`, `time`) is the command itself.
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

    /// `git`'s subcommand and its arguments, skipping global options.
    pub(super) fn git_subcommand(&self) -> Option<(&'a str, &'a [String])> {
        let i = self.git_globals().1?;
        Some((self.args[i].as_str(), &self.args[i + 1..]))
    }

    /// The `key=value` settings given by git's `-c` and `--config-env` options.
    pub(super) fn git_config(&self) -> Vec<&'a str> {
        self.git_globals().0
    }

    /// Walks git's global options: the settings they give, and where the subcommand is.
    fn git_globals(&self) -> (Vec<&'a str>, Option<usize>) {
        let args = self.args;
        let mut config = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let a = args[i].as_str();
            if let Some(setting) = a.strip_prefix("--config-env=") {
                config.push(setting);
                i += 1;
            } else if a == "-c" || a == "--config-env" {
                config.extend(args.get(i + 1).map(String::as_str));
                i += 2;
            } else if matches!(a, "-C" | "--git-dir" | "--work-tree" | "--namespace") {
                i += 2;
            } else if a.starts_with('-') {
                i += 1;
            } else {
                return (config, Some(i));
            }
        }
        (config, None)
    }

    /// The command line given to `env -S` (`--split-string`), with the words after it.
    pub(super) fn env_split_string(&self) -> Option<String> {
        let mut it = self.args.iter();
        while let Some(a) = it.next() {
            let attached = match a.strip_prefix("--split-string=") {
                Some(value) => Some(value),
                None if a == "--split-string" => Some(""),
                None => value_option(ENV_VALUE_OPTIONS, a)
                    .and_then(|(c, rest)| (c == 'S').then_some(rest)),
            };
            let Some(attached) = attached else {
                continue;
            };
            let mut script = if attached.is_empty() {
                it.next().cloned().unwrap_or_default()
            } else {
                attached.to_string()
            };
            for rest in it {
                script.push(' ');
                script.push_str(rest);
            }
            return Some(script);
        }
        None
    }
}

/// How many words the wrapper option `word` takes up: 2 when its value is the next word.
fn option_words(wrapper: &str, word: &str) -> usize {
    let Some(&(_, short, long)) = WRAPPER_VALUE_OPTIONS.iter().find(|(w, ..)| *w == wrapper) else {
        return 1;
    };
    let takes_next = if word.starts_with("--") {
        long.contains(&word)
    } else {
        value_option(short, word).is_some_and(|(_, rest)| rest.is_empty())
    };
    if takes_next { 2 } else { 1 }
}

/// In a short-option cluster such as `-iu`, the first option among `short` that takes a
/// value, and the rest of the word, which is its value (empty: the value is the next word).
fn value_option<'w>(short: &str, word: &'w str) -> Option<(char, &'w str)> {
    let cluster = word.strip_prefix('-').filter(|c| !c.starts_with('-'))?;
    let (i, c) = cluster.char_indices().find(|&(_, c)| short.contains(c))?;
    Some((c, &cluster[i + c.len_utf8()..]))
}

fn is_split_string(word: &str) -> bool {
    word == "--split-string"
        || word.starts_with("--split-string=")
        || value_option(ENV_VALUE_OPTIONS, word).is_some_and(|(c, _)| c == 'S')
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

    #[test]
    fn wrapper_option_values_are_not_the_program() {
        let program = |cmd: &str| {
            let p = parse(cmd);
            Cmd::of(&p.segments[0]).program.to_string()
        };
        for (cmd, want) in [
            ("timeout -s KILL 5 rm -rf /", "rm"),
            ("timeout --signal KILL -k 1 5 rm x", "rm"),
            ("timeout -sKILL 5 rm x", "rm"),
            ("env -u X sudo ls", "sudo"),
            ("env -iu X sudo ls", "sudo"),
            ("env -C /tmp sudo ls", "sudo"),
            ("nice -n 5 rm x", "rm"),
            ("nice -n5 rm x", "rm"),
            ("xargs -I {} rm {}", "rm"),
            ("xargs -n 1 -P 4 rm", "rm"),
            ("stdbuf -o L grep x", "grep"),
            ("exec -a name sudo ls", "sudo"),
            ("env -S 'sudo ls'", "env"),
            ("timeout -s KILL", "timeout"),
        ] {
            assert_eq!(program(cmd), want, "{cmd}");
        }
    }

    #[test]
    fn env_split_string_is_a_command_line() {
        let script = |cmd: &str| {
            let p = parse(cmd);
            Cmd::of(&p.segments[0]).env_split_string()
        };
        assert_eq!(script("env -S 'sudo ls' -l").as_deref(), Some("sudo ls -l"));
        assert_eq!(script("env -iS 'sudo ls'").as_deref(), Some("sudo ls"));
        assert_eq!(script("env -S'sudo ls'").as_deref(), Some("sudo ls"));
        assert_eq!(
            script("env --split-string='sudo ls'").as_deref(),
            Some("sudo ls")
        );
        assert_eq!(script("env -i ls"), None);
    }

    #[test]
    fn git_config_comes_from_global_options() {
        let config = |cmd: &str| {
            let p = parse(cmd);
            Cmd::of(&p.segments[0])
                .git_config()
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            config("git -C r -c a.b=1 --config-env=c.d=E --config-env e.f=G log -c x"),
            ["a.b=1", "c.d=E", "e.f=G"]
        );
        assert!(config("git commit -c alias.p=push").is_empty());
    }
}
