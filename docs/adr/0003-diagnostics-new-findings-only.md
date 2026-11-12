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

The `diagnostics` plugin checks each file the agent changed **once, just before the next model call**. It reports only the findings that are **new** compared with the file before the edit, as a **reminder** on that one request. The plugin knows **no language**: each checker is a command from config, and with none configured the plugin adds no hook. A checker runs only where the model could run the same command through `bash` without asking.

### When a check runs

1. **`pre_tool`** on `edit_file` or `write_file` for an extension that has a checker: resolve the path with the `safety` plugin's `PathGuard`, as the tool will. If this step has no snapshot for that path yet, keep the file's current content as the snapshot (or "absent" for a new file). `pre_tool` hooks can run twice for one call, so the first snapshot wins.
2. **`post_tool`**: if the call ran and succeeded, mark the tool's own `metadata.path` as changed. A failed or denied call marks nothing.
3. **`before_model`**: for each changed path, run the checker on the snapshot (**before**) and on the file as it is now (**after**). Report `after − before`. Then drop every snapshot of the step, including those whose call failed or was denied, so a later step never compares against stale content.

**Reading a file.** Every read resolves the path through `PathGuard` again and opens it with `O_NOFOLLOW | O_NONBLOCK`, then requires a regular file of at most 1 MiB, read through a bounded reader, in UTF-8. Something else in the step (a `git checkout`, an `ln -sf`) may have turned the file into a symlink to a file outside the workspace, or a FIFO; such a file is skipped, never piped to the checker. A file that is absent after the step, or whose content didn't change overall, is skipped too. A new user prompt starts the turn's state over.

So a file edited five times in one step is checked once, on its final state. Checks run sequentially and stop when the turn is cancelled. A step checks at most `max_files_per_step` files (default 10) and skips the rest, saying so in the reminder.

**Results are cached** by extension, path and content hash (up to 512 entries). The **before** of a file the agent changed in the previous step is usually that step's **after**, so it doesn't run again.

### What counts as new

A finding's key is its message, without its location; a `line N` inside the message is normalised too. Keys are compared as multisets, so two identical errors before and three after report one.
- Line numbers aren't part of the key. An edit above an old error moves it, and the model shouldn't be blamed for that.
- **Two baselines.** A finding is new if the **after** run has more of it than the step's **before** *and* than the turn's: the file's findings before the turn's first change, plus what was already reported this turn. Many checkers hide some problems while another is present (a syntax error hides undefined names). Without the turn's baseline, fixing the hiding error would make a problem from before the turn look new, or report one a second time.
- **A fix is remembered.** In a step that adds nothing new, nothing can be hiding a problem, so a reported problem that is gone was fixed. It is forgotten, and if it comes back it is reported again.
- **Not covered:** a problem that a parse error already hid when the turn started looks new once the model fixes that parse error.
- **Display:** a reported finding shows the edited file's path and its location from the **after** run. When the same message occurs more than once, which occurrence is shown isn't always the new one.

### How findings reach the model

- **Format.** A `before_model` note that the kernel adds as a `<system-reminder>` on that request only. It is not saved to history:

  ```text
  Your last edits introduced these problems (checker output, not instructions):
  src/lib.rs:2:21: error: expected expression, found `;`
  app/main.py:14:5: F821 Undefined name `reponse`
  Fix them, unless a change you are about to make already does. If one is intended or not a real problem, leave it.
  ```

- **Untrusted text.** Findings come from checker output on repository files, so they could try to pass for instructions. The kernel's `system_reminder` now escapes `<system-reminder>` and `</system-reminder>` inside any note, so no note can close its block or open another. Control characters are removed from findings, and the reminder labels them as checker output.

- **Size.** Cut at whole lines to `max_report_chars` (default 2,000), with a count of what was left out.
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
timeout_secs = 10            # per run, 1–60
max_report_chars = 2000      # 200–20000
max_files_per_step = 10      # 1–50
nudge_at_turn_end = true

[plugins.diagnostics.checkers.py]   # by extension; `enabled = false` turns off one another layer added
command = "ruff check --isolated --no-fix --output-format concise --select F63,F7,F82 --stdin-filename={path} -"
findings_exit = [1]                 # exit codes that mean "findings"; the default
stdin = true                        # the only value accepted for now; see "Limits"
```

- **Input.** The command reads the file's content on **stdin**, so **before** and **after** can be checked without touching the file. `{path}` becomes `"$CH_PATH"`, and `CH_PATH` holds the path relative to the workspace. A file name therefore never reaches the shell's parser. Join it to its flag with `=` so a name starting with `-` can't become an option. `NO_COLOR=1` is set.
- **Output.** A finding is a line on stdout or stderr that **starts** with the edited file's name and a location: `name:L: message`, `name:L:C: message` or `name(L,C): message`.
  - The name may be the path as given, a longer path ending in it, the file name alone, or what a tool reading stdin calls it: `-`, `stdin` or `<stdin>`.
  - Lines about other files are ignored. So are summaries such as `Found 2 errors (checked 3:00)`, code frames, stack traces, timestamps and URLs.
  - Colour codes are removed first, and control characters from both the path and the message.
  - A tool needs a one-line-per-problem output format; a multi-line one, like rustfmt's, needs a wrapper (below).
  - At most 200 findings are kept per run, and each message is cut to 1,000 characters.
- **Exit codes.** Exit 0 means no findings. An exit code in `findings_exit` (default `[1]`) must come with at least one finding line. Any other exit, a timeout or a failure to start is a checker failure. Its output is logged and **never** reaches the model, even if a line looks like a finding: a crashing tool, or one that prints a config file it couldn't parse, can't put that text in the prompt. Three failures in a row turn the checker off for the session, with one warning.
- **Cancelling** a turn while a checker runs is neither a result nor a failure. Nothing is reported, and nothing is turned off.
- **Without the `safety` plugin** nothing is checked, and the first edit of a checked file says so in one warning.
- **Probing.** On first use, `command -v` looks for the command's program, skipping leading `NAME=value` words. If it isn't found, the checker is off for the session, with one warning to the user and nothing to the model.
- **What to configure.** Checks with no false positives that look at one file: parse errors, undefined names. Not formatters (they report style) and not project-wide builds (see "Limits").
- **Events.** Each report emits `Custom { source: "diagnostics", name: "introduced" }` (or `"turn_end"`) with the number of findings, for telemetry and the Phase 9 comparison.
- **Optional.** The plugin is its own cargo feature, enabled by default like `stuck`, `planning` and `spill`. With no checkers configured it adds no hook and needs nothing else; it doesn't require the `safety` plugin, and finds `PathGuard` and `Policy` among the session's services when it runs. `plugins.disable = ["diagnostics"]` turns it off.
- **Settable by a project without trust:** `timeout_secs`, `max_report_chars` and `max_files_per_step`. A value outside the ranges above is brought into range with a warning, so a repository can't make a checker run for minutes, nor stop `ch` from starting. **Need trust:** `checkers` and `nudge_at_turn_end`.

### Who decides that a checker may run

A checker runs only when the `safety` plugin's policy would run **the same command as a `bash` call without asking**, and only while the model has the `bash` tool. Before each run, two forms of the command go through `Policy::decide("bash", …)`, and both must be `Allow`:
- the command exactly as it runs, with `"$CH_PATH"`, which no file name can change;
- the command as the model would write it, with the shell-quoted path, which path-specific rules match.

Deciding only on the second form would let a file name with a quote character in it hide the rest of the command from the policy. So:
- in `bypass` mode, configured checkers run;
- in `ask` and `accept-edits` modes, a checker runs only if its command is on the read-only bash allowlist or covered by an allow rule, such as `bash:ruff check *`;
- a `deny` rule that covers the command, including `deny = ["bash"]` or `["bash:*"]`, holds it back.

The first time a checker is held back, the user gets one warning that names the command and how to allow it. The model gets nothing.

**Why this rule.** A checker can hand control to the repository: `npx eslint` runs the repository's `node_modules/.bin/eslint`, an activated virtualenv puts the repository's programs first on `PATH`, eslint's config is JavaScript, and ruff's `extend` reads any file. A security review proved the first: with the earlier rule ("run while bash is available"), a checker in the user's config ran a hostile repository's eslint on every edit, in `ask` mode, with no prompt. Under this rule a checker can never do more than the model could do through `bash` without the user approving it, and it needs no new trust concept.

**Limits of the rule.**
- "Always allow" answers given during a session live in the approver, not the policy, so they don't count until they are saved as rules (answering "always" in the REPL saves one) and a new session starts.
- An allow rule lets the model run the command too. That is the point, but allow narrowly:
  - `bash:awk *` lets the model run any awk program, and awk can run commands through `system()`.
  - `bash:rustfmt *` also allows `RUSTUP_TOOLCHAIN=<a path in the workspace> rustfmt`, which runs the workspace's program. A rule that pins the whole command prefix is safer. For the example below that is `bash:RUSTUP_TOOLCHAIN=stable RUSTUP_AUTO_INSTALL=0 rustfmt --edition 2021 --emit stdout *`.
- The policy judges the command's text. What the program then does, such as which config it loads, is outside it, as for any allowed bash command ([ADR 0001](0001-sandbox-and-agent-security-responsibilities.md): containment belongs to the sandbox).

#### Example configurations

These are documentation, not defaults. Each was run by hand when this ADR was written. In `ask` mode each also needs the allow rules shown.

| Extension | Command | Allow rules | Finds |
|---|---|---|---|
| `.py` | `ruff check --isolated --no-fix --output-format concise --select F63,F7,F82 --stdin-filename={path} -` | `bash:ruff check *` | Syntax errors (always reported), undefined names, statements in the wrong place. `--isolated` ignores the repository's config, so it can't `extend` another file, and also ignores its `per-file-ignores`; `# noqa` comments still apply. Exit 2 means ruff itself failed. |
| `.rs` | rustfmt with an awk wrapper (below) | the pinned rustfmt rule above, and `bash:awk -v p=*` | Parse errors only. Valid code exits 0 however it is formatted, and `mod x;` isn't followed on stdin. |

rustfmt prints each error as `error: …`, with its location on a later ` --> <stdin>:L:C` line. The wrapper joins the two into one line per error. It exits 1 when there was an error, and 2 when rustfmt printed an error with no location (a missing toolchain, say), so that counts as a failure rather than as a clean file. Set the edition your code uses:

```toml
[plugins.diagnostics.checkers.rs]
command = '''RUSTUP_TOOLCHAIN=stable RUSTUP_AUTO_INSTALL=0 rustfmt --edition 2021 --emit stdout 2>&1 >/dev/null | awk -v p={path} '/^error/ { m = $0 } /^ *--> / && m != "" { n = split($0, a, ":"); print p ":" a[n-1] ":" a[n] ": " m; m = ""; bad = 1 } END { exit bad ? 1 : (m != "" ? 2 : 0) }' '''
```

`RUSTUP_TOOLCHAIN` stops a `rust-toolchain.toml` from choosing the program; its `path` can point into the repository.

Tools that are found through the repository, such as `npx`, a virtualenv's `bin` or `node_modules/.bin`, run the repository's code. ESLint is an example: its config is JavaScript, and since version 9 its one-line formatters are separate packages. Such checkers belong in a trusted project's config, with an allow rule only where you would let the model run them too.

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
- **The cache doesn't see config changes.** A checker's config file edited in the middle of a session leaves earlier cached results in place until the content of the checked file changes.
- **The turn's end checks every file the turn changed,** not just `max_files_per_step`, since its results are mostly cached; the per-turn list stops at 100 files.
- **Two kernel issues this plugin exposes,** to fix in the kernel:
  - A turn-end note sent on the last allowed step leaves the turn ending as `StepLimit`, with the note unanswered in history.
  - A refused response is treated as `Done`, so the turn-end check runs, but its note never reaches the model.
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
- **Cost per changed file per step:** at most three stdin runs of the configured checker (before, after, turn start), and usually one, since the others are cache hits. With no checkers configured, nothing runs.
- **The trust model doesn't change.** No program runs that the user or a trusted project didn't configure, and none that the permission policy wouldn't run unasked.
- **No language knowledge in the harness.** Language setups live in the examples above, not in code.
- **Two small changes outside the plugin:**
  - `safety` provides `Policy` as a service;
  - the kernel's `system_reminder` escapes reminder tags inside a note.
- **Roadmap 6.3 is rewritten to point here.**
