//! One configured MCP server as a kernel [`ToolSource`].

use std::collections::HashMap;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use async_trait::async_trait;
use ch_executor_local::group::terminate_group;
use ch_kernel::util::printable;
use ch_kernel::{Cx, Tool, ToolSink, ToolSource};
use nix::unistd::Pid;
// Roots are deprecated upstream (SEP-2577), but they are how ch tells a server the
// workspace (ADR 0006 §10).
use rmcp::model::{
    ClientCapabilities, Implementation, InitializeRequestParams, PaginatedRequestParams,
};
#[allow(deprecated)]
use rmcp::model::{ListRootsResult, Root};
use rmcp::service::{NotificationContext, RequestContext, RunningServiceCancellationToken};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientHandler, ErrorData, RoleClient, serve_client};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

use crate::config::{ServerConfig, Transport};
use crate::launch::{self, HttpLaunch, Launch, Places, StdioLaunch};
use crate::procs::{self, StartGuard, StderrTail};
use crate::redact::Redactor;
use crate::tool::{Link, McpTool};
use crate::transport::LineTransport;

/// Most `tools/list` pages followed.
pub const MAX_PAGES: usize = 10;

/// Time between SIGTERM and SIGKILL when stopping a server.
pub const KILL_GRACE: Duration = Duration::from_secs(2);

/// Default time a tool call may take.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Lines of stderr shown with an exit.
const STDERR_LINES: usize = 5;

/// A configured MCP server. Started and stopped by the session; restartable.
pub struct McpServer {
    cfg: ServerConfig,
    places: Places,
    id: String,
    run: AsyncMutex<Option<Running>>,
    /// A process spawned by a start still in its handshake, so a stop that comes then
    /// (a startup timeout, a reconnect, the session ending) can kill it.
    pending: Mutex<Option<Process>>,
    /// Bumped by every stop: a start that began before a stop doesn't install itself.
    epoch: AtomicU64,
    /// For a project server: the trust hash the user trusted. Checked again before every
    /// spawn, so a server whose entry or files changed doesn't start.
    trusted_hash: Option<String>,
    info: Mutex<Option<String>>,
}

/// One start of the server.
struct Running {
    stopping: Arc<AtomicBool>,
    cancel: RunningServiceCancellationToken,
    process: Option<Process>,
    monitor: JoinHandle<()>,
}

#[derive(Clone)]
struct Process {
    pgid: i32,
    child: Arc<AsyncMutex<Child>>,
    stdin: Arc<AsyncMutex<Option<ChildStdin>>>,
    stderr: Arc<StderrTail>,
    /// Set by the first stop. A second one (the exit monitor, then the session) would
    /// signal a group whose leader was reaped, and its number may be reused by then.
    stopped: Arc<AtomicBool>,
}

impl Process {
    /// Closes stdin, then SIGTERM to the group, then SIGKILL after [`KILL_GRACE`]. A
    /// write blocked on a full pipe holds stdin; then the signals come without waiting
    /// for it.
    async fn stop(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Ok(mut stdin) = self.stdin.try_lock() {
            drop(stdin.take());
        }
        let mut child = self.child.lock().await;
        terminate_group(Pid::from_raw(self.pgid), &mut child, KILL_GRACE).await;
        procs::unregister(self.pgid);
    }

    /// The exit status, if the process exits within `wait`.
    async fn status_within(&self, wait: Duration) -> Option<ExitStatus> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if let Ok(Some(status)) = self.child.lock().await.try_wait() {
                return Some(status);
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Waits for the process to exit.
    async fn exited(&self) -> ExitStatus {
        loop {
            if let Ok(Some(status)) = self.child.lock().await.try_wait() {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

impl McpServer {
    /// A server the user's own config defines.
    pub fn new(cfg: ServerConfig, places: Places) -> Self {
        Self {
            id: cfg.source_id(),
            cfg,
            places,
            run: AsyncMutex::new(None),
            pending: Mutex::new(None),
            epoch: AtomicU64::new(0),
            trusted_hash: None,
            info: Mutex::new(None),
        }
    }

    /// A project server the user trusted with `hash` ([`launch::trust_hash`]).
    pub fn trusted(cfg: ServerConfig, places: Places, hash: String) -> Self {
        Self {
            trusted_hash: Some(hash),
            ..Self::new(cfg, places)
        }
    }

    pub fn config(&self) -> &ServerConfig {
        &self.cfg
    }

    /// `name version` from the server's handshake, made printable.
    pub fn server_info(&self) -> Option<String> {
        self.info.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn call_timeout(&self) -> Duration {
        self.cfg
            .call_timeout_secs
            .map_or(DEFAULT_CALL_TIMEOUT, Duration::from_secs)
    }

    async fn stop_running(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(p) = pending {
            p.stop().await;
        }
        let Some(run) = self.run.lock().await.take() else {
            return;
        };
        run.stopping.store(true, Ordering::SeqCst);
        run.cancel.cancel();
        if let Some(p) = &run.process {
            p.stop().await;
        }
        run.monitor.abort();
    }

    async fn connect(&self, sink: ToolSink) -> anyhow::Result<()> {
        let epoch = self.epoch.load(Ordering::SeqCst);
        *self.info.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if let Some(trusted) = &self.trusted_hash
            && launch::trust_hash(&self.cfg, &self.places) != *trusted
        {
            bail!(
                "its entry, or a file it runs, changed since you trusted it; restart ch to \
                 review it again"
            );
        }
        let launch = launch::resolve(&self.cfg, &self.places, &launch::process_env)?;
        let redactor = Arc::new(Redactor::new(launch.secret_values()));
        let shared = Arc::new(Shared {
            sink,
            prefix: self.cfg.prefix(),
            server: self.cfg.name.clone(),
            call_timeout: self.call_timeout(),
            redactor: redactor.clone(),
            link: OnceLock::new(),
            listing: AsyncMutex::new(()),
            dirty: AtomicBool::new(false),
        });
        let handler = Handler {
            shared: shared.clone(),
            root: url::Url::from_directory_path(&self.places.workspace)
                .map(|u| u.to_string())
                .unwrap_or_default(),
        };
        // Kills the process if this start is dropped before handing it over.
        let mut guard: Option<StartGuard> = None;
        let (service, process) = match launch {
            Launch::Stdio(l) => {
                let process = spawn(&l, &self.cfg.name)?;
                guard = Some(StartGuard::new(process.pgid));
                *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(process.clone());
                let transport = {
                    let mut child = process.child.lock().await;
                    let stdout = child.stdout.take().context("no stdout")?;
                    LineTransport::with_writer(stdout, process.stdin.clone(), &self.cfg.name)
                };
                let served = serve_client(handler, transport).await;
                // Whoever takes it stops it: this start, or a stop that came meanwhile.
                let ours = (self.pending.lock().unwrap_or_else(|e| e.into_inner()))
                    .take()
                    .is_some();
                match served {
                    Ok(s) if ours => (s, Some(process)),
                    Ok(s) => {
                        // The stop that took it stops it.
                        if let Some(g) = &mut guard {
                            g.disarm();
                        }
                        s.cancellation_token().cancel();
                        bail!("it was stopped while starting");
                    }
                    Err(e) => {
                        let reason = match process.status_within(Duration::from_millis(500)).await {
                            Some(status) => exit_reason(status, &process.stderr, &redactor),
                            None => format!("it didn't complete the MCP handshake: {e}"),
                        };
                        if ours {
                            process.stop().await;
                        }
                        if let Some(g) = &mut guard {
                            g.disarm();
                        }
                        bail!("{}", redactor.redact(&reason));
                    }
                }
            }
            Launch::Http(h) => {
                let transport = StreamableHttpClientTransport::from_config(http_config(&h)?);
                match serve_client(handler, transport).await {
                    Ok(s) => (s, None),
                    Err(e) => bail!(
                        "couldn't connect to {}: {}",
                        crate::redact::url_without_credentials(&h.url),
                        redactor.redact(&e.to_string())
                    ),
                }
            }
        };
        if let Some(info) = service.peer_info()
            && let Some(s) = &info.server_info
        {
            *self.info.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(printable(&format!("{} {}", s.name, s.version)));
        }
        let peer = service.peer().clone();
        let _ = shared.link.set(Arc::new(Link {
            server: self.cfg.name.clone(),
            peer,
            call_timeout: shared.call_timeout,
            redactor: redactor.clone(),
        }));
        let stopping = Arc::new(AtomicBool::new(false));
        let cancel = service.cancellation_token();
        let end_service = service.cancellation_token();
        let monitor = tokio::spawn({
            let stopping = stopping.clone();
            let shared = shared.clone();
            let process = process.clone();
            async move {
                // The connection ends, or the process exits while something else (a
                // grandchild) keeps its stdout open: either way the server is gone.
                let waiting = service.waiting();
                let exited = match &process {
                    Some(p) => tokio::select! {
                        _ = waiting => None,
                        status = p.exited() => Some(status),
                    },
                    None => {
                        let _ = waiting.await;
                        None
                    }
                };
                // Pending calls fail now rather than at their timeouts.
                end_service.cancel();
                if stopping.load(Ordering::SeqCst) {
                    return;
                }
                let reason = match &process {
                    Some(p) => {
                        let status = match exited {
                            Some(status) => Some(status),
                            None => p.status_within(Duration::from_secs(1)).await,
                        };
                        let reason = match status {
                            Some(status) => exit_reason(status, &p.stderr, &shared.redactor),
                            None => "it closed its stdout".to_string(),
                        };
                        p.stop().await;
                        reason
                    }
                    None => "the connection closed".to_string(),
                };
                shared.sink.exited(shared.redactor.redact(&reason));
            }
        });
        let running = Running {
            stopping,
            cancel,
            process,
            monitor,
        };
        {
            let mut run = self.run.lock().await;
            if self.epoch.load(Ordering::SeqCst) != epoch {
                drop(run);
                running.stopping.store(true, Ordering::SeqCst);
                running.cancel.cancel();
                if let Some(p) = &running.process {
                    p.stop().await;
                }
                if let Some(g) = &mut guard {
                    g.disarm();
                }
                bail!("it was stopped while starting");
            }
            *run = Some(running);
        }
        if let Some(g) = &mut guard {
            g.disarm();
        }
        shared.first_list().await
    }
}

#[async_trait]
impl ToolSource for McpServer {
    fn id(&self) -> &str {
        &self.id
    }

    fn prefix(&self) -> String {
        self.cfg.prefix()
    }

    /// Only the user's own config can mark tools read-only: a project entry's
    /// `read_only_tools` is ignored.
    fn read_only_tools(&self) -> Vec<String> {
        if self.cfg.origin.is_project() {
            return Vec::new();
        }
        let prefix = self.cfg.prefix();
        self.cfg
            .read_only_tools
            .iter()
            .map(|name| {
                if name.starts_with(&prefix) {
                    name.clone()
                } else {
                    ch_kernel::tool_source::outside_tool_name(&prefix, name)
                }
            })
            .collect()
    }

    fn startup_timeout(&self) -> Duration {
        Duration::from_secs(self.cfg.startup_timeout_secs.unwrap_or(10))
    }

    async fn start(&self, sink: ToolSink, _cx: &Cx) -> anyhow::Result<()> {
        self.stop_running().await;
        let epoch = self.epoch.load(Ordering::SeqCst);
        let result = self.connect(sink).await;
        // Clean up only this start: a newer start or a stop owns the server otherwise.
        if result.is_err() && self.epoch.load(Ordering::SeqCst) == epoch {
            self.stop_running().await;
        }
        result.map_err(|e| anyhow!("{}", printable(&format!("{e:#}"))))
    }

    async fn stop(&self) {
        self.stop_running().await;
    }
}

/// Spawns a stdio server in its own process group, with exactly `l.env`.
fn spawn(l: &StdioLaunch, name: &str) -> anyhow::Result<Process> {
    let mut cmd = Command::new(&l.program);
    cmd.args(&l.args)
        .env_clear()
        .envs(l.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&l.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(false);
    let mut child = cmd.spawn().with_context(|| {
        format!(
            "couldn't start {}",
            printable(&l.program.display().to_string())
        )
    })?;
    let pid = child.id().context("the server exited at once")?;
    let pgid = i32::try_from(pid).context("pid out of range")?;
    procs::register(pgid);
    let stderr = Arc::new(StderrTail::default());
    if let Some(pipe) = child.stderr.take() {
        let tail = stderr.clone();
        tokio::spawn(async move { tail.drain(pipe).await });
    }
    let stdin = child.stdin.take();
    tracing::info!(server = name, pid, "started MCP server");
    Ok(Process {
        pgid,
        child: Arc::new(AsyncMutex::new(child)),
        stdin: Arc::new(AsyncMutex::new(stdin)),
        stderr,
        stopped: Arc::new(AtomicBool::new(false)),
    })
}

/// The HTTP transport's settings: the URL and static headers. rmcp's default client
/// follows only same-origin 307 and 308 redirects, so the headers never go to another
/// origin.
fn http_config(h: &HttpLaunch) -> anyhow::Result<StreamableHttpClientTransportConfig> {
    let mut headers = HashMap::new();
    for (k, v) in &h.headers {
        let name = http::HeaderName::try_from(k.as_str())
            .with_context(|| format!("invalid header name `{}`", printable(k)))?;
        let value = http::HeaderValue::try_from(v.as_str())
            .with_context(|| format!("invalid value for header `{}`", printable(k)))?;
        headers.insert(name, value);
    }
    Ok(StreamableHttpClientTransportConfig::with_uri(h.url.as_str()).custom_headers(headers))
}

/// "exited with status 1; stderr: ...". The stderr tail is redacted before it is cut,
/// so a cut can't separate a secret from what marks it as one.
fn exit_reason(status: ExitStatus, stderr: &StderrTail, redactor: &Redactor) -> String {
    use std::os::unix::process::ExitStatusExt as _;
    let how = match (status.code(), status.signal()) {
        (Some(code), _) => format!("it exited with status {code}"),
        (None, Some(sig)) => format!("it was killed by signal {sig}"),
        _ => "it exited".to_string(),
    };
    let tail = redactor.redact(&stderr.tail(STDERR_LINES, 16 * 1024));
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

/// What a connection's handler and its tools share.
struct Shared {
    sink: ToolSink,
    prefix: String,
    server: String,
    call_timeout: Duration,
    redactor: Arc<Redactor>,
    /// Set once the handshake is done.
    link: OnceLock<Arc<Link>>,
    /// Held while listing, so notifications that arrive meanwhile cause one more list.
    listing: AsyncMutex<()>,
    dirty: AtomicBool,
}

impl Shared {
    /// Lists the tools, following up to [`MAX_PAGES`] pages.
    async fn list(&self) -> anyhow::Result<Vec<Arc<dyn Tool>>> {
        let link = self.link.get().context("not connected")?;
        let mut cursor: Option<String> = None;
        let mut listed = Vec::new();
        for page in 1..=MAX_PAGES {
            let params = PaginatedRequestParams::default().with_cursor(cursor.take());
            let result =
                tokio::time::timeout(self.call_timeout, link.peer.list_tools(Some(params)))
                    .await
                    .map_err(|_| {
                        anyhow!(
                            "tools/list got no answer within {} s",
                            self.call_timeout.as_secs()
                        )
                    })?
                    .map_err(|e| {
                        anyhow!(
                            "tools/list failed: {}",
                            self.redactor.redact(&e.to_string())
                        )
                    })?;
            listed.extend(result.tools);
            cursor = result.next_cursor;
            if cursor.is_none() {
                break;
            }
            if page == MAX_PAGES {
                self.sink_warn(format!(
                    "the MCP server `{}` lists its tools in more than {MAX_PAGES} pages; \
                     only the first {MAX_PAGES} are used",
                    self.server
                ));
            }
        }
        Ok(listed
            .iter()
            .filter_map(|t| serde_json::to_value(t).ok())
            .filter_map(|v| McpTool::from_listed(&v, &self.prefix, link))
            .map(|t| Arc::new(t) as Arc<dyn Tool>)
            .collect())
    }

    fn sink_warn(&self, message: String) {
        tracing::warn!("{message}");
        self.sink.warn(message);
    }

    async fn first_list(&self) -> anyhow::Result<()> {
        let guard = self.listing.lock().await;
        self.dirty.store(false, Ordering::SeqCst);
        let tools = self.list().await?;
        self.sink.set_tools(tools);
        drop(guard);
        if self.dirty.load(Ordering::SeqCst) {
            self.relist().await;
        }
        Ok(())
    }

    /// Lists again after `notifications/tools/list_changed`. A burst of notifications
    /// causes at most one list after the one running.
    async fn relist(&self) {
        self.dirty.store(true, Ordering::SeqCst);
        if self.link.get().is_none() {
            // The first list hasn't run yet and will see `dirty`.
            return;
        }
        loop {
            let Ok(guard) = self.listing.try_lock() else {
                return;
            };
            while self.dirty.swap(false, Ordering::SeqCst) {
                match self.list().await {
                    Ok(tools) => self.sink.set_tools(tools),
                    Err(e) => self.sink_warn(format!(
                        "couldn't list the tools of the MCP server `{}` again: {}",
                        self.server,
                        printable(&format!("{e:#}"))
                    )),
                }
            }
            drop(guard);
            if !self.dirty.load(Ordering::SeqCst) {
                return;
            }
        }
    }
}

/// Answers the server: the workspace as the only root, nothing else. No sampling and no
/// elicitation are declared, so the server can't spend the user's API key or prompt
/// the user.
#[derive(Clone)]
struct Handler {
    shared: Arc<Shared>,
    root: String,
}

impl ClientHandler for Handler {
    fn get_info(&self) -> InitializeRequestParams {
        let capabilities: ClientCapabilities =
            serde_json::from_value(serde_json::json!({ "roots": {} })).unwrap_or_default();
        InitializeRequestParams::new(
            capabilities,
            Implementation::new("ch", env!("CARGO_PKG_VERSION")),
        )
    }

    #[allow(deprecated)]
    async fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, ErrorData> {
        Ok(ListRootsResult::new(vec![
            Root::new(self.root.clone()).with_name("workspace"),
        ]))
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        // Not inline: listing needs the service loop this notification arrived on.
        let shared = self.shared.clone();
        tokio::spawn(async move { shared.relist().await });
    }
}

impl std::fmt::Debug for McpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServer").field("cfg", &self.cfg).finish()
    }
}

/// For `/mcp`: how the server is reached, made printable.
pub fn endpoint(cfg: &ServerConfig) -> String {
    match &cfg.transport {
        Transport::Stdio { command, .. } => printable(command),
        Transport::Http { url, .. } => printable(&crate::redact::url_without_credentials(url)),
    }
}
