//! The built-in slash commands.

use std::fmt::Write as _;

use async_trait::async_trait;
use ch_kernel::{CmdCx, Command, Outcome, SessionMeta};

use crate::format::{first_line, mask_table, thousands};

/// How many sessions `/resume` lists.
const RESUME_LIST_LIMIT: usize = 15;

/// `/help`: lists every registered command.
pub struct Help;

#[async_trait]
impl Command for Help {
    fn name(&self) -> &str {
        "help"
    }
    fn description(&self) -> &str {
        "List the available commands"
    }
    async fn run(&self, _args: &str, cx: &mut CmdCx<'_>) -> anyhow::Result<Outcome> {
        let mut cmds: Vec<_> = cx
            .session
            .registry()
            .commands
            .iter()
            .map(|(plugin, c)| {
                (
                    c.name().to_string(),
                    c.usage().to_string(),
                    c.description().to_string(),
                    plugin.clone(),
                )
            })
            .collect();
        cmds.sort_by(|a, b| a.0.cmp(&b.0));
        let mut out = String::from("**Commands**\n\n");
        for (name, usage, description, plugin) in cmds {
            let call = if usage.is_empty() {
                format!("/{name}")
            } else {
                format!("/{name} {usage}")
            };
            let _ = write!(out, "- `{call}`: {description}");
            if plugin != crate::ID {
                let _ = write!(out, " _({plugin})_");
            }
            out.push('\n');
        }
        out.push_str("\nAnything else you type is sent to the agent.");
        Ok(Outcome::Message(out))
    }
}

/// `/clear`: forgets the conversation.
pub struct Clear;

#[async_trait]
impl Command for Clear {
    fn name(&self) -> &str {
        "clear"
    }
    fn description(&self) -> &str {
        "Start a fresh conversation (token and cost totals are kept)"
    }
    async fn run(&self, _args: &str, cx: &mut CmdCx<'_>) -> anyhow::Result<Outcome> {
        cx.session.clear();
        Ok(Outcome::Message("Conversation cleared.".into()))
    }
}

/// `/compact`: asks the context strategy to shrink the history now.
pub struct Compact;

#[async_trait]
impl Command for Compact {
    fn name(&self) -> &str {
        "compact"
    }
    fn description(&self) -> &str {
        "Compact the conversation history now"
    }
    async fn run(&self, _args: &str, cx: &mut CmdCx<'_>) -> anyhow::Result<Outcome> {
        if cx.session.history().is_empty() {
            return Ok(Outcome::Message("Nothing to compact yet.".into()));
        }
        match cx.session.compact().await {
            Ok(summary) => Ok(Outcome::Message(format!("Compaction: {summary}"))),
            Err(e) => Ok(Outcome::Message(format!(
                "Compaction failed: {e:#}. The conversation is unchanged; you can keep going \
                 or use /clear to start over."
            ))),
        }
    }
}

/// `/model [id]`: shows or sets the model for this session.
pub struct Model;

#[async_trait]
impl Command for Model {
    fn name(&self) -> &str {
        "model"
    }
    fn description(&self) -> &str {
        "Show the current model, or switch to another one for this session"
    }
    fn usage(&self) -> &str {
        "[model-id]"
    }
    async fn run(&self, args: &str, cx: &mut CmdCx<'_>) -> anyhow::Result<Outcome> {
        let s = &mut cx.session;
        if args.is_empty() {
            return Ok(Outcome::Message(format!(
                "Model: `{}` (provider `{}`, context window {} tokens).\n\nSwitch with \
                 `/model <id>`.",
                s.model(),
                s.provider_id(),
                thousands(s.context_window())
            )));
        }
        if args.split_whitespace().count() > 1 {
            return Ok(Outcome::Message(
                "Usage: `/model <id>` with a single model id, e.g. `/model claude-sonnet-5-5`."
                    .into(),
            ));
        }
        let old = s.model().to_string();
        s.set_model(args);
        Ok(Outcome::Message(format!(
            "Model changed from `{old}` to `{args}` for this session (provider `{}`). An \
             unknown id shows up as an error on the next request.",
            s.provider_id()
        )))
    }
}

/// `/cost`: token usage and cost so far.
pub struct Cost;

#[async_trait]
impl Command for Cost {
    fn name(&self) -> &str {
        "cost"
    }
    fn description(&self) -> &str {
        "Show token usage and cost for this session"
    }
    async fn run(&self, _args: &str, cx: &mut CmdCx<'_>) -> anyhow::Result<Outcome> {
        let s = &cx.session;
        let u = s.usage();
        let mut out = String::from("**Tokens**\n\n");
        let _ = writeln!(out, "- Input: {}", thousands(u.input_tokens));
        let _ = writeln!(out, "- Output: {}", thousands(u.output_tokens));
        let _ = writeln!(out, "- Cache read: {}", thousands(u.cache_read_tokens));
        let _ = writeln!(out, "- Cache write: {}", thousands(u.cache_write_tokens));
        out.push('\n');
        if s.kernel_config().model.pricing.is_some() || s.cost() > 0.0 {
            let _ = write!(out, "**Cost:** ${:.4}", s.cost());
        } else {
            out.push_str("**Cost:** not tracked. Set `[model.pricing]` in the config to see it.");
        }
        Ok(Outcome::Message(out))
    }
}

/// `/tools`: the tools the model can call.
pub struct Tools;

#[async_trait]
impl Command for Tools {
    fn name(&self) -> &str {
        "tools"
    }
    fn description(&self) -> &str {
        "List the tools available to the agent"
    }
    async fn run(&self, _args: &str, cx: &mut CmdCx<'_>) -> anyhow::Result<Outcome> {
        let specs = cx.session.tool_specs();
        if specs.is_empty() {
            return Ok(Outcome::Message(
                "No tools are registered: the agent can only talk. Enable tool plugins such \
                 as `tools-fs` or `tool-bash` in the config."
                    .into(),
            ));
        }
        let mut out = format!("**Tools** ({})\n\n", specs.len());
        for t in specs {
            let ro = if t.read_only { " (read-only)" } else { "" };
            let _ = writeln!(out, "- `{}`{ro}: {}", t.name, first_line(&t.description));
        }
        Ok(Outcome::Message(out.trim_end().to_string()))
    }
}

/// `/plugins`: every known plugin and why it is or isn't enabled.
pub struct Plugins;

#[async_trait]
impl Command for Plugins {
    fn name(&self) -> &str {
        "plugins"
    }
    fn description(&self) -> &str {
        "List plugins and whether each is enabled"
    }
    async fn run(&self, _args: &str, cx: &mut CmdCx<'_>) -> anyhow::Result<Outcome> {
        let plugins = &cx.session.registry().plugins;
        let on = plugins.iter().filter(|p| p.enabled).count();
        let mut out = format!("**Plugins** ({on} of {} enabled)\n\n", plugins.len());
        for p in plugins {
            let m = &p.manifest;
            let state = if p.enabled { "enabled" } else { "disabled" };
            let _ = writeln!(
                out,
                "- `{}` {} **{state}**: {} · _{}_",
                m.id, m.version, m.description, p.reason
            );
        }
        Ok(Outcome::Message(out.trim_end().to_string()))
    }
}

/// `/config`: the merged config with secrets masked.
pub struct ConfigCmd;

#[async_trait]
impl Command for ConfigCmd {
    fn name(&self) -> &str {
        "config"
    }
    fn description(&self) -> &str {
        "Show the merged configuration (secrets masked)"
    }
    async fn run(&self, _args: &str, cx: &mut CmdCx<'_>) -> anyhow::Result<Outcome> {
        let masked = mask_table(cx.session.config().as_table());
        if masked.is_empty() {
            return Ok(Outcome::Message(
                "The config is empty: built-in defaults are in use.".into(),
            ));
        }
        let text = toml::to_string_pretty(&masked)?;
        Ok(Outcome::Message(format!(
            "**Config** (values of keys containing `key`, `secret` or `token` are masked)\n\n\
             ```toml\n{}\n```",
            text.trim_end()
        )))
    }
}

/// `/resume [id]`: lists saved sessions, or loads one.
pub struct Resume;

#[async_trait]
impl Command for Resume {
    fn name(&self) -> &str {
        "resume"
    }
    fn description(&self) -> &str {
        "List saved sessions, or resume one by id"
    }
    fn usage(&self) -> &str {
        "[session-id]"
    }
    async fn run(&self, args: &str, cx: &mut CmdCx<'_>) -> anyhow::Result<Outcome> {
        let Some(store) = cx.session.store().cloned() else {
            return Ok(Outcome::Message(
                "Sessions aren't being saved in this run, so there is nothing to resume.".into(),
            ));
        };
        let sessions = store.list()?;
        let current = cx.session.id().to_string();

        if args.is_empty() {
            if sessions.is_empty() {
                return Ok(Outcome::Message(
                    "No saved sessions for this workspace yet.".into(),
                ));
            }
            let mut out = String::from("**Recent sessions** (newest first)\n\n");
            for m in sessions.iter().take(RESUME_LIST_LIMIT) {
                out.push_str(&session_line(m, m.id == current));
                out.push('\n');
            }
            if sessions.len() > RESUME_LIST_LIMIT {
                let _ = writeln!(
                    out,
                    "- ... and {} older",
                    sessions.len() - RESUME_LIST_LIMIT
                );
            }
            out.push_str("\nResume one with `/resume <id>` (a unique prefix of the id is enough).");
            return Ok(Outcome::Message(out));
        }

        let id = match resolve_id(&sessions, args) {
            Ok(id) => id,
            Err(msg) => return Ok(Outcome::Message(msg)),
        };
        if id == current {
            return Ok(Outcome::Message(format!(
                "`{id}` is already the current session."
            )));
        }
        let snap = match store.load(&id) {
            Ok(s) => s,
            Err(e) => {
                return Ok(Outcome::Message(format!(
                    "Couldn't load session `{id}`: {e:#}. Run /resume to see the sessions \
                     that can be loaded."
                )));
            }
        };
        // Don't lose the current conversation.
        if let Err(e) = cx.session.save() {
            tracing::warn!("saving the current session before /resume failed: {e:#}");
        }
        let title = snap.meta.title.clone();
        let turns = snap.meta.turns;
        cx.session.restore(snap);
        let title = if title.is_empty() {
            String::new()
        } else {
            format!(": {title}")
        };
        Ok(Outcome::Message(format!(
            "Resumed session `{id}`{title} ({turns} turns, model `{}`).",
            cx.session.model()
        )))
    }
}

fn session_line(m: &SessionMeta, current: bool) -> String {
    let title = if m.title.is_empty() {
        "(untitled)"
    } else {
        m.title.as_str()
    };
    let mark = if current { " **(current)**" } else { "" };
    format!(
        "- `{}`: {title} ({} turns, {}, `{}`){mark}",
        m.id,
        m.turns,
        m.updated_at.format("%Y-%m-%d %H:%M UTC"),
        m.model
    )
}

/// An exact id, or a unique prefix of one.
fn resolve_id(sessions: &[SessionMeta], wanted: &str) -> Result<String, String> {
    if sessions.iter().any(|m| m.id == wanted) {
        return Ok(wanted.to_string());
    }
    let matches: Vec<&SessionMeta> = sessions
        .iter()
        .filter(|m| m.id.starts_with(wanted))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => Err(format!(
            "No saved session matches `{wanted}`. Run /resume to list the sessions."
        )),
        many => Err(format!(
            "`{wanted}` matches {} sessions ({}). Use a longer prefix.",
            many.len(),
            many.iter()
                .map(|m| format!("`{}`", m.id))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// `/exit` and `/quit`.
pub struct Exit {
    name: &'static str,
}

impl Exit {
    pub fn new(name: &'static str) -> Self {
        Self { name }
    }
}

#[async_trait]
impl Command for Exit {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "Exit ch"
    }
    async fn run(&self, _args: &str, _cx: &mut CmdCx<'_>) -> anyhow::Result<Outcome> {
        Ok(Outcome::Exit)
    }
}
