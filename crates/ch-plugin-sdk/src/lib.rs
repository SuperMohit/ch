//! Writing external ch plugins in Rust (roadmap 7.3).
//!
//! A plugin is a program ch starts with the session; this crate speaks the protocol
//! in [`ch_plugin_protocol`] for it. Build a [`Plugin`] with its tools, hooks, prompt
//! contributions, commands and event handlers, then call [`Plugin::run`]:
//!
//! ```no_run
//! use ch_plugin_sdk::{HookDecision, Plugin};
//! use serde_json::json;
//!
//! Plugin::new()
//!     .tool(
//!         "shout",
//!         "Returns the text in capitals.",
//!         json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}),
//!         |input, _cx| Ok(input["text"].as_str().unwrap_or_default().to_uppercase()),
//!     )
//!     .hook("pre_tool", &["bash"], |event, _cx| {
//!         let command = event["call"]["input"]["command"].as_str().unwrap_or_default();
//!         if command.contains("rm -rf /") {
//!             HookDecision::Deny { reason: "not that".into() }
//!         } else {
//!             HookDecision::Continue
//!         }
//!     })
//!     .run()
//!     .unwrap();
//! ```
//!
//! Requests are handled one at a time, in order, on the calling thread. A `$/cancel`
//! for a request already running can't stop it; ch has stopped waiting for it by then.
//! Write logs with [`Cx::log`] or to stderr, never to stdout: stdout is the protocol.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};

pub use ch_plugin_protocol as protocol;
pub use ch_plugin_protocol::HookDecision;
use ch_plugin_protocol::{
    CommandRunParams, CommandRunResult, HookDecl, HookInvokeParams, InitializeParams,
    InitializeResult, LogParams, MAX_MESSAGE, Message, NotifyParams, PROTOCOL_VERSION, PromptCaps,
    PromptSystemResult, PromptTurnParams, PromptTurnResult, RpcError, ToolCallParams,
    ToolCallResult, ToolDef, ToolsChangedParams, code, method,
};
use serde::de::DeserializeOwned;
use serde_json::Value;

/// What handlers can do besides answering: log, notify the user, change the tools.
pub struct Cx<'a> {
    out: &'a mut dyn Write,
    workspace: &'a str,
    turn: u32,
}

impl Cx<'_> {
    /// The workspace root ch runs in.
    pub fn workspace(&self) -> &str {
        self.workspace
    }

    /// The current user turn, for `prompt/turn` (0 elsewhere).
    pub fn turn(&self) -> u32 {
        self.turn
    }

    /// A line in ch's log. `level` is `error`, `warn`, `info` or `debug`.
    pub fn log(&mut self, level: &str, message: impl Into<String>) {
        self.send(Message::notification(
            method::LOG,
            LogParams {
                level: level.into(),
                message: message.into(),
            },
        ));
    }

    /// A line shown to the user.
    pub fn notify(&mut self, message: impl Into<String>) {
        self.send(Message::notification(
            method::UI_NOTIFY,
            NotifyParams {
                message: message.into(),
            },
        ));
    }

    /// Replaces the tools ch offers from this plugin. Calls go to the handlers given to
    /// [`Plugin::tool`]; a tool listed here without one fails when called.
    pub fn tools_changed(&mut self, tools: Vec<ToolDef>) {
        self.send(Message::notification(
            method::TOOLS_CHANGED,
            ToolsChangedParams { tools },
        ));
    }

    fn send(&mut self, m: Message) {
        let _ = self.out.write_all(&m.to_line());
        let _ = self.out.flush();
    }
}

type ToolFn = Box<dyn FnMut(Value, &mut Cx<'_>) -> Result<String, String>>;
type HookFn = Box<dyn FnMut(&Value, &mut Cx<'_>) -> HookDecision>;
type BlocksFn = Box<dyn FnMut(&mut Cx<'_>) -> Vec<String>>;
type CommandFn = Box<dyn FnMut(&str, &mut Cx<'_>) -> CommandRunResult>;
type EventFn = Box<dyn FnMut(&Value, &mut Cx<'_>)>;

/// An external plugin, built up and then run.
#[derive(Default)]
pub struct Plugin {
    tools: Vec<(ToolDef, ToolFn)>,
    hooks: Vec<(HookDecl, HookFn)>,
    system: Option<BlocksFn>,
    turn: Option<BlocksFn>,
    commands: HashMap<String, CommandFn>,
    events: Vec<String>,
    on_event: Option<EventFn>,
    workspace: String,
}

impl Plugin {
    pub fn new() -> Self {
        Self::default()
    }

    /// A tool named `name` (ch calls it `ext__<plugin>__<name>`). `Ok(text)` is the
    /// result; `Err(text)` is an error the model sees.
    pub fn tool(
        mut self,
        name: &str,
        description: &str,
        input_schema: Value,
        f: impl FnMut(Value, &mut Cx<'_>) -> Result<String, String> + 'static,
    ) -> Self {
        self.tools.push((
            ToolDef {
                name: name.into(),
                description: description.into(),
                input_schema,
            },
            Box::new(f),
        ));
        self
    }

    /// A hook at `point` (`pre_tool`, `post_tool`, `user_prompt`, ...). For `pre_tool`
    /// and `post_tool`, `tools` are name globs (`*` matches anything); empty means
    /// every tool. The handler gets the kernel's hook event as JSON. Several hooks at
    /// one point run in order; the first answer other than `Continue` is the answer.
    pub fn hook(
        mut self,
        point: &str,
        tools: &[&str],
        f: impl FnMut(&Value, &mut Cx<'_>) -> HookDecision + 'static,
    ) -> Self {
        self.hooks.push((
            HookDecl {
                point: point.into(),
                tools: tools.iter().map(|t| t.to_string()).collect(),
            },
            Box::new(f),
        ));
        self
    }

    /// Blocks for the system prompt, asked for once per conversation.
    pub fn system_prompt(mut self, f: impl FnMut(&mut Cx<'_>) -> Vec<String> + 'static) -> Self {
        self.system = Some(Box::new(f));
        self
    }

    /// Reminders for each model call. Return one only when it says something new.
    pub fn turn_reminders(mut self, f: impl FnMut(&mut Cx<'_>) -> Vec<String> + 'static) -> Self {
        self.turn = Some(Box::new(f));
        self
    }

    /// The handler for `/name`, which must also be declared in the plugin's
    /// `plugin.toml` (`[[commands]]`).
    pub fn command(
        mut self,
        name: &str,
        f: impl FnMut(&str, &mut Cx<'_>) -> CommandRunResult + 'static,
    ) -> Self {
        self.commands.insert(name.into(), Box::new(f));
        self
    }

    /// Receives the events named in `types` (`tool_call_end`, `turn_end`, ..., or `*`).
    pub fn on_events(
        mut self,
        types: &[&str],
        f: impl FnMut(&Value, &mut Cx<'_>) + 'static,
    ) -> Self {
        self.events = types.iter().map(|t| t.to_string()).collect();
        self.on_event = Some(Box::new(f));
        self
    }

    /// Serves ch over stdin and stdout until ch sends `shutdown` or closes stdin.
    pub fn run(self) -> io::Result<()> {
        let stdin = io::stdin();
        let stdout = io::stdout();
        self.serve(stdin.lock(), stdout.lock())
    }

    /// [`Plugin::run`] over any reader and writer, for tests.
    pub fn serve(mut self, input: impl BufRead, mut output: impl Write) -> io::Result<()> {
        for line in input.split(b'\n') {
            let line = line?;
            if line.len() > MAX_MESSAGE {
                continue;
            }
            let Ok(message) = Message::parse(trim_cr(&line)) else {
                continue;
            };
            match message {
                Message::Request { id, method, params } => {
                    let shutdown = method == method::SHUTDOWN;
                    let reply = match self.handle(&method, params, &mut output) {
                        Ok(result) => Message::result(id, result),
                        Err(e) => Message::error(id, e),
                    };
                    output.write_all(&reply.to_line())?;
                    output.flush()?;
                    if shutdown {
                        break;
                    }
                }
                Message::Notification { method, params } if method == method::EVENT_NOTIFY => {
                    if let Some(f) = &mut self.on_event {
                        let mut cx = Cx {
                            out: &mut output,
                            workspace: &self.workspace,
                            turn: 0,
                        };
                        f(&params["event"], &mut cx);
                    }
                }
                // `$/cancel`: nothing is running concurrently to stop.
                Message::Notification { .. } | Message::Response { .. } => {}
            }
        }
        Ok(())
    }

    fn handle(&mut self, m: &str, params: Value, out: &mut dyn Write) -> Result<Value, RpcError> {
        match m {
            method::INITIALIZE => {
                let p: InitializeParams = parse(params)?;
                self.workspace = p.workspace;
                let result = InitializeResult {
                    protocol: PROTOCOL_VERSION,
                    tools: self.tools.iter().map(|(d, _)| d.clone()).collect(),
                    hooks: self.hooks.iter().map(|(d, _)| d.clone()).collect(),
                    prompt: PromptCaps {
                        system: self.system.is_some(),
                        turn: self.turn.is_some(),
                    },
                    events: self.events.clone(),
                };
                to_value(result)
            }
            method::TOOL_CALL => {
                let p: ToolCallParams = parse(params)?;
                let mut cx = Cx {
                    out,
                    workspace: &self.workspace,
                    turn: 0,
                };
                let Some((_, f)) = self.tools.iter_mut().find(|(d, _)| d.name == p.name) else {
                    return Err(RpcError::new(
                        code::INVALID_PARAMS,
                        format!("no tool `{}`", p.name),
                    ));
                };
                let result = match f(p.input, &mut cx) {
                    Ok(content) => ToolCallResult {
                        content,
                        is_error: false,
                    },
                    Err(content) => ToolCallResult {
                        content,
                        is_error: true,
                    },
                };
                to_value(result)
            }
            method::HOOK_INVOKE => {
                let p: HookInvokeParams = parse(params)?;
                let tool = p.event["call"]["name"].as_str().map(str::to_string);
                let mut cx = Cx {
                    out,
                    workspace: &self.workspace,
                    turn: 0,
                };
                for (decl, f) in &mut self.hooks {
                    let applies = decl.point == p.point
                        && (decl.tools.is_empty()
                            || tool
                                .as_deref()
                                .is_some_and(|t| decl.tools.iter().any(|g| glob(g, t))));
                    if !applies {
                        continue;
                    }
                    let decision = f(&p.event, &mut cx);
                    if decision != HookDecision::Continue {
                        return to_value(decision);
                    }
                }
                to_value(HookDecision::Continue)
            }
            method::PROMPT_SYSTEM => {
                let mut cx = Cx {
                    out,
                    workspace: &self.workspace,
                    turn: 0,
                };
                let blocks = self.system.as_mut().map(|f| f(&mut cx)).unwrap_or_default();
                to_value(PromptSystemResult { blocks })
            }
            method::PROMPT_TURN => {
                let p: PromptTurnParams = parse(params)?;
                let mut cx = Cx {
                    out,
                    workspace: &self.workspace,
                    turn: p.turn,
                };
                let reminders = self.turn.as_mut().map(|f| f(&mut cx)).unwrap_or_default();
                to_value(PromptTurnResult { reminders })
            }
            method::COMMAND_RUN => {
                let p: CommandRunParams = parse(params)?;
                let mut cx = Cx {
                    out,
                    workspace: &self.workspace,
                    turn: 0,
                };
                match self.commands.get_mut(&p.name) {
                    Some(f) => to_value(f(&p.args, &mut cx)),
                    None => Err(RpcError::new(
                        code::INVALID_PARAMS,
                        format!("no command `{}`", p.name),
                    )),
                }
            }
            method::SHUTDOWN => Ok(Value::Null),
            other => Err(RpcError::new(
                code::METHOD_NOT_FOUND,
                format!("unknown method `{other}`"),
            )),
        }
    }
}

fn trim_cr(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|e| RpcError::new(code::INVALID_PARAMS, e.to_string()))
}

fn to_value(v: impl serde::Serialize) -> Result<Value, RpcError> {
    serde_json::to_value(v).map_err(|e| RpcError::new(code::INTERNAL_ERROR, e.to_string()))
}

/// Matches `name` against `pattern`, where `*` matches any run of characters and `?`
/// any one character.
pub fn glob(pattern: &str, name: &str) -> bool {
    fn go(p: &[char], n: &[char]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some('*'), _) => go(&p[1..], n) || (!n.is_empty() && go(p, &n[1..])),
            (Some('?'), Some(_)) => go(&p[1..], &n[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &n[1..]),
            _ => false,
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    go(&p, &n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn exchange(plugin: Plugin, requests: &[Message]) -> Vec<Message> {
        let input: Vec<u8> = requests.iter().flat_map(Message::to_line).collect();
        let mut out = Vec::new();
        plugin.serve(&input[..], &mut out).unwrap();
        out.split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| Message::parse(l).unwrap())
            .collect()
    }

    fn init() -> Message {
        Message::request(
            0,
            method::INITIALIZE,
            InitializeParams {
                protocol: 1,
                ch_version: "0".into(),
                plugin: "t".into(),
                workspace: "/w".into(),
            },
        )
    }

    fn result(m: &Message) -> Value {
        match m {
            Message::Response { result: Ok(v), .. } => v.clone(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tools_hooks_and_prompts() {
        let plugin = Plugin::new()
            .tool(
                "shout",
                "Shouts.",
                json!({"type": "object"}),
                |input, cx| {
                    cx.log("info", "shouting");
                    Ok(input["text"].as_str().unwrap_or_default().to_uppercase())
                },
            )
            .tool("fail", "Fails.", json!({"type": "object"}), |_, _| {
                Err("no".into())
            })
            .hook("pre_tool", &["edit_*"], |_, _| HookDecision::Deny {
                reason: "read-only today".into(),
            })
            .system_prompt(|cx| vec![format!("Workspace: {}", cx.workspace())]);
        let replies = exchange(
            plugin,
            &[
                init(),
                Message::request(
                    1,
                    method::TOOL_CALL,
                    json!({"name": "shout", "input": {"text": "hi"}}),
                ),
                Message::request(2, method::TOOL_CALL, json!({"name": "fail", "input": {}})),
                Message::request(
                    3,
                    method::HOOK_INVOKE,
                    json!({"point": "pre_tool", "event": {"call": {"name": "edit_file"}}}),
                ),
                Message::request(
                    4,
                    method::HOOK_INVOKE,
                    json!({"point": "pre_tool", "event": {"call": {"name": "bash"}}}),
                ),
                Message::request(5, method::PROMPT_SYSTEM, json!({})),
                Message::request(6, "nope", json!({})),
                Message::request(7, method::SHUTDOWN, json!({})),
                Message::request(8, method::PROMPT_SYSTEM, json!({})),
            ],
        );
        let init = result(&replies[0]);
        assert_eq!(init["protocol"], 1);
        assert_eq!(init["tools"].as_array().unwrap().len(), 2);
        assert_eq!(init["prompt"], json!({"system": true, "turn": false}));
        assert!(matches!(&replies[1], Message::Notification { method, .. } if method == "log"));
        assert_eq!(
            result(&replies[2]),
            json!({"content": "HI", "is_error": false})
        );
        assert_eq!(result(&replies[3])["is_error"], true);
        assert_eq!(result(&replies[4])["decision"], "deny");
        assert_eq!(result(&replies[5])["decision"], "continue");
        assert_eq!(result(&replies[6]), json!({"blocks": ["Workspace: /w"]}));
        assert!(
            matches!(&replies[7], Message::Response { result: Err(e), .. } if e.code == code::METHOD_NOT_FOUND)
        );
        assert_eq!(result(&replies[8]), Value::Null);
        assert_eq!(replies.len(), 9, "nothing after shutdown");
    }

    #[test]
    fn globs() {
        assert!(glob("edit_*", "edit_file"));
        assert!(glob("*", ""));
        assert!(glob("b?sh", "bash"));
        assert!(!glob("edit_*", "write_file"));
        assert!(!glob("bash", "bash2"));
    }
}
