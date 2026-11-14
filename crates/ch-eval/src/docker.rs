//! The Docker backend, and building what it needs: the `ch-eval-base` image and static
//! builds of `ch` and `ch-eval`.
//!
//! Everything goes through the `docker` command. Each invocation runs a credential
//! proxy container on the default bridge (see [`crate::proxy`]). Each run gets its own
//! internal network (no route out), which the proxy joins for the run's length, so the
//! agent can reach the proxy and nothing else, not even other runs, and never gets the
//! API key. Each step runs in its own container:
//!
//! - **agent:** the run's network, the workspace at `/work`, its ch home at `/ch-home`,
//!   `ch` read-only at `/usr/local/bin/ch`, and the run's proxy token in
//!   `CH_EVAL_API_KEY`;
//! - **diff:** `ch-eval-base`, the workspace read-only, a scratch copy of the baseline's
//!   git directory, no network;
//! - **check:** the task's image, the workspace, no network.
//!
//! Every container runs as the host user's uid and gid (so files stay removable), with
//! no capabilities, no new privileges, `HOME=/tmp`, memory, CPU and process limits,
//! `--pull never` (an image must be built or pulled beforehand), and the static
//! `ch-eval` as its entrypoint: `ch-eval limit` stops the step at its time limit plus a
//! minute even if the runner is gone. A step is `docker create`, then `docker start
//! --attach`, then `docker inspect` for the exit code, so the step's own exit code
//! (even 125) is never mistaken for Docker failing to start it.
//!
//! The proxy is created with `--rm` and gets the keys on stdin, through a `docker start
//! --attach --interactive` that stays attached and writes its output to `proxy.log`;
//! the keys are never in the container's environment, so `docker inspect` can't show
//! them. The runner restarts the proxy if it stops.
//!
//! Everything `ch-eval` creates carries the label `ch-eval.invocation=<id>`, and the
//! runner's pid and start time. A [`Cleanup`] removes an invocation's containers and
//! networks when it ends, fails, panics or is interrupted. When the runner was killed
//! instead, the proxy exits by itself once its heartbeat file goes stale, and the next
//! invocation removes what is left of any invocation whose runner is gone.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context as _, bail};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::AsyncWriteExt as _;

use crate::backend::{Backend, Exit, Invocation, Io, Job, ProxyRoute, Step, Stopping, execute};
use crate::fingerprint::hash;
use crate::ledger::StateDir;
use crate::proxy::{Keys, ProxySpec};
use crate::workspace::{DIFF_ENV, DIFF_SCRIPT};

/// The default task image, from `eval/docker/Dockerfile`. It also runs the diff and
/// the proxy.
pub const BASE_IMAGE: &str = "ch-eval-base";
/// The image that builds the static binaries, from `eval/docker/builder.Dockerfile`.
pub const BUILDER_IMAGE: &str = "ch-eval-builder";
/// Named volume caching the cargo registry between builds.
pub const REGISTRY_VOLUME: &str = "ch-eval-cargo-registry";
/// Named volume holding the build's target directory.
pub const TARGET_VOLUME: &str = "ch-eval-target";
/// Label on everything `ch-eval` creates: `docker ps --filter label=ch-eval`.
pub const LABEL: &str = "ch-eval";
/// Label naming the invocation, so its containers and networks can be removed together.
pub const INVOCATION_LABEL: &str = "ch-eval.invocation";
/// Labels naming the runner's process and its heartbeat file, so a later invocation can
/// tell it is gone.
pub const PID_LABEL: &str = "ch-eval.pid";
pub const STARTED_LABEL: &str = "ch-eval.started";
pub const HEARTBEAT_LABEL: &str = "ch-eval.heartbeat";
/// The proxy's name on each run's network, and its port.
pub const PROXY_HOST: &str = "ch-eval-proxy";
pub const PROXY_PORT: u16 = 8080;

/// Limits for each step's container.
const MEMORY: &str = "4g";
const CPUS: &str = "2";
const PIDS: &str = "1024";
/// The proxy's memory: room for [`crate::proxy::MAX_BUFFERED`] request bodies.
const PROXY_MEMORY: &str = "1g";
/// How much longer than its limit a step may run before `ch-eval limit` stops it.
const BACKSTOP: Duration = Duration::from_secs(60);
/// How stale the heartbeat may get before the proxy exits.
pub const HEARTBEAT_SECS: u64 = 60;

/// The static binaries mounted into containers.
#[derive(Debug, Clone, PartialEq)]
pub struct Binaries {
    pub ch: PathBuf,
    pub ch_eval: PathBuf,
}

/// Runs each step in a container.
#[derive(Debug, Clone)]
pub struct DockerBackend {
    /// The `docker` program (a stand-in in unit tests).
    docker: String,
    binaries: Binaries,
    /// `uid:gid` of the host user.
    user: String,
    /// `--label` values for everything created.
    labels: Vec<String>,
    proxy: Option<Arc<ProxyProcess>>,
    route: Option<ProxyRoute>,
    /// Image ids, by name, for the fingerprint.
    images: BTreeMap<String, String>,
    ch_hash: String,
    /// Once it has run, no new container is created.
    cleanup: Option<Arc<Cleanup>>,
}

/// What [`DockerBackend::start`] needs.
#[derive(Debug, Clone)]
pub struct Setup {
    pub binaries: Binaries,
    /// The task images; each must exist locally.
    pub images: Vec<String>,
    /// The proxy's state directory (absolute), and what it forwards where.
    pub state: StateDir,
    pub spec: ProxySpec,
    /// Written to the proxy's stdin, never to its environment or a file.
    pub keys: Keys,
}

impl DockerBackend {
    /// A backend that only describes command lines, for `--dry-run`: nothing is created.
    pub fn describe_only(binaries: Binaries, state: StateDir) -> anyhow::Result<Self> {
        Ok(Self {
            docker: "docker".into(),
            binaries,
            user: user()?,
            labels: Vec::new(),
            proxy: None,
            route: Some(ProxyRoute {
                url: format!("http://{PROXY_HOST}:{PROXY_PORT}"),
                state,
            }),
            images: BTreeMap::new(),
            ch_hash: String::new(),
            cleanup: None,
        })
    }

    /// Removes what dead invocations left, then starts the proxy. The returned
    /// [`Cleanup`] removes this invocation's containers and networks; it also runs when
    /// dropped.
    pub async fn start(setup: Setup) -> anyhow::Result<(Self, Arc<Cleanup>)> {
        check_mount_path(&setup.binaries.ch)?;
        check_mount_path(&setup.binaries.ch_eval)?;
        check_mount_path(&setup.state.0)?;
        sweep_stale("docker");
        let invocation = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let labels = vec![
            LABEL.to_string(),
            format!("{INVOCATION_LABEL}={invocation}"),
            format!("{PID_LABEL}={}", std::process::id()),
            format!(
                "{STARTED_LABEL}={}",
                process_started(std::process::id()).unwrap_or_default()
            ),
            format!("{HEARTBEAT_LABEL}={}", setup.state.heartbeat().display()),
        ];
        let cleanup = Arc::new(Cleanup {
            docker: "docker".into(),
            invocation: invocation.clone(),
            done: AtomicBool::new(false),
        });

        let mut images = BTreeMap::new();
        for image in setup.images.iter().chain([&BASE_IMAGE.to_string()]) {
            let id = image_id(image)?.with_context(|| {
                format!(
                    "image {image} isn't available locally; ch-eval runs with --pull never, \
                     so build or pull it first"
                )
            })?;
            images.insert(image.clone(), id);
        }
        setup.state.prepare()?;
        std::fs::write(
            setup.state.config(),
            serde_json::to_string_pretty(&setup.spec)?,
        )
        .context("writing proxy.json")?;
        touch(&setup.state.heartbeat())?;

        let user = user()?;
        let proxy = Arc::new(ProxyProcess {
            docker: "docker".into(),
            name: format!("ch-eval-{invocation}-proxy"),
            generation: AtomicU32::new(0),
            current: Mutex::new(String::new()),
            networks: Mutex::new(BTreeSet::new()),
            child: tokio::sync::Mutex::new(None),
            labels: labels.clone(),
            user: user.clone(),
            ch_eval: setup.binaries.ch_eval.clone(),
            state: setup.state.clone(),
            keys: serde_json::to_string(&setup.keys)?,
        });
        let backend = Self {
            docker: "docker".into(),
            ch_hash: crate::fingerprint::hash_file(&setup.binaries.ch)?,
            binaries: setup.binaries,
            user,
            labels,
            proxy: Some(proxy.clone()),
            route: Some(ProxyRoute {
                url: format!("http://{PROXY_HOST}:{PROXY_PORT}"),
                state: setup.state.clone(),
            }),
            images,
            cleanup: Some(cleanup.clone()),
        };
        proxy.start().await?;
        Ok((backend, cleanup))
    }

    /// Restarts the proxy if it has stopped. Returns whether it did.
    pub async fn supervise(&self) -> anyhow::Result<bool> {
        match &self.proxy {
            Some(proxy) if !self.stopping() => proxy.ensure_running().await,
            _ => Ok(false),
        }
    }

    fn stopping(&self) -> bool {
        self.cleanup.as_ref().is_some_and(|c| c.has_run())
    }

    fn label_args(&self) -> Vec<String> {
        self.labels
            .iter()
            .flat_map(|l| ["--label".to_string(), l.clone()])
            .collect()
    }

    /// `docker create` and the options every step container shares.
    fn common_args(&self, name: &str) -> Vec<String> {
        let mut args = vec!["create".to_string(), "--name".into(), name.into()];
        args.extend(self.label_args());
        args.extend(
            [
                "--init",
                "--pull",
                "never",
                "--user",
                &self.user,
                "--cap-drop",
                "ALL",
                "--security-opt",
                "no-new-privileges",
                "--pids-limit",
                PIDS,
                "--memory",
                MEMORY,
                "--cpus",
                CPUS,
                "-e",
                "HOME=/tmp",
                "--entrypoint",
                "/usr/local/bin/ch-eval",
            ]
            .map(String::from),
        );
        args.extend(volume(
            &self.binaries.ch_eval,
            "/usr/local/bin/ch-eval",
            true,
        ));
        args
    }

    fn create_args(&self, step: Step, job: &Job<'_>, limit: Duration) -> Vec<String> {
        let mut args = self.common_args(&container_name(job.id, step));
        let (image, command) = match step {
            Step::Agent => {
                args.extend(["-i".into(), "--network".into(), network_name(job.id)]);
                args.extend(["-e".into(), "CH_HOME=/ch-home".into()]);
                for (name, _) in job.env {
                    args.extend(["-e".into(), name.clone()]);
                }
                args.extend(volume(job.work, "/work", false));
                args.extend(volume(job.ch_home, "/ch-home", false));
                args.extend(volume(&self.binaries.ch, "/usr/local/bin/ch", true));
                let mut command = vec!["ch".to_string()];
                command.extend(job.ch_args.iter().cloned());
                (job.image, command)
            }
            Step::Diff => {
                args.extend(
                    ["--network", "none", "-e", "GD=/baseline", "-e", "WT=/work"].map(String::from),
                );
                for (k, v) in DIFF_ENV {
                    args.extend(["-e".into(), format!("{k}={v}")]);
                }
                args.extend(volume(job.work, "/work", true));
                args.extend(volume(job.git_dir, "/baseline", false));
                let command = vec!["sh".into(), "-c".into(), DIFF_SCRIPT.into()];
                (BASE_IMAGE, command)
            }
            Step::Verify => {
                args.extend(["--network", "none"].map(String::from));
                args.extend(volume(job.work, "/work", false));
                let command = vec!["sh".into(), "-c".into(), job.verify.into()];
                (job.image, command)
            }
        };
        args.extend(["-w".into(), "/work".into(), "--".into(), image.into()]);
        args.extend([
            "limit".into(),
            (limit + BACKSTOP).as_secs().to_string(),
            "--".into(),
        ]);
        args.extend(command);
        args
    }

    /// Runs `docker ARGS` and fails with its stderr.
    async fn docker(&self, args: &[&str]) -> anyhow::Result<()> {
        let out = tokio::process::Command::new(&self.docker)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .await
            .context("running `docker`")?;
        if !out.status.success() {
            bail!(
                "docker {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// The step's container, start to removal.
    async fn container(
        &self,
        step: Step,
        job: &Job<'_>,
        io: Io<'_>,
        limit: Duration,
    ) -> anyhow::Result<Exit> {
        let name = container_name(job.id, step);
        let mut create = tokio::process::Command::new(&self.docker);
        create
            .args(self.create_args(step, job, limit))
            .stdin(Stdio::null());
        if step == Step::Agent {
            create.envs(job.env.iter().map(|(k, v)| (k, v)));
        }
        let out = create.output().await.context("running `docker create`")?;
        if !out.status.success() {
            bail!(
                "docker couldn't create {name}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        if self.stopping() {
            // The cleanup may have listed the containers before this one existed.
            let _ = self.docker(&["rm", "--force", &name]).await;
            return Err(Stopping.into());
        }
        let mut start = vec!["start".to_string(), "--attach".into()];
        if step == Step::Agent {
            start.push("--interactive".into());
        }
        start.push(name.clone());
        let inv = Invocation {
            program: self.docker.clone(),
            args: start,
            ..Default::default()
        };
        // At the limit the container is killed, not just the client attached to it.
        let exit = execute(&inv, io, limit, async || {
            let _ = self.docker(&["kill", &name]).await;
        })
        .await;
        let state = if exit.as_ref().is_ok_and(|e| !e.timed_out) {
            inspect(&self.docker, &name).await
        } else {
            None
        };
        let _ = self.docker(&["rm", "--force", &name]).await;
        if self.stopping() {
            return Err(Stopping.into());
        }
        let mut exit = exit?;
        if !exit.timed_out {
            match state {
                Some(state) => {
                    exit.code = Some(state.exit_code);
                    if !state.error.is_empty() {
                        exit.start_error = Some(state.error);
                    }
                }
                None => exit.start_error = Some(format!("container {name} disappeared")),
            }
        }
        Ok(exit)
    }
}

fn container_name(id: &str, step: Step) -> String {
    let step = match step {
        Step::Agent => "agent",
        Step::Diff => "diff",
        Step::Verify => "check",
    };
    format!("{id}-{step}")
}

/// The run's own internal network.
fn network_name(id: &str) -> String {
    format!("{id}-net")
}

#[async_trait]
impl Backend for DockerBackend {
    fn command(&self, step: Step, job: &Job<'_>) -> Vec<String> {
        let mut line = vec!["docker".to_string()];
        line.extend(
            self.create_args(step, job, Duration::ZERO)
                .into_iter()
                .map(|a| {
                    if a == DIFF_SCRIPT {
                        "<diff script>".into()
                    } else {
                        a
                    }
                }),
        );
        line
    }

    async fn run(
        &self,
        step: Step,
        job: &Job<'_>,
        io: Io<'_>,
        limit: Duration,
    ) -> anyhow::Result<Exit> {
        if self.stopping() {
            return Err(Stopping.into());
        }
        if step != Step::Agent {
            return self.container(step, job, io, limit).await;
        }
        // The agent's network: internal, its own, with the proxy on it for the run.
        let net = network_name(job.id);
        let mut create = vec!["network", "create", "--internal"];
        for label in &self.labels {
            create.extend(["--label", label]);
        }
        create.push(&net);
        self.docker(&create).await?;
        let attached = match &self.proxy {
            Some(proxy) => proxy.attach(&net).await,
            None => Ok(()),
        };
        let exit = match attached {
            Ok(()) => self.container(step, job, io, limit).await,
            Err(e) => Err(e),
        };
        if let Some(proxy) = &self.proxy {
            proxy.detach(&net).await;
        }
        let _ = self.docker(&["network", "rm", &net]).await;
        exit
    }

    fn proxy(&self) -> Option<&ProxyRoute> {
        self.route.as_ref()
    }

    fn identity(&self, image: &str) -> String {
        let id = self.images.get(image).map(String::as_str).unwrap_or("");
        hash(format!("{}\n{id}", self.ch_hash).as_bytes())
    }
}

/// The credential proxy's container, and what it takes to start it again.
struct ProxyProcess {
    docker: String,
    /// The name's stem; each start adds a generation.
    name: String,
    generation: AtomicU32,
    /// The running container's name.
    current: Mutex<String>,
    /// The runs' networks it is attached to.
    networks: Mutex<BTreeSet<String>>,
    /// The `docker start --attach` that fed it the keys and carries its output.
    child: tokio::sync::Mutex<Option<tokio::process::Child>>,
    labels: Vec<String>,
    user: String,
    ch_eval: PathBuf,
    state: StateDir,
    /// The keys as JSON, for its stdin.
    keys: String,
}

impl std::fmt::Debug for ProxyProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyProcess")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl ProxyProcess {
    fn current(&self) -> String {
        self.current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    async fn docker(&self, args: &[&str]) -> anyhow::Result<String> {
        let out = tokio::process::Command::new(&self.docker)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .await
            .context("running `docker`")?;
        if !out.status.success() {
            bail!(
                "docker {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Creates and starts a proxy container, attaches it to the runs' networks, sends
    /// it the keys and waits until it listens.
    async fn start(&self) -> anyhow::Result<()> {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst);
        let name = format!("{}-{generation}", self.name);
        let _ = std::fs::remove_file(self.state.ready());
        let mut args: Vec<String> = vec![
            "create".into(),
            "--name".into(),
            name.clone(),
            "--rm".into(),
            "-i".into(),
        ];
        for label in &self.labels {
            args.extend(["--label".into(), label.clone()]);
        }
        args.extend(
            [
                "--init",
                "--pull",
                "never",
                "--user",
                &self.user,
                "--cap-drop",
                "ALL",
                "--security-opt",
                "no-new-privileges",
                "--pids-limit",
                "256",
                "--memory",
                PROXY_MEMORY,
                "-e",
                "HOME=/tmp",
                "--entrypoint",
                "/usr/local/bin/ch-eval",
            ]
            .map(String::from),
        );
        args.extend(volume(&self.ch_eval, "/usr/local/bin/ch-eval", true));
        args.extend(volume(&self.state.0, "/state", false));
        args.extend(
            [
                "--",
                BASE_IMAGE,
                "proxy",
                "--state",
                "/state",
                "--listen",
                &format!("0.0.0.0:{PROXY_PORT}"),
            ]
            .map(String::from),
        );
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.docker(&refs)
            .await
            .context("creating the proxy container")?;
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = name.clone();
        let networks: Vec<String> = self
            .networks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect();
        for net in networks {
            self.docker(&["network", "connect", "--alias", PROXY_HOST, &net, &name])
                .await?;
        }

        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.state.log())
            .context("opening proxy.log")?;
        let mut child = tokio::process::Command::new(&self.docker)
            .args(["start", "--attach", "--interactive", &name])
            .stdin(Stdio::piped())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .kill_on_drop(true)
            .spawn()
            .context("starting the proxy container")?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(self.keys.as_bytes())
                .await
                .context("sending the proxy its keys")?;
            drop(stdin);
        }
        *self.child.lock().await = Some(child);
        self.wait_ready(&name).await
    }

    async fn wait_ready(&self, name: &str) -> anyhow::Result<()> {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(30) {
            if self.state.ready().exists() {
                return Ok(());
            }
            if let Some(child) = self.child.lock().await.as_mut()
                && let Ok(Some(_)) = child.try_wait()
            {
                let log = std::fs::read_to_string(self.state.log()).unwrap_or_default();
                let tail: Vec<&str> = log.lines().rev().take(5).collect();
                bail!(
                    "the proxy container {name} stopped: {}",
                    tail.into_iter().rev().collect::<Vec<_>>().join("\n")
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        bail!("the proxy didn't start within 30 seconds")
    }

    /// Starts the proxy again if its container stopped. Returns whether it did.
    async fn ensure_running(&self) -> anyhow::Result<bool> {
        let running = self
            .docker(&["inspect", "--format", "{{.State.Running}}", &self.current()])
            .await
            .is_ok_and(|s| s == "true");
        if running {
            return Ok(false);
        }
        eprintln!("ch-eval: the proxy stopped; starting it again");
        self.start().await?;
        Ok(true)
    }

    /// Joins a run's network, as `ch-eval-proxy`.
    async fn attach(&self, net: &str) -> anyhow::Result<()> {
        self.networks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(net.to_string());
        self.docker(&[
            "network",
            "connect",
            "--alias",
            PROXY_HOST,
            net,
            &self.current(),
        ])
        .await
        .map(|_| ())
    }

    /// Leaves a run's network.
    async fn detach(&self, net: &str) {
        self.networks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(net);
        let _ = self
            .docker(&["network", "disconnect", "--force", net, &self.current()])
            .await;
    }
}

/// The part of `docker inspect`'s `State` that matters here.
#[derive(Debug, Deserialize)]
struct ContainerState {
    #[serde(rename = "ExitCode")]
    exit_code: i32,
    #[serde(rename = "Error", default)]
    error: String,
}

async fn inspect(docker: &str, name: &str) -> Option<ContainerState> {
    let out = tokio::process::Command::new(docker)
        .args(["inspect", "--format", "{{json .State}}", name])
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    serde_json::from_slice(&out.stdout).ok()
}

/// Removes an invocation's containers and networks, once. Runs on drop too.
#[derive(Debug)]
pub struct Cleanup {
    docker: String,
    invocation: String,
    done: AtomicBool,
}

impl Cleanup {
    /// Whether [`Cleanup::run`] has started.
    pub fn has_run(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }

    pub fn run(&self) {
        if self.done.swap(true, Ordering::SeqCst) {
            return;
        }
        remove_labelled(
            &self.docker,
            &format!("{INVOCATION_LABEL}={}", self.invocation),
        );
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        self.run();
    }
}

/// Removes the containers, then the networks, labelled `label`.
fn remove_labelled(docker: &str, label: &str) {
    let list = |args: &[&str]| -> Vec<String> {
        Command::new(docker)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .split_whitespace()
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    };
    let filter = format!("label={label}");
    let quiet = |args: &[&str]| {
        let _ = Command::new(docker)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    };
    let ids = list(&["ps", "--all", "--quiet", "--filter", &filter]);
    if !ids.is_empty() {
        let mut args = vec!["rm", "--force"];
        args.extend(ids.iter().map(String::as_str));
        quiet(&args);
    }
    for net in list(&["network", "ls", "--quiet", "--filter", &filter]) {
        quiet(&["network", "rm", &net]);
    }
}

/// Removes the containers and networks of invocations whose runner is gone (it was
/// killed before its cleanup could run). An invocation whose heartbeat file is fresh is
/// running, whatever else says; otherwise its runner's pid must be running with the
/// start time it was labelled with.
fn sweep_stale(docker: &str) {
    let mut runners = BTreeSet::new();
    let format = [INVOCATION_LABEL, PID_LABEL, STARTED_LABEL, HEARTBEAT_LABEL]
        .map(|l| format!("{{{{.Label \"{l}\"}}}}"))
        .join("\t");
    for what in [&["ps", "--all"][..], &["network", "ls"][..]] {
        let out = Command::new(docker)
            .args(what)
            .args([
                "--filter",
                &format!("label={PID_LABEL}"),
                "--format",
                &format,
            ])
            .stdin(Stdio::null())
            .output();
        let Ok(out) = out else { continue };
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if let [invocation, pid, started, heartbeat] = parts[..] {
                runners.insert(Runner {
                    invocation: invocation.to_string(),
                    pid: pid.to_string(),
                    started: started.to_string(),
                    heartbeat: heartbeat.to_string(),
                });
            }
        }
    }
    for runner in runners {
        if !runner.invocation.is_empty() && runner.is_gone() {
            eprintln!(
                "ch-eval: removing what a stopped ch-eval (pid {}) left behind",
                runner.pid
            );
            remove_labelled(docker, &format!("{INVOCATION_LABEL}={}", runner.invocation));
        }
    }
}

/// An invocation's runner, from its labels.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Runner {
    invocation: String,
    pid: String,
    started: String,
    heartbeat: String,
}

impl Runner {
    fn is_gone(&self) -> bool {
        let beat = std::fs::metadata(&self.heartbeat)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok());
        if beat.is_some_and(|age| age < Duration::from_secs(HEARTBEAT_SECS)) {
            return false;
        }
        let running = self
            .pid
            .parse::<u32>()
            .ok()
            .and_then(process_started)
            .is_some_and(|s| s == self.started);
        !running
    }
}

/// When process `pid` started, as `ps` prints it in the C locale and UTC, if it is
/// running. With the pid, it tells a running process from a later one that reused the
/// pid, whatever the caller's locale and time zone.
fn process_started(pid: u32) -> Option<String> {
    let out = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let started = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !started.is_empty()).then_some(started)
}

/// Touches the heartbeat file the proxy watches.
pub fn touch(path: &Path) -> anyhow::Result<()> {
    std::fs::write(path, b"").with_context(|| format!("writing {}", path.display()))
}

/// The image's id, or `None` when it doesn't exist locally.
fn image_id(image: &str) -> anyhow::Result<Option<String>> {
    let out = Command::new("docker")
        .args(["image", "inspect", "--format", "{{.Id}}", image])
        .stdin(Stdio::null())
        .output()
        .context("running `docker image inspect`")?;
    Ok(out
        .status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()))
}

/// `-v SRC:DST[:ro]`.
fn volume(src: &Path, dst: &str, read_only: bool) -> [String; 2] {
    let ro = if read_only { ":ro" } else { "" };
    ["-v".into(), format!("{}:{dst}{ro}", src.display())]
}

/// A bind mount source must be absolute (a relative one names a volume) and can't
/// hold `:`, which separates the parts of `-v`.
pub fn check_mount_path(path: &Path) -> anyhow::Result<()> {
    if !path.is_absolute() {
        bail!("{}: expected an absolute path", path.display());
    }
    if path.to_string_lossy().contains(':') {
        bail!(
            "{}: Docker can't mount a path that contains `:`; choose another directory",
            path.display()
        );
    }
    Ok(())
}

/// `uid:gid` of the current user.
fn user() -> anyhow::Result<String> {
    Ok(format!("{}:{}", id("-u")?, id("-g")?))
}

fn id(flag: &str) -> anyhow::Result<u32> {
    let out = Command::new("id")
        .arg(flag)
        .output()
        .context("running `id`")?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .with_context(|| format!("reading the output of `id {flag}`"))
}

/// Fails with a helpful message when the `docker` command or its daemon is missing.
pub fn check_docker() -> anyhow::Result<()> {
    let out = Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .stdin(Stdio::null())
        .output()
        .context("running `docker` (is Docker installed?)")?;
    if !out.status.success() {
        bail!(
            "Docker isn't running: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Builds `image` from `eval/docker/<dockerfile>` in `source`, showing Docker's output.
pub fn build_image(source: &Path, image: &str, dockerfile: &str) -> anyhow::Result<()> {
    let context = source.join("eval/docker");
    eprintln!("ch-eval: building image {image}");
    let status = Command::new("docker")
        .arg("build")
        .args(["--tag", image, "--file"])
        .arg(context.join(dockerfile))
        .arg(&context)
        .stdin(Stdio::null())
        .status()
        .context("running `docker build`")?;
    if !status.success() {
        bail!("building image {image} failed");
    }
    Ok(())
}

/// Builds `ch-eval-base` when it doesn't exist yet.
pub fn ensure_base_image(source: &Path) -> anyhow::Result<()> {
    if image_id(BASE_IMAGE)?.is_none() {
        build_image(source, BASE_IMAGE, "Dockerfile")?;
    }
    Ok(())
}

/// The architecture name used in the target triple and the binaries' names. Docker
/// runs containers of the host's architecture.
pub fn arch() -> &'static str {
    std::env::consts::ARCH
}

/// Where the static binaries for `arch` go: `<source>/target/ch-eval/`.
pub fn binary_paths(source: &Path, arch: &str) -> Binaries {
    let dir = source.join("target/ch-eval");
    Binaries {
        ch: dir.join(format!("ch-{arch}")),
        ch_eval: dir.join(format!("ch-eval-{arch}")),
    }
}

/// Builds static `ch` and `ch-eval` for `arch` and returns their paths. Cargo's
/// output is shown.
pub fn build_binaries(source: &Path, arch: &str) -> anyhow::Result<Binaries> {
    if image_id(BUILDER_IMAGE)?.is_none() {
        build_image(source, BUILDER_IMAGE, "builder.Dockerfile")?;
    }
    let paths = binary_paths(source, arch);
    let cache = paths.ch.parent().context("binary path has no parent")?;
    std::fs::create_dir_all(cache).with_context(|| format!("creating {}", cache.display()))?;
    check_mount_path(source)?;
    check_mount_path(cache)?;
    let triple = format!("{arch}-unknown-linux-musl");
    let name = format!("ch-eval-build-{}", std::process::id());
    let user = user()?;
    let script = format!(
        "cargo build --release --locked --target {triple} -p ch-cli --bin ch -p ch-eval --bin ch-eval \
         && install -m 755 /target/{triple}/release/ch /out/ch-{arch} \
         && install -m 755 /target/{triple}/release/ch-eval /out/ch-eval-{arch} \
         && chown {user} /out/ch-{arch} /out/ch-eval-{arch}"
    );
    eprintln!(
        "ch-eval: building ch and ch-eval for {triple} (the first build takes a few minutes)"
    );
    let status = Command::new("docker")
        .args(["run", "--rm", "--name", &name, "--label", LABEL])
        .args([
            "-v",
            &format!("{REGISTRY_VOLUME}:/usr/local/cargo/registry"),
        ])
        .args(["-v", &format!("{TARGET_VOLUME}:/target")])
        .args(["-v", &format!("{}:/src:ro", source.display())])
        .args(["-v", &format!("{}:/out", cache.display())])
        .args([
            "-e",
            "CARGO_TARGET_DIR=/target",
            "-w",
            "/src",
            BUILDER_IMAGE,
        ])
        .args(["sh", "-c", &script])
        .stdin(Stdio::null())
        .status()
        .context("running `docker run`")?;
    if !status.success() {
        bail!("building the static binaries in Docker failed");
    }
    Ok(paths)
}

/// The static binaries for `arch`, built first when either is missing or older than
/// the source.
pub fn ensure_binaries(source: &Path, arch: &str) -> anyhow::Result<Binaries> {
    let paths = binary_paths(source, arch);
    let built = [&paths.ch, &paths.ch_eval]
        .iter()
        .map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .collect::<Option<Vec<_>>>()
        .and_then(|times| times.into_iter().min());
    match built {
        Some(t) if newest_source(source)?.is_none_or(|s| s <= t) => Ok(paths),
        _ => build_binaries(source, arch),
    }
}

/// The newest modification time among the files the binaries are built from: the
/// workspace's `Cargo.toml` and `Cargo.lock`, `crates/`, `plugins/` and `prompts/`.
fn newest_source(source: &Path) -> anyhow::Result<Option<SystemTime>> {
    let mut newest = None;
    for top in ["Cargo.toml", "Cargo.lock", "crates", "plugins", "prompts"] {
        newest_in(&source.join(top), &mut newest)?;
    }
    Ok(newest)
}

fn newest_in(path: &Path, newest: &mut Option<SystemTime>) -> anyhow::Result<()> {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    if meta.is_dir() {
        if path.file_name().is_some_and(|n| n == "target") {
            return Ok(());
        }
        for entry in
            std::fs::read_dir(path).with_context(|| format!("reading {}", path.display()))?
        {
            newest_in(&entry?.path(), newest)?;
        }
    } else if let Ok(t) = meta.modified() {
        *newest = Some(newest.map_or(t, |n| n.max(t)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::backend::Sink;
    use crate::ledger::TOKEN_ENV;

    use super::*;

    fn backend() -> DockerBackend {
        DockerBackend {
            docker: "docker".into(),
            binaries: Binaries {
                ch: "/src/target/ch-eval/ch-aarch64".into(),
                ch_eval: "/src/target/ch-eval/ch-eval-aarch64".into(),
            },
            user: "501:20".into(),
            labels: vec!["ch-eval".into(), "ch-eval.invocation=abc".into()],
            proxy: None,
            route: None,
            images: BTreeMap::new(),
            ch_hash: String::new(),
            cleanup: None,
        }
    }

    fn job<'a>(args: &'a [String], env: &'a [(String, String)]) -> Job<'a> {
        Job {
            id: "ch-eval-abc-1",
            image: "ch-eval-base",
            work: Path::new("/out/runs/t/base/1/work"),
            ch_home: Path::new("/out/runs/t/base/1/ch-home"),
            git_dir: Path::new("/out/runs/t/base/1/diff.git"),
            ch_args: args,
            env,
            verify: "python3 -I test_hidden.py",
        }
    }

    #[test]
    fn agent_container_command_line() {
        let args = ["-p".to_string(), "-".to_string()];
        let env = [(TOKEN_ENV.to_string(), "cheval-secret-token".to_string())];
        let b = backend();
        let line = shell_words::join(b.create_args(
            Step::Agent,
            &job(&args, &env),
            Duration::from_secs(900),
        ));
        assert_eq!(
            line,
            "create --name ch-eval-abc-1-agent --label ch-eval --label 'ch-eval.invocation=abc' \
             --init --pull never --user 501:20 --cap-drop ALL --security-opt no-new-privileges \
             --pids-limit 1024 --memory 4g --cpus 2 -e 'HOME=/tmp' \
             --entrypoint /usr/local/bin/ch-eval \
             -v /src/target/ch-eval/ch-eval-aarch64:/usr/local/bin/ch-eval:ro \
             -i --network ch-eval-abc-1-net -e 'CH_HOME=/ch-home' -e CH_EVAL_API_KEY \
             -v /out/runs/t/base/1/work:/work -v /out/runs/t/base/1/ch-home:/ch-home \
             -v /src/target/ch-eval/ch-aarch64:/usr/local/bin/ch:ro -w /work -- ch-eval-base \
             limit 960 -- ch -p -"
        );
        assert!(
            !line.contains("cheval-secret-token"),
            "variables go by name"
        );
    }

    #[test]
    fn diff_and_check_have_no_network() {
        let b = backend();
        let diff = b.create_args(Step::Diff, &job(&[], &[]), Duration::from_secs(120));
        let line = shell_words::join(&diff);
        assert!(line.contains("--network none"), "{line}");
        assert!(
            line.contains("-v /out/runs/t/base/1/work:/work:ro"),
            "{line}"
        );
        assert!(
            line.contains("-v /out/runs/t/base/1/diff.git:/baseline "),
            "{line}"
        );
        assert!(
            line.contains("-- ch-eval-base limit 180 -- sh -c"),
            "{line}"
        );
        assert_eq!(diff.last().map(String::as_str), Some(DIFF_SCRIPT));

        let check = shell_words::join(b.create_args(
            Step::Verify,
            &job(&[], &[]),
            Duration::from_secs(600),
        ));
        assert!(check.contains("--network none"), "{check}");
        assert!(!check.contains("/usr/local/bin/ch:ro"), "{check}");
        assert!(!check.contains(" -i "), "{check}");
        assert!(
            check.ends_with(
                "-w /work -- ch-eval-base limit 660 -- sh -c 'python3 -I test_hidden.py'"
            ),
            "{check}"
        );
        // The dry-run line names the diff script instead of printing it.
        let shown = b.command(Step::Diff, &job(&[], &[]));
        assert_eq!(shown.last().map(String::as_str), Some("<diff script>"));
    }

    #[test]
    fn identity_covers_the_binary_and_the_image() {
        let mut b = backend();
        b.ch_hash = "ch1".into();
        b.images.insert("a".into(), "sha256:1".into());
        b.images.insert("b".into(), "sha256:2".into());
        let a = b.identity("a");
        assert_ne!(a, b.identity("b"));
        b.ch_hash = "ch2".into();
        assert_ne!(a, b.identity("a"));
    }

    /// A stand-in for `docker` that logs its arguments and answers from files in
    /// `dir`: `state.json` for `inspect`, `start_sleep` for how long `start` runs, and
    /// `create_fails` to make `create` fail.
    fn fake_docker(dir: &Path) -> DockerBackend {
        let script = dir.join("docker");
        let d = dir.display();
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$*\" >> {d}/log\n\
                 case \"$1\" in\n\
                 create) [ -e {d}/create_fails ] && {{ echo 'No such image' >&2; exit 1; }}; echo id ;;\n\
                 start) [ -e {d}/start_sleep ] && exec sleep \"$(cat {d}/start_sleep)\"; echo out ;;\n\
                 inspect) cat {d}/state.json ;;\n\
                 esac\n"
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        DockerBackend {
            docker: script.display().to_string(),
            ..backend()
        }
    }

    fn io(dir: &Path) -> Io<'static> {
        let sink = |n: &str| Sink {
            file: std::fs::File::create(dir.join(n)).unwrap(),
            cap: 1 << 20,
        };
        Io {
            stdin: None,
            stdout: sink("out"),
            stderr: sink("err"),
        }
    }

    fn log(dir: &Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("log"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    #[tokio::test]
    async fn the_exit_code_comes_from_inspect_and_errors_are_start_failures() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let b = fake_docker(d);
        std::fs::write(d.join("state.json"), r#"{"ExitCode":125,"Error":""}"#).unwrap();
        let exit = b
            .run(Step::Verify, &job(&[], &[]), io(d), Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!((exit.code, exit.start_error), (Some(125), None));
        let log = log(d);
        assert!(
            log[0].starts_with("create --name ch-eval-abc-1-check"),
            "{log:?}"
        );
        assert!(log[0].contains("--pull never"));
        assert_eq!(log[1], "start --attach ch-eval-abc-1-check");
        assert!(
            log.iter().any(|l| l == "rm --force ch-eval-abc-1-check"),
            "{log:?}"
        );

        std::fs::write(
            d.join("state.json"),
            r#"{"ExitCode":255,"Error":"exec format error"}"#,
        )
        .unwrap();
        let exit = b
            .run(Step::Verify, &job(&[], &[]), io(d), Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(exit.start_error.as_deref(), Some("exec format error"));

        std::fs::write(d.join("create_fails"), "").unwrap();
        let err = b
            .run(Step::Verify, &job(&[], &[]), io(d), Duration::from_secs(30))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("No such image"), "{err}");
    }

    #[tokio::test]
    async fn the_container_is_killed_at_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let b = fake_docker(d);
        std::fs::write(d.join("start_sleep"), "30").unwrap();
        let started = Instant::now();
        let exit = b
            .run(
                Step::Verify,
                &job(&[], &[]),
                io(d),
                Duration::from_millis(500),
            )
            .await
            .unwrap();
        assert!(exit.timed_out);
        assert!(started.elapsed() < Duration::from_secs(10));
        let log = log(d);
        let kill = log.iter().position(|l| l == "kill ch-eval-abc-1-check");
        let rm = log
            .iter()
            .position(|l| l == "rm --force ch-eval-abc-1-check");
        assert!(kill.is_some() && kill < rm, "{log:?}");
        assert!(!log.iter().any(|l| l.starts_with("inspect")), "{log:?}");
    }

    #[tokio::test]
    async fn the_agent_gets_its_own_network_for_its_run() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let b = fake_docker(d);
        std::fs::write(d.join("state.json"), r#"{"ExitCode":0,"Error":""}"#).unwrap();
        b.run(Step::Agent, &job(&[], &[]), io(d), Duration::from_secs(30))
            .await
            .unwrap();
        let log = log(d);
        assert!(
            log[0].starts_with("network create --internal --label ch-eval")
                && log[0].ends_with("ch-eval-abc-1-net"),
            "{log:?}"
        );
        assert!(log[1].contains("--network ch-eval-abc-1-net"), "{log:?}");
        assert_eq!(log.last().unwrap(), "network rm ch-eval-abc-1-net");
    }

    #[test]
    fn mount_paths_are_absolute_without_colons() {
        assert!(check_mount_path(Path::new("/a/b,c=d")).is_ok());
        assert!(check_mount_path(Path::new("rel/dir")).is_err());
        assert!(check_mount_path(Path::new("/a:b")).is_err());
    }

    #[test]
    fn a_fresh_heartbeat_keeps_an_invocation_whatever_its_pid_says() {
        let dir = tempfile::tempdir().unwrap();
        let beat = dir.path().join("heartbeat");
        touch(&beat).unwrap();
        let runner = |started: &str, heartbeat: &Path| Runner {
            invocation: "x".into(),
            pid: std::process::id().to_string(),
            started: started.into(),
            heartbeat: heartbeat.display().to_string(),
        };
        // Its start time as another locale or time zone would print it.
        assert!(!runner("Thu Jan  1 00:00:00 1970", &beat).is_gone());
        let missing = dir.path().join("missing");
        assert!(runner("Thu Jan  1 00:00:00 1970", &missing).is_gone());
        let me = process_started(std::process::id()).unwrap();
        assert!(!runner(&me, &missing).is_gone());
        let dead = Runner {
            pid: "999999".into(),
            ..runner(&me, &missing)
        };
        assert!(dead.is_gone());
    }

    #[test]
    fn a_running_process_is_told_from_a_gone_one() {
        let me = process_started(std::process::id()).unwrap();
        assert!(!me.is_empty());
        assert_eq!(process_started(std::process::id()), Some(me));
        assert_eq!(process_started(999_999), None);
    }

    #[test]
    fn the_binaries_are_stale_when_a_source_is_newer() {
        let src = tempfile::tempdir().unwrap();
        let s = src.path();
        for dir in [
            "crates/ch-cli/src",
            "crates/ch-eval/src",
            "plugins/p/target",
            "prompts",
        ] {
            std::fs::create_dir_all(s.join(dir)).unwrap();
        }
        std::fs::write(s.join("crates/ch-cli/src/main.rs"), "").unwrap();
        let old = newest_source(s).unwrap().unwrap();
        // Files in target directories don't count; ch-eval's own sources do, since
        // the proxy is built from them.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(s.join("plugins/p/target/x"), "").unwrap();
        assert_eq!(newest_source(s).unwrap().unwrap(), old);
        std::fs::write(s.join("crates/ch-eval/src/lib.rs"), "").unwrap();
        assert!(newest_source(s).unwrap().unwrap() > old);
    }
}
