//! What the strategy's tests share: hand-built histories, the strategy's inputs, and
//! the invariants of ADR 0004 checked on every view.

#![allow(dead_code, unused_imports)]

pub use std::path::{Path, PathBuf};
pub use std::sync::Arc;

pub use ch_context::{Context, ContextConfig, REFUSED, Strategy};
pub use ch_kernel::util::system_reminder;
pub use ch_kernel::{
    AgentEvent, Budget, CompactOutcome, CompactReason, CompactRequest, ContentBlock, ContextCx,
    ContextStrategy, Cx, History, Hook, HookDecision, HookEvent, Message, ModelProvider,
    ModelRequest, Role, TokenCalibrator, ToolCall, ToolSpec, UsageLog,
};
pub use ch_provider_scripted::ScriptedProvider;
pub use serde_json::{Value, json};

// ---------- a conversation ----------

/// Builds a history the way the session does: a task, then steps of a tool call and
/// its result, with a reminder on each message the model is sent.
pub struct Conv {
    pub history: History,
    pub turn: u32,
    pub calls: usize,
    /// Extra characters of thinking in each step.
    pub think: usize,
}

impl Conv {
    pub fn new(task: &str) -> Self {
        let mut history = History::new();
        history.push(
            1,
            Message {
                role: Role::User,
                content: vec![ContentBlock::text(task), reminder("the plan is empty")],
            },
        );
        Self {
            history,
            turn: 1,
            calls: 0,
            think: 0,
        }
    }

    /// The model thinks and calls `tool`; its result comes back with a reminder.
    pub fn call(&mut self, tool: &str, input: Value, result: String) -> &mut Self {
        self.calls += 1;
        let id = format!("c{}", self.calls);
        self.history.push(
            self.turn,
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: format!(
                            "thinking before call {}{}",
                            self.calls,
                            "t".repeat(self.think)
                        ),
                        signature: Some("signed".into()),
                    },
                    ContentBlock::ToolUse {
                        id: id.clone(),
                        name: tool.into(),
                        input,
                    },
                ],
            },
        );
        self.history.push(
            self.turn,
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: id,
                        content: result,
                        is_error: false,
                    },
                    reminder("check the build"),
                ],
            },
        );
        self
    }

    /// A shell command with a `chars`-character numbered log.
    pub fn bash(&mut self, chars: usize) -> &mut Self {
        let n = self.calls + 1;
        self.call(
            "bash",
            json!({"command": format!("make step{n}")}),
            log(n, chars),
        )
    }

    pub fn plan(&mut self) -> &mut Self {
        let n = self.calls + 1;
        let plan = json!({"plan": [{"content": format!("step {n}"), "status": "in_progress"}]});
        self.call("update_plan", plan, "Plan updated: 1 step.".into())
    }

    pub fn write(&mut self, chars: usize) -> &mut Self {
        let input = json!({"path": "src/lib.rs", "content": "x".repeat(chars)});
        self.call("write_file", input, "Wrote src/lib.rs.".into())
    }

    /// The model answers, and the user sends `prompt`, which starts the next turn.
    pub fn turn(&mut self, prompt: &str) -> &mut Self {
        self.history.push(self.turn, Message::assistant("Done."));
        self.turn += 1;
        self.history.push(self.turn, Message::user(prompt));
        self
    }
}

pub fn reminder(text: &str) -> ContentBlock {
    ContentBlock::text(system_reminder(text))
}

pub fn log(n: usize, chars: usize) -> String {
    let mut out = String::new();
    let mut line = 0;
    while out.len() < chars {
        line += 1;
        out.push_str(&format!("step {n} line {line}\n"));
    }
    out
}

// ---------- the strategy's inputs ----------

pub fn budget(window: u64) -> Budget {
    Budget {
        window,
        reserved_output: 0,
        fixed_tokens: 0,
        calibrator: TokenCalibrator::default(),
    }
}

pub fn ccx(session_dir: Option<&Path>, tools: &[&str]) -> ContextCx {
    ccx_for("s1", session_dir, tools)
}

pub fn ccx_for(id: &str, session_dir: Option<&Path>, tools: &[&str]) -> ContextCx {
    let mut cx = Cx::detached("/ws");
    cx.session_id = Arc::from(id);
    cx.session_dir = session_dir.map(|d| Arc::new(d.join(id)));
    cx.tools = Arc::new(
        tools
            .iter()
            .map(|name| ToolSpec {
                name: name.to_string(),
                description: String::new(),
                input_schema: json!({"type": "object"}),
                read_only: true,
            })
            .collect(),
    );
    ContextCx {
        cx,
        provider: Arc::new(ScriptedProvider::new(Vec::new())),
        agent_request: ModelRequest {
            model: "m".into(),
            system: vec!["You are a test agent.".into()],
            messages: Vec::new(),
            tools: vec![ToolSpec {
                name: "bash".into(),
                description: "Runs a command".into(),
                input_schema: json!({"type": "object"}),
                read_only: false,
            }],
            max_tokens: 1000,
            temperature: None,
            thinking_budget: Some(2000),
            purpose: "agent".into(),
            tool_ledger: None,
        },
        summarizer: None,
        usage: UsageLog::default(),
    }
}

/// How [`Summarizer`] answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// A numbered summary.
    Summary,
    /// A numbered summary this many characters longer.
    Longer(usize),
    /// A summary and a tool call.
    WithCall,
    /// A summary cut off at the output limit.
    CutOff,
    /// A reply without text.
    Empty,
    /// The provider fails.
    Fail,
    /// The model refuses.
    Refuse,
    /// The turn is cancelled while the summary is written.
    Cancel,
}

/// A model that writes summaries and records what it was asked.
#[derive(Clone)]
pub struct Summarizer {
    pub requests: Arc<std::sync::Mutex<Vec<ModelRequest>>>,
    pub answer: Arc<std::sync::Mutex<Answer>>,
    /// Answers to the next requests it takes, in order, before `answer` again.
    pub queued: Arc<std::sync::Mutex<std::collections::VecDeque<Answer>>>,
    /// Extra characters in each summary, as one word.
    pub pad: usize,
    /// Extra words in each summary.
    pub words: usize,
    /// Requests over this many tokens are rejected as too long; it is the window the
    /// provider reports.
    pub window: Option<u64>,
}

impl Summarizer {
    pub fn new() -> Self {
        Self {
            requests: Arc::default(),
            answer: Arc::new(std::sync::Mutex::new(Answer::Summary)),
            queued: Arc::default(),
            pad: 0,
            words: 0,
            window: None,
        }
    }

    pub fn answer(&self, answer: Answer) {
        *self.answer.lock().unwrap() = answer;
    }

    /// Answers the next requests it takes (not those over its window) with `answers`.
    pub fn queue(&self, answers: &[Answer]) {
        self.queued.lock().unwrap().extend(answers);
    }

    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[ch_kernel::reexports::async_trait]
impl ModelProvider for Summarizer {
    fn id(&self) -> &str {
        "summarizer"
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancel: ch_kernel::reexports::CancellationToken,
    ) -> Result<ch_kernel::ModelStream, ch_kernel::ModelError> {
        use ch_kernel::reexports::futures::stream;
        use ch_kernel::{ModelError, ModelEvent, ModelResponse, StopReason, Usage};
        let n = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request.clone());
            requests.len()
        };
        if let Some(window) = self.window
            && request_tokens(&request) > window
        {
            return Err(ModelError::ContextOverflow("too long".into()));
        }
        let queued = self.queued.lock().unwrap().pop_front();
        let answer = queued.unwrap_or(*self.answer.lock().unwrap());
        let text = format!(
            "## Goal\nSummary {n} by {}.{}{}",
            request.model,
            "s".repeat(self.pad),
            " word".repeat(self.words)
        );
        let (content, stop_reason) = match answer {
            Answer::Fail => return Err(ModelError::Unavailable("no summaries today".into())),
            Answer::Cancel => {
                cancel.cancel();
                return Ok(Box::pin(stream::pending()));
            }
            Answer::Refuse => (vec![ContentBlock::text("I can't.")], StopReason::Refusal),
            Answer::Summary => (vec![ContentBlock::text(text)], StopReason::EndTurn),
            Answer::Longer(extra) => (
                vec![ContentBlock::text(format!("{text}{}", "s".repeat(extra)))],
                StopReason::EndTurn,
            ),
            Answer::WithCall => (
                vec![
                    ContentBlock::text(text),
                    ContentBlock::ToolUse {
                        id: "c".into(),
                        name: "bash".into(),
                        input: json!({"command": "ls"}),
                    },
                ],
                StopReason::ToolUse,
            ),
            Answer::CutOff => (vec![ContentBlock::text(text)], StopReason::MaxTokens),
            Answer::Empty => (vec![ContentBlock::text(" ")], StopReason::EndTurn),
        };
        let response = ModelResponse {
            message: Message {
                role: Role::Assistant,
                content,
            },
            stop_reason,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        };
        Ok(Box::pin(stream::iter(vec![Ok(ModelEvent::Completed(
            response,
        ))])))
    }

    fn context_window(&self, _model: &str) -> Option<u64> {
        self.window
    }
}

/// A request's size as the providers estimate it: its system blocks, messages and tools.
pub fn request_tokens(request: &ModelRequest) -> u64 {
    let system: usize = request.system.iter().map(String::len).sum();
    let messages: usize = request.messages.iter().map(Message::char_len).sum();
    let tools: usize = (request.tools.iter())
        .map(|t| t.name.len() + t.description.len() + t.input_schema.to_string().len())
        .sum();
    ch_kernel::estimate_tokens(system + messages + tools)
}

/// `ccx_for` with `provider` as the agent's provider, and a separate summarizer.
pub fn ccx_with(
    session_dir: Option<&Path>,
    provider: &Summarizer,
    summarizer: Option<(&Summarizer, &str)>,
) -> ContextCx {
    let mut ccx = ccx_for("s1", session_dir, FILE_TOOLS);
    ccx.provider = Arc::new(provider.clone());
    ccx.summarizer = summarizer.map(|(p, m)| {
        let p: Arc<dyn ModelProvider> = Arc::new(p.clone());
        (p, m.to_string())
    });
    ccx
}

pub const FILE_TOOLS: &[&str] = &["read_file", "grep_text"];

pub fn elide() -> Context {
    Context::new(ContextConfig {
        strategy: Strategy::Elide,
        ..Default::default()
    })
}

pub async fn view(ctx: &Context, history: &History, window: u64, ccx: &ContextCx) -> Vec<Message> {
    ctx.view(history, budget(window), ccx)
        .await
        .unwrap()
        .messages
}

pub async fn compact(
    ctx: &Context,
    history: &History,
    window: u64,
    ccx: &ContextCx,
    reason: CompactReason,
) -> CompactOutcome {
    let request = CompactRequest {
        reason,
        instructions: None,
    };
    ctx.compact(history, budget(window), &request, ccx)
        .await
        .unwrap()
}

pub fn tokens(messages: &[Message]) -> u64 {
    budget(1).measure(messages)
}

pub fn received(rx: &mut tokio::sync::broadcast::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

/// The records in a session's log, as JSON.
pub fn records(session_dir: &Path) -> Vec<Value> {
    match std::fs::read_to_string(session_dir.join("context/log.jsonl")) {
        Ok(text) => text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect(),
        Err(_) => Vec::new(),
    }
}

// ---------- the invariants ----------

pub fn is_thinking(b: &ContentBlock) -> bool {
    matches!(
        b,
        ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
    )
}

pub fn without_thinking(m: &Message) -> Message {
    Message {
        role: m.role,
        content: m
            .content
            .iter()
            .filter(|b| !is_thinking(b))
            .cloned()
            .collect(),
    }
}

/// What holds for every view.
pub fn assert_invariants(history: &History, view: &[Message]) {
    let messages = history.messages();
    assert_eq!(view.len(), messages.len(), "no message is removed");
    assert_eq!(view[0], messages[0], "the preamble is byte-identical");
    assert_eq!(
        view.last(),
        messages.last(),
        "the latest message is unchanged"
    );
    for (i, (v, h)) in view.iter().zip(&messages).enumerate() {
        let role = if i % 2 == 0 {
            Role::User
        } else {
            Role::Assistant
        };
        assert_eq!(v.role, role, "roles alternate from a user message");
        let shape = |m: &Message| -> Vec<String> {
            without_thinking(m)
                .content
                .iter()
                .map(|b| match b {
                    ContentBlock::ToolUse { id, name, .. } => format!("use {id} {name}"),
                    ContentBlock::ToolResult { tool_use_id, .. } => format!("result {tool_use_id}"),
                    ContentBlock::Text { .. } => "text".into(),
                    other => panic!("{other:?}"),
                })
                .collect()
        };
        assert_eq!(shape(v), shape(h), "message {i} keeps its blocks");
        for (vb, hb) in without_thinking(v)
            .content
            .iter()
            .zip(&without_thinking(h).content)
        {
            if let ContentBlock::Text { text } = hb
                && !text.starts_with("<system-reminder>")
            {
                assert_eq!(vb, hb, "user text is unchanged");
            }
        }
    }
    for pair in view.windows(2) {
        let uses: Vec<_> = pair[0].tool_calls().into_iter().map(|c| c.id).collect();
        let results: Vec<_> = (pair[1].content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                _ => None,
            })
            .collect();
        if pair[0].role == Role::Assistant {
            assert_eq!(uses, results, "each tool_use has its tool_result");
        }
    }
}

/// The newest messages that are the history's, apart from dropped thinking.
pub fn untouched_suffix(history: &History, view: &[Message]) -> usize {
    let messages = history.messages();
    view.iter()
        .zip(&messages)
        .rev()
        .take_while(|(v, h)| without_thinking(v) == without_thinking(h))
        .count()
}

/// Every stub's file holds the original result.
pub fn assert_stubs_read_back(history: &History, view: &[Message]) -> usize {
    let mut read = 0;
    for (v, h) in view.iter().zip(history.messages()) {
        for (vb, hb) in v.content.iter().zip(&h.content) {
            let (
                ContentBlock::ToolResult { content: stub, .. },
                ContentBlock::ToolResult {
                    content: original, ..
                },
            ) = (vb, hb)
            else {
                continue;
            };
            if let Some(rest) = stub.split("saved at ").nth(1) {
                let path = rest.split(';').next().unwrap();
                assert_eq!(&std::fs::read_to_string(path).unwrap(), original, "{path}");
                read += 1;
            }
        }
    }
    read
}

/// Thinking blocks after a record's first edit (all of them, for a reset), through its
/// `at_event`, are gone; the others are kept.
pub fn assert_thinking_rule(history: &History, view: &[Message], records: &[Value]) {
    let ranges: Vec<(Option<u64>, u64)> = records
        .iter()
        .map(|r| {
            (
                r["edits"][0]["event"].as_u64(),
                r["at_event"].as_u64().unwrap(),
            )
        })
        .collect();
    for (event, v) in history.events().iter().zip(view) {
        if event.message.role != Role::Assistant {
            continue;
        }
        let dropped = ranges
            .iter()
            .any(|&(first, at)| first.is_none_or(|f| event.id > f) && event.id <= at);
        let kept = v.content.iter().filter(|b| is_thinking(b)).count();
        let had = event
            .message
            .content
            .iter()
            .filter(|b| is_thinking(b))
            .count();
        assert_eq!(kept, if dropped { 0 } else { had }, "event {}", event.id);
    }
}

/// 30 steps of 2,000-character logs, about 18k tokens.
pub fn long_session() -> Conv {
    let mut c = Conv::new("Fix the parser");
    for _ in 0..30 {
        c.bash(2000);
    }
    c
}

pub fn warnings(rx: &mut tokio::sync::broadcast::Receiver<AgentEvent>) -> Vec<String> {
    received(rx)
        .into_iter()
        .filter_map(|e| match e {
            AgentEvent::Warning { message } => Some(message),
            _ => None,
        })
        .collect()
}

pub fn thinking_left(view: &[Message]) -> usize {
    view.iter()
        .flat_map(|m| &m.content)
        .filter(|b| is_thinking(b))
        .count()
}

/// A history trimmed twice, with the model answering after each trim: its view and the
/// two records.
pub async fn trimmed_twice(tmp: &Path) -> (Conv, Vec<Message>, String) {
    let mut c = long_session();
    let ccx = ccx(Some(tmp), FILE_TOOLS);
    let ctx = elide();
    view(&ctx, &c.history, 25_000, &ccx).await;
    for _ in 0..12 {
        c.bash(2000);
    }
    let v = view(&ctx, &c.history, 25_000, &ccx).await;
    c.bash(100);
    let log = std::fs::read_to_string(tmp.join("s1/context/log.jsonl")).unwrap();
    assert_eq!(log.lines().count(), 2);
    (c, v, log)
}

// ---------- summaries ----------

pub fn with(strategy: Strategy) -> Context {
    Context::new(ContextConfig {
        strategy,
        ..Default::default()
    })
}

/// The text of every text block in `message`.
pub fn texts(message: &Message) -> Vec<String> {
    message
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// The summary blocks of a view's first message.
pub fn summary_blocks(view: &[Message]) -> Vec<String> {
    texts(&view[0])
        .into_iter()
        .filter(|t| t.starts_with("<context-summary"))
        .collect()
}

/// What holds for a view with summaries: the first message starts with the history's,
/// then the summaries; roles alternate; every call has its result; the last message is
/// the history's.
pub fn assert_summarized(history: &History, view: &[Message]) {
    let first = &history.messages()[0];
    assert_eq!(view[0].content[..first.content.len()], first.content[..]);
    assert!(
        view[0].content[first.content.len()..].iter().all(
            |b| matches!(b, ContentBlock::Text { text } if text.starts_with("<context-summary"))
        )
    );
    for (i, m) in view.iter().enumerate() {
        let role = if i % 2 == 0 {
            Role::User
        } else {
            Role::Assistant
        };
        assert_eq!(m.role, role, "message {i}");
    }
    for pair in view.windows(2) {
        if pair[0].role != Role::Assistant {
            continue;
        }
        let uses: Vec<_> = pair[0].tool_calls().into_iter().map(|c| c.id).collect();
        let results: Vec<_> = (pair[1].content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(uses, results);
    }
    assert_eq!(view.last(), history.messages().last());
}

/// `prompts/summary-merge.md` with the default word limit.
pub fn summary_merge_prompt() -> String {
    include_str!("../../../../prompts/summary-merge.md")
        .trim()
        .replace("{max_words}", "1000")
}

/// A window at which `history` is just above `share` of it.
pub fn window_above(history: &History, share: f64) -> u64 {
    (tokens(&history.messages()) as f64 / (share + 0.02)) as u64
}

/// Rounds of work: a new prompt, a plan, a message the user sends during the turn, and
/// fifteen steps.
pub fn round(c: &mut Conv, n: usize) {
    c.turn(&format!("user prompt #{n}#"));
    c.plan();
    c.history.push(c.turn, Message::assistant("Working on it."));
    c.history.push(
        c.turn,
        Message::user(format!("user note #{n}#, keep it short")),
    );
    for _ in 0..15 {
        c.bash(2000);
    }
}

/// A history the trims can't bring below `hard`: mostly short results.
pub fn hard_to_trim() -> Conv {
    let mut c = Conv::new("Fix the parser");
    for _ in 0..4 {
        c.bash(2000);
    }
    for _ in 0..150 {
        c.bash(450);
    }
    c
}
