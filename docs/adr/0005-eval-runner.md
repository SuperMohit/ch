# ADR 0005: Measure harness changes with `ch-eval`, running `ch -p` in a container per run

- **Status:** Accepted
- **Date:** 2026-11-13
- **Related:** [roadmap](../../ROADMAP.md) Phase 9 (evals) and 8.3 (offline traces), 10.1 (`executor-docker`), [ADR 0001](0001-sandbox-and-agent-security-responsibilities.md) (sandbox responsibilities)

## Context

The roadmap's third goal is that every harness component can be switched and measured. Switching works: planning, context strategies, spill, stuck detection and the file tools are plugins and settings. Measuring doesn't exist. `crates/ch-eval` is an empty stub, and recent changes (context management, spill, `edit_file` near misses, the plan refresh) each say they will be measured "in the Phase 9 eval".

What `ch -p` already gives a runner:

- `--output-format stream-json` prints every `AgentEvent` as a JSON line, then a `result` line with the termination reason, exit code, steps, token usage, cost and duration. Events carry the signals a report needs: `tool_call_end` metadata with `edit_file` failure reasons, `context_compacted`, `plan_refreshed`, hook denials and warnings.
- `--set KEY=VALUE` overrides any config value, including `plugins.disable`, and `--profile` applies a named profile.
- `CH_HOME` moves config, sessions and logs, so a run doesn't read or write `~/.ch`.
- The exit code says how the run ended.

An eval runs the model's commands unattended, with no approvals, many times. On the host those commands run as the user, with the user's files and credentials. And the event stream is `ch`'s own account: the model's commands run as the same user as `ch`, so they can write to it (through `/proc/<pid>/fd/1`), and a forged `result` line could claim any cost or outcome.

## Decision

`ch-eval` is a binary in `crates/ch-eval`. For each task, matrix cell and repeat, it runs `ch -p` in a fresh Docker container that holds only a copy of the task's repo, then takes the diff and runs the task's check in further containers. The agent's container has no network but its own internal one, where a credential proxy forwards model calls and adds the API key, so the key never enters the agent's container. The proxy also counts every call's tokens and cost. `ch-eval` saves the event stream of every run and writes a report comparing the cells.

### Tasks

A suite is a directory with one subdirectory per task:

```
eval/tasks/<id>/
  task.toml   # the task
  repo/       # the starting workspace
  verify/     # hidden check files, copied in after the run
```

```toml
instruction = "The total in report.py is off by one for empty input. Fix it."
verify = "python3 -I test_hidden.py"   # passes when it exits 0
image = "ch-eval-base"      # optional; the default image
timeout_secs = 900          # optional; wall time for the agent run
verify_timeout_secs = 600   # optional; wall time for the check
max_steps = 80              # optional; sets agent.max_steps
tags = ["bugfix", "python"] # optional; for filtering and the report
```

- **`verify/` is hidden.** The agent never sees it. After the run its files are copied over the workspace, replacing whatever the agent left at those paths (a symlink is replaced, not followed), so an agent that edits or deletes the visible tests can't make the check pass. The repo's own tests may stay visible as part of the task.
- **The check can still be gamed.** It runs code the agent wrote, in the agent's workspace: a module the check imports can patch the test framework, and a file in the workspace can stand in for a library the check uses. Checks should keep the workspace out of the way of their own tools (the starter tasks use `python3 -I`, a `.cjs` file with `node:` modules, and a `bash` stub first on `PATH`), but no check of code the agent wrote is tamper-proof. The report measures what the checks measure.
- **The check is a shell command.** `ch-eval` knows nothing about languages; the task picks its tools, and its image must have them.
- **Each check is tried first.** Before any model call, each selected task's check runs once on its untouched repo. It must fail there, and not because it couldn't run (exit 124 to 127, or a timeout); otherwise nothing runs.
- **A git URL with a revision instead of `repo/`**, and per-task images built from a Dockerfile, come with benchmark adapters (9.5).

### One run

For task T, cell C and repeat R, in `<out>/runs/<T>/<C>/<R>/`:

1. **Workspace.** `repo/` is copied to `work/`. On the host, it gets `git init` and one baseline commit with a fixed author and date, so the agent sees a clean repo and the runner can diff it. That git directory is copied to `baseline.git` next to `work/`, out of the agent's reach.
2. **Config.** `ch-home/config.toml` is the base config given with `--config` (model, provider, pricing). The cell's settings are passed as `--set` flags, so they are visible in the saved command line.
3. **Agent run.** In a container from the task's image, on the run's own network:
   - `ch -p - --output-format stream-json --permission-mode bypass`, `--set` flags that point the providers at the proxy with the run's token as their key, the cell's `--set` flags, and `--set agent.max_steps=N` when the task sets it;
   - the instruction is piped on stdin, stdout is saved as `events.jsonl` and stderr as `stderr.log`, each up to a size cap;
   - past `timeout_secs` the container is killed and the run is recorded as `timeout`.
4. **Diff.** `git add -A` and `git diff --cached` against the baseline go to `diff.patch`, before the check files are copied in. They run in a `ch-eval-base` container with no network and a time limit, with the workspace read-only and a scratch copy of `baseline.git`: the workspace's own `.git` is the agent's, and git on the host must never read it. The diff is best effort: what goes wrong is noted at the top of `diff.patch`.
5. **Check.** `verify/` is copied over `work/`. Then `verify` runs in a fresh container from the task's image, with no network and its own timeout, and its output goes to `verify.log`. Exit 0 is a pass, however the agent run ended: a run killed at its timeout or stopped at the step limit is still checked.
6. **Record.** `run.json` holds:
   - the task, cell, repeat and settings, and a fingerprint of what the run was made from;
   - pass or fail, and the termination reason and exit code (`timeout` for a killed run, `no_result` when `ch` printed no result line);
   - tokens, cost and model calls as the proxy counted them, and separately the steps, tokens and cost `ch` reported, which are untrusted;
   - wall time, and the check's exit code and whether it timed out;
   - counts taken from the events: tool calls by tool, tool errors by `failure` reason, denied calls, compactions, plan refreshes, hook denials and warnings.

**However a run ends** (recorded, stopped by an error, cut off, interrupted), every `.git`, in any case, under `work/` and `ch-home/` is removed, so git on the host never meets the agent's config there: right after the agent's container exits, again at the end, and, for a run a killed `ch-eval` left without a record, when the next invocation starts. The removal works on file descriptors, so no depth, path length or permission the agent left stops it. When what the agent left keeps the check's files out, the run is recorded as failed, with a note.

An existing `run.json` means the run is done: running again with the same `--out` resumes an interrupted matrix instead of starting over. It refuses to resume onto runs whose fingerprint (the `ch` binary and image, the base config as data, the cell's settings, the task) differs from what the new runs would have.

**What the runner does with a run is decided from what it and the proxy saw** (container exit codes, the ledger), never from the event stream, which the agent can write to. The one exception is the plugin list in the stream's first event, which `ch` writes before the agent's first command.

- It ended with an error after a transient model API failure (429, 5xx, no answer, or an overload or API error event inside a stream): no record; the run is tried again later, with a wait that doubles with each of its tries, and the matrix stops when one run has failed this way `--max-attempts` times (default 3).
- It says nothing about the agent (its container didn't start, the API refused the key, no model call of it through the proxy succeeded, or its plugins weren't the cell's): no record, and the matrix stops with an error that quotes the API's last answer.
- Otherwise the run is recorded, however `ch` ended (any exit code of its own, or killed at the timeout).

### Containers

- **`ch` inside the container** is a static Linux build (`<arch>-unknown-linux-musl`), so it runs in any image. So is `ch-eval` itself, which runs the proxy and wraps every container's command. `ch-eval build` makes both in a `ch-eval-builder` container (a `rust` image with `musl-tools` and the musl target, from `eval/docker/builder.Dockerfile`), caching the cargo registry and target directory in the Docker volumes `ch-eval-cargo-registry` and `ch-eval-target`, and copies them to `target/ch-eval/` in the source tree. `ch-eval run` builds them when either is missing or the source is newer. `--ch <path>` uses a given `ch` instead.
- **The default image `ch-eval-base`** comes from `eval/docker/Dockerfile`: Debian slim with git, ca-certificates, python3 and nodejs, and a system git config that trusts every repository and gives commits an author, since runs use a uid with no account in the image. `ch-eval build` builds it too. It also runs the diff and the proxy.
- **Mounts:** for the agent, `work/` at `/work` (the working directory), `ch-home/` at `/ch-home` with `CH_HOME=/ch-home`, and `ch` read-only at `/usr/local/bin/ch`. Every container also gets `ch-eval` read-only at `/usr/local/bin/ch-eval`. Nothing else from the host.
- **Limits:**
  - `--cap-drop ALL` and `--security-opt no-new-privileges`;
  - the host user's uid and gid, so the files stay removable;
  - `HOME=/tmp`;
  - memory, CPU and pids limits;
  - `ch-eval limit` as the entrypoint, which stops the command at its time limit plus a minute, even if the runner is gone.
- **Images are never pulled** (`--pull never`): a task's image must be built or pulled beforehand.
- **Each step is `docker create`, `docker start --attach`, `docker inspect`, `docker rm`**, so the step's own exit code is read from the container, and Docker failing to start it is told apart by its error.
- **Cleanup.** Every container and network carries the label `ch-eval.invocation=<id>` and the runner's pid and start time. When the matrix ends, fails or panics, or on Ctrl-C or SIGTERM, they are removed together. If the runner is killed instead, the proxy exits by itself once the runner stops touching a heartbeat file, agent containers stop at their limit, and the next invocation removes what is left of any invocation whose runner is no longer running.

### Network and keys

- **Each run has a network of its own.** It is internal, with no route to the host, the LAN, metadata endpoints, the internet or other runs. The proxy joins it for the run's length, as `ch-eval-proxy`; the proxy's own container is on the default bridge. So `web_fetch`, package installs and every other network use from the agent fail; tasks must not need them.
- **The credential proxy** (`ch-eval proxy`) forwards `POST /anthropic/v1/messages` and `POST /openai/chat/completions` to the base config's `base_url`, only for the providers the cells use, streams the answers through unchanged, and refuses every other path. It drops the client's auth headers, forwards a short list of others (`anthropic-beta` only with the betas `ch` sends), and adds the real key.
- **Keys reach only the proxy**, on its stdin: never in its environment, a file or a command line. They come from the variables the providers' `api_key_env` names (by default `ANTHROPIC_API_KEY` or `OPENAI_API_KEY`), for the providers the cells use; a missing one is an error before anything runs. The runner restarts the proxy, with the keys again, if it stops.
- **Each run gets a random token** as its API key (`CH_EVAL_API_KEY`). The proxy maps it to the run, refuses tokens it doesn't know, and forgets it when the run ends. A run may call only its cell's agent and summarizer models (so a proxied cell must set `model.id`), and at most two requests at once. The proxy forwards the request it parsed and checked, not the bytes it received.
- **One run can't hold up another.** Each connection carries one request, and each run's address may hold a few connections at once; more are closed at once rather than queued.
- **`ch` hides itself from its own tools.** On Linux, `ch` marks its process not dumpable at startup, so the model's commands, which run as the same user, can't read its environment (with the token, or the real key outside evals) or write to its output through `/proc`.
- **The proxy keeps the books.** It reads each answer's usage (Anthropic's `message_start` and `message_delta`, cache tokens included; OpenAI's `usage`) to its end, even when the client has gone, prices it with the pricing of the model that answered, and appends it to a ledger in `<out>/proxy/ledger/`, which the agent can't reach. The cost and tokens in `run.json` and the report come from there; `ch`'s own figures are kept separately as reported. Summaries the context strategy asks for go through the proxy too.
- **The proxy enforces `--max-cost`.** Once the ledger's total reaches it, model calls get an error instead of being forwarded. A run cut off that way gets no `run.json` and is made again when the matrix resumes with a higher cap.

### The matrix

- **`--vary KEY=V1,V2`** adds a dimension. Each value is a TOML literal passed as `--set KEY=V`, for example `--vary plugins.context.strategy='"none"','"staged"'`. A key must start with `model`, `agent`, `plugins` or `select`.
- **A key without a dot names a plugin,** and its values are `on` and `off`: `--vary planning=on,off`. `off` adds the plugin to `plugins.disable` and `on` adds it to `plugins.enable`, merged across dimensions and with the base config's lists, since `--set` replaces a list. After each run, the plugins `ch` reports as enabled are checked against the cell's switches and its `plugins.<id>.*` settings.
- **Cells** are the cartesian product of the dimensions. With no `--vary`, there is one cell, `base`. A cell's name lists its settings, such as `planning=off,context.strategy=none`, made safe for a directory name, and names must differ in more than case.
- **`--repeat N`** runs each task and cell N times, since model runs vary.
- **`-j N`** runs N at once (default 2). Runs are interleaved by task, so a slower API hour doesn't fall on one cell, and each task and repeat starts with a different cell, so the same cell doesn't always warm the prompt cache for the others.
- **`--max-cost USD`** caps what the ledger records for `<out>`: the proxy refuses model calls past it and no new run starts. Every model the cells use needs a price.
- **`--max-attempts N`** tries a run at most N times when the model API fails transiently; then the matrix stops.
- **`--dry-run`** prints the planned runs and the command line of one, then exits.

### The report

`ch-eval run` ends by writing `report.md` and `results.json` in `<out>`. `ch-eval report <out>` writes them again from the `run.json` files, for example after an interrupted matrix. `ch-eval compare <out1> <out2> --to <dir>` reports several output directories together, each one's cells named after its directory, so two builds of `ch` compare task by task. It refuses directories whose tasks differ, and, unless `--allow-different-config` is given, whose base config or cells differ, naming what differs; the report says the directories' runs weren't interleaved, so a change in the API between them is confounded with the change measured.

- **Per cell:**
  - runs and the pass rate, with a 95% Wilson interval over runs, labelled as such: it treats repeats of a task as independent, so with repeats it is too narrow;
  - mean and median cost, model calls, input, output and cache tokens (from the proxy), steps (as `ch` reported them), and wall time;
  - termination reasons, and how checks ended (passed, failed, timed out, couldn't run);
  - the harness signals above, per run;
  - a note when its runs have more than one fingerprint.
- **Per task and cell:** passes out of runs.
- **Paired, for each pair of cells:** per task, over the repeats both cells have, which cell passed more often. The report gives wins, losses and ties, and a two-sided sign test over the tasks that differ, noting when there are fewer than six such tasks (too few for p < 0.05). Tasks are the unit, because repeats of one task aren't independent evidence about the harness.
- **Strings that came from a run** (reasons, tool names, failure reasons) are escaped for Markdown and HTML.

Results go to Langfuse as scores later, with Phase 8.

### Tests

- **The runner's logic is tested without Docker or a model:**
  - reading tasks;
  - expanding the matrix;
  - building the command lines, and the Docker steps against a stand-in `docker`;
  - parsing events into `run.json`, and deciding what happens to a run, along every way it can end;
  - the proxy, against a fake model API: auth, the path and model allowlists, tokens, streams, usage, limits and `--max-cost`;
  - the statistics and the report.
- **A run of the whole pipeline through `ch -p`** uses the scripted provider on the `calc-py` fixture, and the Anthropic provider through the proxy to a fake model API. A test-only backend runs `ch` on the host instead of in a container. It is behind a cargo feature that only tests turn on, and isn't reachable from the command line.
- **Docker tests** run a task through the proxy in real containers, checking from inside the agent's container that the proxy is reachable, the host and the internet aren't, and the key is nowhere in it; that a timed-out agent's container is stopped; that two runs can't reach each other, and one run's connection flood doesn't hold up another; that a deep, locked tree with `.git` directories is scrubbed and removed and the host's git runs nothing; that a tool can't read `ch`'s `/proc` entries; and that a dead invocation's leftovers are swept while a live one, seen from another time zone, is not. They are `#[ignore]`d, because CI has no Docker.

## Consequences

- **Harness changes get numbers.** "Does the plan refresh help?" becomes `--vary plugins.planning.refresh_after_steps=0,10` over a suite. A change compiled into `ch` (a prompt, say) needs a run per build, into two output directories, compared with `ch-eval compare`.
- **The roadmap's 8.3 offline traces are not needed for evals.** The saved `events.jsonl` is that trace. `ch replay` stays a separate idea.
- **Runs cost money and time.** Every run calls the model. Suites are small, `--max-cost` caps a matrix, and `--dry-run` shows the size first.
- **Docker is required to run evals,** but not to build or test `ch`.
- **Evals have no network.** Tasks that need `web_fetch` or downloads can't be measured until a later change allows chosen hosts.
- **Results depend on the model and the day.** The report keeps per-run records, so a surprising cell can be read run by run.
- **The Anthropic provider gained `official_api`,** so a run through the proxy sends what it would send to api.anthropic.com.

## Alternatives considered

- **Running `ch -p` on the host.** Simplest and fastest, but the model's commands run as the user with no approvals. Rejected.
- **A macOS sandbox profile (`sandbox-exec`).** Fast, but macOS-only, and reads and network stay open. Rejected in favour of one mechanism that also serves benchmark images.
- **Driving `Session` in-process instead of the binary.** Faster, with no build step, but it measures something other than what users run, and it can't be isolated from the host.
- **The `executor-docker` plugin (10.1) with `ch` on the host.** It isolates `bash`, but not the file tools, which would still need a mounted workspace. Running the whole agent in the container is simpler and covers every tool.
- **Passing the API key into the agent's container, with network access.** The first version did. The model could read the key and print it into the events, the diff, the session and the workspace, its commands could reach the host's services and the LAN, and the cost came from a stream the agent could forge. Replaced by the per-run networks and the proxy.
- **Langfuse as the store of record.** It needs a server for every eval. The files in `<out>` are the record, and Langfuse can be fed from them.

## Implementation notes

Where `ch-eval` differs from the decision as first written, or fills in what it left open, and why. The text above describes what is built.

- **The credential proxy replaces passing the key into the agent's container.** A security review of the first version found that the model could read the key and write it anywhere in the output, reach the host's localhost, the LAN and metadata endpoints, and forge the `result` line (a cost of -1e300 disabled `--max-cost` for that output directory; a usage of `u64::MAX` panicked the runner). The proxy is a hidden `ch-eval proxy` subcommand built on hyper, in a `--rm` container from `ch-eval-base`, started with `docker start --attach --interactive` so the runner can write the keys to its stdin; that attached process carries its output to `<out>/proxy/proxy.log`. Its state directory is `<out>/proxy/`: `proxy.json` (upstreams, no keys), `tokens/` (one file per running run, removed when the run ends; all removed when an invocation starts), `ledger/<run>.jsonl` (one line per model request, kept across invocations), `ready` and `heartbeat` (the proxy exits when it is a minute old). It never prints keys or headers. The runner checks every few seconds that it is running and starts it again otherwise; it then rejoins the runs' networks.
- **The proxy's limits.** Request bodies over 32 MiB are refused, at most 8 are held in memory at once, a run may have two requests going (`ch` makes one at a time), headers must arrive within 30 seconds and bodies within 120, at most 256 connections are open, and at most 8 from one address. A connection over a limit is closed at once rather than left waiting, and each connection carries one request, so a run that opens connections and sends nothing holds up no other run (the first version's accept loop waited for a free slot, so one run could stall every other). The proxy's container has 1 GB of memory.
- **Metering reads the upstream to its end.** A separate task reads each answer, passing it to the client while the client is there; disconnecting no longer cuts the count (the first version recorded a million output tokens as one). A request that can't reach the upstream gets a 502 and a ledger line with status 0. An error event inside a stream that began with 200 (such as `overloaded_error`) is recorded, and so is a successful answer without usage, which `ch-eval run` warns about.
- **Models and pricing.** Each token lists its cell's models: the agent's `model.id` (which a proxied cell must set, since the proxy doesn't know a provider's default) and the summarizer's (`model.summarizer.id`, or the agent's model when the summarizer sets neither). A request for another model is refused and recorded. The proxy forwards the JSON it parsed and checked, not the bytes it received: a body with two `model` keys was checked against one and could be read upstream as the other. An answer whose own model isn't one of the cell's is recorded with `unlisted_model`, priced as the model asked for, and `ch-eval run` warns about it. Each model is priced with `[eval.pricing."<model>"]` from the base config (a top-level table `ch` ignores), or `[model.pricing]` for the configured model, as `ch` prices it; a cell that changes `model.id` therefore keeps `[model.pricing]` unless `[eval.pricing]` lists the new model, and `ch-eval run` warns when it does. A malformed `[eval.pricing."<model>"]` is an error, naming the cell and model, rather than being skipped. The answer's own model is recorded and priced, so a fallback to another listed model costs what it costs. With `--max-cost`, every model a cell uses needs a price.
- **Only the betas `ch` sends** (`ch_provider_anthropic::FALLBACK_BETA` and `BINDING_BETA`) pass in `anthropic-beta`; others could change what a call costs.
- **The proxy is told whether the upstream is the official API.** `ch-eval` passes `plugins.provider-anthropic.official_api` (a setting added for this: `fallbacks` and the default `eager_input_streaming` follow it instead of the `base_url`) and `plugins.provider-openai.max_tokens_field`, as they would be for the base config's `base_url`, unless the config sets them. A cell can't change a provider's `base_url` or `api_key_env`.
- **The run's token isn't in the model's commands' environment** either: `executor-local` hides every variable an `api_key_env` names. The first version left it readable in `/proc/<ch>/environ`, and `/proc/<ch>/fd/1` let a command write to `ch`'s event stream. Now `ch` calls `prctl(PR_SET_DUMPABLE, 0)` at startup on Linux, always (through nix, since the workspace denies `unsafe`), which makes those entries root's and blocks same-user `ptrace` and core dumps; a Docker test checks both paths from a tool command. A token that leaked anyway would only work at the proxy, during its run, with everything it spends counted.
- **`--env` is gone.** The keys go to the proxy only; the agent gets no variables from the host.
- **`--max-cost` is enforced by the proxy** against the whole ledger for `<out>`, so it includes runs that were interrupted or cut off. Requests already going when the cap is reached finish, so the total can pass it by what those cost.
- **Deciding what a run means** uses the container's exit (from `docker inspect`) and the ledger only. The first version also trusted the event stream, so an agent could write a fake `result` and get its run treated as an infrastructure failure, and an agent request to the unused provider's route got a 401 from a real upstream with no key, which stopped the matrix; now only the providers in use have routes. The plugin check reads the stream's first `session_start`, which `ch` writes, and flushes, before the agent's first model call. With the scripted provider no call goes through the proxy, so a run that `ch` rejects at startup is recorded (as `no_result`) rather than stopping the matrix.
- **Transient model API failures** used to stop the matrix (any `error` after a 429 or 5xx), or were recorded as task failures (status 0, or an overload inside a stream). Now a run that ended with `ch`'s error code 1 after such a failure gets no record and is queued again after 30 seconds, doubling with each of its own tries up to 15 minutes; when one run has been tried `--max-attempts` times, the matrix stops. An earlier version counted such runs in a row across the matrix, so one task that always hit an overload, among others that passed, was retried forever. A worker waiting out a backoff wakes when the matrix stops. A refused key (401, 403) still stops it at once.
- **A run with no successful model call says nothing about the agent.** A proxied run whose calls all failed (a model the account can't use, a request that is too large) used to be recorded as a task failure. Now, unless it was a transient failure, it stops the matrix with the API's last status and message.
- **The diff runs in a container.** The first version ran git on the host with a separate git directory, but git still read the workspace: a gitfile pointing at a FIFO hung it (with no timeout) and one pointing at a host path made it read there. Now `DIFF_SCRIPT` runs in `ch-eval-base` with the workspace read-only, `--network none` and a two-minute limit, with `git add --ignore-errors`, so an unreadable file or a nested repository without a commit only drops those paths. Failures are noted as `#` lines at the top of `diff.patch`, which `git apply` skips; the patch itself is streamed, not read into memory.
- **`.git` entries are removed in any case, everywhere the agent wrote, on every way out.** A `Scrub` guard, made before the baseline commit, removes `.git`, `.Git` and so on (a directory, gitfile, symlink or FIFO, at any depth) under `work/` and `ch-home/` as soon as the agent's container exits, again when the run is recorded (both counted in `git_entries_removed`), and, when dropped, after an error, a cut-off, a transient failure, a panic or an interruption; Ctrl-C now waits up to 30 seconds for the runs to wind down so their guards run. A run directory a killed `ch-eval` left without `run.json` is scrubbed when the next invocation starts. The first version matched only `.git` exactly (APFS is case-insensitive), skipped `ch-home/`, and skipped runs that stopped early. `baseline.git` keeps the starting commit; the host's git only ever ran on the task's own files.
- **The scrub and removal walk by file descriptor.** They open one directory at a time with `O_NOFOLLOW` relative to its parent and climb back through `..`, making unreadable directories readable as they go, so a tree deeper than the host's path limit (1024 bytes on macOS) or with mode-000 directories doesn't stop them; `std::fs::remove_dir_all` and the path-based first version failed on both, which left `.git` entries behind and broke resuming. The top level's `.git` is removed first, so a failure deeper down can't leave it. When what the agent left keeps the check's files from being copied in (a directory where a check file goes, say), the run is recorded as failed with a `note` instead of stopping the matrix.
- **Copying `verify/` doesn't follow the agent's symlinks,** and directories the agent made unwritable are made writable first.
- **Telling Docker's failures from the step's.** Each step is `docker create`, `start --attach` and `inspect`: the exit code is the container's, and `State.Error` says whether it started. A check that calls `os._exit(125)` used to stop the matrix on every resume.
- **Containers can't be orphaned.** Each invocation has a label, and a `Cleanup` (run when the matrix ends, on any error, on drop after a panic, and on Ctrl-C or SIGTERM) removes its containers and networks. Once it has run, no new container is created, and one created meanwhile removes itself. Every worker is awaited before an error is returned. At a step's timeout the container itself is killed. `ch-eval limit` stops each container's command at its limit plus a minute, as a backstop if the runner is killed, and the next invocation sweeps containers and networks labelled with a runner whose process is gone. A runner counts as gone only when its heartbeat file (touched every few seconds, its path in a label) is over a minute old and no process has its pid and start time; `ps` runs with `LC_ALL=C` and `TZ=UTC`, since the first version compared start times printed in each invocation's own locale and time zone, so an invocation started from another shell removed a live one's containers.
- **Each run's network is its own,** created before the agent's container and removed after it, with the proxy connected and disconnected around the run. On a shared network, runs could reach each other's ports. Docker's default address pools allow about 30 networks, which limits `-j`.
- **Images.** `--pull never`, the entrypoint is always `ch-eval limit`, `--` comes before the image, and a task image starting with `-` is refused. The image's id is part of the fingerprint.
- **Bounded reads and writes.** `events.jsonl` is kept up to 256 MiB, logs up to 16 MiB and `diff.patch` up to 64 MiB; past a cap the rest is read and dropped with a note. `events.jsonl` is read line by line, skipping lines over 8 MiB. Docker Desktop can't cap what the agent writes into `work/` and `ch-home/`, which are bind mounts; a run can fill the disk.
- **Fingerprints.** `run.json` holds hashes of the `ch` binary with the task image's id, the base config as data (keys sorted, comments and layout gone, so editing a comment doesn't block a resume), the cell's arguments (profile, `--set` flags and step limit; not the proxy's settings), `task.toml`, `repo/` and `verify/`. Resuming onto a run with a different fingerprint is refused, naming the parts that differ.
- **Misspelled keys inside a plugin's table aren't caught yet.** Most plugin configs accept unknown fields, so `--vary plugins.context.stratgy=...` makes cells identical to the base. A separate change (PR #48) makes plugin configs reject unknown fields, which catches them.
- **`compare` checks what it compares.** It refuses directories whose task ids or task hashes differ, and lists base configs and cell settings that differ, refusing unless `--allow-different-config` is given; the report then lists them in a note at its top, with a note that the directories' runs weren't interleaved. Directories with the same last component are labelled `out`, `out#2` and so on; the first version gave both the same label and merged their cells.
- **The check is tried on the untouched repo first**, once per invocation, for each task with runs to do, `-j` at a time, in `<out>/preflight/<task>/` (removed when it passes).
- **Self-reported numbers are clamped.** Costs that aren't finite and non-negative count as 0, counts add without overflow, and a termination reason that isn't a short lowercase word is recorded as `invalid`. `unreadable_event_lines` counts lines that weren't JSON.
- **Denied calls are counted apart** (`tool_denials`): a `tool_call_end` that is an error without `failure` metadata and whose content starts as the kernel's denials and refusals do (`Permission denied (`, `Not run:`, `The user denied`) didn't run, so it isn't a tool error.
- **`--vary` keys** must start with `model`, `agent`, `plugins` or `select`. Cell names must differ in more than case, since `x=Fast` and `x=fast` are one directory on macOS. The rotation starts task `t` in repeat `r` at cell `(t + r - 1) mod cells`.
- **The report's interval and pairs.** The Wilson interval is labelled as over runs, with a note that repeats make it too narrow. Pairs compare each task over the repeat numbers both cells have. Pairs with fewer than six differing tasks are marked. The "Checks" table counts timed-out checks and exits 126 and 127 apart from failures.
- **Earlier notes that still hold:** the builder image pins its toolchain with `RUSTUP_TOOLCHAIN`, since the workspace's `rust-toolchain.toml` would otherwise download `stable` with clippy and rustfmt on every build; the binaries are rebuilt when anything under `Cargo.toml`, `Cargo.lock`, `crates/`, `plugins/` or `prompts/` (but `target/`) is newer; the architecture is the host's (`--ch` for another); `ch-eval-base` is `debian:trixie-slim` with Python 3.13 and Node.js 20; containers get `--init` and fixed limits (4 GB, 2 CPUs, 1024 processes); `run.json` is written through a temporary file and renamed; `verify_timeout_secs` defaults to 600 and `timeout_secs` to 900; `--suite` defaults to `eval/tasks` in the source tree, `--out` is required, `--config` and `--profile` are optional, and `--task` and `--tag` narrow the tasks together; values are split at commas outside quotes and brackets, and one that isn't valid TOML is a string, as with `ch --set`; events are read as plain JSON, with a unit test that serializes kernel events; the report sorts cells by name and escapes strings from runs.
- **The host backend** is behind the `host-backend` cargo feature, which only `ch-cli`'s dev-dependency on `ch-eval` turns on. The pipeline tests are in `crates/ch-cli/tests/eval.rs`, because only that package's tests get its `ch` binary. The Docker smoke test reaches the fake model API as `host.docker.internal` (Docker Desktop provides it; Linux needs `--add-host`).
