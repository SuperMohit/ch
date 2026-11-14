//! Tools known only at run time, from MCP servers and external plugins (ADR 0006).
//!
//! A plugin registers a [`ToolSource`]. The session starts every source when it starts,
//! and a source reports its tools through a [`ToolSink`] then and whenever they change.
//! The session sends a change to the model just before its next model call.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::cx::Cx;
use crate::events::{AgentEvent, EventBus};
use crate::ledger::Offered;
use crate::tool::{Tool, ToolSpec};

/// Name prefixes only tools from a [`ToolSource`] may use: `mcp__<server>__` for MCP
/// servers and `ext__<plugin>__` for external plugins.
pub const RESERVED_PREFIXES: &[&str] = &["mcp__", "ext__"];

/// Most tools one source may offer; the rest are skipped with a warning.
pub const MAX_TOOLS_PER_SOURCE: usize = 128;

/// Longest tool description, in bytes. Longer ones are cut, always at the same place.
pub const MAX_DESCRIPTION_BYTES: usize = 4096;

/// How long a source may keep starting in the background after its startup timeout.
pub const BACKGROUND_START: Duration = Duration::from_secs(60);

/// Whether `name` is a tool name only tool sources may use.
pub fn is_outside_tool(name: &str) -> bool {
    RESERVED_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// The tool name for `raw` under `prefix` (such as `mcp__github__`): characters other
/// than ASCII letters, digits, `_` and `-` become `_`, and a name over 64 characters is
/// cut and ends in a hash of `raw`, so different raw names stay different. Sources keep
/// their own map back to the raw name.
pub fn outside_tool_name(prefix: &str, raw: &str) -> String {
    let clean: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = format!("{prefix}{clean}");
    if name.len() <= 64 && clean == raw {
        return name;
    }
    // FNV-1a, stable across runs and platforms.
    let mut h: u32 = 0x811c_9dc5;
    for b in raw.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    let suffix = format!("_{h:08x}");
    let keep = 64usize.saturating_sub(suffix.len());
    let mut cut = name;
    cut.truncate(keep);
    cut + &suffix
}

/// Where a tool source stands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SourceStatus {
    Starting,
    Ready {
        tools: usize,
    },
    Failed {
        reason: String,
    },
    /// It was running and stopped on its own (a process exit, a dropped connection).
    Exited {
        reason: String,
    },
    Stopped,
}

impl SourceStatus {
    /// Why its tools are gone, if they are.
    fn reason(&self) -> Option<&str> {
        match self {
            Self::Failed { reason } | Self::Exited { reason } => Some(reason),
            Self::Stopped => Some("it was stopped"),
            Self::Starting | Self::Ready { .. } => None,
        }
    }
}

#[async_trait]
pub trait ToolSource: Send + Sync {
    /// Unique among sources, such as `mcp:github`.
    fn id(&self) -> &str;

    /// Every tool name this source reports starts with this, such as `mcp__github__`.
    /// It must start with one of [`RESERVED_PREFIXES`].
    fn prefix(&self) -> String;

    /// Names of this source's tools the user's own config marks read-only. Every other
    /// tool from a source counts as not read-only, whatever the source says.
    fn read_only_tools(&self) -> Vec<String> {
        Vec::new()
    }

    /// How long the session waits for this source before going on without it.
    fn startup_timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    /// Connects or spawns, reports the first list of tools through `sink`, and returns.
    /// Later changes, and an exit, are reported through clones of `sink`. An error is
    /// shown to the user as the reason the source failed.
    async fn start(&self, sink: ToolSink, cx: &Cx) -> anyhow::Result<()>;

    /// Disconnects or stops the process. Called once, when the session ends or the
    /// source is removed.
    async fn stop(&self);
}

/// What a running source reports into. Cheap to clone.
#[derive(Clone)]
pub struct ToolSink {
    source: String,
    /// Which start of the source this sink belongs to. A sink from an earlier start
    /// (before a restart, or after a stop) is ignored.
    generation: u64,
    prefix: String,
    read_only: Arc<HashSet<String>>,
    catalog: Arc<Catalog>,
    events: EventBus,
}

impl ToolSink {
    pub(crate) fn new(
        source: &dyn ToolSource,
        generation: u64,
        catalog: Arc<Catalog>,
        events: EventBus,
    ) -> Self {
        Self {
            source: source.id().to_string(),
            generation,
            prefix: source.prefix(),
            read_only: Arc::new(source.read_only_tools().into_iter().collect()),
            catalog,
            events,
        }
    }

    pub fn source_id(&self) -> &str {
        &self.source
    }

    /// Replaces this source's tools. Tools that break a rule (a name outside the
    /// source's prefix, an invalid spec, more than [`MAX_TOOLS_PER_SOURCE`]) are skipped
    /// with a warning; the rest are offered.
    pub fn set_tools(&self, tools: Vec<Arc<dyn Tool>>) {
        let mut admitted: Vec<(ToolSpec, Arc<dyn Tool>)> = Vec::new();
        let mut skipped = Vec::new();
        for tool in tools {
            match self.admit(tool.spec()) {
                Ok(_) if admitted.len() >= MAX_TOOLS_PER_SOURCE => {
                    skipped.push(format!("more than {MAX_TOOLS_PER_SOURCE} tools"));
                    break;
                }
                Ok(spec) if admitted.iter().any(|(s, _)| s.name == spec.name) => {
                    skipped.push(format!("`{}` is listed twice", spec.name));
                }
                Ok(spec) => admitted.push((spec, tool)),
                Err(why) => skipped.push(why),
            }
        }
        for why in skipped {
            self.warn(format!("skipped a tool from `{}`: {why}", self.source));
        }
        let n = admitted.len();
        let status = SourceStatus::Ready { tools: n };
        if self
            .catalog
            .set(&self.source, self.generation, admitted, status)
        {
            self.emit_status();
        }
    }

    /// The source stopped on its own: its tools are withdrawn, and calls to them fail
    /// with `reason`.
    pub fn exited(&self, reason: impl Into<String>) {
        let status = SourceStatus::Exited {
            reason: reason.into(),
        };
        if self
            .catalog
            .set(&self.source, self.generation, Vec::new(), status)
        {
            self.emit_status();
        }
    }

    fn emit_status(&self) {
        if let Some(status) = self.catalog.status(&self.source) {
            self.events.emit(AgentEvent::ToolSource {
                source: self.source.clone(),
                status,
            });
        }
    }

    fn warn(&self, message: String) {
        self.events.emit(AgentEvent::Warning { message });
    }

    /// The spec as the session will use it, or why it is refused.
    fn admit(&self, mut spec: ToolSpec) -> Result<ToolSpec, String> {
        if !is_outside_tool(&self.prefix) {
            return Err(format!(
                "the source's prefix `{}` doesn't start with one of {RESERVED_PREFIXES:?}",
                self.prefix
            ));
        }
        if !spec.name.starts_with(&self.prefix) || spec.name.len() == self.prefix.len() {
            return Err(format!(
                "`{}` doesn't start with `{}`",
                spec.name, self.prefix
            ));
        }
        spec.validate()?;
        spec.read_only = self.read_only.contains(&spec.name);
        if spec.description.len() > MAX_DESCRIPTION_BYTES {
            let mut end = MAX_DESCRIPTION_BYTES - "…".len();
            while !spec.description.is_char_boundary(end) {
                end -= 1;
            }
            spec.description.truncate(end);
            spec.description.push('…');
        }
        Ok(spec)
    }
}

/// The tools built-in plugins and sources offer now. Shared by a session and its sinks.
#[derive(Default)]
pub(crate) struct Catalog {
    state: Mutex<CatalogState>,
}

#[derive(Default)]
struct CatalogState {
    builtin: Vec<(ToolSpec, Arc<dyn Tool>)>,
    /// In the order the sources were added, which is their precedence.
    sources: Vec<Slot>,
}

struct Slot {
    id: String,
    /// Bumped by every start and stop, so reports from an earlier start are ignored.
    generation: u64,
    status: SourceStatus,
    tools: Vec<(ToolSpec, Arc<dyn Tool>)>,
}

/// A tool name more than one source offers: `source` loses it to an earlier one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Shadowed {
    pub name: String,
    pub source: String,
}

impl Catalog {
    pub fn new(builtin: Vec<(ToolSpec, Arc<dyn Tool>)>) -> Self {
        Self {
            state: Mutex::new(CatalogState {
                builtin,
                sources: Vec::new(),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CatalogState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Marks a source as starting and returns the generation of this start. A source
    /// started again keeps its place, which is its precedence.
    pub fn start_source(&self, id: &str) -> u64 {
        let mut st = self.lock();
        match st.sources.iter_mut().find(|s| s.id == id) {
            Some(slot) => {
                slot.generation += 1;
                slot.status = SourceStatus::Starting;
                slot.tools.clear();
                slot.generation
            }
            None => {
                st.sources.push(Slot {
                    id: id.to_string(),
                    generation: 0,
                    status: SourceStatus::Starting,
                    tools: Vec::new(),
                });
                0
            }
        }
    }

    /// Withdraws a source's tools and ignores whatever its current start reports later.
    pub fn stop_source(&self, id: &str) {
        let mut st = self.lock();
        if let Some(slot) = st.sources.iter_mut().find(|s| s.id == id) {
            slot.generation += 1;
            slot.status = SourceStatus::Stopped;
            slot.tools.clear();
        }
    }

    /// Whether `generation` is the source's current start.
    pub fn is_current(&self, id: &str, generation: u64) -> bool {
        let st = self.lock();
        (st.sources.iter()).any(|s| s.id == id && s.generation == generation)
    }

    /// Records what the source's start `generation` reported. Returns false, changing
    /// nothing, when that start is no longer the current one.
    pub fn set(
        &self,
        id: &str,
        generation: u64,
        tools: Vec<(ToolSpec, Arc<dyn Tool>)>,
        status: SourceStatus,
    ) -> bool {
        let mut st = self.lock();
        match st.sources.iter_mut().find(|s| s.id == id) {
            Some(slot) if slot.generation == generation => {
                slot.tools = tools;
                slot.status = status;
                true
            }
            _ => false,
        }
    }

    /// How a start ended: `Ready` only if it reported nothing yet, a failure always.
    /// Returns whether the status changed.
    pub fn settle(&self, id: &str, generation: u64, status: SourceStatus) -> bool {
        let mut st = self.lock();
        let Some(slot) = st.sources.iter_mut().find(|s| s.id == id) else {
            return false;
        };
        if slot.generation != generation {
            return false;
        }
        let replace = match status {
            // `start` returned without reporting tools.
            SourceStatus::Ready { .. } => slot.status == SourceStatus::Starting,
            _ => true,
        };
        if replace {
            if !matches!(status, SourceStatus::Ready { .. }) {
                slot.tools.clear();
            }
            slot.status = status;
        }
        replace
    }

    pub fn status(&self, id: &str) -> Option<SourceStatus> {
        let st = self.lock();
        st.sources
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.status.clone())
    }

    pub fn statuses(&self) -> Vec<(String, SourceStatus)> {
        let st = self.lock();
        (st.sources.iter())
            .map(|s| (s.id.clone(), s.status.clone()))
            .collect()
    }

    /// Why `source`'s tools are gone, if they are.
    pub fn gone_reason(&self, source: &str) -> Option<String> {
        let st = self.lock();
        match st.sources.iter().find(|s| s.id == source) {
            Some(slot) => slot.status.reason().map(str::to_string),
            None => Some("its source is gone".into()),
        }
    }

    /// The built-in tools: what a conversation saved before ledgers existed was sent.
    pub fn builtin(&self) -> Vec<Offered> {
        (self.lock().builtin.iter())
            .map(|(spec, _)| Offered {
                spec: spec.clone(),
                source: None,
            })
            .collect()
    }

    /// Every tool offered now, built-in ones first, then each source's in order. A name
    /// already taken goes to the earlier one; the later ones are returned as shadowed.
    pub fn offered(&self) -> (Vec<Offered>, Vec<Shadowed>) {
        let st = self.lock();
        let mut offered: Vec<Offered> = (st.builtin.iter())
            .map(|(spec, _)| Offered {
                spec: spec.clone(),
                source: None,
            })
            .collect();
        let mut shadowed = Vec::new();
        for slot in &st.sources {
            for (spec, _) in &slot.tools {
                if offered.iter().any(|o| o.spec.name == spec.name) {
                    shadowed.push(Shadowed {
                        name: spec.name.clone(),
                        source: slot.id.clone(),
                    });
                    continue;
                }
                offered.push(Offered {
                    spec: spec.clone(),
                    source: Some(slot.id.clone()),
                });
            }
        }
        (offered, shadowed)
    }

    /// The tool offered under `name` now, with the spec the session uses for it.
    pub fn tool(&self, name: &str) -> Option<(ToolSpec, Arc<dyn Tool>)> {
        let st = self.lock();
        (st.builtin.iter())
            .chain(st.sources.iter().flat_map(|s| &s.tools))
            .find(|(spec, _)| spec.name == name)
            .map(|(spec, tool)| (spec.clone(), tool.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_keep_valid_characters_and_stay_unique_when_cut() {
        assert_eq!(
            outside_tool_name("mcp__gh__", "create_issue"),
            "mcp__gh__create_issue"
        );
        let dotted = outside_tool_name("mcp__gh__", "repo.list");
        assert!(dotted.starts_with("mcp__gh__repo_list_"), "{dotted}");
        assert_ne!(dotted, outside_tool_name("mcp__gh__", "repo_list"));

        let long_a = outside_tool_name("mcp__gh__", &"a".repeat(80));
        let long_b = outside_tool_name("mcp__gh__", &format!("{}b", "a".repeat(79)));
        assert_eq!(long_a.len(), 64);
        assert_ne!(long_a, long_b);
        assert_eq!(long_a, outside_tool_name("mcp__gh__", &"a".repeat(80)));
    }
}
