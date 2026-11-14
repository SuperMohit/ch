//! The subcommands: `build`, `run`, `report` and `compare`, and the hidden `proxy` and
//! `limit` that run inside containers.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, bail};

use crate::args::{Cli, Command, RunArgs};
use crate::docker::{
    BASE_IMAGE, BUILDER_IMAGE, DockerBackend, HEARTBEAT_SECS, Setup, arch, binary_paths,
    build_binaries, build_image, check_docker, check_mount_path, ensure_base_image,
    ensure_binaries, touch,
};
use crate::ledger::StateDir;
use crate::matrix::{PluginLists, cells, parse_vary};
use crate::proxy::{Keys, Provider, ProxySpec, UpstreamSpec};
use crate::report::{self, count};
use crate::run::{Matrix, Options, describe, key_env, run_matrix, upstream_url};
use crate::task::{Filter, load_suite, select};

/// The wait before a transiently failed run is first tried again; it doubles.
const BACKOFF: Duration = Duration::from_secs(30);

/// The source tree `ch-eval` was built from: where `eval/` is, and what the static
/// binaries are built from.
pub fn source_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .unwrap_or(manifest)
        .to_path_buf()
}

/// Runs a subcommand and returns the exit code.
pub async fn main_with(cli: Cli) -> anyhow::Result<i32> {
    match cli.command {
        Command::Build => build(),
        Command::Run(args) => run(*args).await,
        Command::Report { out } => {
            let results = report::write(&out)?;
            println!(
                "ch-eval: wrote {} and results.json ({})",
                out.join("report.md").display(),
                count(results.runs, "run")
            );
            Ok(0)
        }
        Command::Compare {
            dirs,
            to,
            allow_different_config,
        } => {
            let results = report::compare(&dirs, &to, allow_different_config)?;
            println!(
                "ch-eval: wrote {} and results.json ({} from {})",
                to.join("report.md").display(),
                count(results.runs, "run"),
                count(dirs.len(), "directory")
            );
            Ok(0)
        }
        Command::Proxy { state, listen } => crate::proxy::main(&state, &listen).await,
        Command::Limit { secs, command } => {
            Ok(crate::limit::run(Duration::from_secs(secs), &command).await)
        }
    }
}

fn build() -> anyhow::Result<i32> {
    let source = source_root();
    check_docker()?;
    build_image(&source, BASE_IMAGE, "Dockerfile")?;
    build_image(&source, BUILDER_IMAGE, "builder.Dockerfile")?;
    let bins = build_binaries(&source, arch())?;
    println!(
        "ch-eval: images {BASE_IMAGE} and {BUILDER_IMAGE} are built; ch is at {} and \
         ch-eval at {}",
        bins.ch.display(),
        bins.ch_eval.display()
    );
    Ok(0)
}

/// The matrix `args` describe: the selected tasks, the cells, the base config. Reads
/// files but doesn't touch Docker.
pub fn prepare(args: &RunArgs) -> anyhow::Result<Matrix> {
    let suite = args
        .suite
        .clone()
        .unwrap_or_else(|| source_root().join("eval/tasks"));
    let filter = Filter {
        ids: args.tasks.clone(),
        tags: args.tags.clone(),
    };
    let tasks = select(load_suite(&suite)?, &filter)?;

    // The text goes to each run as it is; the table is only read here.
    let (config, table) = match &args.config {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading --config {}", path.display()))?;
            let table: toml::Table = text
                .parse()
                .with_context(|| format!("parsing --config {}", path.display()))?;
            (Some(text), table)
        }
        None => (None, toml::Table::new()),
    };
    if args.profile.is_some() && config.is_none() {
        bail!("--profile names a profile in the base config, so it needs --config");
    }
    let lists = PluginLists::from_config(&table, args.profile.as_deref())?;
    let dims = args
        .vary
        .iter()
        .map(|v| parse_vary(v))
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(Matrix {
        tasks,
        cells: cells(&dims, &lists)?,
        repeats: args.repeat,
        profile: args.profile.clone(),
        config,
    })
}

/// What the cells' configs need from the proxy.
#[derive(Debug, Clone, PartialEq)]
pub struct Needs {
    /// What the proxy forwards where: only the providers the cells use.
    pub spec: ProxySpec,
    /// The providers the cells' models (and summarizers) use, with the variable that
    /// holds each one's key.
    pub providers: BTreeMap<Provider, String>,
    /// Models with no pricing.
    pub unpriced: BTreeSet<String>,
    /// Pricing that is likely not what was meant.
    pub warnings: Vec<String>,
}

/// Reads each cell's config as `ch` would, and works out what the proxy needs. A cell
/// can't move a provider's endpoint or key variable: every run's calls go through one
/// proxy, to the base config's upstreams.
pub fn needs(m: &Matrix, max_cost: Option<f64>) -> anyhow::Result<Needs> {
    let base = m.base()?;
    let mut providers = BTreeMap::new();
    let mut unpriced = BTreeSet::new();
    let mut warnings = Vec::new();
    for (i, cell) in m.cells.iter().enumerate() {
        let config = m.cell_config(i)?;
        for p in [Provider::Anthropic, Provider::OpenAi] {
            if upstream_url(&config, p) != upstream_url(&base, p)
                || key_env(&config, p) != key_env(&base, p)
            {
                bail!(
                    "cell {} changes provider-{}'s base_url or api_key_env; every run's model \
                     calls go through one proxy, to the base config's endpoints",
                    cell.name,
                    p.name()
                );
            }
        }
        let models = m.cell_models(i)?;
        warnings.extend(models.warnings);
        for p in models.providers {
            providers.insert(p, key_env(&base, p));
        }
        unpriced.extend(
            models
                .models
                .into_iter()
                .filter(|(_, pricing)| pricing.is_none())
                .map(|(model, _)| model),
        );
    }
    if max_cost.is_some() && !unpriced.is_empty() {
        let list: Vec<&str> = unpriced.iter().map(String::as_str).collect();
        bail!(
            "--max-cost needs a price for every model the cells use, and these have none: \
             {}. Set [model.pricing] for the configured model, or [eval.pricing.\"<model>\"] \
             in the base config",
            list.join(", ")
        );
    }
    let upstream = |p: Provider| {
        providers.contains_key(&p).then(|| UpstreamSpec {
            base_url: upstream_url(&base, p),
        })
    };
    Ok(Needs {
        spec: ProxySpec {
            anthropic: upstream(Provider::Anthropic),
            openai: upstream(Provider::OpenAi),
            max_cost,
            heartbeat_secs: Some(HEARTBEAT_SECS),
        },
        providers,
        unpriced,
        warnings,
    })
}

/// The keys the proxy needs, read from this process's environment. A missing one is an
/// error before anything runs.
pub fn keys(needs: &Needs) -> anyhow::Result<Keys> {
    let mut keys = Keys::default();
    for (p, var) in &needs.providers {
        let value = match std::env::var(var) {
            Ok(v) if !v.trim().is_empty() => v,
            _ => bail!(
                "{var} is not set, and the proxy needs it to call the {} API. Export it, or \
                 point plugins.provider-{}.api_key_env in the base config at the variable \
                 that holds the key",
                p.name(),
                p.name()
            ),
        };
        match p {
            Provider::Anthropic => keys.anthropic = Some(value),
            Provider::OpenAi => keys.openai = Some(value),
        }
    }
    Ok(keys)
}

async fn run(args: RunArgs) -> anyhow::Result<i32> {
    if args
        .max_cost
        .is_some_and(|max| !(max.is_finite() && max > 0.0))
    {
        bail!("--max-cost must be a positive number of USD");
    }
    let matrix = prepare(&args)?;
    let needs = needs(&matrix, args.max_cost)?;
    let out = std::path::absolute(&args.out)
        .with_context(|| format!("resolving --out {}", args.out.display()))?;
    let source = source_root();
    let given_ch = match &args.ch {
        Some(p) => Some(std::path::absolute(p).with_context(|| format!("--ch {}", p.display()))?),
        None => None,
    };
    let state = StateDir(out.join("proxy"));

    if args.dry_run {
        let mut binaries = binary_paths(&source, arch());
        if let Some(ch) = given_ch {
            binaries.ch = ch;
        }
        let backend = DockerBackend::describe_only(binaries, state)?;
        print!("{}", describe(&matrix, &backend, &out)?);
        return Ok(0);
    }

    let keys = keys(&needs)?;
    for warning in &needs.warnings {
        eprintln!("ch-eval: warning: {warning}");
    }
    if !needs.unpriced.is_empty() {
        let list: Vec<&str> = needs.unpriced.iter().map(String::as_str).collect();
        eprintln!(
            "ch-eval: note: no pricing for {}, so their calls cost 0 in the report",
            list.join(", ")
        );
    }
    check_docker()?;
    check_mount_path(&out)?;
    let mut binaries = ensure_binaries(&source, arch())?;
    if let Some(ch) = given_ch {
        if !ch.is_file() {
            bail!("--ch {}: no such file", ch.display());
        }
        binaries.ch = ch;
    }
    ensure_base_image(&source)?;
    std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    let mut images: Vec<String> = matrix.tasks.iter().map(|t| t.image.clone()).collect();
    images.sort();
    images.dedup();
    let (backend, cleanup) = DockerBackend::start(Setup {
        binaries,
        images,
        state: state.clone(),
        spec: needs.spec.clone(),
        keys,
    })
    .await?;
    let backend = Arc::new(backend);
    // Keeps the proxy alive: touches its heartbeat, and starts it again if it stopped.
    let supervisor = {
        let (backend, heartbeat) = (backend.clone(), state.heartbeat());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(3)).await;
                let _ = touch(&heartbeat);
                if let Err(e) = backend.supervise().await {
                    eprintln!("ch-eval: {e:#}");
                }
            }
        })
    };

    let opts = Options {
        out: out.clone(),
        jobs: args.jobs as usize,
        max_cost: args.max_cost,
        max_attempts: args.max_attempts,
        backoff: BACKOFF,
    };
    let matrix = run_matrix(Arc::new(matrix), backend, &opts);
    tokio::pin!(matrix);
    let finished = tokio::select! {
        r = &mut matrix => r,
        () = shutdown_signal() => {
            // Stopping the containers ends each run's steps; waiting for the runs lets
            // them clean their directories.
            cleanup.run();
            let _ = tokio::time::timeout(Duration::from_secs(30), &mut matrix).await;
            supervisor.abort();
            eprintln!(
                "ch-eval: interrupted; the containers and networks are removed. Runs \
                 without a run.json are made again by the next `ch-eval run` with this --out."
            );
            let _ = report::write(&out);
            return Ok(130);
        }
    };
    supervisor.abort();
    cleanup.run();
    // The report covers whatever was recorded, also when the matrix stopped early.
    let written = report::write(&out);
    let summary = finished?;
    let results = written?;

    println!(
        "ch-eval: ran {} ({} done already); the proxy recorded ${:.4} for this --out",
        count(summary.ran, "run"),
        summary.done_before,
        summary.recorded_cost
    );
    if summary.retried > 0 {
        println!(
            "ch-eval: {} tried again after the model API failed",
            count(summary.retried, "run")
        );
    }
    if summary.usage_missing > 0 {
        println!(
            "ch-eval: warning: in {}, the model API answered without usage, so those \
             calls' cost isn't counted{}",
            count(summary.usage_missing, "run"),
            if args.max_cost.is_some() {
                ", and --max-cost doesn't see it"
            } else {
                ""
            }
        );
    }
    if summary.stopped_by_max_cost > 0 {
        println!(
            "ch-eval: --max-cost reached, so {} didn't finish; run again with the same \
             --out and a higher --max-cost to finish",
            count(summary.stopped_by_max_cost, "run")
        );
    }
    for cell in &results.cells {
        println!("  {}: {}/{} passed", cell.name, cell.passed, cell.runs);
    }
    println!("ch-eval: report in {}", out.join("report.md").display());
    Ok(0)
}

/// Ctrl-C or SIGTERM.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).ok();
    let terminated = async {
        match term.as_mut() {
            Some(t) => {
                t.recv().await;
            }
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = terminated => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::Cell;

    fn matrix(config: &str, cells: Vec<Cell>, profile: Option<&str>) -> Matrix {
        Matrix {
            tasks: vec![],
            cells,
            repeats: 1,
            profile: profile.map(String::from),
            config: Some(config.into()),
        }
    }

    fn cell(settings: &[&str]) -> Cell {
        Cell {
            name: "c".into(),
            settings: settings.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn needs_follow_the_cells_configs() {
        let config =
            "[model]\nprovider = \"openai\"\nid = \"gpt-x\"\n[model.pricing]\ninput = 1.0\n";
        let n = needs(&matrix(config, vec![cell(&[])], None), Some(5.0)).unwrap();
        assert_eq!(
            n.providers,
            BTreeMap::from([(Provider::OpenAi, "OPENAI_API_KEY".to_string())])
        );
        assert!(n.unpriced.is_empty());
        assert_eq!(n.spec.max_cost, Some(5.0));
        // Only the providers in use get an upstream; the others' routes are refused.
        assert!(n.spec.openai.is_some() && n.spec.anthropic.is_none());

        // The scripted provider needs no key; a summarizer on Anthropic does.
        let scripted = "[model]\nprovider = \"scripted\"\n[model.summarizer]\nprovider = \"anthropic\"\nid = \"m\"\n";
        let n = needs(&matrix(scripted, vec![cell(&[])], None), None).unwrap();
        assert_eq!(
            n.providers.keys().copied().collect::<Vec<_>>(),
            [Provider::Anthropic]
        );
        assert_eq!(n.unpriced, BTreeSet::from(["m".to_string()]));
        // --max-cost without pricing for every model can't work.
        assert!(needs(&matrix(scripted, vec![cell(&[])], None), Some(1.0)).is_err());
        // A cell can't move an endpoint.
        let moved = cell(&[r#"plugins.provider-anthropic.base_url="http://x""#]);
        assert!(needs(&matrix(config, vec![moved], None), None).is_err());
    }

    #[test]
    fn missing_keys_are_errors() {
        let n = Needs {
            spec: ProxySpec::default(),
            providers: BTreeMap::from([(
                Provider::OpenAi,
                "CH_EVAL_TEST_SURELY_UNSET_KEY".to_string(),
            )]),
            unpriced: BTreeSet::new(),
            warnings: Vec::new(),
        };
        let err = keys(&n).unwrap_err().to_string();
        assert!(err.contains("CH_EVAL_TEST_SURELY_UNSET_KEY"), "{err}");
        let none = Needs {
            providers: BTreeMap::new(),
            ..n
        };
        assert_eq!(keys(&none).unwrap(), Keys::default());
    }

    #[test]
    fn the_profile_shapes_the_base_plugin_lists() {
        let dir = tempfile::tempdir().unwrap();
        let task = dir.path().join("suite/t");
        std::fs::create_dir_all(task.join("repo")).unwrap();
        std::fs::write(
            task.join("task.toml"),
            "instruction = \"x\"\nverify = \"false\"\n",
        )
        .unwrap();
        let config = dir.path().join("c.toml");
        std::fs::write(
            &config,
            "[plugins]\nenable = [\"provider-scripted\"]\n\
             [profiles.lean.plugins]\nenable = [\"provider-scripted\", \"spill\"]\n",
        )
        .unwrap();
        let suite = dir.path().join("suite");
        let parse = |extra: &[&str]| {
            let mut argv = vec![
                "ch-eval",
                "run",
                "--out",
                "o",
                "--suite",
                suite.to_str().unwrap(),
                "--config",
                config.to_str().unwrap(),
                "--vary",
                "planning=on",
            ];
            argv.extend(extra);
            let cli = <Cli as clap::Parser>::try_parse_from(argv).unwrap();
            let Command::Run(args) = cli.command else {
                panic!()
            };
            prepare(&args).unwrap()
        };
        assert_eq!(
            parse(&[]).cells[0].settings,
            [r#"plugins.enable=["provider-scripted", "planning"]"#]
        );
        assert_eq!(
            parse(&["--profile", "lean"]).cells[0].settings,
            [r#"plugins.enable=["provider-scripted", "spill", "planning"]"#]
        );
    }

    #[test]
    fn the_source_root_holds_the_eval_directory() {
        assert!(source_root().join("eval/docker/Dockerfile").is_file());
        assert!(source_root().join("crates/ch-eval").is_dir());
    }
}
