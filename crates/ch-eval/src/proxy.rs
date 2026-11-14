//! The credential proxy (`ch-eval proxy`). Each run's agent container has no network
//! but an internal one where this proxy listens, so model calls are its only way out,
//! and the API key never enters it.
//!
//! - **Routes.** `/anthropic/v1/messages` goes to the Anthropic upstream and
//!   `/openai/chat/completions` to the OpenAI-compatible one, both `POST`, and only for
//!   the providers the base config uses. Anything else is refused.
//! - **Auth.** Each run gets a random token as its API key. A request whose token isn't
//!   current is refused. The client's auth headers are dropped, only a short list of
//!   headers is forwarded (`anthropic-beta` only with the betas `ch` itself sends), and
//!   the real key is added. Keys arrive on stdin and are never written or logged.
//! - **Models.** A run may call only its cell's agent and summarizer models; a request
//!   for another is refused and recorded.
//! - **Limits.** Request bodies are capped at [`MAX_REQUEST_BYTES`], at most
//!   [`MAX_BUFFERED`] are held at once, a run may have [`MAX_PER_TOKEN`] requests going,
//!   and headers and bodies must arrive within a time limit. Each connection carries
//!   one request, and each source address (each run has its own network) may hold a
//!   few at once; more are closed at once rather than queued, so one run can't hold up
//!   another.
//! - **Bodies.** The request is parsed, checked, and forwarded as parsed, so what the
//!   upstream reads is what was checked (a body with two `model` keys can't pass one
//!   and send the other).
//! - **Streams** pass through unchanged. The upstream is read to its end even when the
//!   client goes away, so what it charges for is counted.
//! - **Accounting.** The usage in each answer (Anthropic `message_start` and
//!   `message_delta`, cache tokens included; OpenAI's `usage`) is priced with the
//!   pricing of the model that answered, and appended to the run's ledger with the
//!   status and any error event in the stream (see [`crate::ledger`]).
//! - **`--max-cost`.** Once the ledger's total reaches it, new requests get an error
//!   instead of being forwarded, and the refusal is recorded; requests already going
//!   finish.

use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::io::Read as _;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use bytes::Bytes;
use ch_kernel::Pricing;
use futures::StreamExt;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use crate::events::Usage;
use crate::ledger::{self, Call, StateDir, TokenInfo, clean_cost};

/// The largest request body forwarded. A long conversation with tool results is a few
/// MB of JSON; this leaves room and bounds the proxy's memory.
pub const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;
/// Request bodies held in memory at once, across runs.
pub const MAX_BUFFERED: usize = 8;
/// Requests one run may have going at once. `ch` makes one at a time.
pub const MAX_PER_TOKEN: usize = 2;
/// Open connections at once, from all runs.
const MAX_CONNECTIONS: usize = 256;

/// Connection limits; tests shorten them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Open connections from one source address. `ch` uses one or two.
    pub per_source: usize,
    /// How long a request's headers, and then its body, may take to arrive.
    pub header_timeout: Duration,
    pub body_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            per_source: 8,
            header_timeout: Duration::from_secs(30),
            body_timeout: Duration::from_secs(120),
        }
    }
}
/// How much of a response is kept to read its usage when it isn't a stream.
const MAX_JSON_BYTES: usize = 8 * 1024 * 1024;
/// Request headers passed upstream; everything else, auth included, is dropped.
/// `anthropic-beta` is filtered separately.
const FORWARDED: &[&str] = &["content-type", "accept", "anthropic-version", "user-agent"];
/// The betas `ch`'s Anthropic provider sends; others could change what a call costs.
const BETAS: &[&str] = &[
    ch_provider_anthropic::FALLBACK_BETA,
    ch_provider_anthropic::BINDING_BETA,
];
/// Response headers that belong to one connection.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
];

/// A model API the proxy forwards to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Anthropic,
    #[serde(rename = "openai")]
    OpenAi,
}

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
        }
    }

    /// The paths, after the provider's prefix, that ch's provider plugin calls.
    fn allows(self, rest: &str) -> bool {
        match self {
            Self::Anthropic => rest == "/v1/messages",
            Self::OpenAi => rest == "/chat/completions",
        }
    }
}

/// Where a provider's requests go: its `base_url`, without `/v1` for Anthropic and
/// with it for OpenAI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpstreamSpec {
    pub base_url: String,
}

/// `proxy.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProxySpec {
    /// Only the providers the base config uses; the others' routes are refused.
    pub anthropic: Option<UpstreamSpec>,
    pub openai: Option<UpstreamSpec>,
    /// USD; see the module docs.
    pub max_cost: Option<f64>,
    /// Exit once the runner's heartbeat file is older than this many seconds.
    pub heartbeat_secs: Option<u64>,
}

/// The keys, as the runner writes them to the proxy's stdin.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Keys {
    pub anthropic: Option<String>,
    pub openai: Option<String>,
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Keys { .. }")
    }
}

/// An upstream with its key.
#[derive(Clone)]
pub struct Upstream {
    pub base_url: String,
    pub key: String,
}

impl std::fmt::Debug for Upstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Upstream")
            .field("base_url", &self.base_url)
            .field("key", &"<hidden>")
            .finish()
    }
}

/// What the proxy forwards where.
#[derive(Debug, Clone, Default)]
pub struct ProxyConfig {
    pub anthropic: Option<Upstream>,
    pub openai: Option<Upstream>,
    pub max_cost: Option<f64>,
    pub limits: Limits,
}

impl ProxySpec {
    /// The config with `keys`. A configured upstream without a key gets an empty one,
    /// so its calls fail upstream with an auth error, which stops the matrix.
    pub fn with_keys(&self, keys: &Keys) -> ProxyConfig {
        let upstream = |spec: &Option<UpstreamSpec>, key: &Option<String>| {
            spec.as_ref().map(|u| Upstream {
                base_url: u.base_url.clone(),
                key: key.clone().unwrap_or_default(),
            })
        };
        ProxyConfig {
            anthropic: upstream(&self.anthropic, &keys.anthropic),
            openai: upstream(&self.openai, &keys.openai),
            max_cost: self.max_cost,
            limits: Limits::default(),
        }
    }
}

struct Shared {
    state: StateDir,
    config: ProxyConfig,
    http: reqwest::Client,
    /// The ledger's total, kept up to date as calls finish.
    spent: Mutex<f64>,
    /// Request bodies held in memory.
    buffers: Arc<Semaphore>,
    /// Requests going, per token.
    per_token: Mutex<HashMap<String, Arc<Semaphore>>>,
    /// Open connections, per source address.
    sources: Mutex<HashMap<IpAddr, usize>>,
    connections: AtomicUsize,
}

/// One accepted connection, counted until it closes.
struct Connection {
    shared: Arc<Shared>,
    source: IpAddr,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.shared.connections.fetch_sub(1, Ordering::SeqCst);
        let mut sources = self
            .shared
            .sources
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(n) = sources.get_mut(&self.source) {
            *n -= 1;
            if *n == 0 {
                sources.remove(&self.source);
            }
        }
    }
}

impl Shared {
    fn upstream(&self, provider: Provider) -> Option<&Upstream> {
        match provider {
            Provider::Anthropic => self.config.anthropic.as_ref(),
            Provider::OpenAi => self.config.openai.as_ref(),
        }
    }

    fn spent(&self) -> f64 {
        *self.spent.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn add_spent(&self, cost: f64) {
        let mut spent = self.spent.lock().unwrap_or_else(|e| e.into_inner());
        *spent = (*spent + cost).min(f64::MAX);
    }

    /// Counts a new connection from `source`, unless that source or everyone together
    /// has too many already.
    fn admit(self: &Arc<Self>, source: IpAddr) -> Option<Connection> {
        let mut sources = self.sources.lock().unwrap_or_else(|e| e.into_inner());
        let from_source = sources.entry(source).or_default();
        if *from_source >= self.config.limits.per_source
            || self.connections.load(Ordering::SeqCst) >= MAX_CONNECTIONS
        {
            if *from_source == 0 {
                sources.remove(&source);
            }
            return None;
        }
        *from_source += 1;
        self.connections.fetch_add(1, Ordering::SeqCst);
        Some(Connection {
            shared: self.clone(),
            source,
        })
    }

    fn record(&self, run: &str, call: &Call) {
        if let Err(e) = ledger::append(&self.state, run, call) {
            eprintln!("ch-eval proxy: {e:#}");
        }
    }

    /// A slot for one more request of `token`, if it has one free.
    fn slot(&self, token: &str) -> Option<OwnedSemaphorePermit> {
        let sem = self
            .per_token
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(token.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(MAX_PER_TOKEN)))
            .clone();
        sem.try_acquire_owned().ok()
    }
}

type Body = BoxBody<Bytes, std::io::Error>;

/// Serves on `listener` until it fails. The ledger's total so far counts toward
/// `--max-cost`.
pub async fn serve(
    listener: TcpListener,
    state: StateDir,
    config: ProxyConfig,
) -> anyhow::Result<()> {
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let spent = ledger::total_cost(&state);
    let shared = Arc::new(Shared {
        state,
        config,
        http,
        spent: Mutex::new(spent),
        buffers: Arc::new(Semaphore::new(MAX_BUFFERED)),
        per_token: Mutex::new(HashMap::new()),
        sources: Mutex::new(HashMap::new()),
        connections: AtomicUsize::new(0),
    });
    loop {
        let (stream, peer) = listener.accept().await.context("accepting a connection")?;
        // Over a limit, the connection is closed at once: accepting goes on, so one
        // run's connections can't hold up another's.
        let Some(connection) = shared.admit(peer.ip()) else {
            drop(stream);
            continue;
        };
        let shared = shared.clone();
        tokio::spawn(async move {
            let _connection = connection;
            let header_timeout = shared.config.limits.header_timeout;
            let service = service_fn(move |req| {
                let shared = shared.clone();
                async move { Ok::<_, Infallible>(handle(shared, req).await) }
            });
            // One request per connection: an idle connection lasts no longer than
            // the header timeout.
            let _ = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(header_timeout)
                .keep_alive(false)
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

/// `ch-eval proxy`: reads `proxy.json` from `state` and the keys from stdin, listens on
/// `listen`, writes `ready`, and serves until the runner's heartbeat goes stale.
pub async fn main(state: &Path, listen: &str) -> anyhow::Result<i32> {
    let state = StateDir(state.to_path_buf());
    let text = std::fs::read_to_string(state.config())
        .with_context(|| format!("reading {}", state.config().display()))?;
    let spec: ProxySpec = serde_json::from_str(&text).context("parsing proxy.json")?;
    let mut input = String::new();
    std::io::stdin()
        .take(1024 * 1024)
        .read_to_string(&mut input)
        .context("reading the keys from stdin")?;
    let keys: Keys = if input.trim().is_empty() {
        Keys::default()
    } else {
        serde_json::from_str(&input).context("parsing the keys from stdin")?
    };
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("listening on {listen}"))?;
    std::fs::write(state.ready(), listener.local_addr()?.to_string())
        .context("writing the ready file")?;
    if let Some(secs) = spec.heartbeat_secs {
        tokio::spawn(watchdog(state.heartbeat(), Duration::from_secs(secs)));
    }
    eprintln!("ch-eval proxy: listening on {listen}");
    serve(listener, state, spec.with_keys(&keys)).await?;
    Ok(0)
}

/// Exits when `path` hasn't been touched for `max_age`: the runner is gone, and the
/// proxy shouldn't keep the key available.
async fn watchdog(path: PathBuf, max_age: Duration) {
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let age = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok());
        if age.is_none_or(|a| a > max_age) {
            eprintln!("ch-eval proxy: the runner's heartbeat stopped; exiting");
            std::process::exit(0);
        }
    }
}

async fn handle(shared: Arc<Shared>, req: Request<Incoming>) -> Response<Body> {
    let path = req.uri().path().to_string();
    let route = [Provider::Anthropic, Provider::OpenAi]
        .into_iter()
        .find_map(|p| path.strip_prefix(&format!("/{}", p.name())).map(|r| (p, r)));
    let Some((provider, rest)) = route.filter(|(p, r)| p.allows(r)) else {
        return error(StatusCode::NOT_FOUND, "not_found_error", "not forwarded");
    };
    if req.method() != Method::POST {
        return error(StatusCode::NOT_FOUND, "not_found_error", "not forwarded");
    }
    let Some(upstream) = shared.upstream(provider).cloned() else {
        return error(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "the eval's config doesn't use this provider",
        );
    };
    let token = client_token(req.headers()).unwrap_or_default().to_string();
    let Some(info) = ledger::lookup_token(&shared.state, &token) else {
        return error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "unknown or expired run token",
        );
    };
    let Some(slot) = shared.slot(&token) else {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            "too many requests at once for this run",
        );
    };
    let refuse = |reason: &str, model: &str| Call {
        provider: provider.name().into(),
        model: model.into(),
        refused: Some(reason.into()),
        ..Default::default()
    };
    if shared
        .config
        .max_cost
        .is_some_and(|max| shared.spent() >= max)
    {
        shared.record(&info.run, &refuse("max_cost", ""));
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "--max-cost was reached, so the request was not sent",
        );
    }

    let headers = forwarded_headers(provider, req.headers());
    let buffer = match shared.buffers.clone().acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "shutting down",
            );
        }
    };
    let collected = tokio::time::timeout(
        shared.config.limits.body_timeout,
        Limited::new(req.into_body(), MAX_REQUEST_BYTES).collect(),
    )
    .await;
    let body = match collected {
        Ok(Ok(b)) => b.to_bytes(),
        Ok(Err(_)) => {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "the request body is too large or broken",
            );
        }
        Err(_) => {
            return error(
                StatusCode::REQUEST_TIMEOUT,
                "invalid_request_error",
                "the request body took too long",
            );
        }
    };
    // What is forwarded is the parsed body, so the upstream reads what was checked.
    let Ok(parsed) = serde_json::from_slice::<Value>(&body) else {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "the request body isn't JSON",
        );
    };
    let body = match serde_json::to_vec(&parsed) {
        Ok(b) => Bytes::from(b),
        Err(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "the request body isn't JSON",
            );
        }
    };
    let model = parsed
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if !info.models.contains_key(&model) {
        shared.record(&info.run, &refuse("model", &model));
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "this run may only call the models its cell configures",
        );
    }

    let url = format!("{}{rest}", upstream.base_url.trim_end_matches('/'));
    let mut request = shared.http.post(url).headers(headers).body(body);
    request = match provider {
        Provider::Anthropic => request.header("x-api-key", &upstream.key),
        Provider::OpenAi => request.bearer_auth(&upstream.key),
    };
    let sent = request.send().await;
    drop(buffer);
    let response = match sent {
        Ok(r) => r,
        Err(e) => {
            let call = Call {
                provider: provider.name().into(),
                model,
                note: Some(format!("upstream unreachable: {}", without_url(&e))),
                ..Default::default()
            };
            shared.record(&info.run, &call);
            return error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "couldn't reach the model API",
            );
        }
    };

    let status = response.status();
    let mut out = Response::new(Full::new(Bytes::new()).map_err(|n| match n {}).boxed());
    *out.status_mut() = status;
    for (name, value) in response.headers() {
        if !HOP_BY_HOP.contains(&name.as_str()) {
            out.headers_mut().append(name.clone(), value.clone());
        }
    }
    let meter = Meter::new(shared, info, provider, model, status.as_u16());
    let (tx, rx) = mpsc::channel::<Result<Frame<Bytes>, std::io::Error>>(16);
    tokio::spawn(pump(response, meter, tx, slot));
    let frames = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    *out.body_mut() = BodyExt::boxed(StreamBody::new(frames));
    out
}

/// Reads the upstream's answer to its end, metering it and passing it to the client
/// while the client is there. The run's slot is held until the answer ends.
async fn pump(
    response: reqwest::Response,
    mut meter: Meter,
    tx: mpsc::Sender<Result<Frame<Bytes>, std::io::Error>>,
    _slot: OwnedSemaphorePermit,
) {
    let mut client = Some(tx);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                meter.feed(&bytes);
                if let Some(tx) = &client
                    && tx.send(Ok(Frame::data(bytes))).await.is_err()
                {
                    // The client went away; the upstream still charges for the rest.
                    client = None;
                }
            }
            Err(e) => {
                meter.broken(&without_url(&e));
                if let Some(tx) = client.take() {
                    let _ = tx.send(Err(std::io::Error::other(without_url(&e)))).await;
                }
                break;
            }
        }
    }
    meter.finish();
}

/// A reqwest error without the URL, which may carry a query string.
fn without_url(e: &reqwest::Error) -> String {
    let mut e = e.to_string();
    if let Some(i) = e.find(" for url") {
        e.truncate(i);
    }
    e
}

/// The token the client sent as its key: `x-api-key`, or `Authorization: Bearer`.
fn client_token(headers: &HeaderMap) -> Option<&str> {
    if let Some(key) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        return Some(key.trim());
    }
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
}

fn forwarded_headers(provider: Provider, headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for name in FORWARDED {
        let name = HeaderName::from_static(name);
        for value in headers.get_all(&name) {
            out.append(name.clone(), value.clone());
        }
    }
    if provider == Provider::Anthropic {
        let betas: Vec<&str> = headers
            .get_all("anthropic-beta")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|b| BETAS.contains(b))
            .collect();
        if let Ok(value) = HeaderValue::from_str(&betas.join(","))
            && !betas.is_empty()
        {
            out.insert(HeaderName::from_static("anthropic-beta"), value);
        }
    }
    out
}

/// An error in the shape both providers parse: `{"type": "error", "error": {...}}`.
fn error(status: StatusCode, kind: &str, message: &str) -> Response<Body> {
    let body = json!({
        "type": "error",
        "error": {"type": kind, "message": format!("ch-eval proxy: {message}")},
    });
    let mut resp = Response::new(
        Full::new(Bytes::from(body.to_string()))
            .map_err(|n| match n {})
            .boxed(),
    );
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

/// Reads the usage, the model and any error event out of an answer, and records the
/// call when it ends. An answer is read as server-sent events line by line; one with
/// no `data:` line is read as a JSON body (a non-streamed reply or an error).
struct Meter {
    shared: Arc<Shared>,
    run: String,
    models: BTreeMap<String, Option<Pricing>>,
    provider: Provider,
    model: String,
    status: u16,
    /// The unfinished line.
    line: Vec<u8>,
    /// The body so far, up to [`MAX_JSON_BYTES`], in case it isn't a stream.
    body: Vec<u8>,
    saw_events: bool,
    saw_usage: bool,
    usage: Usage,
    response_model: String,
    stream_error: Option<String>,
    error_message: Option<String>,
    note: Option<String>,
    recorded: bool,
}

impl Meter {
    fn new(
        shared: Arc<Shared>,
        info: TokenInfo,
        provider: Provider,
        model: String,
        status: u16,
    ) -> Self {
        Self {
            shared,
            run: info.run,
            models: info.models,
            provider,
            model,
            status,
            line: Vec::new(),
            body: Vec::new(),
            saw_events: false,
            saw_usage: false,
            usage: Usage::default(),
            response_model: String::new(),
            stream_error: None,
            error_message: None,
            note: None,
            recorded: false,
        }
    }

    fn feed(&mut self, chunk: &[u8]) {
        if self.body.len() + chunk.len() <= MAX_JSON_BYTES {
            self.body.extend_from_slice(chunk);
        } else {
            self.body.clear();
        }
        self.line.extend_from_slice(chunk);
        while let Some(end) = self.line.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.line.drain(..=end).collect();
            self.read_line(&line);
        }
        if self.line.len() > MAX_JSON_BYTES {
            self.line.clear();
        }
    }

    /// The upstream's stream broke off.
    fn broken(&mut self, why: &str) {
        self.stream_error
            .get_or_insert_with(|| "connection_error".into());
        self.note = Some(format!("the answer broke off: {why}"));
    }

    fn read_line(&mut self, line: &[u8]) {
        let line = String::from_utf8_lossy(line);
        let Some(data) = line.trim_end().strip_prefix("data:") else {
            return;
        };
        self.saw_events = true;
        if let Ok(event) = serde_json::from_str::<Value>(data.trim()) {
            self.apply(&event);
        }
    }

    fn apply(&mut self, event: &Value) {
        let error_type = |e: &Value| {
            e.get("error")
                .and_then(|e| e.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("api_error")
                .to_string()
        };
        if let Some(message) = event
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
        {
            self.error_message = Some(message.chars().take(300).collect());
        }
        let mut usage = None;
        match self.provider {
            Provider::Anthropic => match event.get("type").and_then(Value::as_str) {
                Some("message_start") => {
                    self.set_model(&event["message"]["model"]);
                    usage = Some(&event["message"]["usage"]);
                }
                Some("message_delta") => usage = Some(&event["usage"]),
                Some("message") => {
                    self.set_model(&event["model"]);
                    usage = Some(&event["usage"]);
                }
                Some("error") if self.status == 200 => {
                    self.stream_error = Some(error_type(event));
                }
                _ => {}
            },
            Provider::OpenAi => {
                self.set_model(&event["model"]);
                if event.get("error").is_some() && self.status == 200 {
                    self.stream_error = Some(error_type(event));
                }
                usage = event.get("usage");
            }
        }
        if let Some(u) = usage.filter(|u| u.is_object()) {
            self.saw_usage = true;
            match self.provider {
                Provider::Anthropic => merge_anthropic(&mut self.usage, u),
                Provider::OpenAi => merge_openai(&mut self.usage, u),
            }
        }
    }

    fn set_model(&mut self, model: &Value) {
        if let Some(m) = model.as_str().filter(|m| !m.is_empty()) {
            self.response_model = m.chars().take(200).collect();
        }
    }

    fn finish(&mut self) {
        if self.recorded {
            return;
        }
        self.recorded = true;
        let rest = std::mem::take(&mut self.line);
        self.read_line(&rest);
        if !self.saw_events
            && let Ok(body) = serde_json::from_slice::<Value>(&self.body)
        {
            self.apply(&body);
        }
        // A fallback model is priced as itself when its pricing is known.
        let pricing = self
            .models
            .get(&self.response_model)
            .copied()
            .flatten()
            .or_else(|| self.models.get(&self.model).copied().flatten());
        let cost = pricing.map_or(0.0, |p| clean_cost(p.cost(&self.usage.to_kernel())));
        self.shared.add_spent(cost);
        let unlisted =
            !self.response_model.is_empty() && !self.models.contains_key(&self.response_model);
        let call = Call {
            provider: self.provider.name().into(),
            unlisted_model: unlisted,
            model: std::mem::take(&mut self.model),
            response_model: std::mem::take(&mut self.response_model),
            status: self.status,
            stream_error: self.stream_error.take(),
            usage_missing: self.status == 200 && !self.saw_usage,
            usage: self.usage,
            cost,
            refused: None,
            note: self.note.take(),
            error_message: self.error_message.take(),
        };
        self.shared.record(&self.run, &call);
    }
}

impl Drop for Meter {
    fn drop(&mut self) {
        // The pump finishes the meter; this covers a runtime shutting down mid-answer.
        self.finish();
    }
}

/// As the Anthropic provider reads it: each field present replaces the one before
/// (`message_delta` carries cumulative counts).
fn merge_anthropic(usage: &mut Usage, from: &Value) {
    let get = |k: &str| from.get(k).and_then(Value::as_u64);
    if let Some(n) = get("input_tokens") {
        usage.input_tokens = n;
    }
    if let Some(n) = get("output_tokens") {
        usage.output_tokens = n;
    }
    if let Some(n) = get("cache_read_input_tokens") {
        usage.cache_read_tokens = n;
    }
    if let Some(n) = get("cache_creation_input_tokens") {
        usage.cache_write_tokens = n;
    }
}

/// As the OpenAI provider reads it: cached prompt tokens are cache reads, the rest
/// input.
fn merge_openai(usage: &mut Usage, from: &Value) {
    let prompt = from["prompt_tokens"].as_u64().unwrap_or(0);
    let cached = from["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0);
    *usage = Usage {
        input_tokens: prompt.saturating_sub(cached),
        output_tokens: from["completion_tokens"].as_u64().unwrap_or(0),
        cache_read_tokens: cached,
        cache_write_tokens: 0,
    };
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use wiremock::matchers::{header, method, path};
    use wiremock::{Match, Mock, MockServer, Request as MockRequest, ResponseTemplate};

    use super::*;

    const REAL_KEY: &str = "sk-real-upstream-key";
    const MODEL: &str = "claude-opus-5-5";
    const PRICING: Pricing = Pricing {
        input: 3.0,
        output: 15.0,
        cache_read: 0.3,
        cache_write: 3.75,
    };

    struct Running {
        _dir: tempfile::TempDir,
        state: StateDir,
        addr: SocketAddr,
        upstream: MockServer,
    }

    impl Running {
        fn url(&self, path: &str) -> String {
            format!("http://{}{path}", self.addr)
        }

        fn token(&self, run: &str) -> String {
            let models = BTreeMap::from([
                (MODEL.to_string(), Some(PRICING)),
                ("gpt-x".to_string(), Some(PRICING)),
                (
                    "claude-fallback".to_string(),
                    Some(Pricing {
                        output: 100.0,
                        ..PRICING
                    }),
                ),
            ]);
            ledger::issue_token(&self.state, run, models).unwrap()
        }
    }

    async fn start(max_cost: Option<f64>, earlier_cost: f64) -> Running {
        start_with(max_cost, earlier_cost, true).await
    }

    async fn start_with(max_cost: Option<f64>, earlier_cost: f64, openai: bool) -> Running {
        start_limited(max_cost, earlier_cost, openai, Limits::default()).await
    }

    async fn start_limited(
        max_cost: Option<f64>,
        earlier_cost: f64,
        openai: bool,
        limits: Limits,
    ) -> Running {
        let upstream = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir(dir.path().join("state"));
        state.prepare().unwrap();
        if earlier_cost > 0.0 {
            let call = Call {
                cost: earlier_cost,
                ..Default::default()
            };
            ledger::append(&state, "earlier-run-1", &call).unwrap();
        }
        let config = ProxyConfig {
            anthropic: Some(Upstream {
                base_url: upstream.uri(),
                key: REAL_KEY.into(),
            }),
            openai: openai.then(|| Upstream {
                base_url: format!("{}/v1", upstream.uri()),
                key: "sk-openai".into(),
            }),
            max_cost,
            limits,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, state.clone(), config));
        Running {
            _dir: dir,
            state,
            addr,
            upstream,
        }
    }

    struct NoHeader(&'static str);

    impl Match for NoHeader {
        fn matches(&self, request: &MockRequest) -> bool {
            !request.headers.contains_key(self.0)
        }
    }

    fn sse(events: &[Value]) -> String {
        events
            .iter()
            .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
            .collect()
    }

    fn anthropic_stream(model: &str, output: u64) -> String {
        sse(&[
            json!({"type":"message_start","message":{"id":"m","type":"message","role":"assistant","model":model,"content":[],"usage":{"input_tokens":120,"output_tokens":1,"cache_creation_input_tokens":50,"cache_read_input_tokens":2000}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":output}}),
            json!({"type":"message_stop"}),
        ])
    }

    fn ok_stream(body: String) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .insert_header("request-id", "req_1")
            .set_body_string(body)
    }

    fn post(url: String, token: &str) -> reqwest::RequestBuilder {
        post_model(url, token, MODEL)
    }

    fn post_model(url: String, token: &str, model: &str) -> reqwest::RequestBuilder {
        reqwest::Client::new()
            .post(url)
            .header("x-api-key", token)
            .header("authorization", "Bearer client-junk")
            .header("cookie", "session=1")
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .body(json!({"model": model, "stream": true}).to_string())
    }

    async fn calls(p: &Running, run: &str) -> Vec<Call> {
        // The pump records once the upstream's answer has ended.
        for _ in 0..50 {
            let calls = ledger::read_run(&p.state, run);
            if !calls.is_empty() {
                return calls;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Vec::new()
    }

    #[tokio::test]
    async fn forwards_with_the_real_key_and_meters_the_stream() {
        let p = start(None, 0.0).await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", REAL_KEY))
            .and(header("anthropic-version", "2023-06-01"))
            .and(header(
                "anthropic-beta",
                ch_provider_anthropic::FALLBACK_BETA,
            ))
            .and(NoHeader("authorization"))
            .and(NoHeader("cookie"))
            .respond_with(ok_stream(anthropic_stream(MODEL, 12)))
            .expect(1)
            .mount(&p.upstream)
            .await;
        let token = p.token("run-00000001");
        let betas = format!("{},costly-beta-2027", ch_provider_anthropic::FALLBACK_BETA);
        let resp = post(p.url("/anthropic/v1/messages"), &token)
            .header("anthropic-beta", betas)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["request-id"], "req_1");
        assert_eq!(resp.text().await.unwrap(), anthropic_stream(MODEL, 12));
        for request in p.upstream.received_requests().await.unwrap() {
            for (_, value) in request.headers.iter() {
                assert!(
                    !value.to_str().unwrap().contains(&token),
                    "the token went upstream"
                );
            }
        }

        let calls = calls(&p, "run-00000001").await;
        assert_eq!(calls.len(), 1, "{calls:?}");
        let call = &calls[0];
        assert_eq!(
            (
                call.provider.as_str(),
                call.model.as_str(),
                call.response_model.as_str()
            ),
            ("anthropic", MODEL, MODEL)
        );
        assert_eq!(call.status, 200);
        assert!(!call.usage_missing);
        assert_eq!(
            call.usage,
            Usage {
                input_tokens: 120,
                output_tokens: 12,
                cache_read_tokens: 2000,
                cache_write_tokens: 50
            }
        );
        let expected = (120.0 * 3.0 + 12.0 * 15.0 + 2000.0 * 0.3 + 50.0 * 3.75) / 1e6;
        assert!((call.cost - expected).abs() < 1e-12, "{}", call.cost);
    }

    #[tokio::test]
    async fn keeps_metering_after_the_client_goes_away() {
        let p = start(None, 0.0).await;
        Mock::given(method("POST"))
            .respond_with(ok_stream(anthropic_stream(MODEL, 1_000_000)))
            .mount(&p.upstream)
            .await;
        let token = p.token("run-0000000a");
        let resp = post(p.url("/anthropic/v1/messages"), &token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        drop(resp);
        let calls = calls(&p, "run-0000000a").await;
        assert_eq!(calls[0].usage.output_tokens, 1_000_000, "{calls:?}");
    }

    #[tokio::test]
    async fn prices_the_model_that_answered_and_refuses_others() {
        let p = start(None, 0.0).await;
        Mock::given(method("POST"))
            .respond_with(ok_stream(anthropic_stream("claude-fallback", 10)))
            .mount(&p.upstream)
            .await;
        let token = p.token("run-0000000b");
        post(p.url("/anthropic/v1/messages"), &token)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let call = &calls(&p, "run-0000000b").await[0];
        assert_eq!(call.response_model, "claude-fallback");
        let expected = (120.0 * 3.0 + 10.0 * 100.0 + 2000.0 * 0.3 + 50.0 * 3.75) / 1e6;
        assert!((call.cost - expected).abs() < 1e-12, "{}", call.cost);

        let resp = post_model(p.url("/anthropic/v1/messages"), &token, "claude-pricier")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let all = ledger::read_run(&p.state, "run-0000000b");
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].refused.as_deref(), Some("model"));
        assert_eq!(all[1].model, "claude-pricier");
        assert_eq!(p.upstream.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn meters_openai_streams_json_bodies_and_missing_usage() {
        let p = start(None, 0.0).await;
        let chunks = [
            json!({"model":"gpt-x","choices":[{"delta":{"content":"ok"}}]}),
            json!({"model":"gpt-x","choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":64}}}),
        ];
        let body: String = chunks
            .iter()
            .map(|c| format!("data: {c}\n\n"))
            .collect::<String>()
            + "data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer sk-openai"))
            .respond_with(ok_stream(body))
            .up_to_n_times(1)
            .mount(&p.upstream)
            .await;
        let openai = |token: &str| {
            reqwest::Client::new()
                .post(p.url("/openai/chat/completions"))
                .bearer_auth(token)
                .body(r#"{"model":"gpt-x"}"#)
        };
        let token = p.token("run-00000002");
        let resp = openai(&token).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        resp.text().await.unwrap();
        // A stream without usage: its cost is unknown, and that is recorded.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ok_stream(
                "data: {\"choices\":[]}\n\ndata: [DONE]\n\n".into(),
            ))
            .mount(&p.upstream)
            .await;
        openai(&token).send().await.unwrap().text().await.unwrap();
        // A non-streamed Anthropic message carries its usage in the body.
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "type": "message", "model": MODEL, "content": [],
                "usage": {"input_tokens": 9, "output_tokens": 4}
            })))
            .mount(&p.upstream)
            .await;
        post(p.url("/anthropic/v1/messages"), &token)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;
        let calls = ledger::read_run(&p.state, "run-00000002");
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert_eq!(
            calls[0].usage,
            Usage {
                input_tokens: 36,
                output_tokens: 20,
                cache_read_tokens: 64,
                cache_write_tokens: 0
            }
        );
        assert_eq!(calls[0].response_model, "gpt-x");
        assert!(calls[1].usage_missing);
        assert_eq!(
            (calls[2].usage.input_tokens, calls[2].usage.output_tokens),
            (9, 4)
        );
        assert!(!calls[2].usage_missing);
    }

    #[tokio::test]
    async fn records_stream_errors_and_unreachable_upstreams() {
        let p = start(None, 0.0).await;
        let body = sse(&[
            json!({"type":"message_start","message":{"id":"m","type":"message","role":"assistant","model":MODEL,"content":[],"usage":{"input_tokens":5,"output_tokens":1}}}),
            json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}),
        ]);
        Mock::given(method("POST"))
            .respond_with(ok_stream(body))
            .mount(&p.upstream)
            .await;
        let token = p.token("run-0000000c");
        post(p.url("/anthropic/v1/messages"), &token)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let call = &calls(&p, "run-0000000c").await[0];
        assert_eq!(call.stream_error.as_deref(), Some("overloaded_error"));
        assert!(call.is_transient());

        // An upstream nobody listens on.
        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir(dir.path().join("s"));
        state.prepare().unwrap();
        let config = ProxyConfig {
            anthropic: Some(Upstream {
                base_url: dead,
                key: REAL_KEY.into(),
            }),
            ..Default::default()
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, state.clone(), config));
        let models = BTreeMap::from([(MODEL.to_string(), None)]);
        let token = ledger::issue_token(&state, "run-0000000d", models).unwrap();
        let resp = post(format!("http://{addr}/anthropic/v1/messages"), &token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 502);
        let call = &ledger::read_run(&state, "run-0000000d")[0];
        assert_eq!(call.status, 0);
        assert!(call.is_transient());
        assert!(call.note.as_deref().unwrap().contains("unreachable"));
    }

    #[tokio::test]
    async fn only_the_configured_providers_endpoints_are_forwarded() {
        let p = start_with(None, 0.0, false).await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&p.upstream)
            .await;
        let token = p.token("run-00000003");
        let client = reqwest::Client::new();
        let get = client
            .get(p.url("/anthropic/v1/messages"))
            .header("x-api-key", &token)
            .send()
            .await
            .unwrap();
        assert_eq!(get.status(), 404);
        for path in [
            "/anthropic/v1/models",
            "/anthropic/v1/messages/count_tokens",
            "/anthropic/v1/messages/../files",
            "/openai/chat/completions",
            "/openai/v1/messages",
            "/v1/messages",
            "/",
        ] {
            let resp = post(p.url(path), &token).send().await.unwrap();
            assert_eq!(resp.status(), 404, "{path}");
        }
    }

    #[tokio::test]
    async fn unknown_and_revoked_tokens_are_refused() {
        let p = start(None, 0.0).await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&p.upstream)
            .await;
        let token = p.token("run-00000004");
        ledger::revoke_token(&p.state, &token);
        for key in [token.as_str(), "cheval-0123456789abcdef", REAL_KEY, ""] {
            let resp = post(p.url("/anthropic/v1/messages"), key)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 401, "{key}");
            let body: Value = resp.json().await.unwrap();
            assert_eq!(body["error"]["type"], "authentication_error");
        }
        assert!(ledger::read_run(&p.state, "run-00000004").is_empty());
    }

    #[tokio::test]
    async fn bodies_and_concurrency_are_capped() {
        let p = start(None, 0.0).await;
        Mock::given(method("POST"))
            .respond_with(
                ok_stream(anthropic_stream(MODEL, 1)).set_delay(Duration::from_millis(700)),
            )
            .mount(&p.upstream)
            .await;
        let token = p.token("run-0000000e");
        let big = vec![b'x'; MAX_REQUEST_BYTES + 1];
        let resp = reqwest::Client::new()
            .post(p.url("/anthropic/v1/messages"))
            .header("x-api-key", &token)
            .body(big)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 413);
        // Two requests of one run may go at once; a third waits its turn elsewhere.
        let one = post(p.url("/anthropic/v1/messages"), &token).send();
        let two = post(p.url("/anthropic/v1/messages"), &token).send();
        let three = async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            post(p.url("/anthropic/v1/messages"), &token).send().await
        };
        let (one, two, three) = tokio::join!(one, two, three);
        assert_eq!(one.unwrap().status(), 200);
        assert_eq!(two.unwrap().status(), 200);
        assert_eq!(three.unwrap().status(), 429);
    }

    #[tokio::test]
    async fn max_cost_stops_new_requests() {
        // The first call costs more than the cap; the second is refused.
        let p = start(Some(0.0001), 0.0).await;
        Mock::given(method("POST"))
            .respond_with(ok_stream(anthropic_stream(MODEL, 12)))
            .expect(1)
            .mount(&p.upstream)
            .await;
        let token = p.token("run-00000005");
        let first = post(p.url("/anthropic/v1/messages"), &token)
            .send()
            .await
            .unwrap();
        assert_eq!(first.status(), 200);
        first.text().await.unwrap();
        calls(&p, "run-00000005").await;
        let second = post(p.url("/anthropic/v1/messages"), &token)
            .send()
            .await
            .unwrap();
        assert_eq!(second.status(), 400);
        let body: Value = second.json().await.unwrap();
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("--max-cost")
        );
        let calls = ledger::summarize(&ledger::read_run(&p.state, "run-00000005"));
        assert!(calls.cut_by_max_cost);
        assert_eq!(calls.forwarded, 1);

        // A ledger that already holds exactly the cap refuses at once.
        let p = start(Some(1.5), 1.5).await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&p.upstream)
            .await;
        let token = p.token("run-00000006");
        let resp = post(p.url("/anthropic/v1/messages"), &token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn upstream_auth_failures_are_passed_on_and_recorded() {
        let p = start(None, 0.0).await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "type": "error", "error": {"type": "authentication_error", "message": "bad key"}
            })))
            .mount(&p.upstream)
            .await;
        let token = p.token("run-00000007");
        let resp = post(p.url("/anthropic/v1/messages"), &token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        resp.text().await.unwrap();
        let calls = ledger::summarize(&calls(&p, "run-00000007").await);
        assert!(calls.auth_failed);
        assert!(!calls.last_transient);
    }

    #[test]
    fn keys_are_hidden_in_debug_output() {
        let spec = ProxySpec {
            anthropic: Some(UpstreamSpec {
                base_url: "https://api.anthropic.com".into(),
            }),
            ..Default::default()
        };
        let keys = Keys {
            anthropic: Some("sk-secret-value".into()),
            openai: Some("sk-other".into()),
        };
        let config = spec.with_keys(&keys);
        assert_eq!(config.anthropic.as_ref().unwrap().key, "sk-secret-value");
        assert!(config.openai.is_none(), "only configured upstreams");
        assert!(!format!("{config:?}{keys:?}").contains("sk-"));
    }

    #[tokio::test]
    async fn one_source_cant_hold_the_proxy() {
        let limits = Limits {
            per_source: 4,
            header_timeout: Duration::from_millis(500),
            body_timeout: Duration::from_secs(5),
        };
        let p = start_limited(None, 0.0, false, limits).await;
        Mock::given(method("POST"))
            .respond_with(ok_stream(anthropic_stream(MODEL, 1)))
            .mount(&p.upstream)
            .await;
        // Idle connections past the source's limit are closed at once, not queued.
        let mut idle = Vec::new();
        for _ in 0..40 {
            idle.push(tokio::net::TcpStream::connect(p.addr).await.unwrap());
        }
        let mut closed = 0;
        for conn in &mut idle {
            use tokio::io::AsyncReadExt as _;
            let mut byte = [0u8; 1];
            if let Ok(Ok(0)) =
                tokio::time::timeout(Duration::from_millis(200), conn.read(&mut byte)).await
            {
                closed += 1;
            }
        }
        assert!(closed >= 36, "{closed} of 40 closed");
        // The ones it kept go at the header timeout, and requests are served again.
        tokio::time::sleep(Duration::from_millis(700)).await;
        let token = p.token("run-0000000f");
        let resp = post(p.url("/anthropic/v1/messages"), &token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        drop(idle);
    }

    #[tokio::test]
    async fn what_is_forwarded_is_what_was_checked() {
        let p = start(None, 0.0).await;
        struct OneModel;
        impl Match for OneModel {
            fn matches(&self, request: &MockRequest) -> bool {
                let body = String::from_utf8_lossy(&request.body);
                body.matches("\"model\"").count() == 1 && !body.contains("claude-pricier")
            }
        }
        Mock::given(method("POST"))
            .and(OneModel)
            .respond_with(ok_stream(anthropic_stream(MODEL, 1)))
            .up_to_n_times(1)
            .expect(1)
            .mount(&p.upstream)
            .await;
        let token = p.token("run-00000010");
        // serde_json keeps the last key; an upstream reading the first would serve the
        // pricier model if the raw bytes went through.
        let body = format!(r#"{{"model":"claude-pricier","stream":true,"model":"{MODEL}"}}"#);
        let resp = reqwest::Client::new()
            .post(p.url("/anthropic/v1/messages"))
            .header("x-api-key", &token)
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        resp.text().await.unwrap();
        // An answer from a model the run may not call is flagged.
        Mock::given(method("POST"))
            .respond_with(ok_stream(anthropic_stream("claude-elsewhere", 1)))
            .mount(&p.upstream)
            .await;
        let resp = reqwest::Client::new()
            .post(p.url("/anthropic/v1/messages"))
            .header("x-api-key", &token)
            .body(format!(r#"{{"model":"{MODEL}"}}"#))
            .send()
            .await
            .unwrap();
        resp.text().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let sum = ledger::summarize(&ledger::read_run(&p.state, "run-00000010"));
        assert_eq!(sum.unlisted_model.as_deref(), Some("claude-elsewhere"));
    }
}
