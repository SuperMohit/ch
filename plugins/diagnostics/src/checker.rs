//! Checkers: the configured command for one file extension, its probe and how a run is
//! read.

use std::time::Duration;

use ch_kernel::{Cx, ExecRequest, ExecResult};

use crate::findings::{Finding, parse};

/// The variable that holds the file's path while a checker runs. `{path}` in a command
/// becomes `"$CH_PATH"`, so a file name is never parsed by the shell.
const PATH_VAR: &str = "CH_PATH";

/// Findings kept from one run; the rest are dropped.
const MAX_FINDINGS: usize = 200;

/// One extension's checker, from config.
#[derive(Debug, Clone)]
pub struct Checker {
    /// The command as configured; `{path}` marks the file's path.
    template: String,
    /// The program the command starts, for the probe.
    program: String,
    /// Exit codes that mean "findings" (anything else but 0 is a failure).
    findings_exit: Vec<i32>,
    timeout: Duration,
}

/// Why a run gave no findings to compare.
#[derive(Debug, Clone, PartialEq)]
pub enum Failure {
    /// The turn was cancelled while it ran: no verdict on the checker.
    Cancelled,
    /// The checker couldn't run or its output couldn't be read.
    Failed(String),
}

impl Checker {
    pub fn new(command: &str, findings_exit: Vec<i32>, timeout: Duration) -> Self {
        // The first word that isn't a `NAME=value` assignment.
        let program = shell_words::split(command)
            .unwrap_or_default()
            .into_iter()
            .find(|word| !is_assignment(word))
            .unwrap_or_default();
        Self {
            template: command.into(),
            program,
            findings_exit,
            timeout,
        }
    }

    /// The command for `rel` as the model would send it to bash, with the path
    /// shell-quoted: what the permission policy decides on and what messages show.
    pub fn command(&self, rel: &str) -> String {
        self.template.replace("{path}", &shell_words::quote(rel))
    }

    /// Checks that the command's program can be found.
    pub async fn probe(&self, cx: &Cx) -> Result<(), Failure> {
        let program = &self.program;
        if program.is_empty() {
            return Err(Failure::Failed("its command names no program".into()));
        }
        let command = format!("command -v {}", shell_words::quote(program));
        let found = self.exec(cx, command, Vec::new(), None).await?;
        if found.exit_code != Some(0) || found.stdout.trim().is_empty() {
            return Err(Failure::Failed(format!("`{program}` was not found")));
        }
        Ok(())
    }

    /// Checks `content` as the file `rel`. Exit 0 means no findings; an exit code in
    /// `findings_exit` must come with findings to read; anything else is a failure.
    pub async fn run(&self, cx: &Cx, rel: &str, content: &str) -> Result<Vec<Finding>, Failure> {
        let script = self.template.replace("{path}", &format!("\"${PATH_VAR}\""));
        let env = vec![
            (PATH_VAR.to_string(), rel.to_string()),
            ("NO_COLOR".to_string(), "1".to_string()),
        ];
        let result = self
            .exec(cx, script, env, Some(content.to_string()))
            .await?;
        let mut findings = match result.exit_code {
            Some(0) => return Ok(Vec::new()),
            Some(code) if self.findings_exit.contains(&code) => {
                parse(rel, &result.stdout, &result.stderr)
            }
            _ => Vec::new(),
        };
        if findings.is_empty() {
            let why = match result.exit_code {
                Some(code) => format!("exited with {code}"),
                None if result.timed_out => "timed out".into(),
                None => "was stopped".into(),
            };
            let first = result.stderr.lines().next().unwrap_or("").trim();
            return Err(Failure::Failed(format!(
                "`{}` {why} without findings to read: {first}",
                self.command(rel)
            )));
        }
        findings.truncate(MAX_FINDINGS);
        Ok(findings)
    }

    async fn exec(
        &self,
        cx: &Cx,
        command: String,
        env: Vec<(String, String)>,
        stdin: Option<String>,
    ) -> Result<ExecResult, Failure> {
        let executor = cx
            .executor
            .as_ref()
            .ok_or_else(|| Failure::Failed("there is no executor".into()))?;
        let request = ExecRequest {
            command,
            cwd: cx.workspace().to_path_buf(),
            timeout: self.timeout,
            env,
            stdin,
        };
        let result = executor.run(request, cx.cancel.clone()).await;
        if cx.cancel.is_cancelled() || result.as_ref().is_ok_and(|r| r.cancelled) {
            return Err(Failure::Cancelled);
        }
        result.map_err(|e| Failure::Failed(format!("it didn't start: {e:#}")))
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
    fn the_program_skips_assignments_and_the_path_is_quoted() {
        let t = Duration::from_secs(1);
        let checker = Checker::new("lint --stdin --name={path}", vec![1], t);
        assert_eq!(checker.program, "lint");
        assert_eq!(
            checker.command("a b/$(x).ts"),
            "lint --stdin --name='a b/$(x).ts'"
        );
        assert_eq!(
            Checker::new("A=1 _B2=x lint --x=1 -", vec![1], t).program,
            "lint"
        );
        assert_eq!(
            Checker::new("./bin/lint=x -", vec![1], t).program,
            "./bin/lint=x"
        );
        assert_eq!(Checker::new("A=1", vec![1], t).program, "");
    }
}
