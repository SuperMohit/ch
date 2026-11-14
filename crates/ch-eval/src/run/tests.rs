use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ch_kernel::Config;
use serde_json::json;

use super::schedule::MAX_BACKOFF;
use super::*;
use crate::backend::{Backend, Exit, Io, Job, ProxyRoute, Step};
use crate::events::{self, Stream};
use crate::ledger::{self, Call, RunCalls, StateDir};
use crate::matrix::{Cell, PluginLists, cells, parse_vary};
use crate::proxy::Provider;
use crate::record::{NO_RESULT, TIMEOUT};

fn stream(lines: &[serde_json::Value]) -> Stream {
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    events::parse(&text)
}

fn exit(code: Option<i32>) -> Exit {
    Exit {
        code,
        ..Default::default()
    }
}

fn base_cell() -> Cell {
    Cell {
        name: "base".into(),
        ..Default::default()
    }
}

#[test]
fn timeout_wins_over_a_result_line() {
    let s = stream(&[json!({"type": "result", "reason": "done", "steps": 3})]);
    let killed = Exit {
        timed_out: true,
        ..Default::default()
    };
    let (reason, detail) = reason_of(&killed, &s, Duration::from_secs(9));
    assert_eq!(reason, TIMEOUT);
    assert_eq!(detail.as_deref(), Some("killed after 9s"));
    assert_eq!(reason_of(&exit(Some(0)), &s, Duration::ZERO).0, "done");
    assert_eq!(
        reason_of(&exit(Some(1)), &stream(&[]), Duration::ZERO).0,
        NO_RESULT
    );
}

#[test]
fn classifies_from_what_the_runner_and_proxy_saw() {
    let cell = base_cell();
    let calls = |forwarded, succeeded, last_status: Option<u16>, transient| RunCalls {
        forwarded,
        succeeded,
        last_status,
        last_transient: transient,
        ..Default::default()
    };
    let ok = calls(3, 3, Some(200), false);
    assert_eq!(
        classify(&exit(Some(0)), Some(&ok), true, None, &cell),
        Verdict::Record
    );
    // A container's own exit code 125 is not Docker failing to start it.
    assert_eq!(
        classify(&exit(Some(125)), Some(&ok), true, None, &cell),
        Verdict::Record
    );
    let not_started = Exit {
        start_error: Some("exec format error".into()),
        ..Default::default()
    };
    assert!(matches!(
        classify(&not_started, Some(&ok), true, None, &cell),
        Verdict::Infra(m) if m.contains("exec format")
    ));
    // No model call made through the proxy: the run never reached the model.
    let none = calls(0, 0, None, false);
    assert!(matches!(
        classify(&exit(Some(1)), Some(&none), true, None, &cell),
        Verdict::Infra(m) if m.contains("before its first model call")
    ));
    // Calls made, none succeeded: a misspelled model, no credit, a request too big.
    let rejected = RunCalls {
        last_error: Some("model: claude-typo".into()),
        ..calls(1, 0, Some(404), false)
    };
    assert!(matches!(
        classify(&exit(Some(1)), Some(&rejected), true, None, &cell),
        Verdict::Infra(m) if m.contains("HTTP 404: model: claude-typo")
    ));
    // Without a proxy in the path (the scripted provider), that's the agent's.
    assert_eq!(
        classify(&exit(Some(1)), Some(&none), false, None, &cell),
        Verdict::Record
    );
    assert_eq!(
        classify(&exit(Some(1)), None, true, None, &cell),
        Verdict::Record
    );
    // An error after a transient failure is tried again, after real work too.
    let failing = calls(6, 5, Some(529), true);
    assert!(matches!(
        classify(&exit(Some(1)), Some(&failing), true, None, &cell),
        Verdict::Transient(m) if m.contains("529")
    ));
    // A run that ended normally is the agent's, whatever the API did last.
    for code in [Some(0), Some(2), Some(3), Some(6), Some(130)] {
        assert_eq!(
            classify(&exit(code), Some(&failing), true, None, &cell),
            Verdict::Record
        );
    }
    let killed = Exit {
        timed_out: true,
        ..Default::default()
    };
    assert_eq!(
        classify(&killed, Some(&failing), true, None, &cell),
        Verdict::Record
    );
    let refused = RunCalls {
        auth_failed: true,
        ..calls(1, 0, Some(401), false)
    };
    assert!(matches!(
        classify(&exit(Some(1)), Some(&refused), true, None, &cell),
        Verdict::Infra(m) if m.contains("refused the key")
    ));
    let cut = RunCalls {
        cut_by_max_cost: true,
        ..calls(2, 2, Some(200), false)
    };
    assert_eq!(
        classify(&exit(Some(1)), Some(&cut), true, None, &cell),
        Verdict::CutByMaxCost
    );
}

#[test]
fn the_backoff_doubles_per_attempt() {
    let base = Duration::from_secs(30);
    assert_eq!(backoff(base, 1), Duration::from_secs(30));
    assert_eq!(backoff(base, 2), Duration::from_secs(60));
    assert_eq!(backoff(base, 3), Duration::from_secs(120));
    assert_eq!(backoff(base, 40), MAX_BACKOFF);
}

#[test]
fn a_run_whose_plugins_differ_from_its_cell_is_not_recorded() {
    let dims = vec![parse_vary("planning=off").unwrap()];
    let cell = cells(&dims, &PluginLists::default()).unwrap().remove(0);
    let ok = ["tools-fs".to_string()];
    assert_eq!(
        classify(&exit(Some(0)), None, false, Some(&ok), &cell),
        Verdict::Record
    );
    let wrong = ["tools-fs".to_string(), "planning".to_string()];
    assert!(matches!(
        classify(&exit(Some(0)), None, false, Some(&wrong), &cell),
        Verdict::Infra(m) if m.contains("planning")
    ));
}

#[test]
fn the_preflight_refuses_checks_that_pass_or_cannot_run() {
    for code in [0, 124, 125, 126, 127] {
        assert!(preflight_problem(&exit(Some(code))).is_some(), "{code}");
    }
    assert!(preflight_problem(&exit(None)).is_some());
    for code in [1, 2, 123, 128] {
        assert_eq!(preflight_problem(&exit(Some(code))), None, "{code}");
    }
}

#[test]
fn proxy_settings_route_both_providers_and_keep_official_behaviour() {
    let base = Config::parse("").unwrap();
    let s = proxy_settings("http://ch-eval-proxy:8080", &base);
    assert_eq!(
        s,
        [
            r#"plugins.provider-anthropic.base_url="http://ch-eval-proxy:8080/anthropic""#,
            r#"plugins.provider-anthropic.api_key_env="CH_EVAL_API_KEY""#,
            r#"plugins.provider-openai.base_url="http://ch-eval-proxy:8080/openai""#,
            r#"plugins.provider-openai.api_key_env="CH_EVAL_API_KEY""#,
            "plugins.provider-anthropic.official_api=true",
            r#"plugins.provider-openai.max_tokens_field="max_completion_tokens""#,
        ]
    );
    let custom = Config::parse(
        "[plugins.provider-anthropic]\nbase_url = \"http://gateway:9\"\n\
         [plugins.provider-openai]\nbase_url = \"http://vllm:8000/v1\"\n",
    )
    .unwrap();
    let s = proxy_settings("http://p:1", &custom);
    assert!(s.contains(&"plugins.provider-anthropic.official_api=false".to_string()));
    assert!(s.contains(&r#"plugins.provider-openai.max_tokens_field="max_tokens""#.to_string()));
    let explicit = Config::parse(
        "[plugins.provider-anthropic]\nofficial_api = true\n[plugins.provider-openai]\nmax_tokens_field = \"x\"\n",
    )
    .unwrap();
    assert_eq!(proxy_settings("http://p:1", &explicit).len(), 4);
    assert_eq!(
        upstream_url(&custom, Provider::OpenAi),
        "http://vllm:8000/v1"
    );
    assert_eq!(key_env(&base, Provider::OpenAi), "OPENAI_API_KEY");
}

fn matrix(config: &str, settings: &[&str]) -> Matrix {
    Matrix {
        tasks: vec![],
        cells: vec![Cell {
            name: "c".into(),
            settings: settings.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }],
        repeats: 1,
        profile: None,
        config: Some(config.into()),
    }
}

#[test]
fn cells_list_their_models_with_each_ones_pricing() {
    let config = r#"
        [model]
        provider = "anthropic"
        id = "claude-a"
        [model.pricing]
        input = 3.0
        [model.summarizer]
        provider = "openai"
        id = "gpt-small"
        [eval.pricing."gpt-small"]
        input = 0.5
    "#;
    let c = matrix(config, &[]).cell_models(0).unwrap();
    assert_eq!(
        c.providers,
        BTreeSet::from([Provider::Anthropic, Provider::OpenAi])
    );
    assert_eq!(c.models["claude-a"].unwrap().input, 3.0);
    assert_eq!(c.models["gpt-small"].unwrap().input, 0.5);
    // [model.pricing] is the configured model's, as in ch: a cell that changes the
    // model takes it along unless [eval.pricing] lists the new one.
    let c = matrix(config, &[r#"model.id="claude-b""#])
        .cell_models(0)
        .unwrap();
    assert_eq!(c.models["claude-b"].unwrap().input, 3.0);
    let listed = format!("{config}\n[eval.pricing.\"claude-b\"]\ninput = 15.0\n");
    let c = matrix(&listed, &[r#"model.id="claude-b""#])
        .cell_models(0)
        .unwrap();
    assert_eq!(c.models["claude-b"].unwrap().input, 15.0);
    // The proxy needs to know the model.
    assert!(
        matrix("[model]\nprovider = \"anthropic\"\n", &[])
            .cell_models(0)
            .is_err()
    );
    let scripted = matrix("[model]\nprovider = \"scripted\"\n", &[])
        .cell_models(0)
        .unwrap();
    assert!(scripted.providers.is_empty() && scripted.models.is_empty());
}

#[test]
fn unpriced_model_changes_warn_and_malformed_prices_fail() {
    let config = "[model]\nprovider = \"anthropic\"\nid = \"a\"\n[model.pricing]\ninput = 3.0\n";
    let c = matrix(config, &[r#"model.id="b""#]).cell_models(0).unwrap();
    assert!(c.warnings[0].contains("model b"), "{:?}", c.warnings);
    assert!(
        matrix(config, &[])
            .cell_models(0)
            .unwrap()
            .warnings
            .is_empty()
    );
    let bad = format!("{config}[eval.pricing.a]\ninput = \"lots\"\n");
    let err = matrix(&bad, &[]).cell_models(0).unwrap_err();
    assert!(format!("{err:#}").contains("eval.pricing"), "{err:#}");
}

#[test]
fn the_config_fingerprint_ignores_comments_and_order() {
    let a = normalized("# a comment\n[model]\nid = \"x\"\nprovider = \"p\"\n").unwrap();
    let b = normalized("[model]\nprovider = \"p\" # here\nid = \"x\"\n").unwrap();
    assert_eq!(a, b);
    assert_ne!(
        a,
        normalized("[model]\nid = \"y\"\nprovider = \"p\"\n").unwrap()
    );
}

/// What the fake agent does in one task.
#[derive(Clone)]
struct Act {
    agent: Exit,
    /// Calls the "proxy" records for the run.
    calls: Vec<Call>,
    check: Exit,
}

/// A backend whose agent plants `.git` entries and finishes as its task's `Act`
/// says, so every way a run can end is reachable without Docker or a model.
struct Fake {
    state: Option<ProxyRoute>,
    acts: BTreeMap<String, Act>,
    /// Agent runs, per task.
    tries: Mutex<BTreeMap<String, usize>>,
}

impl Fake {
    fn new(dir: &Path, acts: &[(&str, Act)]) -> Self {
        let state = StateDir(dir.join("out/proxy"));
        state.prepare().unwrap();
        Self {
            state: Some(ProxyRoute {
                url: "http://proxy".into(),
                state,
            }),
            acts: acts
                .iter()
                .map(|(t, a)| (t.to_string(), a.clone()))
                .collect(),
            tries: Mutex::new(BTreeMap::new()),
        }
    }

    fn tries(&self, task: &str) -> usize {
        self.tries.lock().unwrap().get(task).copied().unwrap_or(0)
    }
}

/// `<out>/runs/<task>/<cell>/<repeat>/work`: the task.
fn task_of(job: &Job<'_>) -> String {
    let runs = job.work.ancestors().nth(3).unwrap();
    runs.file_name().unwrap().to_string_lossy().into_owned()
}

#[async_trait]
impl Backend for Fake {
    fn command(&self, _: Step, _: &Job<'_>) -> Vec<String> {
        vec!["fake".into()]
    }

    async fn run(
        &self,
        step: Step,
        job: &Job<'_>,
        _io: Io<'_>,
        _: Duration,
    ) -> anyhow::Result<Exit> {
        if step == Step::Verify && job.ch_args.is_empty() {
            // The pre-flight: the check fails on the untouched repo.
            return Ok(exit(Some(1)));
        }
        let task = task_of(job);
        let act = self.acts[&task].clone();
        match step {
            Step::Agent => {
                *self.tries.lock().unwrap().entry(task).or_default() += 1;
                std::fs::create_dir_all(job.work.join("sub/.Git")).unwrap();
                std::fs::create_dir_all(job.ch_home.join(".git")).unwrap();
                if let (Some(route), Some((_, token))) = (&self.state, job.env.first()) {
                    let run = ledger::lookup_token(&route.state, token).unwrap().run;
                    for call in &act.calls {
                        ledger::append(&route.state, &run, call).unwrap();
                    }
                }
                Ok(act.agent)
            }
            Step::Diff => {
                // The agent's `.git` is gone before the diff and the check.
                assert!(!job.work.join("sub/.Git").exists());
                assert!(!job.ch_home.join(".git").exists());
                Ok(exit(Some(0)))
            }
            Step::Verify => Ok(act.check),
        }
    }

    fn proxy(&self) -> Option<&ProxyRoute> {
        self.state.as_ref()
    }

    fn identity(&self, _: &str) -> String {
        "fake".into()
    }
}

fn suite(dir: &Path, tasks: &[&str]) -> Matrix {
    for id in tasks {
        let task = dir.join("suite").join(id);
        std::fs::create_dir_all(task.join("repo")).unwrap();
        std::fs::write(task.join("repo/a.txt"), "a").unwrap();
        std::fs::write(
            task.join("task.toml"),
            "instruction = \"x\"\nverify = \"false\"\n",
        )
        .unwrap();
    }
    Matrix {
        tasks: crate::task::load_suite(&dir.join("suite")).unwrap(),
        cells: vec![Cell {
            name: "base".into(),
            ..Default::default()
        }],
        repeats: 1,
        profile: None,
        config: Some("[model]\nprovider = \"anthropic\"\nid = \"m\"\n".into()),
    }
}

async fn run_fake(
    dir: &Path,
    fake: Arc<Fake>,
    jobs: usize,
    backoff: Duration,
) -> anyhow::Result<Summary> {
    let tasks: Vec<&str> = fake.acts.keys().map(String::as_str).collect();
    let opts = Options {
        out: dir.join("out"),
        jobs,
        max_cost: None,
        max_attempts: 3,
        backoff,
    };
    run_matrix(Arc::new(suite(dir, &tasks)), fake, &opts).await
}

fn call(status: u16, refused: Option<&str>) -> Call {
    Call {
        status,
        refused: refused.map(String::from),
        ..Default::default()
    }
}

fn act(agent: Option<i32>, calls: Vec<Call>) -> Act {
    Act {
        agent: exit(agent),
        calls,
        check: exit(Some(1)),
    }
}

fn no_git_left(dir: &Path, task: &str) {
    let run = dir.join("out/runs").join(task).join("base/1");
    assert!(
        run.join("work").is_dir(),
        "the run got as far as its workspace"
    );
    assert!(
        !run.join("work/sub/.Git").exists(),
        "work/ still has a .git"
    );
    assert!(
        !run.join("ch-home/.git").exists(),
        "ch-home/ still has a .git"
    );
    assert!(run.join("baseline.git/HEAD").is_file());
}

const QUICK: Duration = Duration::from_millis(10);

#[tokio::test]
async fn every_way_a_run_ends_leaves_no_git_behind() {
    // Recorded.
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::new(
        dir.path(),
        &[("t", act(Some(0), vec![call(200, None)]))],
    ));
    run_fake(dir.path(), fake, 1, QUICK).await.unwrap();
    no_git_left(dir.path(), "t");
    assert!(dir.path().join("out/runs/t/base/1/run.json").is_file());

    // Infrastructure: the run never reached the model.
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::new(dir.path(), &[("t", act(Some(1), vec![]))]));
    assert!(run_fake(dir.path(), fake, 1, QUICK).await.is_err());
    no_git_left(dir.path(), "t");

    // Cut off by --max-cost.
    let dir = tempfile::tempdir().unwrap();
    let calls = vec![call(200, None), call(0, Some("max_cost"))];
    let fake = Arc::new(Fake::new(dir.path(), &[("t", act(Some(1), calls))]));
    let summary = run_fake(dir.path(), fake, 1, QUICK).await.unwrap();
    assert_eq!(summary.stopped_by_max_cost, 1);
    no_git_left(dir.path(), "t");

    // The check's container didn't start.
    let dir = tempfile::tempdir().unwrap();
    let mut broken = act(Some(0), vec![call(200, None)]);
    broken.check.start_error = Some("no such image".into());
    let fake = Arc::new(Fake::new(dir.path(), &[("t", broken)]));
    assert!(run_fake(dir.path(), fake, 1, QUICK).await.is_err());
    no_git_left(dir.path(), "t");
}

#[tokio::test]
async fn a_run_is_tried_at_most_max_attempts_times() {
    // "t" fails transiently every time; "u" is recorded meanwhile, which must not
    // give "t" more tries.
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::new(
        dir.path(),
        &[
            ("t", act(Some(1), vec![call(200, None), call(529, None)])),
            ("u", act(Some(0), vec![call(200, None)])),
        ],
    ));
    let err = run_fake(dir.path(), fake.clone(), 1, QUICK)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("on all 3 attempts"), "{err:#}");
    assert_eq!(fake.tries("t"), 3);
    assert_eq!(fake.tries("u"), 1);
    assert!(dir.path().join("out/runs/u/base/1/run.json").is_file());
    no_git_left(dir.path(), "t");
}

#[tokio::test]
async fn a_waiting_worker_wakes_when_the_matrix_stops() {
    // "t" fails transiently and waits an hour for its retry; "u" stops the matrix.
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::new(
        dir.path(),
        &[
            ("t", act(Some(1), vec![call(200, None), call(529, None)])),
            ("u", act(Some(1), vec![])),
        ],
    ));
    let started = Instant::now();
    let err = run_fake(dir.path(), fake, 2, Duration::from_secs(3600))
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("before its first model call"),
        "{err:#}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
}
