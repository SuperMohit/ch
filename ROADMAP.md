# Roadmap

`ch` is a plugin-based CLI coding agent in Rust. The design is in [docs/architecture.md](docs/architecture.md).

**Goals, in priority order:**
1. **A command-line coding agent you use day to day**: interactive REPL, streaming output, approvals, sessions.
2. **Everything is a plugin.** The core is a small kernel (the agent loop, an event bus and a plugin registry). Every model provider, tool, safety check, context strategy, planner, frontend and telemetry sink is a plugin, including the built-in ones.
3. **Every harness component can be switched and measured.** Planning, action space (structured tools vs bash-only) and context management ship as plugins, so comparing them is "enable or disable plugin X" on your own models and tasks.

Status legend: `[ ]` todo · `[~]` in progress · `[x]` done

---

## Milestones

| Milestone | You get | Phases | Status |
|---|---|---|---|
| **M1 — Usable CLI agent** | `ch` REPL with streaming, the file/bash tools, approvals, slash commands, Ctrl-C to interrupt, sessions with resume, and `ch -p` headless mode. All of it built as plugins on the kernel | 0–5 | Done except 5.2 expandable output and 5.9–5.11 |
| **M2 — Long-session harness** | Planning, staged context management, stuck detection and post-edit diagnostics as plugins. Long sessions never overflow the context window | 6 | In progress: 6.1 done |
| **M3 — Open to outside plugins** | External JSON-RPC plugins, MCP tools, shell hooks, markdown commands, and a `ch plugins` command to manage them | 7 | Not started |
| **M4 — Observable & measurable** | Langfuse telemetry, an eval runner and ablation matrices | 8–9 | Not started |
| **M5 — Polish** | Docker executor, TUI frontend, per-model tuned profiles | 10 | Not started |

---

## Phases and tasks

### Phase 0 — Scaffolding
- [x] **0.1** Create the Cargo workspace and the crate skeleton from [architecture §6](docs/architecture.md#6-crate-layout) (empty plugin crates are fine).
- [x] **0.2** `rustfmt.toml`, clippy with `-D warnings`, `justfile` (`fmt`, `lint`, `test`, `run`).
- [x] **0.3** CI (GitHub Actions): fmt, clippy, test.
- [x] **0.4** `tracing` set up with the level from `CH_LOG`, writing to `~/.ch/logs/` so logs don't mix with REPL output.

### Phase 1 — Kernel (`ch-kernel`)
- [x] **1.1** Conversation types: `Message`, `ContentBlock` (Text / ToolUse / ToolResult / Thinking), `Role`. Kernel types are independent of any provider.
- [x] **1.2** `History` with stable event IDs (`Event { id, turn, message, tokens_est }`). It stores the full log; context strategies produce a *view* of it.
- [x] **1.3** Extension traits from [architecture §2](docs/architecture.md#2-extension-points): `ModelProvider`, `Tool`, `Hook`, `PromptContributor`, `ContextStrategy`, `Command`, `EventSink`, `Executor`, `Approver`, `Frontend`, plus the `Plugin`/`PluginManifest` traits with `api_version`.
- [x] **1.4** `Registry` and `RegisterCx`. Multi-slot points (tools, hooks, …) are ordered by priority. Single-slot points (provider, context, executor, approver, frontend) are chosen by config. Typed `Services` map (`provide`/`get::<T>()`). Validation errors on missing `requires`, duplicate tool names and API version mismatches.
- [x] **1.5** Hook model: `HookPoint` (session_start, user_prompt, before_model, after_model, pre_tool, post_tool, turn_end, session_end) and `HookDecision` (Continue, Deny(reason), Ask, Rewrite(input), AppendNote(text), Stop(reason)). Document how multiple hooks' decisions combine (the first Deny wins, notes are concatenated).
- [x] **1.6** `AgentEvent` stream (tokio broadcast): TurnStart, TextDelta, ThinkingDelta, ToolCallStart, ToolCallEnd, ApprovalRequested, Note, ContextCompacted, Usage, TurnEnd, SessionEnd. Frontends and sinks subscribe to it.
- [x] **1.7** `Session` and turn pipeline ([architecture §1](docs/architecture.md#1-kernel-and-plugins)): multi-turn conversation, each turn running until the model stops calling tools or a step limit. A `CancellationToken` covers the model stream and tool calls. Each tool call is wrapped so errors and panics become `is_error` observations, and unknown tools get "available tools are …". Read-only tools run concurrently (semaphore = 8).
- [x] **1.8** Config system: layered TOML merge ([architecture §5](docs/architecture.md#5-config-and-file-locations)), per-plugin config sections deserialized through `cx.config::<T>()`, profiles, CLI overrides (`--set plugins.context.strategy=elide`).
- [x] **1.9** Token estimator (chars ÷ ~3.5), corrected each turn with the provider's reported usage. `TerminationReason` enum.
- [x] **1.10** Kernel tests with **fake plugins** (scripted provider, echo tool, recording hook and sink): hook ordering, decision merging, cancellation mid-tool, an unknown-tool error, a panicking tool.

### Phase 2 — Model provider plugins
- [x] **2.1** `provider-anthropic`: Messages API with **SSE streaming**, assembling text, thinking and tool-call deltas from the stream; maps to and from kernel types; retries with backoff on 429/5xx/overload; maps a context-overflow error to `ContextOverflow`.
- [x] **2.2** `provider-openai`: OpenAI-compatible chat-completions with streaming and `tool_calls` delta assembly (covers OpenAI, OpenRouter, vLLM, SGLang).
- [x] **2.3** Usage and cost: providers report token counts, and prices come from config (`[model.pricing]`).
- [x] **2.4** `provider-scripted` (test only): replays recorded responses so e2e tests are deterministic.
- [x] **2.5** Live tests (`#[ignore]`, need an API key): one streamed tool-use round trip per provider.

### Phase 3 — Tool, safety and executor plugins
- [x] **3.1** `executor-local`: `/bin/sh -c` in a new process group, timeout plus cancellation kills the group, keeps partial output.
- [x] **3.2** `safety` plugin provides the services `PathGuard` (canonicalize, reject paths outside the workspace and symlink escapes) and `FileState` (`sha256`, `full_read`, external-change detection). It also adds a `pre_tool` hook with the permission policy: allow/ask/deny rules from config, the built-in bash deny list and read-only allowlist, and `Ask` for sensitive paths.
- [x] **3.3** `tools-fs`: `read_file`, `edit_file`, `write_file`, `list_files`, `glob_files`, `grep_text`, following the tool table in [architecture §8](docs/architecture.md#8-design-defaults) (descriptions in When / Protocol / Error modes / Side effects form). Read-before-write goes through the `FileState` service, and edits return a unified diff.
- [x] **3.4** `tool-bash`: uses the active `Executor`; nonzero exit sets `is_error`. Two description variants, chosen by whether `tools-fs` is enabled (bash-only mode tells the model nothing validates its edits).
- [x] **3.5** `tool-web-fetch`: http(s) only, size cap, HTML → markdown, HTTP errors become recoverable errors.
- [x] **3.6** Shared helpers: output truncation (24k chars, keep head and tail), binary detection, ignore patterns.
- [x] **3.7** **Tool conformance test suite**, run against every Tool plugin (built-in and, later, external): valid JSON schema, a description in the required format, correct read-only flag, errors returned as recoverable observations.
- [x] **3.8** Unit tests per tool on `tempfile` workspaces covering every error mode, plus safety tests (path escape, symlink escape, unread edit, file changed on disk since the read, file changed through bash).

### Phase 4 — Prompt and command plugins
- [x] **4.1** `prompt-base`: base system prompt (`prompts/system.md`, written for interactive use) and a tool-rules block generated from the active tools, so it stays correct whichever tools are enabled. A bash-only variant when the file tools are off.
- [x] **4.2** `project-instructions`: loads `CH.md` / `AGENTS.md` (repo root up to the git root) as a system block, plus environment info (cwd, OS, git branch and status summary, date).
- [x] **4.3** `commands-core`: `/help`, `/clear`, `/compact`, `/model`, `/cost`, `/tools`, `/plugins`, `/config`, `/resume`, `/exit`.
- [x] **4.4** `commands-markdown`: `.ch/commands/<name>.md` become `/name` commands (a prompt template with `$ARGUMENTS`).

### Phase 5 — Frontends & CLI → **Milestone M1**
- [x] **5.1** `ch-cli` binary: parse args (clap), load config, discover and register plugins, validate the registry, start the selected frontend. `ch --list-plugins` shows what's loaded and why.
- [~] **5.2** `frontend-repl`:
  - reedline prompt with history and multi-line input
  - text streamed as it arrives, rendered as markdown once a block completes
  - spinner while waiting
  - compact tool-call lines (`● read_file src/main.rs`) with expandable output *(expandable output not done yet)*
  - colored diffs for edits
- [x] **5.3** Interactive `Approver` in the REPL: shows the command or diff with `[y]es / [n]o / [a]lways for this session`. Rules approved with "always" persist to `.ch/config.toml` only if you confirm.
- [x] **5.4** Interrupts: Esc or Ctrl-C cancels the current turn and returns to the prompt with history kept; Ctrl-D (or Ctrl-C twice) exits; the cancellation token is wired through.
- [x] **5.5** Sessions: auto-save to JSONL every turn; `ch --continue` resumes the last session in this project and `ch --resume` picks from a list.
- [x] **5.6** `frontend-print`: `ch -p "task"` runs non-interactively with the `auto-deny` approver by default (or `--allow "<rules>"`), with `--output-format text|json|stream-json`. The exit code reflects the termination reason. Eval and CI use this.
- [x] **5.7** End-to-end tests: `provider-scripted` + `frontend-print` against a fixture repo (read → edit → test → done). Snapshot the assembled prompts with `insta`.
- [x] **5.8** Live smoke test: a real model fixes a failing test in a fixture repo using `ch -p`.
- [ ] **5.9** Sanitize tool output before it reaches the terminal. Error first lines and edit diffs are printed as they are ([format.rs](plugins/frontend-repl/src/format.rs) `tool_end_lines`), so a file or command output holding escape sequences can retitle the window, write the clipboard (OSC 52) or rewrite earlier lines. One helper that strips or escapes C0/C1 control characters (keeping `\n` and `\t`), applied on every path that prints tool content, including `stream-json` stderr in `frontend-print`. Test with OSC, CSI and bare `\r` payloads.
- [ ] **5.10** Small REPL fixes:
  - one terminal bell when an approval is waiting
  - Ctrl-C at a non-empty prompt clears the line before it counts toward exiting
  - Esc must be pressed twice within one second to interrupt, so a stray key doesn't stop a long turn (Ctrl-C still interrupts at once)
- [ ] **5.11** Record the provider and `base_url` in `SessionMeta`. Resuming a session against a different endpoint asks first in the REPL and fails in `-p`, so a conversation is never sent somewhere other than where it started.

**Done when (M1):** you can use `ch` daily on a real repo, and removing any built-in plugin from the config still leaves a working binary (for example, no web-fetch means no `web_fetch` tool and nothing breaks).

### Phase 6 — Harness plugins → **Milestone M2**
- [x] **6.1** `planning`: `update_plan` tool (full-list replace, exactly one `in_progress`), `PlanStore` service, system block (`prompts/planning.md`), first-turn reminder until a plan exists, fresh plan reminder every turn, never stored in history. The REPL shows a todo panel by subscribing to plan events.
- [ ] **6.2** `stuck`: `post_tool` hook. Streaks are counted over identical tool + canonical arguments, and permission denials don't count. A one-time reminder at 5 calls (`prompts/stuck-*.md`), and `Stop(Stuck)` at 8 identical failures.
  - Also a `pre_tool` check: the third identical call that already failed twice is refused before it runs, with the earlier error repeated so the model changes approach. Measure against the 5/8 thresholds in `ch-eval` before settling the numbers.
  - Three steps in a row whose tool calls all have malformed input (unparseable JSON, schema mismatch) end the turn. Check that a malformed call stored in history is still accepted by the provider on the next request.
- [ ] **6.3** `diagnostics`: `post_tool` hook on successful edits and writes. A per-extension command from config (`.py` → ruff, `.ts` → configurable, `.rs` → `rustfmt --check` by default) with a short timeout; findings are appended as a note.
- [ ] **6.4** `context`: `ContextStrategy` with region split (preamble | middle | recent ≥ 0.30 × window, at least 2 turns), per [architecture §8](docs/architecture.md#8-design-defaults).
  - Elision stubs above `soft`.
  - Summaries above `hard`, using the summarizer model and `prompts/summary.md`, inserted as `<context-summary>` and updated incrementally.
  - Strategies `none`, `elide`, `summarize` and `staged` selected by config.
  - Elided originals written to the session directory.
  - `/compact` triggers a summary manually.
  - When the provider rejects a request as too long (`ContextOverflow`), compact once and retry the step before ending the turn with exit code 3.
  - The compaction trigger is configurable as a share of the window (`plugins.context.compact_at_percent`, 10–80) on top of the token thresholds.
  - Messages the user sent during the turn (6.7) and the current plan survive compaction unchanged.
  - Newer Claude models reject history edits that come before a signed thinking block ("invalid signature … bound to a different conversation"). Elision and summaries must leave everything up to the last signed thinking block untouched, or strip those blocks consistently.
- [ ] **6.5** Tests: thresholds trigger correctly, the preamble and recent window stay byte-identical, tool_use/tool_result pairs stay valid, a long scripted session never overflows under `staged`, `none` ends with `ContextOverflow`, planning never appears twice in a prompt, stuck thresholds behave as specified.
- [ ] **6.6** Keep truncated tool output. Results over `tool_result_max_chars` are cut with `truncate_middle` and the middle is lost. Write the full output to the session's `aux_dir`, end the truncated result with a handle (`[full output: result 12, 184,302 chars — read_tool_result]`), and add a read-only `read_tool_result` tool (handle, offset, limit) that works after resume. Shares the storage with 6.4's elided originals. `--no-save` keeps it in memory until exit.
- [ ] **6.7** Mid-turn messages. A `SteerHandle` beside `CancelHandle` queues user text while a turn runs; the session adds it as a user message at the next model call (after pending tool results), emits it as an event and saves it to history. In the REPL, Enter during a turn sends a message instead of being ignored. A message queued when the turn ends starts the next turn. Cancelling keeps messages already added.

### Phase 7 — Plugins from outside the binary → **Milestone M3**
- [ ] **7.1** `ch-plugin-protocol`: JSON-RPC 2.0 over stdio with newline-delimited framing and a versioned protocol.
  - Host → plugin: `initialize` (the plugin returns its capabilities: tools, hooks with matchers, commands, prompt contributions, event subscriptions), `tool/call`, `hook/invoke`, `prompt/system`, `prompt/turn`, `command/run`, `event/notify`, `shutdown`.
  - Plugin → host: `log` and `ui/notify`.
- [ ] **7.2** `external-host` plugin: reads `plugin.toml` manifests, spawns plugin processes lazily, and wraps their capabilities as kernel Tool/Hook/Command/PromptContributor/EventSink adapters. Per-call timeouts. If a plugin crashes, disable it and warn; never crash the agent.
- [ ] **7.3** `ch-plugin-sdk` (Rust) and a Python example in `examples/plugins/`, for example a `todo-scanner` tool and a `block-secrets` pre_tool hook.
- [ ] **7.4** `mcp-bridge`: connects to configured MCP servers (stdio and streamable HTTP) with `rmcp` and registers their tools as `mcp__<server>__<tool>`.
  - Servers from a project's `.mcp.json` or `.ch/config.toml` stay off until you trust them, through the same trust store as project config, and are asked about again when their command, URL or env changes.
  - Servers connect independently: one that fails or hangs at startup is reported with its reason (exited, timed out, invalid output) and doesn't block the others.
  - Secrets in MCP error messages (tokens, `Authorization` values, env values) are redacted before they reach the model, the terminal or logs.
- [ ] **7.5** `hooks-shell`: hooks defined in config ([architecture §5](docs/architecture.md#5-config-and-file-locations)). The command gets the event JSON on stdin; exit code 0 means continue, exit code 2 means deny with stderr as the reason, or the command can return a JSON `HookDecision`.
- [ ] **7.6** `ch plugins list|info|enable|disable|install <path|git-url>` and a trust prompt for project-level plugins and hooks.
- [ ] **7.7** Run the conformance suite (3.7) against the example external plugins and an MCP test server.
- [ ] **7.8** *(optional, off by default)* Per-turn tool and skill router backed by a decision model (Cloudflare Clef or TypeSafe Jev, which share one API). On each user turn it asks "is a skill or extra tool needed?" and "which one?" over the catalog's names and descriptions. High confidence injects the chosen skill or exposes the chosen tools, a middle score lists the top candidates for the main model to load, and low confidence adds nothing. A timeout or error falls back to listing every description, so routing never blocks a turn. Worth doing only once MCP servers (7.4) make the catalog large; compare it in `ch-eval` (wrong-load rate, tokens saved, added latency) before turning it on by default.

### Phase 8 — Telemetry plugins (Langfuse) → **Milestone M4, part 1**
- [ ] **8.0** Spike: send one trace to Langfuse through the OTLP HTTP exporter (`{LANGFUSE_HOST}/api/public/otel`, Basic auth with `public:secret`), and check which attributes become generations, usage, cost, input and output. If that doesn't work, fall back to a thin `reqwest` client for Langfuse's ingestion API.
- [ ] **8.1** `telemetry-langfuse` EventSink. Mapping:
  - **Trace** = one user turn (or the whole session in `-p` mode), grouped by `session_id`.
  - **Tags** = active plugins and profile.
  - **Metadata** = resolved config.
  - **Generations** = model calls, including `context.summarize`.
  - **Spans** = tool calls (arguments, truncated result, error flag, duration, permission decision).
  - **Events** = elision, plan update, stuck reminder, diagnostics, hook denial.
- [ ] **8.2** Keys come from `LANGFUSE_PUBLIC_KEY`, `LANGFUSE_SECRET_KEY` and `LANGFUSE_HOST`. Sending is asynchronous and batched, and flushed on exit and Ctrl-C. A `redact_file_contents` option. A telemetry failure never breaks the agent.
- [ ] **8.3** `telemetry-jsonl` EventSink for offline traces, plus `ch replay <session>`.

### Phase 9 — Evals → **Milestone M4**
- [ ] **9.1** Task format `{ id, repo|image, instruction, verify_cmd }`; success means `verify_cmd` exits 0. Each run uses `ch -p` in a fresh copy of the repo.
- [ ] **9.2** A local suite of 10–20 fixture tasks (bug fixes, small features, CLI chores).
- [ ] **9.3** `ch-eval matrix --suite X --profile p --vary planning=on,off --vary context.strategy=none,staged --vary tools-fs=on,off`. Each cell is just a different plugin config.
- [ ] **9.4** Report with success rate, mean cost and turns per cell, and paired win/loss counts between configs. Results are written to Langfuse as `success` scores; optionally use Langfuse Datasets and Dataset Runs.
- [ ] **9.5** Benchmark adapters: a SWE-Bench Verified subset and Terminal-Bench (Harbor). Spike first to see how Harbor runs third-party agent binaries.

### Phase 10 — Polish → **Milestone M5**
- [ ] **10.1** `executor-docker`: bash runs in a container with the workspace mounted.
- [ ] **10.2** `frontend-tui` (ratatui): split panes for the conversation, plan, and tool output.
- [ ] **10.3** Tuned profiles: run the ablation matrix on your target models and set defaults per profile (planning, tools vs bash-only, context strategy).
- [ ] **10.4** Optional: a WASM plugin tier (wasmtime + WIT) for sandboxed in-process plugins.
- [ ] **10.5** Docs: `docs/plugins.md` (writing built-in and external plugins, protocol reference, hook semantics) and a README.
- [ ] **10.6** Long-running processes: a `bash_background` tool with `start`, `read` (new output since the last read) and `stop`, for dev servers and watchers that `bash` would kill at its timeout. Processes run through the `Executor`, go through the same safety checks as `bash`, are listed by a new `/tasks` command, and are killed when the session ends. A cap on live processes.
- [ ] **10.7** Images: an image content block in the kernel, `read_file` returning PNG/JPEG/GIF/WebP to models that support vision, images in tool results for both providers, and a size limit with downscaling. `ToolResult.content` is a `String` today, so this changes the kernel types and the session format.

## Open questions

1. **Name:** `ch` is a placeholder for the binary and config directory.
2. **First models:** which provider and models go in the default profile?
3. **External plugin trust:** is a trust prompt enough, or do external plugins need sandboxing (Docker or WASM) in v1?
4. ~~**Project config trust**~~ Answered in [ADR 0001](docs/adr/0001-sandbox-and-agent-security-responsibilities.md): settings that could loosen permissions, run commands, read paths or pick endpoints and credentials need the user's trust, recorded per workspace for the exact values; a project's `deny`/`ask` rules are added to the user's. Project plugins (7.6) should reuse the same trust store.
5. **Toolchain:** `reedline` is pinned to 0.49 because 0.50+ needs rustc 1.95. Unpin once the toolchain moves.
