# ADR 0006: Tools from MCP servers and external plugins, added and removed during a session

- **Status:** Accepted
- **Date:** 2026-11-13
- **Related:** [roadmap](../../ROADMAP.md) Phase 7 (7.1–7.7), [ADR 0001](0001-sandbox-and-agent-security-responsibilities.md) (project config trust), [ADR 0004](0004-context-management.md) (append-only history, signed thinking), [architecture §3](../architecture.md#3-three-ways-to-extend) and [§5](../architecture.md#5-config-and-file-locations)

Code references are to `5dfa0a5` (`main`). ADR 0005 is taken by the eval runner (`feat/eval-runner`).

## Context

Phase 7 brings in tools that ch doesn't compile: MCP servers (7.4) and external JSON-RPC plugins (7.1–7.3). Both are separate processes that report what they offer only once they run: an MCP server answers `tools/list`, an external plugin answers `initialize`. A server or plugin added while a session runs must be usable in that session, without restarting `ch`.

### What exists

- **Registration is synchronous and happens once.** `Plugin::register(&self, &mut RegisterCx)` returns `anyhow::Result<()>` (`plugin.rs:85-88`), and the registry calls it for each plugin before any session exists (`registry.rs:175-186`). `ToolSpec::validate` runs only there (`registry.rs:213-218`), and one invalid tool fails startup.
- **The tool set is fixed when the session is built.** The builder copies the registry's tools into a name map and an `Arc<Vec<ToolSpec>>` (`session/builder.rs:119-124`). Every request sends that list (`registry.rs:363-365`, `model.rs:20`), and so does the OpenAI provider (`provider-openai/src/request.rs:39-42`).
- **The system prompt is re-rendered for every model call** (`session/model_call.rs:25-36`, `70`), and `prompt-base` renders it from the tool list (`prompt-base/src/lib.rs:139`, `rules.rs:62-106`).
- **A model call stores its additions before it is sent.** Reminders and `before_model` notes are appended to the newest user message in history (`model_call.rs:119-121`) and then sent (`:123`). A `before_model` Deny or Stop returns earlier (`:103-108`). A send that fails, is cancelled or gives up after an overflow ends the turn with that user message last, and the next turn pushes another user message (`mod.rs:400-404`).
- **History has only user and assistant messages** (`types.rs:8-11`). The Anthropic provider renders them one to one and puts the cache breakpoint on the last block of the last message (`provider-anthropic/src/request.rs:126-169`). It sends some options only to `api.anthropic.com` (`provider-anthropic/src/lib.rs:85-92`).
- **The session is saved as one file** with an atomic rename (`store.rs:150-160`), holding `SessionSnapshot { meta, history, usage, cost }` (`store.rs:63-68`).
- **Safety checks are chosen by tool name.** `bash`, the file tools and `web_fetch` get their own checks; everything else goes to `decide_other` (`plugins/safety/src/policy.rs:136-148`). That allows a tool whose `read_only` is true, or `update_plan`, without asking (`:340-346`, `:30`). It matches rules against the first of the input's `path`, `command` or `url` (`:332-335`), and for tools other than file tools `*` in a rule also matches `/` (`rules.rs:20-22`). `is_sensitive` covers `.git`, `.env*` and `.ch/config.toml` (`policy.rs:357-372`).
- **Trust.** The trust store keeps one record per workspace, the hash of the project's restricted settings, and trusting replaces it (`crates/ch-cli/src/trust.rs:271-276`). The prompt runs once, before the frontend starts. `--trust-project` applies every restricted setting without a prompt (`crates/ch-cli/src/config.rs:115-128`).
- **Exits.** `main.rs:9` and the print frontend (`frontend-print/src/lib.rs:121`) leave through `process::exit`, so destructors don't run.
- **Conformance.** `assert_spec_conforms`, documented for outside tools, requires snake_case names and the built-in description sections (`ch-testkit/src/lib.rs:38-60`).
- `ch-plugin-protocol`, `ch-plugin-sdk`, `external-host`, `mcp-bridge` and `hooks-shell` are empty crates, and their lines in `crates/ch-cli/src/plugins.rs:61-68` are commented out.

### Constraints from the model API

1. **The tool list and the system prompt are part of what signed thinking is checked against.** On Claude Opus 5.5 (the default), Sonnet 5.5, Haiku 5.5, Fable 5.1/5.2 and Mythos 5.1/5.2, a thinking block's signature records the system prompt, the tool set and every earlier message. Editing, removing or reordering an entry in `tools`, or changing `system`, invalidates every earlier thinking block. On enforced accounts, replaying them is a 400. That covers every account on Fable 5.2 and Mythos 5.2, accounts created on or after 2026-08-31 on the others, and any request that sets `prefix_mismatch_behavior` (`provider-anthropic/src/request.rs:77`).
2. **Tools render first in the prompt cache,** so any change to `tools` makes the whole prefix a cache write.

The API has an append-only way to change tools between turns, behind the beta `mid-conversation-tool-changes-2026-07-01`. It covers Claude Opus 5, Opus 5.5, Opus 4.8, Fable 5/5.1/5.2, Mythos, Sonnet 5.5 and Haiku 5.5. It doesn't cover Sonnet 5, and it isn't on Microsoft Foundry.

- **Add:** append the tool to the end of `tools` with `"defer_loading": true`, and announce it with a `tool_addition` block in a `{"role": "system"}` message appended to `messages`. A deferred tool that nothing has referenced yet is outside the checked prefix, so appending it is safe. The system message must follow a user message.
- **Remove:** leave the definition in `tools` and announce a `tool_removal`. It must sit immediately before an assistant message or last in `messages`.
- **A system message** must be last or followed by an assistant message.
- **Change a tool under the same name:** this beta can't. `inline-tools-2026-09-15` can, on the Claude API, Claude Platform on AWS and Anthropic-managed Google Cloud. Without it, each tool's description and schema stay byte-for-byte the same for the conversation.

## Decision

### 1. Tool sources start with the session, concurrently

The kernel gets a new extension point for plugins whose tools are known only at run time:

```rust
#[async_trait]
pub trait ToolSource: Send + Sync {
    fn id(&self) -> &str;
    /// Connects or spawns, then reports tools through `sink`, now and whenever they change.
    /// Returns once the first list is reported, or with the reason it couldn't be.
    async fn start(&self, sink: ToolSink, cx: &Cx) -> anyhow::Result<()>;
    async fn stop(&self);
}
```

- A plugin adds sources in `register` (`cx.add_tool_source(..)`), so registration stays synchronous.
- **Startup.** When a session starts, the kernel starts every source concurrently and waits for each until it reports its first list, fails, or reaches its startup timeout: 10 s by default, `startup_timeout_secs` per server or plugin.
- **Failures.** A failure is reported with its reason (exited with the tail of its stderr, timed out, invalid output) and holds up neither the others nor the session.
- **Slow starts.** A source that times out keeps starting in the background, for up to 60 s or its startup timeout if that is longer. Its tools then arrive like any other mid-session addition (§3). A `start` that panics counts as failed.
- **Restarts.** Each start of a source has a generation. Reports from an earlier start, or from a start that was stopped, are ignored, so a stale instance (a process noticing its closed pipe after a reconnect) can't change its replacement or turn "stopped" into "exited". A source removed and added again keeps its place in the precedence order.
- **Order.** Precedence between sources follows config order, never arrival order, so names resolve the same way every run.
- **No lazy start.** Tools have to be known before the model can call them, and declaring them again in `plugin.toml` would be a copy that drifts. Roadmap 7.2 is updated to say so.

### 2. Names, validation and limits

`ToolSink` admits a tool only after these checks. A tool that fails one is skipped with a warning; the rest of its source still loads.

- **Every outside tool is namespaced:**
  - MCP tools are named `mcp__<server>__<tool>`.
  - External plugin tools are named `ext__<plugin>__<tool>`.
  - Server and plugin names must match `[a-z0-9-]{1,24}`.
  - The tool part keeps `[A-Za-z0-9_-]` and turns every other character into `_`.
  - A name longer than 64 characters is cut and given an 8-hex-digit hash of the original. The source keeps a map back to the original name.
  - The prefixes `mcp__` and `ext__` are reserved for outside tools.
- **The `read_only` flag is forced off** unless the user's config lists the tool by its exact name (§8).
- **The spec must pass `ToolSpec::validate`.** A schema without `"type": "object"` is refused, not repaired.
- **Limits:**
  - 128 tools per source.
  - 4 KB per description; longer descriptions are cut deterministically.
  - 512 entries in a conversation's ledger. Additions past that are queued until `/clear`, with a warning.

### 3. Each conversation keeps an append-only tool ledger

The session replaces its fixed list with a **ledger**. The ledger holds every tool spec sent in this conversation, in the order first sent. Each entry records its source, whether it was in the first request, and whether it is active.

- **Stored specs are frozen.** A spec is stored as first sent and is never edited, removed or reordered.
- **Withdrawn tools stay in the ledger.** A call to one returns an error observation, for example: "`mcp__github__create_issue` is no longer available: the `github` server exited."
- **Same name, different spec** (a server re-lists after a reconnect): the first-sent spec stays for the conversation, and a warning says the new text applies after `/clear` or in a new session. With `Replace` (below), whose models don't check their reasoning, the new spec replaces the old one.
- **Re-adding a withdrawn tool** with an identical spec makes it active again.
- **Saving.** The ledger is part of `SessionSnapshot` and saved in the same atomic write as the history. A session file without it loads with a ledger built from the current tools.

`ModelRequest.tools` keeps its meaning: the active tools. The OpenAI provider, the scripted provider, `before_model` hooks and token estimates are unchanged. A new optional field, `ModelRequest.tool_ledger`, carries every entry with its `deferred` and `active` flags. Only a provider rendering changes natively reads it.

### 4. How a change reaches the model

Sources report changes at any time. The kernel queues them and nets them per tool (an addition followed by a removal is nothing). It applies them only as part of a model call:

- **When.** The pending changes are staged before the view is built, so staged deferred entries are in the request. They are committed to the ledger, and the queue drained, in the same step that stores the call's reminders in history (`model_call.rs:119-121`). A call that returns before that point keeps them pending.
- **History.** A new kernel block, `ContentBlock::ToolsChanged { added: Vec<String>, removed: Vec<String> }`, is stored on the newest user message along with the reminders.
- **What the provider does.** Each provider says how it handles a change, through a new `ModelProvider::tool_changes(model) -> ToolChanges`:
  - **`Native`**: the Anthropic provider, on `api.anthropic.com`, for a model with the new `tool_changes` flag on `ModelCaps` (`models.rs:18`). It sends the ledger with `defer_loading: true` on deferred entries and adds the beta.
    - For each run of consecutive user messages, it nets every `ToolsChanged` in the run into one `{"role": "system"}` message placed after the run's last user message. That position is immediately before the next assistant message, or last.
    - It never emits two system messages in a row. So a turn that failed after storing a change still produces a valid request.
    - The cache breakpoint goes on the last block of the user message before the system message.
  - **`Replace`**: providers and models that don't check thinking signatures (OpenAI, older Claude models). The provider sends the active tools and renders `ToolsChanged` as a short text note ("Tools now available: …; no longer available: …"). The cost is one cache rewrite.
  - **`Frozen`**: a model that checks thinking, where the beta isn't available (Foundry, Sonnet 5, a non-official base URL). Changes stay queued until `/clear` or a new session, and the user is told once which tools are waiting. Every definition the first request had is sent, withdrawn ones included, and a withdrawn tool fails when called.
    - A conversation that already announced changes natively and continues where they can't be (a `/model` switch, a resume through a proxy) can't be sent as its reasoning was written: tools added later can't be deferred there. The session warns once that `/clear` may be needed.
  - **Override.** The Anthropic provider's `tool_changes = "auto" | "on" | "off"` setting overrides the choice. `auto` is the default. `on` uses the beta through another base URL too, but only for models that accept it.
  - **Refusal fallback.** Claude Sonnet 5.5's server-side fallback answers some refusals with Claude Sonnet 5, which takes neither tool changes nor system messages, so a Sonnet 5.5 request that carries tool changes is sent without `fallbacks`.
- **Context strategies.** Under ADR 0004, a summary stores one netted `ToolsChanged` for the state at its last event, appended to the first message with the summary. `elide` never removes `ToolsChanged` blocks. The `[model.summarizer]` transcript renders them as a line of text.
- **Frontend.** It gets an `AgentEvent::ToolsChanged` and prints one line.

A turn already running sees new tools from its next model call. A withdrawal affects calls made after it. A call already running finishes or fails against its source.

### 5. The system prompt is rendered once per conversation

The kernel renders `system` at the conversation's first request and saves it in the snapshot. Every later request sends the saved text. `/clear` and a new session render it again. This replaces the per-call render (`model_call.rs:70`). Today's contributors already return the same text each time, so nothing they produce changes.

- `prompt-base` sees the tools of the first request: the ledger's non-deferred entries. Tools added later are announced by `ToolsChanged`.
- An external plugin that starts with the session contributes system blocks like a built-in one. A plugin that arrives later has its blocks added once as a reminder at the next model call.
- A resumed session keeps its saved system prompt.

### 6. Adding and removing plugins mid-session

- **`/mcp`** lists servers with their state (connected, starting, failed with reason, exited) and tool counts. `/mcp reconnect <server>` restarts one.
- **`/plugins reload`** re-reads `~/.ch/config.toml`, `.ch/config.toml`, `.mcp.json` and both plugin directories. It starts sources that are new or whose entry changed, and stops ones that were removed. So `ch plugins enable foo` or an edited `.mcp.json` in another terminal takes effect after `/plugins reload`. There is no file watching.
- **Tool list changes.** MCP `notifications/tools/list_changed` makes the bridge list that server's tools again. Changes are applied at the next model call, which also debounces a burst of notifications.
- **Exits.** A process that exits withdraws its tools, and the user is warned once. Nothing restarts automatically.
- **Hooks, commands and event sinks** from external plugins don't affect the prompt, so the session swaps its lists of them between turns, without a ledger.

### 7. Resume

A resumed session loads its ledger and system prompt, then starts its sources. Tools in the ledger that no source offers become withdrawn. Tools a source offers that aren't in the ledger become pending additions. Both reach the model through §4 at the first call. A spec that changed keeps its saved form.

A session file saved before ledgers existed counts only the built-in tools as its first request's, whether it is resumed with `ch -c` or with `/resume` while sources already run. Source tools are announced as additions, and its system prompt is rendered from the built-in tools.

### 8. Trust, approval and what a server can touch

**Approval**

- **Outside tools are never read-only by their own word.** An MCP server's `readOnlyHint` and an external plugin's own flag are ignored. The user's config can list exact tool names in `read_only_tools`; globs aren't accepted.
- **Safety recognises outside tools by their reserved prefix.** For them:
  - allow rules must be bare tool names or name globs (`mcp__github__*`), with no input pattern;
  - deny and ask patterns match against every string value in the input, at any depth;
  - a built-in name can never reach an outside tool's checks, or the other way round.
- **Bypass mode** allows every outside tool, as it does every other tool. The docs say so.
- **The approval prompt** for an outside tool shows a bounded key/value preview of the whole input, not the 120-character summary.

**Trust**

- **One trust record per server or plugin.** The trust store gets a record keyed by workspace, source file and name. It holds the hash of the whole entry: name, resolved absolute command, arguments, URL, headers, env and `read_only_tools`. Any change asks again. The record for `.ch/config.toml` settings stays separate, so trusting one never replaces the other.
- **Workspace executables.** An executable inside the workspace is hashed by content too.
- **Project sources.** Servers from `.mcp.json` or a project's `.ch/config.toml`, and plugins in `.ch/plugins/`, start only once trusted.
  - Each gets its own prompt at startup, showing the command, URL, env names and the variables it reads.
  - A user-level entry with the same name wins, and the project entry is refused with a warning.
- **User-level sources** in `~/.ch/config.toml` and `~/.ch/plugins/` are trusted.
- **Installed plugins.** `ch plugins install` leaves a plugin disabled until `ch plugins enable`.
- **Mid-session.** `/plugins reload` never starts an untrusted project entry. It lists them, and `/mcp trust <name>` trusts one after showing what it runs. That is a command the user types; it never goes through the `Approver`, so `--approver auto-allow` can't trust anything.
- **`ch -p`** never prompts. `--trust-project` doesn't start project servers or plugins; a separate `--trust-project-servers` does.

**Protected files**

- `.mcp.json` and everything under `.ch/plugins/` join `is_sensitive`, so writes to them always ask outside bypass mode.
- ADR 0001's list of paths to mount read-only in a sandbox gains them too.

**Environment**

- **What a child gets.** Children start with a cleared environment plus `PATH`, `HOME`, `USER`, `LANG`, `TERM`, `TMPDIR` and `SHELL`, plus their configured `env`. They don't get `ANTHROPIC_API_KEY` or other credentials ch holds.
- **Variables.** `${VAR}` is expanded only inside `env` and header values, and the trust prompt lists every variable an entry reads.
- **Working directory.** User-level servers start in the user's home directory unless they set `cwd = "workspace"`. They learn the workspace through MCP roots and `CH_WORKSPACE`. This keeps a repo's `.npmrc` or `uv.toml` away from a trusted `npx` or `uvx` command. Project servers start in the workspace.

**Untrusted text**

- **Secrets** are redacted from tool results, error messages and stderr before they reach the model, the terminal, logs or session files. That covers:
  - configured env and header values of 8 characters or more;
  - `Authorization` values;
  - bearer tokens;
  - credentials in URLs.
  
  Config types holding them use a `Secret` type whose `Debug` prints `[redacted]`.
- **Terminal text.** Server and plugin text shown in the terminal (names, `serverInfo`, stderr tails, error reasons, `/mcp` output) goes through `printable`.

### 9. Processes and calls

- **Process groups.** Each server or plugin runs in its own process group, so Ctrl-C at the terminal doesn't reach it.
- **Stopping.** Stopping reuses `executor-local`'s group handling: it closes stdin, sends SIGTERM to the group, then SIGKILL after 2 s. The session stops its sources when it ends, and a session dropped without ending aborts their start tasks and asks them to stop. `process::exit` (in `main.rs` and on the print frontend's second Ctrl-C) runs no destructors, so the process groups sources spawn are recorded in a process-wide registry that those exit paths kill first; it comes with the MCP bridge, which is the first source that spawns processes.
- **stderr** is drained continuously into a 64 KB ring buffer, so a chatty child never blocks.
- **Timeouts.** Each call has a timeout: 120 s by default, `call_timeout_secs` per server or plugin. A timeout or a cancelled turn sends the protocol's cancel notification and returns an error observation.
- **Exits.** A source that exits fails its pending calls at once.
- **External `pre_tool` hooks** fail closed by default (`on_error = "deny"`): a crashed `block-secrets` hook blocks calls rather than silently allowing them. Hooks at other points fail open.

### 10. MCP scope

- **Transports.** Stdio and streamable HTTP through `rmcp`. HTTP servers authenticate with static headers. OAuth is later work, and no request carrying custom headers follows a redirect to another origin.
- **Capabilities.** ch declares `roots` (the workspace) and nothing else: no sampling, which would spend the user's API key, and no elicitation. Resources and prompts are out of scope.
- **The server's `instructions` field** is ignored for now. It would be unreviewed text in the system prompt.
- **`tools/list` pagination** is followed for up to 10 pages.
- **Stdout.** A line that isn't a JSON-RPC message is logged and skipped. Messages over 16 MB fail the call.
- **Results.**
  - `isError: true` becomes an error observation, and so does a JSON-RPC error; both are redacted.
  - Text content is joined.
  - An image or audio item becomes `[image omitted: image/png, 48 KB]` until 10.7.
  - `structuredContent` is used as JSON text when there is no text content.
  - Oversized results go through `spill` like any other.

### 11. Conformance

7.7 uses a separate profile for outside tools: the reserved prefixes, the validation rules from §2, and no requirement for the built-in description sections.

## Consequences

- **New kernel pieces:**
  - `ToolSource` and `ToolSink`;
  - the ledger and `ModelRequest.tool_ledger`;
  - `ContentBlock::ToolsChanged` and `AgentEvent::ToolsChanged`;
  - `ModelProvider::tool_changes`;
  - a system prompt rendered once per conversation;
  - a registry of live sources that is stopped on every exit path.
- **Session files** gain the ledger and the system prompt. Older files load without them.
- **Built-in tools behave as before.** They are ledger entries present from the first request.
- **Cost of a mid-session addition.** Nothing on the official API with a supported model, and one cache rewrite on providers that don't check thinking. Where thinking is checked but the beta isn't available, additions wait for `/clear`.
- **Startup** is slower by up to the slowest source's startup, capped by its timeout.
- **Approvals.** Outside tools ask more often than a server's own hints suggest. Allow rules for them are coarser than for built-in tools.
- **Trust prompts.** Users with project servers get one prompt per server, and again whenever an entry changes.

## Alternatives considered

- **A fixed tool list per session.** Simplest, but adding a server would need a restart.
- **Rewriting `tools` when it changes.** A 400 on enforced accounts once earlier thinking is replayed, and a full cache rewrite everywhere.
- **Dropping thinking blocks when the tool set changes where the beta is missing.** It works, but throws away the model's reasoning so far on every server hiccup. Queuing until `/clear` is more predictable.
- **Declaring tools in `plugin.toml` so processes start on first use.** The schemas would live in two places, and MCP servers can't do it at all.
- **`inline-tools-2026-09-15` for same-name changes.** It isn't on every platform, and servers rarely change a tool mid-session. Revisit if that changes.
- **The tool search tool with every outside tool deferred.** Worth it once catalogs get large; that belongs with the router in 7.8.

## Implementation order

1. **Kernel.**
   - Changes: `ToolSource`, the ledger, `ToolsChanged`, provider rendering, the system prompt rendered once.
   - Tests: a scripted source in `ch-testkit` that adds, re-lists, withdraws and fails.
   - Check: the signed-thinking validator provider from 6.5 finds no violations across additions, withdrawals, a failed call followed by a new turn, a summary, an overflow retry and a resume.
2. **`mcp-bridge` (7.4)** with `/mcp`, per-server trust, the cleared environment, process groups (and the registry the exit paths kill) and redaction.
3. **`ch-plugin-protocol`, `external-host` and `ch-plugin-sdk` (7.1–7.3)**, including hooks, commands, prompt contributions and events.
4. **`hooks-shell` (7.5)**, with timeouts and the same trust rules, then **`ch plugins` and `/plugins reload` (7.6)**.

The conformance profile (7.7) runs in each PR against what that PR adds.
