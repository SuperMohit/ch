# ADR 0002: Keep oversized tool output readable instead of cutting it

- **Status:** Accepted
- **Date:** 2026-11-07
- **Related:** [roadmap](../../ROADMAP.md) 6.6 (keep truncated tool output, replaced by this ADR) and 6.4 (context management), [architecture §8](../architecture.md#8-design-defaults) (truncation)

## Context

The kernel cuts every tool result longer than `agent.tool_result_max_chars` (24k characters) in the middle. The model sees the head and the tail, and the middle is gone. A long test log, build output or fetched page loses exactly the part the model may need, and the only way back is to run the command again. Re-running can be slow, and some commands give a different result the second time.

Roadmap 6.6 planned to keep the full output and add a `read_tool_result` tool. Optional tools like that are rarely called. Models reach for the tools they already use every step, and they don't learn a recall tool that exists only for this case.

## Decision

A result small enough is **inlined** as it is. A larger one is **spilled**: its full text is written to a file, and the model gets a head-and-tail preview with a notice naming the file. The model **fetches** what it needs with the tools it already has: `read_file` with `offset`/`limit`, or `grep_text` on that path. In bash-only mode it uses `sed -n` or `grep`. There is no new tool.

### Pieces

1. **Kernel: `post_tool` may replace a result's text.**
   - A hook that returns `Rewrite` with a string at `post_tool` replaces the result text. Before this change the kernel ignored it.
   - The kernel now runs `post_tool` hooks on the full output and applies the `tool_result_max_chars` cut *after* them, as the fallback. That way a hook sees what the tool really returned.
   - Hook notes are still appended after the cut.
   - A rewrite at `post_tool` doesn't re-run the chain. That re-run exists so the permission check sees the final input of a call, which doesn't apply after the call.
2. **Kernel: a `ReadOnlyRoots` service.** It lists directories outside the workspace that the file tools may read but never write. It starts empty. A plugin adds the directories it owns.
3. **`safety` and `tools-fs`: read-only paths.**
   - `read_file`, `grep_text` and the permission check for read-only file tools accept a path that resolves, after following symlinks, inside a read-only root.
   - Writes keep the strict workspace check, so a spill file can't be edited.
   - A symlink placed in a spill directory that points elsewhere resolves outside the root, so it is refused.
4. **The `spill` plugin.** A `post_tool` hook that runs after every other hook. When a result is longer than `max_inline_chars`, the hook:
   - saves the full text to `<session dir>/spill/<call id>-<tool>.txt`. The directory is `0700` and is added to `ReadOnlyRoots`. Each file is created with `create_new` and mode `0600`, so a file or symlink already at that path can't redirect the write.
   - replaces the result with a head and a tail of equal size plus the notice, within the same budget.
   - skips `read_file` results, which already page with `offset`/`limit`. Spilling them would send the model in a loop.
   - leaves the result to the kernel's cut if saving fails, and logs a warning. A tool call that worked never turns into an error because of the spill.

   With `--no-save` there is no session directory. The plugin then uses a private temp directory, created on first use and deleted when `ch` exits. Spilled files of a saved session stay with the session, so their paths still work after `--continue` or `/resume`.

### Settings

```toml
[plugins.spill]
max_inline_chars = 24000   # default: agent.tool_result_max_chars
```

`max_inline_chars` can only make results shorter than the kernel's own cut, never longer. A project can set it without being trusted.

Turning the plugin off (`plugins.disable = ["spill"]`) brings back the plain middle cut.

### The notice

```text
<head>

[… 182302 of 206302 characters omitted. The full output is saved at /…/spill/toolu_01…-bash.txt.
Read it with read_file (offset/limit) or search it with grep_text on that path.]

<tail>
```

The wording of the last line follows the tools that are registered.

## Consequences

- The model can recover any part of a long output without re-running the command, and the context budget is the same as before.
- Spill files take disk space under the session directory, like the session log. Deleting a session deletes them.
- The workspace check now has one exception, for reading runtime artifacts `ch` wrote itself. It is limited to directories a plugin registers, and to reading.
- Context management (6.4) builds on this: when it trims an old result, it can point the stub at that result's spill file so the content stays fetchable.

## Alternatives considered

- **A `read_tool_result` tool (the old 6.6).** Rejected, because a recall tool goes unused while the file tools are used all the time.
- **Spill files inside the workspace (`.ch/spill/`).** Rejected. They would show up in `git status` and in the model's own searches, and they would need ignore rules in every project.
- **Spilling inside each tool.** Rejected, because every tool would repeat the same code, and tools that don't spill (MCP and external tools) would still be cut. One policy after the call covers every tool.
- **Re-running the hook chain after a `post_tool` rewrite.** Rejected. Hooks that count calls, such as `stuck`, would count the same call twice.
