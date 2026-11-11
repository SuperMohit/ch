//! Checkers: the configured command for one file extension, its probe and how a run is
//! read.

use std::time::Duration;

use ch_kernel::{Cx, ExecRequest, ExecResult};

use crate::findings::{Finding, parse};

/// One extension's checker, from config.
#[derive(Debug, Clone)]
pub struct Checker {
    /// The command; `{path}` is filled in per file. It reads the content on stdin.
    template: String,
    /// The program the command starts, for the probe.
    program: String,
    timeout: Duration,
}

/// Why a run produced no findings to compare.
pub type Failure = String;

impl Checker {
    pub fn new(command: &str, timeout: Duration) -> Self {
        // The first word that isn't a `NAME=value` assignment.
        let program = shell_words::split(command)
            .unwrap_or_default()
            .into_iter()
            .find(|word| !is_assignment(word))
            .unwrap_or_default();
        Self {
            template: command.into(),
            program,
            timeout,
        }
    }

    /// The command for `rel`, with the path shell-quoted.
    pub fn command(&self, rel: &str) -> String {
        self.template.replace("{path}", &shell_words::quote(rel))
    }

    /// Checks that the command's program can be found. `Err` says why not, for the user.
    pub async fn probe(&self, cx: &Cx) -> Result<(), Failure> {
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
        if found.exit_code != Some(0) || found.stdout.trim().is_empty() {
            return Err(format!("`{program}` was not found"));
        }
        Ok(())
    }

    /// Checks `content` as the file `rel`. Exit 0 means no findings; any other exit
    /// except 126 and 127 may carry them, and must, or the run counts as failed.
    pub async fn run(&self, cx: &Cx, rel: &str, content: &str) -> Result<Vec<Finding>, Failure> {
        let command = self.command(rel);
        let result = self
            .exec(cx, command.clone(), Some(content.to_string()))
            .await?;
        let findings = match result.exit_code {
            Some(0) => return Ok(Vec::new()),
            Some(126 | 127) | None => Vec::new(),
            Some(_) => parse(&result.stdout, &result.stderr),
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
            env: Vec::new(),
            stdin,
        };
        executor
            .run(request, cx.cancel.clone())
            .await
            .map_err(|e| format!("it didn't start: {e:#}"))
    }
}

/// Whether `word` sets a variable for the command after it (`NAME=value`).
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_path_is_quoted_and_the_program_found() {
        let checker = Checker::new(
            "npx --no-install eslint --stdin --stdin-filename={path}",
            Duration::from_secs(1),
        );
        assert_eq!(checker.program, "npx");
        assert_eq!(
            checker.command("a b/$(x).ts"),
            "npx --no-install eslint --stdin --stdin-filename='a b/$(x).ts'"
        );
        let t = Duration::from_secs(1);
        assert_eq!(Checker::new("A=1 _B2=x lint --x=1 -", t).program, "lint");
        assert_eq!(Checker::new("./bin/lint=x -", t).program, "./bin/lint=x");
        assert_eq!(Checker::new("A=1", t).program, "");
    }
}
