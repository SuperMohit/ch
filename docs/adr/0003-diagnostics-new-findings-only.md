# ADR 0003: Diagnostics report only what an edit broke, once per step

- **Status:** Accepted
- **Date:** 2026-11-11
- **Related:** [roadmap](../../ROADMAP.md) 6.3 (diagnostics, replaced by this ADR), [ADR 0001](0001-sandbox-and-agent-security-responsibilities.md) (who protects what), the `safety` plugin's permission policy

## Context

Roadmap 6.3 planned a `post_tool` hook that runs a checker after every edit and appends its output to the tool result. The first attempt (PR #31, closed) built exactly that. Two adversarial reviews found five problems:

- **Wrong timing.** A change made in several edits is checked after each one, so the model is told about states it is halfway through fixing.
- **Wrong blame.** Every finding in the file is reported, including ones that were there before the edit.
- **A formatter is not a check.** `rustfmt --check` reports style, and on a file path it also formats child modules the edit never touched.
- **A missing checker looks like a finding.** A rustup proxy without the `rustfmt` component exits 1, the same code as a real problem.
- **The repo chooses what runs.** `rust-toolchain.toml` can set `path` to a directory in the repo. rustup then runs that directory's `bin/rustfmt`, automatically and without the model or the user asking for a command.

A first version of this design fixed those problems by keeping built-in checkers (rustfmt for `.rs`, ruff for `.py`) and pinning them so the repository couldn't choose the program. That put Rust and Python knowledge into the harness: two output parsers, a `Cargo.toml` edition lookup, rustup environment variables. It was removed before release. `ch` is a generic harness: it provides the mechanism, and language tooling comes from configuration or from the model running it through bash.

The published evidence points the same way: catching broken edits helps a little, checking every step costs more than it gains, and detailed error feedback matters mostly for weaker models. See [Evidence](#evidence) below.

## Decision

The `diagnostics` plugin checks each file the agent changed **once, just before the next model call**. It reports only the findings that are **new** compared with the file before the edit, as a **reminder** on that one request. The plugin knows **no language**: each checker is a command from config, and with none configured the plugin does nothing.

### When a check runs

1. **`pre_tool`** on `edit_file` or `write_file`: resolve the path with the `safety` plugin's `PathGuard`, as the tool will. If this step has no snapshot for that path yet, keep the file's current content as the snapshot (or "absent" for a new file). `pre_tool` hooks can run twice for one call, so the first snapshot wins.
2. **`post_tool`**: if the call ran and succeeded, mark the tool's own `metadata.path` as changed. A failed or denied call marks nothing.
3. **`before_model`**: for each changed path, run the checker on the snapshot (**before**) and on the file as it is now (**after**). Report `after − before`. Then drop every snapshot of the step, including those whose call failed or was denied, so a later step never compares against stale content.

A file that is absent, larger than 1 MiB or not UTF-8 after the step is skipped. A file whose content didn't change overall is skipped too. A new user prompt starts the turn's state over.

So a file edited five times in one step is checked once, on its final state. Checks run sequentially. A step checks at most `max_files_per_step` files (default 10) and skips the rest, saying so in the reminder.

**Results are cached** by command and content hash (up to 512 entries). The **before** of a file the agent changed in the previous step is usually that step's **after**, so it doesn't run again.

### What counts as new

A finding's key is the rule plus the message, with line and column numbers removed. Keys are compared as multisets, so two identical errors before and three after report one.
- Line numbers aren't part of the key. An edit above an old error moves it, and the model shouldn't be blamed for that.
- **Display:** a reported finding shows its location from the **after** run.

### How findings reach the model

- **Format.** A `before_model` note that the kernel adds as a `<system-reminder>` on that request only. It is not saved to history:

  ```text
  Your last edits introduced these problems:
  src/lib.rs:2:21: error: expected expression, found `;`
  app/main.py:14:5: F821 Undefined name `reponse`
  Fix them, unless a change you are about to make already does.
  ```

- **Size.** Capped at `max_report_chars` (default 2,000).
- **No repeat.** A finding is reported once. If the model leaves it and edits the same file again, the old finding is in both **before** and **after**, so it isn't reported again. The turn-end check below catches what's still open.

**Turn-end check.** When the model ends its turn (`turn_end` with `Done`), compare each file changed this turn with its state at its first change this turn.
- If findings the turn introduced are still there, send **one** note listing them. As with every `turn_end` note, the turn continues and the note is saved to history.
- It runs at most once per turn and needs no new checker runs, because the cache holds both states.
- It is a narrow form of "check before finishing": only for findings the turn's own edits introduced, with no nudge to run tests.
- `nudge_at_turn_end = false` turns it off.

### Checkers come from config

There are **no built-in checkers**. A checker is a command for one file extension, set in the user's config or a trusted project's:

```toml
[plugins.diagnostics]
timeout_secs = 10
max_report_chars = 2000
max_files_per_step = 10
nudge_at_turn_end = true

[plugins.diagnostics.checkers.ts]      # by extension; `enabled = false` turns off one another layer added
command = "npx --no-install eslint --format unix --stdin --stdin-filename={path}"
stdin = true                           # the only value accepted for now; see "Limits"
```

- **Input.** The command reads the file's content on **stdin**, so **before** and **after** can be checked without touching the file. `{path}` is the file's path relative to the workspace, shell-quoted; join it to its flag with `=` so a path starting with `-` can't become an option.
- **Output.** Any line on stdout or stderr with a location (`path:L`, `path:L:C`, `path(L,C)`) is a finding. Its key is the line with the locations removed. Lines without a location, such as summaries, are ignored.
- **Exit codes.** Exit 0 means no findings. Any other exit except 126 and 127 may carry findings, and must: a non-zero exit with no line to read is a checker failure. So are 126, 127, a timeout and a failure to start. Failures are logged and never reported to the model.
- **Probing.** On first use, `command -v` looks for the command's program. If it isn't found, the checker is off for the session, with one warning to the user and nothing to the model. Three failed runs in a row do the same.
- **What to configure.** Checks with no false positives that look at one file: parse errors, undefined names. Not formatters (they report style) and not project-wide builds (see "Limits").
- **Events.** Each report emits `Custom { source: "diagnostics", name: "introduced" }` (or `"turn_end"`) with the number of findings, for telemetry and the Phase 9 comparison.
- **Optional.** The plugin is its own cargo feature, enabled by default like `stuck`, `planning` and `spill`. Without checkers it only records which files changed. `plugins.disable = ["diagnostics"]` turns it off.
- **Settable by a project without trust:** `timeout_secs`, `max_report_chars` and `max_files_per_step`. **Need trust:** `checkers` and `nudge_at_turn_end`.

#### Example configurations

These are documentation, not defaults. Each was tried by hand when this ADR was written.

| Extension | Command | Finds |
|---|---|---|
| `.rs` | rustfmt with an awk wrapper (below) | Parse errors only. Valid code exits 0 however it is formatted, and `mod x;` isn't followed on stdin. |
| `.py` | `ruff check --no-fix --force-exclude --output-format concise --select F63,F7,F82 --stdin-filename={path} -` | Syntax errors (always reported), undefined names, statements in the wrong place. `--select` replaces the project's own rule set, so style isn't reported. Exit 2 means ruff itself failed. |
| `.ts`, `.js` | `npx --no-install eslint --format unix --stdin --stdin-filename={path}` | Whatever the project's eslint config enables. `npx` runs the repository's own eslint. |

rustfmt prints each error as `error: …` with its location on a later ` --> <stdin>:L:C` line, so the wrapper joins the two into one line per error and exits 1 when there was one. Set the edition your code uses:

```toml
[plugins.diagnostics.checkers.rs]
command = '''RUSTUP_TOOLCHAIN=stable RUSTUP_AUTO_INSTALL=0 rustfmt --edition 2021 --emit stdout 2>&1 >/dev/null | awk -v p={path} '/^error/ { m = $0 } /^ *--> / && m != "" { n = split($0, a, ":"); print p ":" a[n-1] ":" a[n] ": " m; m = ""; bad = 1 } END { exit bad }' '''
```

The probe skips leading `NAME=value` words, so it looks for `rustfmt` here. The pipeline's exit code is awk's, so a rustfmt that fails without printing an error counts as "no findings" rather than as a failure.

Mind what a command lets the repository choose. A rustup proxy follows `rust-toolchain.toml`, whose `path` can point into the repository; set `RUSTUP_TOOLCHAIN` in the command to stop that. `npx` and anything else found through the repository runs the repository's code.

### Who decides that a checker may run

Checkers run without anyone approving a command, so two rules limit them:

1. **Only while `bash` is available to the model.** That means the `bash` tool is registered and the safety policy doesn't deny it outright (`deny = ["bash"]`). For that, `safety` provides its `Policy` as a service. With bash off, the user has chosen an agent that runs no programs, and diagnostics would quietly break that.
2. **Only commands the user chose.** The user's own config, or a project's `.ch/config.toml` once the project is trusted, as for any setting that runs a command. The harness ships none.

**Bash in `ask` mode.** Under the default `ask` mode most bash commands need the user's approval, so the first rule alone doesn't stop a hostile repository. The second does: no checker runs that the user didn't configure, and the example commands above say what each lets the repository choose.

**Alternative: gate on the permission policy.** Run a checker only if the same command, sent as a bash call, would run without asking.
- It is stricter.
- But a checker the user configured would still need an allow rule as well.
- It also misses "always allow" answers given during the session, which the approver holds and the policy doesn't.

Rejected for now.

## Evidence

These are the findings the design rests on. The figures are as their authors report them, and were read through summaries rather than checked against the full papers. Phase 9 measures this design in `ch` itself.

| What was found | Where | What it means here |
|---|---|---|
| Agents fail by claiming success. About a quarter of failed runs of terminal coding agents claim a success that didn't happen. On one benchmark of app-using agents, three quarters of failures are false claims of success. Strong agents spend 35–37% of their steps checking their work; weak ones 12–19%. | Annotated trajectory studies of CLI coding agents and of tool-using agents | The model needs to hear what its edit broke instead of assuming it worked. The turn-end note targets "done" claims made over a known problem. |
| In about 1,800 annotated terminal-agent runs, the decisive error comes at a median of step 7, but outward signs show only around step 16. Successful recoveries take a median of 5 steps, failed ones 12. | The same trajectory study | Reporting on the very next step, rather than whenever tests next run, closes most of that gap for errors a parser can see. |
| A lint gate that rejected syntactically broken edits raised the solve rate on a SWE-bench subset from 15.0% to 18.0%. | The SWE-agent ablations | Parse errors are worth catching. This design reports instead of blocking, so a change spread over several edits isn't refused halfway. |
| Verifying under 4% of steps gained 1.7 points at 1.1× the cost. Verifying every step raised cost without that gain. | A study of selective step verification | Check only steps that changed a file, once per file, and stay silent when nothing new is wrong. |
| Detailed error messages with locations took a 14B open model from 41% to 63% on fixing type errors. A frontier model was at 95–100% either way. | An ablation of type-error feedback for coding agents | Show rule, message and location. Expect the gain mainly on weaker and open-weight models; for frontier models this is cheap insurance. |
| Syntax checks have zero false positives. Edits anchored on line numbers fail silently after a shift. | A study of verifying agent actions before they run | Configure parse errors and undefined names, not style. Match findings on their message, never on their line. |
| Models rarely call optional tools: language-server tools were used in 0–6% of localisation tasks. | A study of language-server tools in coding agents | Push findings as a reminder instead of offering a "diagnostics" tool the model would have to remember. |
| Open agents ship a capped "check before finishing" nudge (Hermes, OpenDev). | Source reviews of open coding agents | One turn-end note per turn, then accept the stop. |
| Strong agents already check their own work through bash. | The trajectory studies above | Ship the mechanism, not language defaults: users who want push-style checks configure them, and the model can always run its own. |
| No controlled study of advisory, per-edit linting was found. | — | This design is a bet. The measurement below decides whether it is worth configuring. |

## Limits and later work

- **Only `edit_file` and `write_file` changes are seen.** Changes made through bash (`sed -i`, a code generator) aren't checked.
- **Checkers that can't read stdin get no before/after diff.** A checker with `stdin = false` only reads the file on disk, and by check time the **before** content exists only in the snapshot. Two options:
  - write the snapshot to a temp file beside the original (which some tools mis-resolve);
  - or run it only at turn end against a baseline.

  Version 1 accepts `stdin = true` checkers only, and the setting exists so the config doesn't change later.
- **Project-wide checks** (type errors through a build or a type checker) could come as a separate, configured turn-end tier with its own short design. Its open question is the baseline: the first run of a session has nothing to compare against unless it runs before the first edit, which blocks that edit for the length of a build.
- **Measuring it** (Phase 9). Compare off, the PR #31 behavior, this design, and this design with the turn-end check. Report:
  - success, cost and extra steps;
  - how often the model claims done with a known finding still open;
  - findings blamed on an edit that existed before it. The target is zero, and the before/after diff is what makes it zero.

## Consequences

- **What the model gets:** at most one short reminder per step, only about problems its own edits introduced, and at most one turn-end nudge. Nothing is added to history except that nudge.
- **Cost per changed file per step:** two stdin runs of the configured checker. One of them is usually a cache hit. With no checkers configured, nothing runs.
- **The trust model doesn't change.** No program runs that the user or a trusted project didn't configure.
- **No language knowledge in the harness.** Language setups live in the examples above, not in code.
- **Two small changes outside the plugin:**
  - `safety` provides `Policy` as a service;
  - `diagnostics` declares that it requires `safety`, for `PathGuard`.
- **Roadmap 6.3 is rewritten to point here.**
