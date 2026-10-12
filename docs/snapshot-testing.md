# Snapshot testing in `ch`

This guide explains what a saved snapshot is, why `ch` uses them for its prompts, and how to work with them when you change code or prompts.

## 1. What a snapshot test is

A normal test checks specific facts about a result:

```rust
let prompt = build_prompt(&tools);
assert!(prompt.contains("read_file"));
assert!(!prompt.contains("update_plan"));
```

That works for a few properties. It doesn't work when what matters is the **whole** output, such as a 150-line system prompt, where any added, removed or reworded line changes the model's behaviour.

A snapshot test checks the whole output:

```rust
let prompt = build_prompt(&tools);
insta::assert_snapshot!("tools_full", prompt);
```

1. **The first time it runs**, there is nothing to compare against. The output is saved to a file, the **snapshot**, and you review it once and commit it.
2. **Every later run** compares the new output to the saved snapshot. If they're identical, the test passes. If anything differs, even one character, the test fails and shows a line-by-line diff.

So a saved snapshot is simply **the reviewed, approved output of a piece of code, kept in a file next to the tests**. A failing snapshot test means the output changed. You then decide whether the change is a bug to fix or an intended change to accept.

`ch` uses the [insta](https://insta.rs) library for this.

## 2. Why `ch` needs them

In `ch`, the most important output isn't a number or a return value. It's **the text sent to the model**: the system prompt, the tool-rules block and the shape of every request. That text is assembled from several plugins:

- `prompt-base` adds the base prompt from `prompts/system.md`, then a tool-rules block that depends on which tools are enabled.
- `project-instructions` adds `CH.md` / `AGENTS.md` and an environment block.
- The kernel adds per-step reminders, the history view and the tool specs.

Small changes in any of these change what the model sees. For example: editing a prompt file, enabling or disabling a plugin, changing the order of contributors, or a refactor that accidentally drops a block. Ordinary assertions would miss most of these. Snapshots catch all of them, and they turn every prompt change into a **diff you can review**, both in the test output and in the pull request.

They also act as documentation: the `.snap` files show exactly what the model receives in each configuration, without running anything.

## 3. The snapshots in this repo

| Snapshot file | Test | What it records |
|---|---|---|
| `plugins/prompt-base/src/snapshots/ch_prompt_base__tests__tools_full.snap` | `prompt-base` unit tests | Tool-rules block with all file tools and `bash` |
| `…__tools_full_no_bash.snap` | same | File tools without `bash` |
| `…__tools_bash_only.snap` | same | Bash-only mode ("Working with only a shell") |
| `…__tools_bash_only_extras.snap` | same | Bash-only plus `web_fetch` / `update_plan` notes |
| `…__tools_mixed.snap` | same | An unusual mix of tools |
| `…__tools_none.snap` | same | No tools at all |
| `…__assembled_full.snap` | same | All `prompt-base` system blocks together |
| `crates/ch-cli/tests/snapshots/e2e__assembled_prompts_snapshot.snap` | `crates/ch-cli/tests/e2e.rs` | A full scripted run of `ch -p` on the fixture repo: the model id, the tool list, every system block, and the message sequence of all four model requests |

The end-to-end snapshot is the most complete: if anything about what the model is sent changes, it fails.

## 4. Anatomy of a `.snap` file

A snapshot is a plain text file with a small header:

```
---
source: plugins/prompt-base/src/lib.rs
expression: "blocks.join(\"\\n\\n----\\n\\n\")"
---
You are `ch`, an interactive coding agent that runs in the user's terminal. You work
inside the user's repository: …
```

- `source` is the file containing the test.
- `expression` is the code that produced the value, for orientation.
- Everything after the second `---` is the saved output.

The file name is `<crate>__<module path>__<snapshot name>.snap`. The name comes from the first argument of `assert_snapshot!("tools_full", …)`, or from the test function's name if there is none.

## 5. What happens when a snapshot doesn't match

Suppose you edit `prompts/system.md`:

```diff
- - Refer to code as `path/to/file.rs:42` so the user can jump to it.
+ - Refer to code as `path/to/file.ext:42` so the user can jump to it.
```

and run the tests. Two things happen:

1. The test fails and prints the difference:

   ```
   -old snapshot
   +new results
   ────────────┬───────────────────────────────────────────────────────────
      15       │-- Refer to code as `path/to/file.rs:42` so the user can jump to it.
            15 │+- Refer to code as `path/to/file.ext:42` so the user can jump to it.
   ```

2. insta writes the new output next to the old one as a **pending snapshot**, `<name>.snap.new`. The original `.snap` is not touched.

Nothing changes until you decide. You then either:

- **reject** it: the change was a mistake. Fix the code, and the pending file is discarded; or
- **accept** it: the change is intended. The `.snap.new` replaces the `.snap`, and you commit the updated `.snap` together with the code or prompt change.

## 6. Day-to-day workflow

### With `cargo-insta` (recommended)

Install the companion tool once:

```sh
cargo install cargo-insta
```

Then:

```sh
cargo insta test --review     # run the tests, then step through every changed snapshot
cargo insta review            # review pending .snap.new files from an earlier run
cargo insta accept            # accept all pending snapshots without reviewing
cargo insta reject            # discard all pending snapshots
cargo insta pending-snapshots # list what's pending
```

`review` shows each diff and asks **a**ccept, **r**eject or **s**kip. Prefer it to `accept`, which takes everything without showing you anything.

### Without `cargo-insta`

The `INSTA_UPDATE` environment variable controls what a test run does with changed snapshots:

| Value | Effect |
|---|---|
| `auto` (default) | Write `.snap.new` files locally. In CI (when `CI` is set), write nothing |
| `always` | Overwrite the `.snap` files directly with the new output |
| `new` | Always write `.snap.new` files |
| `no` | Never write anything; just fail |

So, after checking the diffs in the test output:

```sh
INSTA_UPDATE=always cargo test -p ch-prompt-base -p ch-cli
git diff -- '*.snap'          # check what changed before committing
```

### The rule

**A changed `.snap` belongs in the same commit as the change that caused it**, and a reviewer should read it like code. If a `.snap` changes and you can't explain why, don't accept it. Find out first.

`.snap.new` files are local scratch files and are ignored by git.

## 7. Writing a new snapshot test

```rust
#[test]
fn tools_bash_only() {
    let block = rules_for(&["bash"]);
    insta::assert_snapshot!("tools_bash_only", block);
}
```

1. Add `insta` to the crate's `[dev-dependencies]` (`insta.workspace = true`).
2. Write the test and run it. It fails because no snapshot exists yet, and a `.snap.new` is written.
3. Read the new file carefully. This first review is what makes it the approved output.
4. Accept it (`cargo insta review`), and commit the `.snap` with the test.

For short values you can keep the snapshot inside the test file (an **inline snapshot**). `cargo insta` fills in the string for you:

```rust
insta::assert_snapshot!(fmt_bytes(49_357), @"48.2 KB");
```

### Keep snapshots deterministic

A snapshot must be identical on every machine and every run, or it fails randomly. Remove or replace anything that varies before snapshotting:

- **Paths.** Temp directories differ per run. The e2e test replaces the workspace path with `[WS]`:

  ```rust
  let text = describe(&requests).replace(&ws_str, "[WS]");
  ```

- **Dates, the OS, git state, the user's home directory.** The e2e test turns them off at the source with `--set plugins.project-instructions.environment=false` and `user_file=""`.
- **Ordering.** Sort anything that comes from a `HashMap` or a directory listing.
- **Ids, timestamps, durations.** Leave them out, or replace them with placeholders.

### Keep snapshots readable

A snapshot is only useful if a reviewer can understand its diff. Snapshot text, not debug dumps. The e2e test, for example, doesn't snapshot raw JSON requests; it formats each message as one readable line, such as `Assistant: tool_use(read_file {"path":"calc.py"})`.

## 8. In CI

CI sets `CI=true`, so insta writes no files there and a changed snapshot simply fails the build. That guarantees every prompt change on the main branch went through a reviewed, committed `.snap` update.

## 9. When to use a snapshot, and when not to

| Use a snapshot when… | Use ordinary assertions when… |
|---|---|
| the whole output matters and is large (prompts, rendered help, formatted reports) | one or two properties matter (`is_error`, a count, a status) |
| you want changes to show up as reviewable diffs | the output legitimately varies (timings, random ids) |
| writing the expected value by hand would be tedious | a precise rule is the point of the test ("never mentions a tool that isn't registered") |

The two work well together. `prompt-base`, for example, snapshots each prompt shape *and* has a plain test that checks, across 12 tool sets, that the prompt never mentions a tool that isn't registered.

## 10. Quick reference

```sh
cargo test                          # a changed snapshot fails and writes .snap.new
cargo insta review                  # accept or reject each change
INSTA_UPDATE=always cargo test      # accept everything without the tool
git diff -- '*.snap'                # always check before committing
```
