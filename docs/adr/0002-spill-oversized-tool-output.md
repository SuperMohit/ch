# ADR 0002: Keep oversized tool output readable instead of cutting it

- **Status:** Accepted
- **Date:** 2026-11-07
- **Related:** [roadmap](../../ROADMAP.md) 6.6 (keep truncated tool output, replaced by this ADR) and 6.4 (context management), [architecture §8](../architecture.md#8-design-defaults) (truncation)

## Context

The kernel cuts every tool result longer than `agent.tool_result_max_chars` (24k characters) in the middle. The model sees the head and the tail, and the middle is gone. A long test log, build output or fetched page loses exactly the part the model may need. The only way back is to run the command again, which can be slow and may give a different result.

Roadmap 6.6 planned to keep the full output and add a `read_tool_result` tool. Optional tools like that are rarely called. Models reach for the tools they use every step, not for a recall tool that exists only for this case.

## Decision

A result small enough is **inlined** as it is. A larger one is **spilled**: its full text is written to a file, and the model gets the head and tail with a notice that names the file and the omitted lines. The model **fetches** what it needs with the tools it already has:

- `read_file` from the offset the notice gives, or `grep_text` on that path;
- in bash-only mode, `tail -n +N | head`, `grep -n` or `cut -c`.

There is no new tool.

### Pieces

1. **Kernel: `post_tool` may replace a result's text.**
   - A hook that returns `Rewrite` with a string at `post_tool` replaces the result text.
   - `post_tool` hooks run on the full output. The `tool_result_max_chars` cut comes *after* them, as the fallback.
   - Hook notes are appended after the cut.
   - A rewrite at `post_tool` doesn't re-run the chain. That re-run exists so the permission check sees a call's final input, which doesn't apply after the call. Hooks that count calls, such as `stuck`, would count the call twice.
2. **Kernel: the session directory is readable.**
   - `Cx::session_dir` (`<sessions>/<id>`) is where plugins keep files for the model to read back.
   - `read_file`, `grep_text` and the permission check for those two tools accept a path inside it. No other tool does.
   - The path is checked after following symlinks, against the directory with its *parent* canonicalized. So a symlink at the session directory, or anywhere under it, resolves outside the directory and is refused.
   - Writes keep the strict workspace check, so a spill file can't be edited.
   - The readable directory belongs to the session the call runs in. After `/clear` or `/resume`, another session's files aren't readable.
3. **The `spill` plugin.** A `post_tool` hook that runs after every other hook. When a result is longer than `max_inline_chars`, it does the following.
   - **Where it saves.** The full text goes to `<session dir>/spill/<call id>-<tool>-<random>.txt`.
     - The `spill` directory is created `0700`. Saving is refused if it or the session directory is a symlink.
     - Each file is created with `create_new` and mode `0600`, so nothing already at that path can redirect the write.
     - The save runs off the async runtime and doesn't fsync. The file is a cache of output the model has already seen in part.
   - **What replaces the result.** The head and tail, cut at line breaks where one is near, with this notice in between: `[… lines A–B of N omitted (C characters). The full output is saved at <path>.` and how to read it, starting at `offset=A`. All of it stays within the budget.
     - When some line is longer than `read_file` shows (2,000 characters), the notice says so and suggests `head -n N | tail -n 1 | cut -c A-B` in bash.
     - When the output contains NUL bytes, which the file tools treat as binary, the notice says so and suggests `cat -v` in bash.
     - If the notice doesn't fit in the budget, the file is removed and the kernel's cut applies.
   - **What it doesn't spill.**
     - Results from `read_file`, `grep_text`, `glob_files` and `list_files`, which page or cap their own output. `grep_text` stops adding matches at the tool output budget and, for a single file, leaves the path off each line.
     - Calls that never ran.
   - **When the kernel's cut applies instead.** There is no session directory, the save fails, or the session's spill files would pass `max_session_bytes`. A tool call that worked never turns into an error because of the spill.

   Spill files stay in the session's directory, so their paths still work after `--continue` or `/resume`. The end-to-end test resumes a saved session in a fresh registry and session, as `--continue` builds them, and fetches the omitted lines by following the notice.

### Settings

```toml
[plugins.spill]
max_inline_chars = 24000          # default and upper bound: agent.tool_result_max_chars
max_session_bytes = 268435456     # 256 MiB of spill files per session
```

- `max_inline_chars` can only make results shorter than the kernel's cut, never longer.
- A value below 1,000 is raised to 1,000 with a warning. If `agent.tool_result_max_chars` itself is below 1,000, spilling is off.
- Both settings can be set by a project without being trusted. A value out of range is corrected with a warning rather than stopping `ch` from starting. A value of the wrong type, such as a negative number, is a config error like any other.
- Turning the plugin off (`plugins.disable = ["spill"]`) brings back the plain middle cut.

### The notice

```text
log line 00791

[… lines 792–4208 of 5000 omitted (51243 characters). The full output is saved at /…/s1/spill/call_0_0-big-3f9a1c2e.txt.
Read the rest with read_file with offset=792 (and a limit), or search it with grep_text on that path.]

log line 04209
```

The last line follows the tools that are registered.

## Consequences

- The model can recover any part of a long output without re-running the command, within the same context budget.
- Spill files live in `<sessions>/<id>/spill/`, next to the session's `<id>.jsonl`. They take up to `max_session_bytes` per session. Nothing deletes them yet, so deleting a session's `.jsonl` by hand leaves its directory behind. Session cleanup is future work.
- The workspace check now has one exception: `read_file` and `grep_text` may read the current session's own directory.
- If `CH_HOME` is inside the workspace, the session directory is part of the workspace. Spill files are then ordinary workspace files, and the file tools can edit them, just as they could the session log.
- When an earlier `post_tool` hook stops the turn (for example `stuck` on its last refusal), the chain ends before `spill`, and that one result gets the kernel's cut.
- The directory checks run by path just before the write. A process running as the same user could swap a directory for a symlink in between. The model's tools can't do that without an approved command, and a sandbox (ADR 0001) closes it fully.
- Context management (6.4) builds on this. When it trims an old result, it can point the stub at that result's spill file, so the content stays fetchable.

## Alternatives considered

- **A `read_tool_result` tool (the old 6.6).** Rejected: a recall tool goes unused, and the file tools are used all the time.
- **Spill files inside the workspace (`.ch/spill/`).** Rejected: they would show in `git status` and in the model's own searches, and every project would need ignore rules.
- **A registry of read-only directories that plugins add to.** Tried first, and rejected after review:
  - Directories were added only when something was spilled, so a resumed session couldn't read its own files.
  - The list belonged to the process, so it stayed open across `/clear`.
  - Only one plugin could provide the list.

  Deriving the readable directory from the session fixes all three.
- **Spilling inside each tool.** Rejected: every tool would repeat the same code, and tools that don't (MCP and external tools) would still be cut. One policy after the call covers every tool.
- **Re-running the hook chain after a `post_tool` rewrite.** Rejected, because counting hooks would count the call twice.
