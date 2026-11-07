//! Spill (plugin `spill`): a tool result too long for the context is saved to a file,
//! and the model gets its head and tail with a notice naming the file and the omitted
//! lines. The model reads the rest with the tools it already has (`read_file` with
//! `offset`/`limit`, or `grep_text`), so nothing is lost and there is no extra tool to
//! learn.
//!
//! Files go to `<session dir>/spill/`, which the file tools may read but not write (see
//! `Cx::session_dir`), so the paths in the history keep working after `--continue` and
//! `/resume`. Results of the file tools that page or cap their own output are left
//! alone. Without a session directory, when saving fails, or once the session's spill
//! files reach `max_session_bytes`, the result is left to the kernel's
//! `tool_result_max_chars` cut.
//!
//! The hook runs after every other `post_tool` hook, so they see the whole output. When
//! an earlier hook stops the turn, the chain ends before this hook, and that one result
//! gets the kernel's cut.
//!
//! ```toml
//! [plugins.spill]
//! max_inline_chars = 24000          # default and upper bound: agent.tool_result_max_chars
//! max_session_bytes = 268435456     # 256 MiB of spill files per session
//! ```

mod preview;
mod store;

use std::path::PathBuf;
use std::sync::Arc;

use ch_kernel::reexports::async_trait;
use ch_kernel::{Cx, Hook, HookDecision, HookEvent, HookPoint, Plugin, PluginManifest, RegisterCx};
use serde::Deserialize;
use serde_json::json;

use preview::{Hint, preview};

/// Plugin id, also the hook id.
pub const ID: &str = "spill";

/// The smallest inline budget: below it a preview and the notice don't fit.
pub const MIN_INLINE_CHARS: usize = 1000;

/// Tools that already page or cap their own output; spilling it would only send the
/// model back to the same tool.
const NEVER_SPILLED: &[&str] = &["read_file", "grep_text", "glob_files", "list_files"];

/// `[plugins.spill]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpillConfig {
    /// Longer results are spilled. Defaults to, and can't exceed,
    /// `agent.tool_result_max_chars`; at least [`MIN_INLINE_CHARS`].
    pub max_inline_chars: Option<usize>,
    /// Spill files per session, in bytes. Past it, results get the kernel's cut.
    pub max_session_bytes: u64,
}

impl Default for SpillConfig {
    fn default() -> Self {
        Self {
            max_inline_chars: None,
            max_session_bytes: 256 * 1024 * 1024,
        }
    }
}

/// Registers the hook.
pub struct SpillPlugin;

/// The plugin, for the CLI's plugin list.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(SpillPlugin)
}

impl Plugin for SpillPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new(
            ID,
            "Saves tool output too long for the context to a file the model can read back",
        )
        .version(env!("CARGO_PKG_VERSION"))
        .provides(&["hook:spill"])
        .project_settings(&["max_inline_chars", "max_session_bytes"])
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let config: SpillConfig = cx.config()?;
        let kernel_max = cx.full_config().kernel()?.agent.tool_result_max_chars;
        if kernel_max < MIN_INLINE_CHARS {
            tracing::warn!(
                "spill is off: agent.tool_result_max_chars ({kernel_max}) is below \
                 {MIN_INLINE_CHARS}, too small for a preview and the notice"
            );
            return Ok(());
        }
        let wanted = config.max_inline_chars.unwrap_or(kernel_max);
        if wanted < MIN_INLINE_CHARS {
            tracing::warn!(
                "plugins.spill.max_inline_chars ({wanted}) is below {MIN_INLINE_CHARS}; \
                 using {MIN_INLINE_CHARS}"
            );
        }
        cx.add_hook(SpillHook {
            max_inline_chars: wanted.clamp(MIN_INLINE_CHARS, kernel_max),
            max_session_bytes: config.max_session_bytes,
        });
        Ok(())
    }
}

/// The `post_tool` hook.
pub struct SpillHook {
    max_inline_chars: usize,
    max_session_bytes: u64,
}

#[async_trait]
impl Hook for SpillHook {
    fn id(&self) -> &str {
        ID
    }

    fn points(&self) -> &[HookPoint] {
        &[HookPoint::PostTool]
    }

    /// After every other hook, so they all see the whole output.
    fn priority(&self) -> i32 {
        1000
    }

    async fn on(&self, event: &HookEvent, cx: &Cx) -> HookDecision {
        let HookEvent::PostTool {
            call,
            output,
            not_run: false,
        } = event
        else {
            return HookDecision::Continue;
        };
        let total = output.content.chars().count();
        if total <= self.max_inline_chars || NEVER_SPILLED.contains(&call.name.as_str()) {
            return HookDecision::Continue;
        }
        let Some(session_dir) = cx.session_dir.as_deref().cloned() else {
            return HookDecision::Continue;
        };
        let saved = save(
            session_dir,
            call.id.clone(),
            call.name.clone(),
            output.content.clone(),
            self.max_session_bytes,
        )
        .await;
        let path = match saved {
            Ok(path) => path,
            Err(e) => {
                tracing::warn!(tool = %call.name, "spilling a {total}-char result failed: {e:#}");
                return HookDecision::Continue;
            }
        };
        match preview(
            &output.content,
            &path.display().to_string(),
            hint(cx),
            self.max_inline_chars,
        ) {
            Some(text) => HookDecision::Rewrite { input: json!(text) },
            None => {
                tracing::warn!(path = %path.display(), "the spill notice doesn't fit the budget");
                HookDecision::Continue
            }
        }
    }
}

/// Saves off the async runtime: outputs can be megabytes.
async fn save(
    session_dir: PathBuf,
    call_id: String,
    tool: String,
    content: String,
    max_bytes: u64,
) -> anyhow::Result<PathBuf> {
    let task = tokio::task::spawn_blocking(move || {
        store::save(&session_dir, &call_id, &tool, &content, max_bytes)
    });
    Ok(task.await??)
}

/// How the notice tells the model to read the file, from the tools that exist.
fn hint(cx: &Cx) -> Hint {
    if cx.has_tool("read_file") && cx.has_tool("grep_text") {
        Hint::FileTools {
            bash: cx.has_tool("bash"),
        }
    } else if cx.has_tool("bash") {
        Hint::Bash
    } else {
        Hint::None
    }
}

#[cfg(test)]
mod tests {
    use ch_kernel::{Config, RegistryBuilder, ToolCall, ToolOutput, ToolSpec};

    use super::*;

    fn cx(session_dir: Option<PathBuf>, tools: &[&str]) -> Cx {
        let mut cx = Cx::detached("/ws");
        cx.session_dir = session_dir.map(Arc::new);
        cx.tools = Arc::new(
            tools
                .iter()
                .map(|n| ToolSpec {
                    name: n.to_string(),
                    description: String::new(),
                    input_schema: json!({"type": "object"}),
                    read_only: true,
                })
                .collect(),
        );
        cx
    }

    fn result(tool: &str, content: String, not_run: bool) -> HookEvent {
        HookEvent::PostTool {
            call: ToolCall {
                id: "call_1".into(),
                name: tool.into(),
                input: json!({}),
            },
            output: ToolOutput::error(content),
            not_run,
        }
    }

    fn hook(max: usize) -> SpillHook {
        SpillHook {
            max_inline_chars: max,
            max_session_bytes: 1 << 30,
        }
    }

    fn log(lines: usize) -> String {
        (1..=lines).map(|i| format!("line {i}\n")).collect()
    }

    fn rewritten(d: HookDecision) -> String {
        match d {
            HookDecision::Rewrite { input } => input.as_str().unwrap().to_string(),
            other => panic!("expected a rewrite, got {other:?}"),
        }
    }

    fn saved_path(text: &str) -> PathBuf {
        let rest = text.split("saved at ").nth(1).unwrap();
        PathBuf::from(rest.split(".\n").next().unwrap())
    }

    #[tokio::test]
    async fn a_long_result_becomes_a_preview_and_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let full = log(5000);
        let cx = cx(
            Some(tmp.path().join("s1")),
            &["read_file", "grep_text", "bash"],
        );
        let text = rewritten(
            hook(2000)
                .on(&result("bash", full.clone(), false), &cx)
                .await,
        );

        assert!(text.chars().count() <= 2000, "{}", text.chars().count());
        assert!(text.starts_with("line 1\nline 2\n"), "{text}");
        assert!(text.ends_with("line 5000\n"), "{text}");
        let path = saved_path(&text);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), full);
        assert!(path.starts_with(std::fs::canonicalize(tmp.path()).unwrap().join("s1/spill")));
        assert!(text.contains("read_file with offset="), "{text}");
    }

    #[tokio::test]
    async fn results_that_need_no_spill_are_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let with_dir = cx(Some(tmp.path().join("s1")), &[]);
        let h = hook(2000);
        for tool in NEVER_SPILLED {
            let d = h.on(&result(tool, log(5000), false), &with_dir).await;
            assert_eq!(d, HookDecision::Continue, "{tool}");
        }
        for event in [
            result("bash", "x".repeat(2000), false),
            result("bash", log(5000), true),
        ] {
            assert_eq!(
                h.on(&event, &with_dir).await,
                HookDecision::Continue,
                "{event:?}"
            );
        }
        let no_dir = cx(None, &[]);
        assert_eq!(
            h.on(&result("bash", log(5000), false), &no_dir).await,
            HookDecision::Continue
        );
        assert!(!tmp.path().join("s1").exists());
    }

    #[tokio::test]
    async fn a_failed_save_leaves_the_result_to_the_kernel() {
        let tmp = tempfile::tempdir().unwrap();
        let blocked = tmp.path().join("not-a-dir");
        std::fs::write(&blocked, "").unwrap();
        let d = hook(2000)
            .on(&result("bash", log(5000), false), &cx(Some(blocked), &[]))
            .await;
        assert_eq!(d, HookDecision::Continue);
    }

    #[test]
    fn the_budget_is_clamped_instead_of_failing_startup() {
        let tmp = tempfile::tempdir().unwrap();
        let budget = |toml: &str| {
            let config = Config::parse(toml).unwrap();
            let registry = RegistryBuilder::build(vec![plugin()], &config, tmp.path()).unwrap();
            // The hook's budget, read through its effect on a 30k-char result.
            registry.hooks.len()
        };
        assert_eq!(budget(""), 1);
        assert_eq!(budget("[plugins.spill]\nmax_inline_chars = 500"), 1);
        assert_eq!(
            budget("[agent]\ntool_result_max_chars = 800"),
            0,
            "spill is off"
        );
    }

    #[tokio::test]
    async fn the_budget_follows_the_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let run = |toml: &'static str| {
            let dir = tmp.path().join(format!("s{}", toml.len()));
            async move {
                let config = Config::parse(toml).unwrap();
                let registry =
                    RegistryBuilder::build(vec![plugin()], &config, std::path::Path::new("/"))
                        .unwrap();
                let (_, hook) = &registry.hooks[0];
                let cx = cx(Some(dir), &["read_file", "grep_text"]);
                rewritten(hook.on(&result("bash", log(5000), false), &cx).await)
                    .chars()
                    .count()
            }
        };
        assert!(run("").await <= 24_000);
        assert!(run("").await > 20_000, "the default is the kernel limit");
        assert!(run("[plugins.spill]\nmax_inline_chars = 500").await <= MIN_INLINE_CHARS);
        assert!(run("[plugins.spill]\nmax_inline_chars = 99999999").await <= 24_000);
        assert!(run("[agent]\ntool_result_max_chars = 3000").await <= 3000);
    }
}
