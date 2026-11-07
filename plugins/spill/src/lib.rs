//! Spill (plugin `spill`): a tool result too long for the context is saved to a file,
//! and the model gets its head and tail with a notice naming the file. The model reads
//! the rest with the tools it already has (`read_file` with `offset`/`limit`, or
//! `grep_text`), so nothing is lost and there is no extra tool to learn.
//!
//! The `post_tool` hook runs after every other hook. Results within `max_inline_chars`
//! are left alone, and so are `read_file` results, which already page. Files go to
//! `<session dir>/spill/` (or, with `--no-save`, to a private temp directory removed at
//! exit), which is added to the [`ReadOnlyRoots`] service so the file tools may read it.
//! If saving fails, the result is left to the kernel's `tool_result_max_chars` cut.
//!
//! ```toml
//! [plugins.spill]
//! max_inline_chars = 24000   # default and upper bound: agent.tool_result_max_chars
//! ```

mod store;

use std::sync::Arc;

use anyhow::bail;
use ch_kernel::reexports::async_trait;
use ch_kernel::{
    Cx, Hook, HookDecision, HookEvent, HookPoint, Plugin, PluginManifest, ReadOnlyRoots, RegisterCx,
};
use serde::Deserialize;
use serde_json::json;

pub use store::SpillStore;

/// Plugin id, also the hook id.
pub const ID: &str = "spill";

/// The smallest inline budget: below it the notice alone wouldn't fit.
pub const MIN_INLINE_CHARS: usize = 1000;

/// Results of these tools are never spilled: `read_file` already pages, and spilling
/// its result would send the model back to the same file.
const NEVER_SPILLED: &[&str] = &["read_file"];

/// `[plugins.spill]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpillConfig {
    /// Longer results are spilled. Defaults to, and can't exceed,
    /// `agent.tool_result_max_chars`.
    pub max_inline_chars: Option<usize>,
}

/// Registers the hook and provides [`ReadOnlyRoots`].
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
        .provides(&["hook:spill", "service:ReadOnlyRoots"])
        .project_settings(&["max_inline_chars"])
    }

    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        let config: SpillConfig = cx.config()?;
        let kernel_max = cx.full_config().kernel()?.agent.tool_result_max_chars;
        let max = config
            .max_inline_chars
            .unwrap_or(kernel_max)
            .min(kernel_max);
        if max < MIN_INLINE_CHARS {
            bail!(
                "plugins.spill.max_inline_chars is {max}, but it must be at least \
                 {MIN_INLINE_CHARS} (and agent.tool_result_max_chars at least that much) \
                 to fit a preview and the notice"
            );
        }
        let roots = Arc::new(ReadOnlyRoots::default());
        cx.provide_service(roots.clone())?;
        cx.add_hook(SpillHook {
            max_inline_chars: max,
            store: SpillStore::new(roots),
        });
        Ok(())
    }
}

/// The `post_tool` hook.
pub struct SpillHook {
    max_inline_chars: usize,
    store: SpillStore,
}

impl SpillHook {
    pub fn new(max_inline_chars: usize, store: SpillStore) -> Self {
        Self {
            max_inline_chars,
            store,
        }
    }
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
        match self.store.save(cx, &call.id, &call.name, &output.content) {
            Ok(path) => {
                let notice = notice(total, &path.display().to_string(), cx);
                let preview = preview(&output.content, total, &notice, self.max_inline_chars);
                HookDecision::Rewrite {
                    input: json!(preview),
                }
            }
            Err(e) => {
                tracing::warn!(tool = %call.name, "spilling a {total}-char result failed: {e:#}");
                HookDecision::Continue
            }
        }
    }
}

/// The notice that replaces the middle. `{omitted}` is filled in by [`preview`].
fn notice(total: usize, path: &str, cx: &Cx) -> String {
    let how = if cx.has_tool("read_file") && cx.has_tool("grep_text") {
        "Read it with read_file (offset/limit) or search it with grep_text on that path."
    } else if cx.has_tool("bash") {
        "Read it with `sed -n 'A,Bp'` or search it with `grep -n` in bash."
    } else {
        "It is kept for the user."
    };
    format!(
        "[… {{omitted}} of {total} characters omitted. The full output is saved at \
         {path}.\n{how}]"
    )
}

/// The head and tail of `content` around `notice`, at most `max` characters in all.
fn preview(content: &str, total: usize, notice: &str, max: usize) -> String {
    // The count is at most as long as `total`, so size the notice for that.
    let notice_len = notice.chars().count() - "{omitted}".len() + total.to_string().len();
    let keep = max.saturating_sub(notice_len + "\n\n".len() * 2);
    let head = keep / 2;
    let tail = keep - head;
    let omitted = total - head - tail;
    let start: String = content.chars().take(head).collect();
    let end: String = content.chars().skip(total - tail).collect();
    let notice = notice.replace("{omitted}", &omitted.to_string());
    format!("{start}\n\n{notice}\n\n{end}")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ch_kernel::{ToolCall, ToolOutput, ToolSpec};

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

    fn hook(max: usize) -> (SpillHook, Arc<ReadOnlyRoots>) {
        let roots = Arc::new(ReadOnlyRoots::default());
        (SpillHook::new(max, SpillStore::new(roots.clone())), roots)
    }

    /// A numbered log, so head and tail are recognisable.
    fn log(lines: usize) -> String {
        (1..=lines).map(|i| format!("line {i}\n")).collect()
    }

    fn rewritten(d: HookDecision) -> String {
        match d {
            HookDecision::Rewrite { input } => input.as_str().unwrap().to_string(),
            other => panic!("expected a rewrite, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_long_result_becomes_a_preview_and_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (h, roots) = hook(2000);
        let full = log(5000);
        let cx = cx(
            Some(tmp.path().join("s1")),
            &["read_file", "grep_text", "bash"],
        );
        let text = rewritten(h.on(&result("bash", full.clone(), false), &cx).await);

        assert!(text.chars().count() <= 2000, "{}", text.chars().count());
        assert!(text.starts_with("line 1\nline 2\n"), "{text}");
        assert!(text.ends_with("line 5000\n"), "{text}");
        assert!(text.contains("Read it with read_file (offset/limit) or search it with grep_text"));

        let dir = std::fs::canonicalize(tmp.path().join("s1/spill")).unwrap();
        let file = dir.join("call_1-bash.txt");
        assert!(
            text.contains(&format!("saved at {}.", file.display())),
            "{text}"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), full);
        assert!(roots.contains(&file));
        let omitted: usize = text
            .split("[… ")
            .nth(1)
            .and_then(|s| s.split(' ').next())
            .unwrap()
            .parse()
            .unwrap();
        let kept = text.chars().count() - notice_chars(&text);
        assert_eq!(omitted + kept, full.chars().count());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode();
            assert_eq!(mode(&dir) & 0o777, 0o700);
            assert_eq!(mode(&file) & 0o777, 0o600);
        }
    }

    /// Characters of the notice and the blank lines around it.
    fn notice_chars(text: &str) -> usize {
        let start = text.find("\n\n[… ").unwrap();
        let end = text.rfind("]\n\n").unwrap() + 3;
        text[start..end].chars().count()
    }

    #[tokio::test]
    async fn short_results_read_file_and_calls_that_never_ran_are_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let (h, _) = hook(2000);
        let cx = cx(Some(tmp.path().to_path_buf()), &[]);
        for event in [
            result("bash", "x".repeat(2000), false),
            result("read_file", log(5000), false),
            result("bash", log(5000), true),
        ] {
            assert_eq!(h.on(&event, &cx).await, HookDecision::Continue, "{event:?}");
        }
        assert!(!tmp.path().join("spill").exists());
    }

    #[tokio::test]
    async fn the_hint_follows_the_registered_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let (h, _) = hook(2000);
        let bash_only = cx(Some(tmp.path().to_path_buf()), &["bash"]);
        let text = rewritten(h.on(&result("bash", log(5000), false), &bash_only).await);
        assert!(text.contains("`sed -n 'A,Bp'`"), "{text}");
        let none = cx(Some(tmp.path().to_path_buf()), &[]);
        let text = rewritten(h.on(&result("mcp__x__dump", log(5000), false), &none).await);
        assert!(text.contains("It is kept for the user."), "{text}");
    }

    #[tokio::test]
    async fn without_a_session_dir_a_private_temp_dir_is_used_and_removed() {
        let (h, roots) = hook(2000);
        let text = rewritten(
            h.on(&result("bash", log(5000), false), &cx(None, &[]))
                .await,
        );
        let path = text
            .split("saved at ")
            .nth(1)
            .unwrap()
            .split(".\n")
            .next()
            .unwrap();
        let path = PathBuf::from(path);
        assert!(path.is_file());
        assert!(roots.contains(&path));
        drop(h);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn a_failed_save_leaves_the_result_to_the_kernel() {
        let tmp = tempfile::tempdir().unwrap();
        let blocked = tmp.path().join("not-a-dir");
        std::fs::write(&blocked, "").unwrap();
        let (h, _) = hook(2000);
        let d = h
            .on(&result("bash", log(5000), false), &cx(Some(blocked), &[]))
            .await;
        assert_eq!(d, HookDecision::Continue);
    }

    #[test]
    fn the_budget_has_a_floor_and_follows_the_kernel_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let build = |toml: &str| {
            let config = ch_kernel::Config::parse(toml).unwrap();
            ch_kernel::RegistryBuilder::build(vec![plugin()], &config, tmp.path())
        };
        assert!(build("").is_ok());
        assert!(build("[plugins.spill]\nmax_inline_chars = 500").is_err());
        assert!(build("[agent]\ntool_result_max_chars = 800").is_err());
        let registry = build("[plugins.spill]\nmax_inline_chars = 99999999").unwrap();
        assert!(registry.services.contains::<ReadOnlyRoots>());
    }
}
