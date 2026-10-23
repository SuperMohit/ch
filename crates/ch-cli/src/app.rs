//! Wiring a run together: config, registry, session, frontend.

use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use anyhow::{Context as _, bail};
use ch_kernel::{
    ChHome, KernelConfig, Plugin, RegistryBuilder, Services, Session, SessionMeta, SessionStore,
};

use crate::args::Cli;
use crate::config::{
    Overrides, Paths, ProjectTrust, build_config, ch_home, describe_paths, resolve_workspace,
    trust_saved_rule,
};
use crate::list::format_plugin_list;
use crate::plugins::{OnRuleSaved, print_options, repl_options};
use crate::sessions::{Resume, format_session_list, load, parse_choice};
use crate::trust::{ProjectSettings, TrustStore, describe};

/// Which frontend and approver to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub frontend: String,
    /// `None` leaves it to `select.approver` (or the only registered approver).
    pub approver: Option<String>,
}

/// `print` with the `--approver` choice for `-p`; otherwise `select.frontend` (default
/// `repl`), with the `repl` approver when the frontend is the REPL.
pub fn select(cli: &Cli, kcfg: &KernelConfig) -> Selection {
    if cli.is_print() {
        return Selection {
            frontend: "print".into(),
            approver: Some(cli.approver.as_str().into()),
        };
    }
    let frontend = kcfg
        .select
        .frontend
        .clone()
        .unwrap_or_else(|| "repl".into());
    let approver = if frontend == "repl" {
        Some("repl".into())
    } else {
        None
    };
    Selection { frontend, approver }
}

/// What piped stdin is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdinUse {
    /// It is the task (`-p -`, or no prompt given): read all of it.
    Task,
    /// It is added to a prompt given on the command line.
    Extra,
}

/// The task for `-p`: the `-p` value or PROMPT, with piped stdin appended (or used
/// alone when the prompt is `-` or missing).
pub fn resolve_print_prompt(
    cli: &Cli,
    stdin_piped: bool,
    read_stdin: impl FnOnce(StdinUse) -> anyhow::Result<String>,
) -> anyhow::Result<String> {
    let given = match (&cli.print, &cli.prompt) {
        (Some(Some(p)), Some(rest)) => bail!(
            "`-p` took `{p}` as the task and `{rest}` was left over; quote the whole task: \
             ch -p \"{p} {rest}\""
        ),
        (Some(Some(p)), None) => Some(p.clone()),
        (_, prompt) => prompt.clone(),
    };
    let prompt = match given.as_deref() {
        Some("-") => read_stdin(StdinUse::Task)?,
        Some(p) if stdin_piped => {
            let input = read_stdin(StdinUse::Extra)?;
            if input.trim().is_empty() {
                p.to_string()
            } else {
                format!("{p}\n\n{}", input.trim_end())
            }
        }
        Some(p) => p.to_string(),
        None if stdin_piped => read_stdin(StdinUse::Task)?,
        None => String::new(),
    };
    if prompt.trim().is_empty() {
        bail!("no task given: use `ch -p \"<task>\"`, or pipe the task on stdin");
    }
    Ok(prompt)
}

/// How long piped stdin may stay silent before a prompt from the command line runs
/// without it.
const EXTRA_STDIN_WAIT: Duration = Duration::from_secs(3);

fn read_process_stdin(usage: StdinUse) -> anyhow::Result<String> {
    let wait = match usage {
        StdinUse::Task => None,
        StdinUse::Extra => Some(EXTRA_STDIN_WAIT),
    };
    match read_unless_silent(std::io::stdin(), wait).context("reading stdin")? {
        Some(text) => Ok(text),
        None => {
            eprintln!(
                "ch: nothing arrived on stdin within {}s, so the task runs without it. \
                 Redirect stdin from /dev/null to skip the wait.",
                EXTRA_STDIN_WAIT.as_secs()
            );
            Ok(String::new())
        }
    }
}

/// Reads `reader` to the end, or returns `None` if its first byte doesn't arrive within
/// `wait`: a stdin that is open but never written (ssh, a parent process) must not
/// block the run. The reader thread is left blocked in that case.
fn read_unless_silent(
    mut reader: impl Read + Send + 'static,
    wait: Option<Duration>,
) -> anyhow::Result<Option<String>> {
    enum Progress {
        Started,
        Done(std::io::Result<Vec<u8>>),
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = vec![0; 8192];
        let n = match reader.read(&mut bytes) {
            Ok(n) => n,
            Err(e) => {
                let _ = tx.send(Progress::Done(Err(e)));
                return;
            }
        };
        bytes.truncate(n);
        if n > 0 {
            let _ = tx.send(Progress::Started);
            if let Err(e) = reader.read_to_end(&mut bytes) {
                let _ = tx.send(Progress::Done(Err(e)));
                return;
            }
        }
        let _ = tx.send(Progress::Done(Ok(bytes)));
    });
    let first = match wait {
        Some(wait) => match rx.recv_timeout(wait) {
            Ok(p) => p,
            Err(mpsc::RecvTimeoutError::Timeout) => return Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("the stdin reader stopped"),
        },
        None => rx.recv().context("the stdin reader stopped")?,
    };
    let done = match first {
        Progress::Started => rx.recv().context("the stdin reader stopped")?,
        done => done,
    };
    let Progress::Done(bytes) = done else {
        bail!("the stdin reader stopped");
    };
    Ok(Some(
        String::from_utf8(bytes?).context("stdin is not UTF-8")?,
    ))
}

/// Prints the numbered session list to stderr and reads the choice from stdin.
fn pick_interactively(sessions: &[SessionMeta]) -> anyhow::Result<usize> {
    if !std::io::stdin().is_terminal() {
        bail!("choose a session with `ch --resume <ID>` (stdin is not a terminal)");
    }
    let mut err = std::io::stderr();
    let _ = write!(
        err,
        "{}Resume which session? [1-{}]: ",
        format_session_list(sessions),
        sessions.len()
    );
    let _ = err.flush();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("reading the choice")?;
    parse_choice(&line, sessions.len())
}

/// Shows the project's restricted settings on stderr and asks whether to trust them.
fn confirm_trust(settings: &[(String, String)]) -> anyhow::Result<bool> {
    let mut err = std::io::stderr();
    let _ = writeln!(
        err,
        "ch: this project's .ch/config.toml changes settings that need your trust:"
    );
    for (key, value) in settings {
        let _ = writeln!(err, "  {key} = {value}");
    }
    let _ = write!(
        err,
        "These can loosen permissions, run commands, read files outside the project or \
         send your API key to another server.\nTrust them for this project? [y/N]: "
    );
    let _ = err.flush();
    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .context("reading the answer")?;
    let yes = matches!(answer.trim(), "y" | "Y" | "yes" | "Yes");
    if yes {
        let _ = writeln!(
            err,
            "ch: trusted. You'll be asked again if these settings change."
        );
    }
    Ok(yes)
}

/// Where `-p` may read its task from besides the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stdin {
    /// The process's stdin: read when it is not a terminal.
    Process,
    /// Fixed text, as if piped (for tests).
    Text(String),
    /// Nothing is piped.
    None,
}

/// Runs `ch` with `plugins`, keeping files under `$CH_HOME` (or `~/.ch`). Returns the
/// process exit code.
pub async fn run(cli: Cli, plugins: Vec<Arc<dyn Plugin>>) -> anyhow::Result<i32> {
    run_with(cli, plugins, ch_home()?, Stdin::Process).await
}

/// [`run`] with an explicit ch home directory (config, sessions) and stdin source.
pub async fn run_with(
    cli: Cli,
    plugins: Vec<Arc<dyn Plugin>>,
    home: PathBuf,
    stdin: Stdin,
) -> anyhow::Result<i32> {
    let workspace = resolve_workspace(cli.cwd.as_deref())?;
    let paths = Paths {
        home,
        workspace: workspace.clone(),
    };
    if cli.config_path {
        print!("{}", describe_paths(&paths));
        return Ok(0);
    }
    if cli.cwd.is_some() {
        std::env::set_current_dir(&workspace)
            .with_context(|| format!("changing to {}", workspace.display()))?;
    }

    let overrides = Overrides::from_cli(&cli);
    let project_settings = ProjectSettings::of(&plugins);
    let mut loaded = build_config(&paths, &overrides, &project_settings)?;
    if let ProjectTrust::Untrusted { settings, hash } = &loaded.project {
        let interactive = matches!(stdin, Stdin::Process)
            && !cli.is_print()
            && !cli.list_plugins
            && std::io::stdin().is_terminal()
            && std::io::stderr().is_terminal();
        if interactive {
            if confirm_trust(&describe(settings))? {
                TrustStore::new(&paths.home).trust(&workspace, hash)?;
                loaded = build_config(&paths, &overrides, &project_settings)?;
            } else {
                eprintln!("ch: not applying them this session.");
            }
        } else {
            let keys: Vec<String> = describe(settings).into_iter().map(|(k, _)| k).collect();
            eprintln!(
                "ch: ignoring settings in .ch/config.toml that need your trust: {}. Run `ch` \
                 in a terminal to review and trust them, or pass --trust-project.",
                keys.join(", ")
            );
        }
    }
    // After the user saves an "always" rule in the REPL, keep the project trusted if
    // that rule is the only change (only when trust was given before, not per run).
    let trusted_settings = match &loaded.project {
        ProjectTrust::NothingToTrust => Some(toml::Table::new()),
        ProjectTrust::Trusted {
            settings,
            saved: true,
        } => Some(settings.clone()),
        _ => None,
    };
    let config = loaded.config;
    tracing::info!(files = ?loaded.files, workspace = %workspace.display(), "config loaded");
    let kcfg = config.kernel()?;

    if cli.list_plugins {
        let registry = RegistryBuilder::build(plugins, &config, &workspace)?;
        print!("{}", format_plugin_list(&registry));
        return Ok(0);
    }

    // Options for the frontends travel as services in the registry.
    let mut services = Services::default();
    services.provide(Arc::new(ChHome(paths.home.clone())));
    let selection = select(&cli, &kcfg);
    if cli.is_print() {
        let prompt = match stdin {
            Stdin::Process => {
                resolve_print_prompt(&cli, !std::io::stdin().is_terminal(), read_process_stdin)?
            }
            Stdin::Text(text) => resolve_print_prompt(&cli, true, move |_| Ok(text))?,
            Stdin::None => resolve_print_prompt(&cli, false, |_| Ok(String::new()))?,
        };
        print_options(
            &mut services,
            prompt,
            cli.output_format.as_str(),
            cli.verbose,
        )?;
    } else {
        let on_rule_saved = trusted_settings.map(|settings| {
            let paths = paths.clone();
            let trusted = std::sync::Mutex::new(settings);
            Box::new(move |rule: &str| {
                let mut trusted = trusted.lock().unwrap_or_else(|p| p.into_inner());
                if let Err(e) = trust_saved_rule(&paths, &project_settings, &mut trusted, rule) {
                    tracing::warn!("keeping the project trusted after saving a rule: {e:#}");
                }
            }) as OnRuleSaved
        });
        repl_options(&mut services, cli.prompt.clone(), on_rule_saved);
    }
    let registry = Arc::new(RegistryBuilder::build_with_services(
        plugins, &config, &workspace, services,
    )?);
    let frontend = registry.frontend(Some(&selection.frontend))?;

    let store = SessionStore::for_workspace(&paths.sessions(), &workspace);
    let resume = Resume::from_flags(cli.continue_last, cli.resume.clone());
    let snapshot = load(&store, &resume, pick_interactively)?;
    if resume == Resume::Latest && snapshot.is_none() {
        eprintln!("ch: no earlier session in this project; starting a new one");
    }

    let mut builder = Session::builder(registry.clone(), Arc::new(config))
        .workspace(workspace)
        .store(store);
    if let Some(approver) = &selection.approver {
        builder = builder.approver(approver.clone());
    }
    if let Some(snap) = snapshot {
        builder = builder.restore(snap);
    }
    if let Some(model) = &cli.model {
        builder = builder.model(model.clone());
    }
    let session = builder.build()?;
    frontend.run(session).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn piped_stdin_that_stays_silent_is_skipped() {
        let wait = Some(Duration::from_millis(50));
        let (reader, writer) = std::io::pipe().unwrap();
        assert_eq!(read_unless_silent(reader, wait).unwrap(), None);
        drop(writer);

        let (reader, mut writer) = std::io::pipe().unwrap();
        writer.write_all(b"diff text").unwrap();
        let slow = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            writer.write_all(b" and more").unwrap();
        });
        assert_eq!(
            read_unless_silent(reader, wait).unwrap().as_deref(),
            Some("diff text and more")
        );
        slow.join().unwrap();

        let (reader, writer) = std::io::pipe().unwrap();
        drop(writer);
        assert_eq!(
            read_unless_silent(reader, wait).unwrap().as_deref(),
            Some("")
        );
        assert_eq!(
            read_unless_silent(&b"task"[..], None).unwrap().as_deref(),
            Some("task")
        );
    }
}
