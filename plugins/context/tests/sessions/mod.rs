//! What the session tests share: a tool with big results, a session over a provider,
//! and `Signed`, a provider that signs the model's reasoning and checks it the way an
//! API with signed thinking does.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ch_kernel::reexports::futures::{StreamExt, stream};
use ch_kernel::reexports::{CancellationToken, async_trait};
use ch_kernel::{
    AgentEvent, Config, ContentBlock, Cx, Message, ModelError, ModelEvent, ModelProvider,
    ModelRequest, ModelResponse, ModelStream, Plugin, PluginManifest, RegisterCx, RegistryBuilder,
    Role, Session, SessionStore, StopReason, Tool, ToolOutput, ToolSpec, Usage,
};
use ch_provider_scripted::{ScriptedProvider, estimate_request_tokens};
use serde_json::{Value, json};

/// `big`: a log of `lines` lines (100 by default), different for each `n`.
pub struct Big;

#[async_trait]
impl Tool for Big {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "big".into(),
            description: "When: tests. Protocol: -. Error modes: -. Side effects: none.".into(),
            input_schema: json!({"type": "object", "properties": {"n": {"type": "integer"}}}),
            read_only: true,
        }
    }

    async fn call(&self, input: Value, _cx: &Cx) -> ToolOutput {
        let n = input["n"].as_u64().unwrap_or(0);
        let lines = input["lines"].as_u64().unwrap_or(100);
        ToolOutput::ok(
            (1..=lines)
                .map(|i| format!("big {n:02} line {i:03} of the log\n"))
                .collect::<String>(),
        )
    }
}

pub struct BigPlugin;

impl Plugin for BigPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new("big", "test tool")
    }
    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        cx.add_tool(Big);
        Ok(())
    }
}

/// A session with `provider`'s plugin and `toml`, in `ws`, saved under `sessions` as
/// `id`, like one `ch` process.
pub fn build(
    id: &str,
    provider: Arc<dyn Plugin>,
    ws: &Path,
    sessions: &Path,
    toml: &str,
) -> Session {
    let plugins: Vec<Arc<dyn Plugin>> = vec![
        provider,
        Arc::new(BigPlugin),
        ch_safety::plugin(),
        ch_tools_fs::plugin(),
        ch_planning::plugin(),
        ch_context::plugin(),
    ];
    let config = Arc::new(Config::parse(toml).unwrap());
    let registry = RegistryBuilder::build(plugins, &config, ws).unwrap();
    Session::builder(Arc::new(registry), config)
        .workspace(ws)
        .id(id)
        .store(SessionStore::new(sessions))
        .build()
        .unwrap()
}

pub struct Dirs {
    _tmp: tempfile::TempDir,
    pub ws: PathBuf,
    pub sessions: PathBuf,
}

pub fn dirs() -> Dirs {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let sessions = tmp.path().join("sessions");
    Dirs {
        ws,
        sessions,
        _tmp: tmp,
    }
}

pub fn received(rx: &mut tokio::sync::broadcast::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

// ---------- signed reasoning ----------

/// The scripted provider, with every reply's reasoning signed and every request
/// checked, summary calls included. A signature records the system blocks, the tools
/// and the messages before the reply, without their thinking. A request is rejected
/// when a thinking block follows other messages than it was signed after; when the
/// blocks before a kept block aren't those it was signed after, less a leading run
/// (a gap, or a block brought back); or when a block that was left out once is sent
/// again. Summary calls are answered here, so they don't use up the script, and every
/// request is held to the scripted provider's window.
#[derive(Clone)]
pub struct Signed {
    pub inner: ScriptedProvider,
    pub state: Arc<Mutex<Signing>>,
    /// Extra characters in each summary.
    pub pad: usize,
}

#[derive(Default)]
pub struct Signing {
    /// Each signature, and the signatures of the blocks before it when it was written.
    issued: HashMap<String, Vec<String>>,
    /// Each signed reply without its thinking, and its signature.
    replies: HashMap<String, String>,
    /// Signatures whose message was sent without them.
    dropped: HashSet<String>,
    /// What was wrong with each request that broke the rules.
    pub violations: Vec<String>,
    /// Requests rejected as too long.
    pub rejected: usize,
    /// The summary requests.
    pub summaries: Vec<ModelRequest>,
}

impl Signed {
    pub fn new(inner: ScriptedProvider) -> Self {
        Self {
            inner,
            state: Arc::default(),
            pad: 0,
        }
    }

    /// Another script, with what was signed so far: for a resumed session.
    pub fn then(&self, inner: ScriptedProvider) -> Self {
        Self {
            inner,
            state: self.state.clone(),
            pad: self.pad,
        }
    }

    pub fn violations(&self) -> Vec<String> {
        self.state.lock().unwrap().violations.clone()
    }

    pub fn rejected(&self) -> usize {
        self.state.lock().unwrap().rejected
    }

    pub fn summaries(&self) -> Vec<ModelRequest> {
        self.state.lock().unwrap().summaries.clone()
    }
}

fn without_thinking(m: &Message) -> Message {
    Message {
        role: m.role,
        content: (m.content.iter())
            .filter(|b| !matches!(b, ContentBlock::Thinking { .. }))
            .cloned()
            .collect(),
    }
}

fn digest(value: &impl serde::Serialize) -> String {
    let mut h = DefaultHasher::new();
    serde_json::to_string(value).unwrap().hash(&mut h);
    format!("{:016x}", h.finish())
}

fn sign(request: &ModelRequest, before: usize) -> String {
    let messages: Vec<Message> = request.messages[..before]
        .iter()
        .map(without_thinking)
        .collect();
    format!(
        "sig-{}",
        digest(&(&request.system, &request.tools, messages))
    )
}

impl Signing {
    /// What is wrong with `request`.
    fn check(&mut self, request: &ModelRequest) -> Vec<String> {
        let mut wrong = Vec::new();
        let mut kept: Vec<String> = Vec::new();
        let mut left_out = Vec::new();
        for (k, m) in request.messages.iter().enumerate() {
            if m.role != Role::Assistant {
                continue;
            }
            let signatures: Vec<&String> = (m.content.iter())
                .filter_map(|b| match b {
                    ContentBlock::Thinking { signature, .. } => signature.as_ref(),
                    _ => None,
                })
                .collect();
            for &sig in &signatures {
                let Some(before) = self.issued.get(sig) else {
                    wrong.push(format!("message {k}: a signature that wasn't issued"));
                    continue;
                };
                if *sig != sign(request, k) {
                    wrong.push(format!("message {k}: thinking after changed messages"));
                }
                if self.dropped.contains(sig) {
                    wrong.push(format!(
                        "message {k}: thinking left out before and sent again"
                    ));
                }
                if !before.ends_with(&kept) {
                    wrong.push(format!(
                        "message {k}: the thinking before it isn't what it was written after, \
                         less a leading run"
                    ));
                }
                kept.push(sig.clone());
            }
            if signatures.is_empty()
                && let Some(sig) = self.replies.get(&digest(&without_thinking(m)))
            {
                left_out.push(sig.clone());
            }
        }
        self.dropped.extend(left_out);
        wrong
    }

    /// Signs `reply` to `request`.
    fn sign(&mut self, request: &ModelRequest, reply: &mut Message) {
        let sig = sign(request, request.messages.len());
        let before = (request.messages.iter())
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::Thinking { signature, .. } => signature.clone(),
                _ => None,
            })
            .collect();
        self.issued.insert(sig.clone(), before);
        self.replies
            .insert(digest(&without_thinking(reply)), sig.clone());
        reply.content.insert(
            0,
            ContentBlock::Thinking {
                thinking: "reasoning".into(),
                signature: Some(sig),
            },
        );
    }
}

#[async_trait]
impl ModelProvider for Signed {
    fn id(&self) -> &str {
        "signed"
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        {
            let mut st = self.state.lock().unwrap();
            let wrong = st.check(&request);
            if !wrong.is_empty() {
                let why = format!("{}: {}", request.purpose, wrong.join("; "));
                st.violations.push(why.clone());
                return Err(ModelError::InvalidRequest(why));
            }
            if let Some(window) = self.inner.context_window("")
                && estimate_request_tokens(&request) > window
            {
                st.rejected += 1;
                return Err(ModelError::ContextOverflow("over the window".into()));
            }
            if request.purpose == "context.summarize" {
                st.summaries.push(request.clone());
                let n = st.summaries.len();
                let text = format!(
                    "## Goal\nRead the logs.\n## Next step\nKeep reading, summary {n}.{}",
                    "s".repeat(self.pad)
                );
                let response = ModelResponse {
                    message: Message::assistant(text),
                    stop_reason: StopReason::EndTurn,
                    usage: Usage::default(),
                };
                return Ok(Box::pin(stream::iter(vec![Ok(ModelEvent::Completed(
                    response,
                ))])));
            }
        }
        let events: Vec<_> = self
            .inner
            .stream(request.clone(), cancel)
            .await?
            .collect()
            .await;
        let mut st = self.state.lock().unwrap();
        let events = events
            .into_iter()
            .map(|e| match e {
                Ok(ModelEvent::Completed(mut r)) => {
                    st.sign(&request, &mut r.message);
                    Ok(ModelEvent::Completed(r))
                }
                other => other,
            })
            .collect::<Vec<_>>();
        Ok(Box::pin(stream::iter(events)))
    }

    fn context_window(&self, model: &str) -> Option<u64> {
        self.inner.context_window(model)
    }

    fn default_model(&self) -> Option<&str> {
        Some("scripted")
    }
}

pub struct SignedPlugin(pub Signed);

impl Plugin for SignedPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new("signed", "test provider")
    }
    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        cx.add_provider(self.0.clone());
        Ok(())
    }
}

/// A session over `provider` under `[plugins.context]` `toml` (`staged` by default).
pub fn signed_session(provider: &Signed, d: &Dirs, id: &str, toml: &str) -> Session {
    build(
        id,
        Arc::new(SignedPlugin(provider.clone())),
        &d.ws,
        &d.sessions,
        &format!("[model]\nprovider = \"signed\"\nmax_output_tokens = 2000\n{toml}"),
    )
}
