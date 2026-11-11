//! Checkers: the command for one file extension, its probe and how a run is read.

use std::path::Path;
use std::time::Duration;

use ch_kernel::{Cx, ExecRequest, ExecResult};

use crate::edition::rust_edition;
use crate::findings::{Finding, Parser};

/// The built-in `.rs` checker. Only the exit code and stderr count: valid code exits 0
/// however it is formatted, and `mod x;` isn't followed on stdin.
const RUSTFMT: &str = "rustfmt --edition {edition} --emit stdout >/dev/null";

/// The built-in `.py` checker: syntax errors (always reported), undefined names and
/// statements in the wrong place. `--select` replaces the project's own rule set.
const RUFF: &str = "ruff check --no-fix --force-exclude --output-format concise \
                    --select F63,F7,F82 --stdin-filename={path} -";

/// One extension's checker.
#[derive(Debug, Clone)]
pub struct Checker {
    /// The command; `{path}` and `{edition}` are filled in per file.
    template: String,
    /// The program the command starts, for the probe.
    program: String,
    /// The built-in checkers run programs the repository can't choose: they are probed
    /// with `--version`, turned off if they resolve inside the workspace, and only exit
    /// 1 means findings.
    builtin: bool,
    env: Vec<(String, String)>,
    parser: Parser,
    timeout: Duration,
}

/// Why a run produced no findings to compare.
pub type Failure = String;

impl Checker {
    /// The built-in checker for `ext`, if there is one.
    pub fn builtin(ext: &str, rust_toolchain: &str, timeout: Duration) -> Option<Self> {
        let (template, program, env, parser) = match ext {
            // The toolchain is pinned so a `rust-toolchain.toml` in the repository can't
            // pick the program (its `path` may point into the repository).
            "rs" => (
                RUSTFMT,
                "rustfmt",
                vec![
                    ("RUSTUP_TOOLCHAIN".into(), rust_toolchain.to_string()),
                    ("RUSTUP_AUTO_INSTALL".into(), "0".into()),
                ],
                Parser::Rustfmt,
            ),
            "py" => (RUFF, "ruff", Vec::new(), Parser::Ruff),
            _ => return None,
        };
        Some(Self {
            template: template.into(),
            program: program.into(),
            builtin: true,
            env,
            parser,
            timeout,
        })
    }

    /// A checker from the user's config.
    pub fn custom(command: &str, timeout: Duration) -> Self {
        let program = shell_words::split(command)
            .ok()
            .and_then(|words| words.into_iter().next())
            .unwrap_or_default();
        Self {
            template: command.into(),
            program,
            builtin: false,
            env: Vec::new(),
            parser: Parser::Generic,
            timeout,
        }
    }

    /// The command for `rel`.
    pub fn command(&self, root: &Path, rel: &str) -> String {
        let mut command = self.template.replace("{path}", &shell_words::quote(rel));
        if command.contains("{edition}") {
            command = command.replace("{edition}", &rust_edition(root, rel));
        }
        command
    }

    /// Checks that the checker can run here. `Err` says why it can't, for the user.
    pub async fn probe(&self, cx: &Cx, root: &Path) -> Result<(), Failure> {
        let program = &self.program;
        if program.is_empty() {
            return Err("its command names no program".into());
        }
        let found = self
            .exec(
                cx,
                format!("command -v {}", shell_words::quote(program)),
                None,
            )
            .await?;
        let location = found.stdout.trim();
        if found.exit_code != Some(0) || location.is_empty() {
            return Err(format!("`{program}` was not found"));
        }
        if !self.builtin {
            return Ok(());
        }
        let resolved = std::fs::canonicalize(root.join(location));
        if resolved
            .as_deref()
            .unwrap_or(Path::new(location))
            .starts_with(root)
        {
            return Err(format!(
                "`{program}` resolves to `{location}`, inside the workspace, so the \
                 repository could choose what runs"
            ));
        }
        let version = self
            .exec(
                cx,
                format!("{} --version", shell_words::quote(program)),
                None,
            )
            .await?;
        if version.exit_code != Some(0) {
            let why = version.stderr.lines().next().unwrap_or("").trim();
            return Err(format!("`{program} --version` failed: {why}"));
        }
        Ok(())
    }

    /// Checks `content` as the file `rel`.
    pub async fn run(
        &self,
        cx: &Cx,
        root: &Path,
        rel: &str,
        content: &str,
    ) -> Result<Vec<Finding>, Failure> {
        let command = self.command(root, rel);
        let result = self
            .exec(cx, command.clone(), Some(content.to_string()))
            .await?;
        let findings_code = match result.exit_code {
            Some(0) => return Ok(Vec::new()),
            Some(1) => true,
            Some(126 | 127) | None => false,
            Some(_) => !self.builtin,
        };
        let findings = if findings_code {
            self.parser.parse(rel, &result.stdout, &result.stderr)
        } else {
            Vec::new()
        };
        if findings.is_empty() {
            let why = match result.exit_code {
                Some(code) => format!("exited with {code}"),
                None if result.timed_out => "timed out".into(),
                None => "was stopped".into(),
            };
            let first = result.stderr.lines().next().unwrap_or("").trim();
            return Err(format!(
                "`{command}` {why} without findings to read: {first}"
            ));
        }
        Ok(findings)
    }

    async fn exec(
        &self,
        cx: &Cx,
        command: String,
        stdin: Option<String>,
    ) -> Result<ExecResult, Failure> {
        let executor = cx.executor.as_ref().ok_or("there is no executor")?;
        let request = ExecRequest {
            command,
            cwd: cx.workspace().to_path_buf(),
            timeout: self.timeout,
            env: self.env.clone(),
            stdin,
        };
        executor
            .run(request, cx.cancel.clone())
            .await
            .map_err(|e| format!("it didn't start: {e:#}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_quoted_and_the_edition_filled_in() {
        let t = Duration::from_secs(1);
        let root = Path::new("/nonexistent");
        let py = Checker::builtin("py", "stable", t).unwrap();
        assert!(
            py.command(root, "a b/-x.py")
                .contains("--stdin-filename='a b/-x.py' -")
        );
        let rs = Checker::builtin("rs", "stable", t).unwrap();
        assert_eq!(
            rs.command(root, "src/lib.rs"),
            "rustfmt --edition 2021 --emit stdout >/dev/null"
        );
        assert!(Checker::builtin("ts", "stable", t).is_none());
        let custom = Checker::custom("npx --no-install eslint --stdin {path}", t);
        assert_eq!(custom.program, "npx");
        assert_eq!(
            custom.command(root, "$(x).ts"),
            "npx --no-install eslint --stdin '$(x).ts'"
        );
    }
}
