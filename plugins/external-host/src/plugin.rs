//! One external plugin: its process, its connection, and its tools as a tool source.

use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use async_trait::async_trait;
use ch_executor_local::group::terminate_group;
use ch_kernel::reexports::CancellationToken;
use ch_kernel::tool_source::BACKGROUND_START;
use ch_kernel::util::printable;
use ch_kernel::{AgentEvent, Cx, EventBus, Tool, ToolOutput, ToolSink, ToolSource, ToolSpec};
use ch_mcp_bridge::launch::{INHERITED_ENV, expand, process_env, resolve_program, variables_in};
use ch_mcp_bridge::procs::{self, StartGuard, StderrTail};
use ch_mcp_bridge::redact::Redactor;
use ch_plugin_protocol::{
    InitializeParams, InitializeResult, LogParams, NotifyParams, PROTOCOL_VERSION, ToolCallParams,
    ToolCallResult, ToolDef, ToolsChangedParams, method,
};
use nix::unistd::Pid;
use serde_json::Value;
use tokio::process::{Child, Command};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

use crate::conn::{CallError, Conn};
use crate::manifest::Manifest;

pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(120);
pub const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(10);
const KILL_GRACE: Duration = Duration::from_secs(2);
const SHUTDOWN_WAIT: Duration = Duration::from_secs(1);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the plugin's source and its adapters (hook, prompt, commands, events) share.
pub struct Shared {
    pub m: Manifest,
    /// The connection while the plugin runs.
    live: Mutex<Option<Arc<Conn>>>,
    /// What the plugin declared at its last `initialize`. Kept after it exits, so its
    /// pre_tool hooks keep failing closed.
    caps: Mutex<Option<InitializeResult>>,
    redactor: Mutex<Arc<Redactor>>,
    /// Whether this conversation's system prompt has the plugin's blocks.
    pub system_delivered: AtomicBool,
}

impl Shared {
    pub fn new(m: Manifest) -> Arc<Self> {
        Arc::new(Self {
            m,
            live: Mutex::new(None),
            caps: Mutex::new(None),
            redactor: Mutex::new(Arc::new(Redactor::default())),
            system_delivered: AtomicBool::new(false),
        })
    }

    pub fn conn(&self) -> Option<Arc<Conn>> {
        lock(&self.live).clone().filter(|c| !c.is_closed())
    }

    pub fn caps(&self) -> Option<InitializeResult> {
        lock(&self.caps).clone()
    }

    pub fn redact(&self, text: &str) -> String {
        lock(&self.redactor).redact(text)
    }

    pub fn call_timeout(&self) -> Duration {
        self.m
            .call_timeout_secs
            .map_or(DEFAULT_CALL_TIMEOUT, Duration::from_secs)
    }

    pub fn hook_timeout(&self) -> Duration {
        self.m
            .hook_timeout_secs
            .map_or(DEFAULT_HOOK_TIMEOUT, Duration::from_secs)
    }

    /// Calls the plugin, if it runs.
    pub async fn call(
        &self,
        method: &str,
        params: impl serde::Serialize,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value, CallError> {
        match self.conn() {
            Some(conn) => conn.call(method, params, timeout, cancel).await,
            None => Err(CallError::Closed),
        }
    }

    fn tools(self: &Arc<Self>, defs: &[ToolDef]) -> Vec<Arc<dyn Tool>> {
        let prefix = self.m.prefix();
        defs.iter()
            .map(|d| {
                Arc::new(ExtTool {
                    spec: ToolSpec {
                        name: ch_kernel::tool_source::outside_tool_name(&prefix, &d.name),
                        description: self.redact(&d.description),
                        input_schema: d.input_schema.clone(),
                        read_only: false,
                    },
                    raw: d.name.clone(),
                    shared: self.clone(),
                }) as Arc<dyn Tool>
            })
            .collect()
    }
}

/// One of a plugin's tools.
struct ExtTool {
    spec: ToolSpec,
    raw: String,
    shared: Arc<Shared>,
}

#[async_trait]
impl Tool for ExtTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn call(&self, input: Value, cx: &Cx) -> ToolOutput {
        let params = ToolCallParams {
            name: self.raw.clone(),
            input,
            tool_use_id: cx.tool_use_id.clone(),
        };
        let s = &self.shared;
        let fail = |why: String| {
            ToolOutput::error(s.redact(&format!(
                "`{}` (plugin `{}`): {why}",
                self.spec.name, s.m.name
            )))
        };
        match s
            .call(method::TOOL_CALL, params, s.call_timeout(), &cx.cancel)
            .await
        {
            Ok(v) => match serde_json::from_value::<ToolCallResult>(v) {
                Ok(r) if r.is_error => ToolOutput::error(s.redact(&r.content)),
                Ok(r) => ToolOutput::ok(s.redact(&r.content)),
                Err(_) => fail("its result isn't a tool result ch can read".into()),
            },
            Err(CallError::Timeout(d)) => fail(format!(
                "no answer within {} s, so ch asked the plugin to cancel the call",
                d.as_secs()
            )),
            Err(CallError::Cancelled) => fail("cancelled: the user interrupted the turn".into()),
            Err(e) => fail(e.to_string()),
        }
    }
}

#[derive(Clone)]
struct Process {
    pgid: i32,
    child: Arc<AsyncMutex<Child>>,
    stderr: Arc<StderrTail>,
    /// Set by the first stop: a second (the exit monitor, then the session) would signal
    /// a group whose leader was reaped, and its number may be reused by then.
    stopped: Arc<AtomicBool>,
}

impl Process {
    /// SIGTERM to the group, then SIGKILL after [`KILL_GRACE`]. Stdin is closed by the
    /// connection before this.
    async fn stop(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut child = self.child.lock().await;
        terminate_group(Pid::from_raw(self.pgid), &mut child, KILL_GRACE).await;
        procs::unregister(self.pgid);
    }

    async fn exited(&self) -> ExitStatus {
        loop {
            if let Ok(Some(status)) = self.child.lock().await.try_wait() {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn status_within(&self, wait: Duration) -> Option<ExitStatus> {
        tokio::time::timeout(wait, self.exited()).await.ok()
    }
}

/// The child's variables, and the values the redactor strips.
type Environment = (Vec<(String, String)>, Vec<String>);

struct Running {
    stopping: Arc<AtomicBool>,
    conn: Arc<Conn>,
    process: Process,
    monitor: JoinHandle<()>,
}

/// An external plugin as a tool source. Started and stopped by the session;
/// restartable.
pub struct ExternalPlugin {
    shared: Arc<Shared>,
    id: String,
    workspace: PathBuf,
    read_only: Vec<String>,
    run: AsyncMutex<Option<Running>>,
    /// Spawned by a start still in `initialize`, so a stop then can kill it.
    pending: Mutex<Option<(Process, Arc<Conn>)>>,
    epoch: AtomicU64,
}

impl ExternalPlugin {
    pub fn new(shared: Arc<Shared>, workspace: PathBuf, read_only: Vec<String>) -> Self {
        Self {
            id: shared.m.source_id(),
            shared,
            workspace,
            read_only,
            run: AsyncMutex::new(None),
            pending: Mutex::new(None),
            epoch: AtomicU64::new(0),
        }
    }

    async fn stop_running(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        *lock(&self.shared.live) = None;
        let pending = lock(&self.pending).take();
        if let Some((process, conn)) = pending {
            conn.close();
            process.stop().await;
        }
        let Some(run) = self.run.lock().await.take() else {
            return;
        };
        run.stopping.store(true, Ordering::SeqCst);
        // Ask politely first; a plugin that doesn't answer is stopped anyway.
        let cancel = CancellationToken::new();
        let _ = run
            .conn
            .call(
                method::SHUTDOWN,
                serde_json::json!({}),
                SHUTDOWN_WAIT,
                &cancel,
            )
            .await;
        run.conn.close();
        run.process.stop().await;
        run.monitor.abort();
    }

    /// The child's whole environment: the inherited few, `CH_WORKSPACE`,
    /// `CH_PLUGIN_DIR`, and the manifest's `env` (with `${VAR}` from ch's). Also the
    /// values the redactor should strip.
    fn environment(&self) -> anyhow::Result<Environment> {
        let mut env: Vec<(String, String)> = INHERITED_ENV
            .iter()
            .filter_map(|k| process_env(k).map(|v| (k.to_string(), v)))
            .collect();
        env.push(("CH_WORKSPACE".into(), self.workspace.display().to_string()));
        env.push((
            "CH_PLUGIN_DIR".into(),
            self.shared.m.dir.display().to_string(),
        ));
        let mut secrets = Vec::new();
        for (k, v) in &self.shared.m.env {
            for name in variables_in(v) {
                secrets.extend(process_env(&name));
            }
            let value = expand(v, &process_env).with_context(|| format!("env `{k}`"))?;
            secrets.push(value.clone());
            env.retain(|(existing, _)| existing != k);
            env.push((k.clone(), value));
        }
        Ok((env, secrets))
    }

    /// Spawns the plugin in its own process group. Returns the child (its stdin and
    /// stdout still in it), its process group and its stderr tail.
    fn spawn(&self, env: &[(String, String)]) -> anyhow::Result<(Child, i32, Arc<StderrTail>)> {
        let m = &self.shared.m;
        let program = resolve_program(&m.command, &m.dir, process_env("PATH").as_deref())?;
        let mut cmd = Command::new(&program);
        cmd.args(&m.args)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k, v)))
            .current_dir(&m.dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .process_group(0)
            .kill_on_drop(false);
        let mut child = cmd.spawn().with_context(|| {
            format!(
                "couldn't start {}",
                printable(&program.display().to_string())
            )
        })?;
        let pid = child.id().context("the plugin exited at once")?;
        let pgid = i32::try_from(pid).context("pid out of range")?;
        procs::register(pgid);
        let stderr = Arc::new(StderrTail::default());
        if let Some(pipe) = child.stderr.take() {
            let tail = stderr.clone();
            tokio::spawn(async move { tail.drain(pipe).await });
        }
        tracing::info!(plugin = %m.name, pid, "started external plugin");
        Ok((child, pgid, stderr))
    }
}

/// Ends a start that failed after the spawn: closes the connection, stops the group,
/// and says why, preferring the exit status and stderr when the plugin exited.
async fn fail_start(conn: &Conn, process: &Process, shared: &Shared, why: String) -> anyhow::Error {
    conn.close();
    let status = process.status_within(Duration::from_millis(300)).await;
    process.stop().await;
    match status {
        Some(_) => anyhow!("{}", exit_reason(status, &process.stderr, shared)),
        None => anyhow!("{why}"),
    }
}

/// "it exited with status 1; stderr: ...", redacted before it is cut.
fn exit_reason(status: Option<ExitStatus>, stderr: &StderrTail, shared: &Shared) -> String {
    use std::os::unix::process::ExitStatusExt as _;
    let how = match status.map(|s| (s.code(), s.signal())) {
        Some((Some(code), _)) => format!("it exited with status {code}"),
        Some((None, Some(sig))) => format!("it was killed by signal {sig}"),
        Some(_) => "it exited".into(),
        None => "it closed its stdout".into(),
    };
    let tail = shared.redact(&stderr.tail(5, 16 * 1024));
    let mut cut = tail.len().saturating_sub(1000);
    while !tail.is_char_boundary(cut) {
        cut += 1;
    }
    let tail = &tail[cut..];
    if tail.is_empty() {
        how
    } else {
        let lines: Vec<String> = tail.lines().map(printable).collect();
        format!("{how}; stderr: {}", lines.join(" | "))
    }
}

/// Handles the plugin's notifications.
fn on_notify(shared: Arc<Shared>, sink: ToolSink, events: EventBus) -> crate::conn::OnNotify {
    Box::new(move |m: &str, params: Value| match m {
        method::LOG => {
            if let Ok(p) = serde_json::from_value::<LogParams>(params) {
                // One log line per message, whatever the plugin sends.
                let msg = printable(&shared.redact(&p.message));
                let plugin = shared.m.name.as_str();
                match p.level.as_str() {
                    "error" => tracing::error!(plugin, "{msg}"),
                    "warn" => tracing::warn!(plugin, "{msg}"),
                    "debug" => tracing::debug!(plugin, "{msg}"),
                    _ => tracing::info!(plugin, "{msg}"),
                }
            }
        }
        method::UI_NOTIFY => {
            if let Ok(p) = serde_json::from_value::<NotifyParams>(params) {
                events.emit(AgentEvent::Note {
                    source: shared.m.source_id(),
                    text: ch_kernel::util::terminal_text(&shared.redact(&p.message)),
                });
            }
        }
        method::TOOLS_CHANGED => match serde_json::from_value::<ToolsChangedParams>(params) {
            Ok(p) => {
                if let Some(caps) = lock(&shared.caps).as_mut() {
                    caps.tools = p.tools.clone();
                }
                sink.set_tools(shared.tools(&p.tools));
            }
            Err(_) => sink.warn(format!(
                "the plugin `{}` sent a tool list ch can't read",
                shared.m.name
            )),
        },
        _ => {}
    })
}

#[async_trait]
impl ToolSource for ExternalPlugin {
    fn id(&self) -> &str {
        &self.id
    }

    fn prefix(&self) -> String {
        self.shared.m.prefix()
    }

    fn read_only_tools(&self) -> Vec<String> {
        self.read_only.clone()
    }

    fn startup_timeout(&self) -> Duration {
        Duration::from_secs(self.shared.m.startup_timeout_secs.unwrap_or(10))
    }

    async fn start(&self, sink: ToolSink, cx: &Cx) -> anyhow::Result<()> {
        self.stop_running().await;
        let epoch = self.epoch.load(Ordering::SeqCst);
        let result = self.connect(sink, cx, epoch).await;
        result.map_err(|e| anyhow!("{}", printable(&self.shared.redact(&format!("{e:#}")))))
    }

    async fn stop(&self) {
        self.stop_running().await;
    }
}

impl ExternalPlugin {
    async fn connect(&self, sink: ToolSink, cx: &Cx, epoch: u64) -> anyhow::Result<()> {
        let shared = &self.shared;
        let (env, secrets) = self.environment()?;
        *lock(&shared.redactor) = Arc::new(Redactor::new(secrets));
        let (mut child, pgid, stderr) = self.spawn(&env)?;
        // Kills the process if this start is dropped (aborted) before handing it over.
        let mut guard = StartGuard::new(pgid);
        let (stdout, stdin) = (child.stdout.take(), child.stdin.take());
        let process = Process {
            pgid,
            child: Arc::new(AsyncMutex::new(child)),
            stderr,
            stopped: Arc::new(AtomicBool::new(false)),
        };
        let (Some(stdout), Some(stdin)) = (stdout, stdin) else {
            process.stop().await;
            guard.disarm();
            bail!("the plugin's pipes weren't set up");
        };
        let conn = Arc::new(Conn::new(
            stdout,
            stdin,
            &shared.m.name,
            on_notify(shared.clone(), sink.clone(), cx.events.clone()),
        ));
        // Until it is running, a stop finds it here.
        *lock(&self.pending) = Some((process.clone(), conn.clone()));

        let params = InitializeParams {
            protocol: PROTOCOL_VERSION,
            ch_version: env!("CARGO_PKG_VERSION").into(),
            plugin: shared.m.name.clone(),
            workspace: self.workspace.display().to_string(),
        };
        // As long as the kernel lets a start go on in the background (ADR 0006 §1).
        let wait = self.startup_timeout().max(BACKGROUND_START);
        let cancel = CancellationToken::new();
        let answer = conn.call(method::INITIALIZE, params, wait, &cancel).await;
        let caps = match answer.map(serde_json::from_value::<InitializeResult>) {
            Ok(Ok(caps)) if caps.protocol == PROTOCOL_VERSION => Ok(caps),
            Ok(Ok(caps)) => Err(format!(
                "it speaks protocol {}, and this ch speaks {PROTOCOL_VERSION}",
                caps.protocol
            )),
            Ok(Err(_)) => Err("its initialize result isn't one ch can read".to_string()),
            Err(e) => Err(format!("initialize failed: {e}")),
        };
        let caps = match caps {
            Ok(caps) => caps,
            Err(why) => {
                // Whoever takes it from `pending` stops it: this start, or a stop.
                if lock(&self.pending).take().is_some() {
                    let e = fail_start(&conn, &process, shared, why).await;
                    guard.disarm();
                    return Err(e);
                }
                guard.disarm();
                bail!("it was stopped while starting");
            }
        };

        let mut run = self.run.lock().await;
        let ours = lock(&self.pending).take().is_some();
        if !ours || self.epoch.load(Ordering::SeqCst) != epoch {
            drop(run);
            if ours {
                conn.close();
                process.stop().await;
            }
            guard.disarm();
            bail!("it was stopped while starting");
        }
        // From here to `*run = ...` nothing waits, so the handover can't be cut short.
        let tools = shared.tools(&caps.tools);
        *lock(&shared.caps) = Some(caps);
        let stopping = Arc::new(AtomicBool::new(false));
        let monitor = tokio::spawn({
            let stopping = stopping.clone();
            let shared = shared.clone();
            let conn = conn.clone();
            let process = process.clone();
            let sink = sink.clone();
            async move {
                let status = tokio::select! {
                    _ = conn.ended.cancelled() => None,
                    status = process.exited() => Some(status),
                };
                conn.close();
                if stopping.load(Ordering::SeqCst) {
                    return;
                }
                *lock(&shared.live) = None;
                let status = match status {
                    Some(s) => Some(s),
                    None => process.status_within(Duration::from_secs(1)).await,
                };
                let reason = exit_reason(status, &process.stderr, &shared);
                process.stop().await;
                sink.exited(reason);
            }
        });
        *run = Some(Running {
            stopping,
            conn: conn.clone(),
            process,
            monitor,
        });
        guard.disarm();
        *lock(&shared.live) = Some(conn);
        drop(run);
        sink.set_tools(tools);
        Ok(())
    }
}
