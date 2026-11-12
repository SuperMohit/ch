# ADR 0004: Trim the context in rare batches, and append summaries instead of rewriting them

- **Status:** Accepted
- **Date:** 2026-11-11
- **Related:** [roadmap](../../ROADMAP.md) 6.4 (context management, rewritten to point here), 6.5 (its tests), 6.7 (mid-turn messages), [ADR 0002](0002-spill-oversized-tool-output.md) (spill), [architecture §8](../architecture.md#8-design-defaults)

Code references are to `4d337c9` (branch `fix/append-only-reminders`, PR #40), which this ADR builds on: the code before the kernel changes below.

## Context

### What exists

- **The extension point.** The session calls `ContextStrategy::view(&History, Budget, &ContextCx)` before every model call (`model_call.rs:70-77`).
  - A view must keep `tool_use`/`tool_result` pairs, start with a user message and end with the latest user message unchanged (`context.rs:62-64`).
  - `View` is only a list of messages (`context.rs:44-48`).
  - `ContextCx` carries a summarizer provider and model id (`context.rs:50-56`). They default to the agent's provider and *configured* model, not the one `/model` switched to (`session/builder.rs:97-102`).
- **No strategy yet.** The registry falls back to `PassthroughContext`, which sends the whole history (`registry.rs:406-416`, `context.rs:84-103`).
  - `plugins/context` is an empty crate (`plugins/context/src/lib.rs:1-3`), and the CLI has its feature and registration commented out (`crates/ch-cli/Cargo.toml:65`, `crates/ch-cli/src/plugins.rs:49-50`).
  - `prompts/summary.md` is a placeholder (`prompts/summary.md:1`).
- **History.** Events are append-only, with stable and increasing ids (`history.rs:11-22`), and in the log "nothing that was sent is changed, except by explicit user actions" (`history.rs:1-4`).
  - The session saves the whole history as `<id>.jsonl` at the end of each turn (`session/mod.rs:563`, `store.rs:150-162`).
  - Plugins keep their files in `<id>/` (`store.rs:145-148`, `cx.rs:34-37`): `plan.json` (`plugins/planning/src/store.rs:10`) and `spill/` (ADR 0002).
- **Caching.** The Anthropic provider sets two cache breakpoints: on the last system block, and on the last message block that isn't thinking (`request.rs:102-112`, `126-146`). Usage reports cache reads and writes (`provider-anthropic/src/stream.rs:218-231`, `types.rs:131-139`).
- **Thinking.** Signed and redacted thinking blocks are replayed as received (`types.rs:19-28`, `request.rs:166-176`). The OpenAI provider drops thinking (`plugins/provider-openai/src/request.rs:110`). The default model is `claude-opus-5-5` (`provider-anthropic/src/models.rs:41`), which always thinks and has a 1M window.
- **Overflow.** A provider `ContextOverflow` (`provider-anthropic/src/errors.rs:83-86`, `108-111`) ends the turn at once (`model_call.rs:144-146`, `session/mod.rs:391-394`), and `ch -p` exits 3 (`plugins/frontend-print/src/run.rs:48`).
  - The REPL then suggests `/compact` (`plugins/frontend-repl/src/format.rs:240`).
  - `/compact` reaches the passthrough, which answers "not supported" (`context.rs:73-81`, `plugins/commands-core/src/commands.rs:88-111`).
- **Events.** `ContextCompacted { kind: Elided | Summarized, tokens_before, tokens_after }` exists (`events.rs:10-15`, `101-105`), but nothing emits it.
- **Bulky content.**
  - `spill` keeps each result under 24k characters and saves the rest (ADR 0002), but nothing bounds the sum.
  - Each `update_plan` call stays in history with its whole list (`planning/src/lib.rs:4-9`).
  - `write_file` and `edit_file` inputs carry whole file bodies.

### Fixed before this ADR: reminders stay in history

Until `fix/append-only-reminders`, the kernel added prompt reminders and `before_model`/`after_model` notes to one request and left them out of the next. With signed thinking that invalidated the model's earlier reasoning: `planning` adds a reminder to the first call of every turn (`planning/src/lib.rs:84-93`), so the second call of a turn broke the prefix that the first reply's thinking was signed against. It also rewrote the prompt cache after every reminder.

That change makes the history match what was sent:
- A call's reminders, `after_model` notes and `before_model` notes are added to the request after `view`, and stored on the latest user message in history once the `before_model` hooks let the call go ahead. A call that a hook stops stores nothing.
- The kernel checks that the request ends with the newest history message (`debug_assert_eq!` in `call_model`).
- `History::append_to_last_user` appends to a user message that hasn't been sent yet (`history.rs:54-74`).
- `ModelRequest.volatile_tail` is gone (`model.rs:14-30`), and the provider's breakpoint sits on the last block (`request.rs:126-146`).
- `first_user_text` returns the prompt's own text block (`history.rs:96-104`).

So a view contains every reminder from earlier calls, not the current call's, and reminders accumulate in history. This ADR trims them like other old content.

### What goes wrong without context management

- A long run grows until the provider rejects it, and the turn ends mid-task with exit code 3. On a 200k window (`agent.default_context_window`, `config.rs:243`) a few dozen bulky steps are enough.
- On the 1M default model a run rarely overflows, but every step re-reads the whole history. Cached reads are cheap, not free.
- Superseded plans, old reminders, old file bodies and old logs stay in front of the model.

### Three constraints

1. **The cache pays off only for an unchanged prefix.**
   - Anthropic bills a cache write at 1.25× the input price and a read at 0.1× (0.05× on Claude Opus 5.5). Any byte changed in the messages turns everything after it into a write.
   - A trim that leaves V tokens and frees F tokens pays back its rewrite after about V × (1.25 − r) / (F × r) steps, where r is the read price. With V = 0.40 and F = 0.20 of the window, that is 23 steps at r = 0.1 and 48 at r = 0.05.
   - So trims must be rare and large. Trimming a little on every step costs more than it saves.
2. **Signed thinking is bound to its prefix.** On Claude Opus 5.5, Sonnet 5.5, Haiku 5.5 and Fable 5.1, a thinking block's signature records the system prompt, the tool set and every message before the block.
   - Changing anything before a block invalidates it and every later block.
   - Accounts created on or after 2026-08-31 get a 400 ("… bound to a different conversation"). Older accounts aren't enforced unless the request opts in.
   - Removing a *leading* run of thinking blocks is accepted. So is removing all of them, keeping text and `tool_use` blocks; that is the API's documented recovery.
   - Opus 5.5 can't turn thinking off, so every assistant message carries a block.
3. **The view must be reproducible.** If the same history gives a different view after a restart, the cache misses. Under constraint 2, every thinking block written since the last compaction also becomes invalid.

## Decision

The `context` plugin provides one `ContextStrategy`, and `plugins.context.strategy` selects its behaviour. The design is generic: it works the same with every provider and is testable with the scripted one.

- The history is never changed.
- The view is a pure function of the history and a small, append-only **compaction log** in the session directory.
- The log changes only at a **trim** or a **summary**. Between them, each view is the previous one with the new messages appended, so the cached prefix survives.

### Regions

- **Preamble:** the first user message: the task, plus any reminders and notes added to it. Its blocks are never edited; summaries are appended after them.
- **Recent window:** the newest steps, at least `recent` × usable and at least `min_recent_steps` steps.
  - A step is an assistant message plus the user message that answers it. The window always starts at an assistant message.
  - The roadmap's "at least 2 turns" is read as 2 steps: a `ch -p` run is one turn of up to 300 steps.
- **Middle:** everything in between. Only the middle is trimmed or summarized, except on overflow.

The regions are fixed when a trim or summary is planned, and written into its record. Between those points they don't matter, because nothing is edited.

"Usable" is `Budget::usable()`: the window minus reserved output, system and tools (`context.rs:27-32`, `model_call.rs:37-47`). It also subtracts the current call's reminders, which are small and are part of the history from the next call on. Sizes are `Budget::measure`, the calibrated estimate (`context.rs:34-41`, `tokens.rs:28-42`). The calibrator restarts at 1.0 on resume (`builder.rs:163`), so estimates only drive new decisions, never a recomputation of old ones.

### When to trim

On each `view`, the plugin measures the view as it would be with no new decision.

- **At or below `soft` × usable:** it sends that view.
- **Above `soft`:** it plans a trim of the middle.
  - Edits are added in priority order until the projected view is at or below `trim_to` × usable, or nothing is left to trim.
  - **The trim is committed only if it frees at least `min_free` × usable.** Otherwise the view goes out unchanged.
  - As the recent window slides forward, more history becomes trimmable, and a later step reaches the threshold.
  - A trim's cache cost is set by its first edit, not by how much it trims, so a trim goes all the way down to `trim_to`.
- **Above `hard` after any trim** (under `summarize`, with no trim): a summary is written.

Older results are already stubs, so each trim's first edit lies past the earlier trims, and the prefix before it stays cached.

**Decided: `min_free` is a share of the usable window, default 0.10** (20k tokens at 200k, 100k at 1M). The rewrite that a trim pays for scales with the window, so the gain must too. Rejected: a fixed 20,000 tokens, which on a 1M window allows a 400k-token rewrite to save 20k per step; and a value computed from cache prices, which are optional, per-model config (`config.rs:204-222`).

### What a trim changes

A trim changes only the **contents** of blocks in the middle. It never removes a block, so every `tool_use` keeps its `tool_result`. In priority order:

1. **Superseded calls.**
   - Every call to a tool in `superseded_tools` (default `["update_plan"]`) except the latest gets the input `{"superseded": "a later update_plan call replaced this"}`.
   - Each `update_plan` call holds the full list, so only the latest matters. Its result is a short count and stays.
2. **Tool results** longer than `elide_min_chars` (default 500), oldest first.
   - Short results (exit codes, grep hits, an error line) stay word for word.
   - The stub says what was removed and where to read it:

     ```text
     [Output elided to save context: 1,204 lines, 48,113 characters. It is saved at /…/s1/context/elided/412-1.txt; read it with read_file (offset, limit) or grep_text.]
     ```

   - **A spilled result** points at its spill file, which holds the full output.
   - **Any other result** is saved to `<id>/context/elided/<event id>-<block index>.txt` with spill's rules: a `0700` directory, `create_new` with mode `0600`, and refusal through a symlink. The name comes from the event, not a random suffix, so the stub is reproducible.
   - **With no session directory, or a failed save,** the stub ends "Run the command again if you need it." instead.
   - The hint follows the registered tools, as spill's notice does. In bash-only mode it suggests `tail -n +N <path> | head` and `grep -n`.
3. **Tool inputs.**
   - Any string in a `tool_use` input longer than `input_min_chars` (default 1,000) becomes `"[elided from an earlier call to save context: 81,220 characters]"`.
   - This covers `write_file` content and long `edit_file` texts without naming tools. The input stays a JSON object; the Anthropic provider would send `{}` for anything else (`request.rs:177-184`).
   - **Risk:** the model may copy a shortened input. The plugin adds a `pre_tool` hook that refuses a call when one of its input strings *is* a marker: equal to one the plugin wrote, or matching the whole pattern `[elided from an earlier call to save context: N characters]`. It says the full text is needed. A string that only contains the marker is allowed, or edits to this ADR and to the plugin's own source would be refused.
4. **Old reminders.** Text blocks in the middle that start with the kernel's `<system-reminder>` wrapper (`util.rs:47-49`) become `[reminder elided]`. That covers the plan reminder, hook notes and `turn_end` notes.

**User text is never changed.** Text the user wrote stays byte for byte, either in place in the middle or copied into the summary area when its step is summarized. Thinking blocks follow their own rule (below).

**Decided: the spill file is found by looking in the result text for a path under `<session dir>/spill/`, and it is used only if the file exists.** History keeps only the result text: `ToolOutput.metadata` is dropped (`session/tools.rs:306-312`), and spill file names end in a random suffix (`plugins/spill/src/store.rs:29-32`). This needs no new state and no crate dependency, and it trusts only files that spill made in this session's directory, so a tool output that fakes a notice can only point at another spill file. Rejected: a `spill/index.jsonl` for the context plugin to read (a second format to keep in step); and ignoring spill and saving its preview (two hops to the full output).

**Decided: superseded tools are a config list, `superseded_tools = ["update_plan"]`.** No kernel change is needed, and MCP or external plan tools can be added. Rejected: a `ToolSpec` flag (`tool.rs:10-19`), which is a kernel change for one tool; and hard-coding `update_plan`, which couples two plugins.

**Decided: the user's text is found by position and wrapper.** History doesn't record who wrote a user-role block: a turn's prompt, hook notes (`session/mod.rs:372-376`), reminders, `turn_end` notes (`mod.rs:499-500`) and tool results are all user-role. The rules are:
- block 0 of a turn's first event is the prompt (the same block `first_user_text` returns, `history.rs:96-104`);
- a text block that starts with `<system-reminder>` is a note or reminder;
- any other text block in a user event is treated as the user's and kept, which covers 6.7's messages.

An unknown case errs toward keeping text, and no format change is needed. Rejected: an `origin` field on `history::Event` (`history.rs:14-22`). It is exact for whole events, but reminders appended to a tool-result message would still need the rules.

### Summaries

- **When:** above `hard` after trimming (`staged`), above `hard` (`summarize`), on `/compact`, and on overflow.
- **What:** the whole middle since the previous summary, up to the recent window. Each summary covers its own span, which ends just before an assistant message, so roles still alternate.
- **Appended, never rewritten.** Each summary is a new block after the earlier ones, which stay byte for byte. The summarizer sees the earlier summaries in its context and is told not to repeat them. Re-summarizing summaries loses detail each time, and instructions about scope and process go first.
- **Where:** text blocks appended to the first user message, after its existing blocks:

  ```text
  <context-summary n="2" covers="turn 1, steps 41–163">
  …the model's summary…
  </context-summary>
  <user-messages n="2">
  [turn 1, step 97] …the user's text, verbatim…
  </user-messages>
  <latest-call tool="update_plan">{…its input, verbatim…}</latest-call>
  ```

  - `<user-messages>` appears only when the span held user text, and `<latest-call>` only when the latest superseded call was in the span.
  - The plugin copies those two; the model writes only the summary. So the user's words and the current plan survive any number of compactions unchanged, as the roadmap requires (`ROADMAP.md:106`).
  - Closing tags inside copied or generated text are escaped.
- **The call.** The summary request is the agent's own request: the same model, system, tools and thinking settings, with the current view plus the summary prompt as a last text block.
  - Its prefix is what the previous step sent. The provider's breakpoint goes on the prompt block, and the cache lookback finds the previous step's entry a few positions back, so the history is read from cache, not paid for again.
  - Tools and tool choice stay as they are, since changing either invalidates the cache. The prompt says not to call tools, and any `tool_use` in the reply is ignored.
  - `max_tokens` is the agent's, because thinking counts toward it.
- **The prompt** (`prompts/summary.md`):

  ```text
  This conversation is close to the limit of the context window. Summarize the messages
  since the last <context-summary> (or since the task, if there is none); the summary
  replaces them. The task, earlier summaries and everything the user wrote are kept word
  for word, so don't repeat them, but say how each user message was handled.

  Write for yourself, to continue the work without those messages, under these headings:
  ## Goal
  ## Files touched
  ## Done
  ## Pending
  ## Errors & fixes
  ## Current state
  ## Next step

  Keep exact file paths, function names, commands and error messages. Record every
  instruction or limit the user gave about scope or process, and what was tried and
  failed. Don't call tools. Reply with the summary only, in at most {max_words} words.
  {instructions}
  ```

- **Failure.** If the call fails, is refused or returns no text:
  - nothing is logged and a `Warning` is emitted;
  - the view stays as trimmed, and the overflow path applies if it doesn't fit;
  - cancelling the turn cancels the call.

**Decided: summaries are text blocks appended to the first user message.** Roles keep alternating, which some chat templates behind OpenAI-compatible servers require, and the first message's existing blocks stay a byte-identical prefix. Rejected: a separate user message after the first (two user messages in a row, which not every server accepts); and a system block (changing the system prompt invalidates the system and messages caches and, on Opus 5.5, every thinking block).

**Decided: the agent's own current model writes summaries, replaying the cached prefix.** A summary then costs about one step, and the model that will continue the work decides what to keep; a weak summarizer drops what the agent needs. When `[model.summarizer]` is set (`config.rs:195-202`), that model gets the span as a plain-text transcript instead, because caches are per model. Rejected: always sending a transcript, which makes the agent model pay full input price for the span. This needs the agent's provider and current model when `[model.summarizer]` is unset (kernel change 3).

**Decided: when the summaries pass `max_summary_share` (default 0.15) of usable, the two oldest are merged into one with the same prompt.** It is the design's only rewrite; it is rare and counted in telemetry. Rejected: never merging, so a long REPL session eventually ends with overflow; and re-summarizing everything each time, which is the rewrite this design avoids.

### Strategies and settings

| `strategy` | Trim above `soft` | Summary above `hard` | On overflow | `/compact` |
|---|---|---|---|---|
| `none` | – | – | the turn ends (exit 3) | "not supported" |
| `elide` | ✓ | – | forced trim, retry | forced trim |
| `summarize` | – | ✓ | summary, retry | summary |
| `staged` (default) | ✓ first | ✓ if still above | forced trim, then summary, retry | summary |

```toml
[plugins.context]
strategy = "staged"            # none | elide | summarize | staged
soft = 0.60                    # 0.30–0.90
hard = 0.85                    # 0.50–0.95, at least soft + 0.05; the only summary trigger
recent = 0.30                  # 0.10–0.50, below soft
min_recent_steps = 2           # 1–10
trim_to = 0.40                 # 0.20 up to soft − 0.05
min_free = 0.10                # 0.02–0.30
elide_min_chars = 500          # 200–20000
input_min_chars = 1000         # 200–20000
superseded_tools = ["update_plan"]
summary_max_words = 1000       # 200–3000
max_summary_share = 0.15       # 0.05–0.30
```

- Shares are of the usable window.
- A project may set all of these without being trusted: none of them runs a command, reads a path or picks an endpoint.
- A value out of range is corrected with a warning, as spill does. A value of the wrong type is a config error.
- To manage a 1M-window model as if it had 200k, set `model.context_window = 200000` (`config.rs:174-175`).

**Decided: `compact_at_percent` is dropped, and `hard` is the only summary trigger, with range 0.50–0.95.** The roadmap's extra trigger "as a share of the window" (`ROADMAP.md:105`) is what `hard` already is, and two keys for one decision raise "which one wins?". The 0.50 floor guards against summarizing early, which throws away context the model is still using. Rejected: keeping both keys, with the lower one winning; and an absolute token cap, which `model.context_window` already provides.

### Overflow

- **Kernel change.** When the provider rejects a request with `ModelError::ContextOverflow`, the session calls `compact(reason: Overflow)` once. If that changed anything, it retries the call.
  - A second overflow in the same step, or a compaction that changed nothing, ends the turn with `ContextOverflow`, and `ch -p` exits 3.
  - Only a rejection that comes before any output is retried. One that arrives after some of the answer streamed ends the turn, because that text was already shown.
- **The retry adds nothing to history.**
  - By the time the rejected request was sent, its reminders and notes were stored in history.
  - Running `call_model` again would compute the reminders again and run `before_model` again, which would store the same text twice.
  - So the retry computes only the view again, and the `before_model` hooks don't run. Those hooks can't change the request, so the retry resends the same system blocks, tools and settings with a new view, which already holds the reminders and notes because they are in history.
  - The compaction and the new view get the same `ContextCx` as the step's first view.
- **The plugin's overflow compaction** ignores `min_free` and `trim_to`.
  - It trims every candidate in the middle, then summarizes (except under `elide`).
  - Because the estimate was wrong, it may also trim results in the recent window, except in the newest step.
- **`StopReason::ContextWindowExceeded`** (a response cut off by the window, `mod.rs:401-407`) still ends the turn. The partial message is already in history (`mod.rs:418`), and a retry would put two assistant messages in a row. Reserving output tokens in the budget makes it rare.

### `/compact`

- `/compact [instructions]` calls `compact(reason: Manual)`.
  - Under `staged` and `summarize` it summarizes the whole middle, with the recent window cut to `min_recent_steps`. The instructions are added to the summary prompt, for example "focus on the parser work".
  - Under `elide` it forces a trim; under `none` it says compaction isn't supported.
- It runs between turns, when no tool round is open.
- It reports the span and the size: `Summarized turn 1, steps 3–148: 412k → 61k tokens.`
- **Kernel change:** `Session::compact` passes the command's argument on. Today `commands.rs:99` ignores it.

### Persistence and resume

```text
<sessions>/<project>/<id>.jsonl                 history (kernel, unchanged)
<sessions>/<project>/<id>/context/log.jsonl     compaction records, append-only
<sessions>/<project>/<id>/context/elided/       copies of elided results, readable by the model
```

- **A record** holds everything needed to render its effect:
  - kind and trigger;
  - `at_event`, the last event id when it was decided;
  - the first edited position;
  - each edit, with a hash of the original block and the exact replacement text;
  - the range of thinking blocks to drop;
  - for a summary: its span, text, copied user text and model.
- **The view is recomputed on every step** from the history and the valid records. The rendering uses no clock, randomness or estimate. Because the replacement text is stored, a stub renders the same after resume even if the tools or settings changed.
- **On load,** a record whose events are missing or whose hashes don't match is ignored, with a warning. That covers a crash between writing the log and the turn-end save (`mod.rs:563`). *Changed in the implementation:* the log is applied up to its first record that doesn't match, and what happens next depends on whether the model answered after that record (see Implementation notes).
- **One session at a time.** The plugin keeps one session's state, keyed by `cx.cx.session_id`, and reloads when the id changes, as `PlanStore` does (`plugins/planning/src/store.rs:36-42`).
  - `/clear` starts an empty log; `/resume` and `--continue` load the session's log.
  - Without a session directory, records live in memory only.

**Decided: the view is recomputed from the history plus the compaction log, kept in the plugin's directory.** The history stays the full record, the log is small, and "same input, same bytes" is a pure function that 6.5 can test directly. The hash check handles a log that is ahead of the saved history, so the log needs no kernel-owned line type in `<id>.jsonl` (`store.rs:72-76`). Rejected: saving every view that was sent (the conversation stored twice, growing every step); and rewriting the history at each compaction (it loses the originals and contradicts `history.rs:1-4`).

### Signed thinking blocks

- **Rule.** A compaction drops every `Thinking` and `RedactedThinking` block from assistant messages after its first edit, up to its `at_event`.
  - Blocks before the first edit keep their prefix and stay.
  - Blocks written after the compaction were written against the new view and stay.
  - A summary edits the first message, so it drops all thinking up to its `at_event`.
  - The range is in the record, so the rule is reproducible.
- **No compaction while a tool call is waiting.** The API asks that an assistant turn whose `tool_use` still waits for its `tool_result` go back with its thinking intact.
  - In `ch`, a compaction is decided only inside `view`. `view` runs at the start of a step, after every result of the previous step is in history (`mod.rs:508-515`, then the next `call_model` at `mod.rs:391`).
  - So no `tool_use` is ever waiting when a compaction happens. `/compact` runs between turns.
- **The newest step loses its reasoning too.** Mid-turn, the newest assistant message's thinking was signed against the old prefix, so a compaction drops it with the rest. Keeping it would get the request rejected.
  - This is the API's documented handling for compaction that keeps recent turns: strip their thinking, keep their text and tool calls.
  - A live `#[ignore]` test in 6.5 confirms that the default model accepts it.
- **Providers.** The rule applies everywhere. The OpenAI provider drops thinking anyway, and older Claude models keep little thinking across turns.

**Decided: the plugin drops thinking consistently at each compaction (the rule above).** It is generic, deterministic, testable with the scripted provider, and the loss happens once per compaction. Rejected: leaving the blocks in and having the Anthropic provider send `prefix_mismatch_behavior: "drop_block"` (an Anthropic beta that has to go on every later request, and `ch`'s view would no longer say what the model saw). **Later work:** Anthropic's server-side context editing or compaction, which keep thinking valid, as a provider-specific strategy. It is Anthropic-only and beta, invisible to the view and telemetry, and not testable offline.

### Events and telemetry

- **`ContextCompacted`** (`events.rs:101-105`) for each trim (`Elided`) and summary (`Summarized`). The print frontend already renders it (`plugins/frontend-print/src/render.rs:34-42`).
- **`Custom { source: "context", name: "compaction" }`** with the details:
  - the trigger: `soft`, `hard`, `overflow` or `manual`;
  - results elided, inputs shortened, reminders elided, superseded calls, and thinking blocks dropped;
  - the first edited event, and the summary's span and length.
- **The summary call** goes through `ContextCx::complete`, which emits `ModelRequest` with purpose `context.summarize` and records the call's usage. When the strategy returns, the session emits `Usage` for it and adds it to the totals, with its cost when the call's model has pricing. Today only agent calls count (`model_call.rs:158-160`).
- **`Warning`** when a copy can't be saved, a summary fails, or log records are ignored on load.
- **For Phase 9** (`architecture.md:291`):
  - peak context ÷ window, and the number of trims and summaries;
  - cache writes per compaction, and thinking blocks dropped;
  - re-reads of elided output (`read_file` or `grep_text` on `context/elided/` or `spill/`);
  - refused copies of the marker.

### Kernel changes

The strategy owns all the policy, so the kernel changes stay small:

1. **Overflow retry** (`model_call.rs:144-146`, `mod.rs:391-394`). Compact once, then send again with only the view recomputed: no new reminders, no second `before_model` run. Only an overflow before any output streamed is retried.
2. **The `compact` signature** (`context.rs:73-81`).
   - It takes `{ reason: Manual | Overflow, instructions }` and returns `{ changed: bool, message }`, so the kernel knows whether a retry makes sense. The message is shown only after `/compact`.
   - `PassthroughContext` returns `changed: false`.
3. **`ContextCx`** (`context.rs:50-56`) gains:
   - the agent's provider and request template (model, system blocks, tools, `max_tokens`, temperature, thinking budget), so a summary can replay the cached prefix. The template's model is the current one, so it follows `/model`;
   - the summarizer only when `[model.summarizer]` is set. Today it is always there and stays on the configured model (`builder.rs:97-102`). A summarizer `provider` without an `id` means that provider's default model;
   - `complete`, which runs a plugin's model call, emits `ModelRequest` and records the usage, so plugin calls count in `/cost` and the saved totals.

Nothing changes in `View`, `Budget` or `History`. Spill's safe file-creation helpers move to `ch_kernel::util`, so the two plugins share them.

## Invariants for 6.5

Each one holds for every view, checked over scripted sessions:

- [ ] Every `tool_use` has exactly one `tool_result` in the next message with the same id, and the reverse. No trim removes a block.
- [ ] No compaction is decided while a `tool_use` lacks its `tool_result`: `view` runs only after every result of the step is in history.
- [ ] The view starts with a user message, roles alternate, and the last message is the latest history message, unchanged.
- [ ] The first history message's blocks are a byte-identical prefix of the view's first message.
- [ ] The recent window is byte-identical to the history, except for thinking blocks dropped by the rule.
- [ ] Between two compactions, each request is a prefix of the next (`next[..prev.len()] == prev`). *Changed in the implementation:* this was stated for views, but the session adds a call's reminders to the latest message after the view, so the latest message of one view is a prefix of that message in the next, and the requests are what extend each other.
- [ ] Rendering from a reloaded history and log, as `--continue` builds them, gives the same bytes.
- [ ] Thresholds:
  - no trim at or below `soft`;
  - no trim that frees less than `min_free`;
  - after a trim, the view is at or below `trim_to`, or the middle has no candidates left;
  - no summary at or below `hard`, except on overflow or `/compact`.
- [ ] User text stays byte-identical, in place or in a `<user-messages>` block, after any number of compactions.
- [ ] Outside the recent window, each tool in `superseded_tools` has at most one un-shortened call, the latest. The latest plan survives a summary verbatim.
- [ ] No thinking block appears after the first edit of a compaction that it predates. Blocks written after the last compaction are all present.
- [ ] Every stub's path exists and can be read with `read_file`, and the file holds the original text (or, for a spill file, the full output).
- [ ] A summary's text never changes once written, and summaries appear in the order they were written.
- [ ] A long scripted session never overflows under `staged`. Under `none` it ends with `ContextOverflow`, and `ch -p` exits 3.
- [ ] An overflow compacts and retries at most once per step; a second overflow ends the turn.
- [ ] An overflow retry adds no block to history: each reminder and note of the step is stored exactly once.
- [ ] A log record with a missing event or a wrong hash is never applied, and neither is any record after it. A record the model answered after that no longer matches leads to a reset record.
- [ ] A call with an input string that is an elision marker is refused; one whose input only mentions the marker runs.

## Consequences

- **Most steps send an append-only view, and the cache holds.** A compaction costs one cache rewrite and, on Claude models with signed thinking, the reasoning of the steps it covers, including the newest one.
- **Nothing is lost on disk.** The full history stays in `<id>.jsonl`, and the model can read back any elided output through the session directory (ADR 0002's read exception, no new tool).
- **Summaries lose detail.** User text and the latest plan never pass through the summarizer.
- **Disk use grows.** Elided copies about double the space of large results that spill didn't already save. They are bounded by the history and live in the session directory; session cleanup is still future work.
- **With the 1M default window, most sessions never compact.** The settings matter mainly for 200k models and long autonomous runs.
- **Roadmap 6.4 and architecture §8 change:**
  - summaries are appended, not updated;
  - every user message survives, not only mid-turn ones;
  - `compact_at_percent` is dropped;
  - the stub names a path instead of saying "re-read or re-run".

## Alternatives considered

- **Trimming a little every step to stay just under `soft`.** Rejected: every step would rewrite the cache from the trim point.
- **One running summary, updated in place** (architecture §8's "updated incrementally"). Rejected: each rewrite loses detail from the last, and its bytes change every time.
- **A recall tool for elided output.** Rejected, as in ADR 0002: the file tools already read the session directory, and a recall tool would rarely be called.
- **Dropping old steps without a summary.** The cheapest option, but the model loses track of what it did. It survives here only as elision of bulky content.
- **A cheap summarizer model by default.** Rejected as the default: it can't use the agent's cache, and a weak summary drops instructions and constraints.
- **Anthropic's server-side context management now.** Deferred to later work (see Signed thinking blocks): this ADR keeps one generic, testable path.

## Implementation notes

Where the `context` plugin differs from the decision above, and why. Trimming is implemented; summaries are not yet.

- **The record doesn't store the thinking range.** It is the span after the first edit (the first of the record's sorted edits) through `at_event`, both of which are stored.
- **Records are bound to their history.** Event ids are reused after a crash, so a record also stores `at_blocks`, how many blocks `at_event` had when it was decided, and a hash of those blocks. Only those count, because the session adds the call's reminders to that message after the view. A record whose `at_event` doesn't match is not applied.
- **The log is applied up to its first record that doesn't match**, and never past it:
  - If the model never answered after that record (the log is ahead of the saved history after a crash, or its last line was torn while being written), nothing in the history depends on it. It and every later record are dropped, and the log is rewritten without them (a new file, made durable, renamed over the old one), so they can't come back once event ids are reused.
  - Otherwise a view that was sent can't be rendered again: the history was edited, or the log was damaged. The records from that one on are dropped, and a **reset record** takes their place. It drops every thinking block through the newest event, which is the API's documented recovery, and the log keeps it, so later requests send the same stripped history.
- **A trim takes effect only once its record is durable.** With a session directory, the copies of the trimmed results are saved first, then the record is appended and synced; if that fails, the view goes out unchanged, with a warning.
- **A record's edits are checked against the planner's own rules on load**, with one shared predicate: never in the first message, before `at_event`, a superseded call's input is exactly its marker, an input keeps its shape with only long strings replaced by markers for their length, a result's replacement is a stub, and a reminder is a `<system-reminder>` text block of a user message that isn't a turn's prompt, replaced by `[reminder elided]`.
- **A spilled result's stub** says it shows "a preview of N lines, M characters" and gives the size of the spill file, read when the trim is made, and it keeps spill's notes on long lines and NUL bytes. A spill file is used only if its name starts with the result's call id, as spill names them, and it starts with the result's head.
- **A result whose stub would be longer than the result is left as it is.** That happens just above `elide_min_chars` when the session path is long. Superseded calls are always replaced, so their invariant holds.
- **The trim's projection counts the thinking it drops**, so a trim above `soft` stops near `trim_to`. Once there, it also takes every other candidate between its first and last edit: they cost no further cache rewrite, and later trims then rarely reach back before earlier ones.
- **The `pre_tool` hook** also refuses an input string that contains a result stub or an input marker this session's trims wrote. Those carry paths and sizes, so text that only describes them, such as this ADR, isn't refused.
- **`none` still renders the log**, so a session trimmed before keeps sending the same bytes; it makes no new trims. Disabling the plugin instead sends the raw history, which new accounts reject for such a session.
- **`/compact` under `elide`** trims the middle with the recent window cut to `min_recent_steps`, as for a summary, and ignores the instructions, which are for summaries.
- **Until summaries are written**, `elide` is the default, and `summarize` and `staged` act as `elide`, with a warning at startup. `hard`, `summary_max_words` and `max_summary_share` are checked now and take effect then.
