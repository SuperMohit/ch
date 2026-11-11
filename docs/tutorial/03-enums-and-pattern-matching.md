# 3. Enums and pattern matching

Rust enums are **sum types**: a value is exactly one of the variants, and each variant carries its own data. Patterns take those values apart. Together they replace most of what other languages do with class hierarchies, nullable fields and flag combinations, and the compiler checks that every case is handled.

`ch` uses enums for nearly every decision that crosses a plugin boundary: what a hook decided, why a turn ended, what a model stream produced.

## 1. Variants that carry exactly their data

A hook can let a call continue, deny it with a reason, ask the user, rewrite its input, add a note, or stop the turn. From [hook.rs:134](../../crates/ch-kernel/src/hook.rs#L134):

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum HookDecision {
    Continue,
    Deny {
        reason: String,
    },
    /// Ask the user. `key` identifies what "always allow" would cover for the rest of
    /// the session (e.g. `bash:git commit`); `None` means it can't be remembered.
    Ask {
        reason: String,
        #[serde(default)]
        key: Option<String>,
    },
    Rewrite {
        input: Value,
    },
    AppendNote {
        text: String,
    },
    Stop {
        reason: String,
    },
}
```

Compare a struct with `kind`, `reason: Option<String>`, `key: Option<String>` and `input: Option<Value>`: nothing would stop a `Continue` with a reason, or a `Rewrite` without input. With the enum those states can't be built. The `serde(tag = ...)` attribute serializes it as one flat JSON object, `{"decision": "deny", "reason": "..."}`, with the variant name in the `decision` field.

## 2. Exhaustive matches, and when to give them up

A `match` must cover every variant. That is the main reason to use enums: when someone adds a variant, every match that doesn't handle it stops compiling. From [run.rs:43](../../plugins/frontend-print/src/run.rs#L43):

```rust
pub fn exit_code(reason: &TerminationReason) -> i32 {
    match reason {
        TerminationReason::Done => 0,
        TerminationReason::Error { .. } => 1,
        TerminationReason::StepLimit => 2,
        TerminationReason::ContextOverflow { .. } => 3,
        TerminationReason::HookStop { .. } => 4,
        TerminationReason::PromptDenied { .. } => 5,
        TerminationReason::Cancelled => 130,
    }
}
```

There is no `_` arm, on purpose. A new `TerminationReason` must get its own exit code, and E0004 ("non-exhaustive patterns") forces the author to pick one. `{ .. }` ignores a variant's fields.

A wildcard is right when new variants should get the default. From [hook.rs:126](../../crates/ch-kernel/src/hook.rs#L126):

```rust
pub fn tool_call(&self) -> Option<&ToolCall> {
    match self {
        Self::PreTool { call, .. } | Self::PostTool { call, .. } => Some(call),
        _ => None,
    }
}
```

A future hook point that isn't about a tool call should return `None`, so `_` is correct here. Ask the question for each match you write: should a new variant break this code?

## 3. Guards

A guard (`if ...` after a pattern) adds a condition. If it is false, matching continues with the next arm. The hook chain ignores rewrites during its second pass. From [hook.rs:289](../../crates/ch-kernel/src/hook.rs#L289):

```rust
HookDecision::Rewrite { input } if allow_rewrite => {
    rewritten |= apply_rewrite(event, input, &id);
}
HookDecision::Rewrite { .. } => {
    tracing::debug!(hook = %id, "rewrite ignored while re-checking a rewritten input");
}
```

The guard pattern for I/O errors appears all over the repo: treat "not found" specially and pass everything else on. From [file_state.rs:126](../../plugins/safety/src/file_state.rs#L126):

```rust
let current = match std::fs::read(path) {
    Ok(bytes) => bytes,
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
    Err(e) => return Err(format!("Could not read `{shown}` to check it: {e}.")),
};
```

Guards are not part of the exhaustiveness check: the compiler doesn't reason about the conditions, so the unguarded `Err(e)` arm is still required.

## 4. Matching several values at once

Matching a tuple handles combinations without nested `if`s. The stuck detector classifies a finished call from two booleans. From [lib.rs:188](../../plugins/stuck/src/lib.rs#L188):

```rust
let mut tracker = self.tracker();
let outcome = match (*not_run, output.is_error) {
    (true, _) if tracker.take_refused(&call.id) => Outcome::Refused,
    (true, _) => return HookDecision::Continue,
    (false, true) => Outcome::Failed(&output.content),
    (false, false) => Outcome::Ok,
};
tracker.record(&call.name, &call.input, outcome, cfg)
```

The guard here has a side effect (`take_refused` removes the id from a set). That is safe because a guard only runs when its pattern matches, and arms are tried in order.

Tuples combine well with literals and bindings. The plan check returns a different error for zero and for several items in progress. From [plan.rs:85](../../plugins/planning/src/plan.rs#L85):

```rust
match (is_open(plan), in_progress) {
    (false, _) | (true, 1) => Ok(()),
    (true, 0) => Err(
        "no item is `in_progress`. Mark the step you are working on \
         `in_progress` (exactly one), or mark every item `completed`"
            .into(),
    ),
    (true, n) => Err(format!(
        "{n} items are `in_progress`. Keep exactly one `in_progress` and set the \
         others to `pending` or `completed`"
    )),
}
```

`(true, n)` binds whatever count is left after the earlier arms. Patterns can also nest: the CLI matches `(&cli.print, &cli.prompt)` against `(Some(Some(p)), Some(rest))` to catch `-p` taking only the first word of a task ([app.rs:69](../../crates/ch-cli/src/app.rs#L69)).

## 5. Or-patterns, ranges, slices and `@`

`|` inside a pattern matches any alternative. When alternatives bind variables, each must bind the same names with the same types. From [model_call.rs:106](../../crates/ch-kernel/src/session/model_call.rs#L106):

```rust
if let Verdict::Deny { hook, reason } | Verdict::Stop { hook, reason } = outcome.verdict {
    return Err(TerminationReason::HookStop {
        source: hook,
        reason,
    });
}
```

Integer and character ranges are patterns too. HTTP statuses map to retry decisions in [errors.rs:103](../../plugins/provider-anthropic/src/errors.rs#L103):

```rust
429 => Failure::retry(ModelError::RateLimited(message), retry_after),
408 | 409 | 500..=599 => Failure::retry(
    ModelError::Unavailable(format!("HTTP {status}: {message}")),
    retry_after,
),
413 => Failure::fatal(ModelError::ContextOverflow(message)),
400 if is_context_overflow(&message) => {
    Failure::fatal(ModelError::ContextOverflow(message))
}
```

Slice patterns match on length and contents. When several hooks ask for approval, only a single ask can be remembered with "always". From [tools.rs:217](../../crates/ch-kernel/src/session/tools.rs#L217):

```rust
let (reason, key) = match pending.as_slice() {
    [] => return (Prepared::Run { call, tool, notes }, None),
    [one] => (one.reason.clone(), one.key.clone()),
    // No single key covers several asks, so "always" isn't offered.
    many => (
        many.iter()
            .map(|a| a.reason.as_str())
            .collect::<Vec<_>>()
            .join("; "),
        None,
    ),
};
```

`name @ pattern` binds the whole value while also testing its shape. From [stream.rs:310](../../plugins/provider-anthropic/src/stream.rs#L310):

```rust
match serde_json::from_str::<Value>(raw) {
    Ok(v @ Value::Object(_)) => v,
    _ => json!({ "_invalid_json": raw }),
}
```

## 6. `matches!`

`matches!(value, pattern)` is a `match` that returns `bool`. It reads well for allowlists and for testing a variant without caring about its data. From [mod.rs:111](../../plugins/safety/src/bash/mod.rs#L111):

```rust
fn is_harmless_target(target: &str) -> bool {
    matches!(target, "/dev/null" | "/dev/stdout" | "/dev/stderr")
}
```

It accepts guards and nested or-patterns: `matches!(answer, Some(Approval::Allow | Approval::AllowAlways))` ([tools.rs:254](../../crates/ch-kernel/src/session/tools.rs#L254)) tests two variants at once and ignores the data of `Deny`. Unlike `==`, `matches!` doesn't need the type to implement `PartialEq`.

## 7. `let ... else`

`let PATTERN = EXPR else { DIVERGE };` binds on success and otherwise runs a block that must leave the function (`return`, `break`, `continue`, a panic). It keeps the happy path unindented. Hooks use it to pick out the event they handle. From [lib.rs:121](../../plugins/safety/src/lib.rs#L121):

```rust
let HookEvent::PreTool { call, read_only } = event else {
    return HookDecision::Continue;
};
```

Patterns in `let ... else` can include literals. The spill hook only handles results of calls that actually ran. From [lib.rs:133](../../plugins/spill/src/lib.rs#L133):

```rust
let HookEvent::PostTool {
    call,
    output,
    not_run: false,
} = event
else {
    return HookDecision::Continue;
};
```

The `else` block can't use the value that failed to match. That is why tools still write `match parse_input(...) { Ok(i) => i, Err(e) => return e }` ([read.rs:75](../../plugins/tools-fs/src/read.rs#L75)): the error is needed in the early return.

## 8. Let chains

`if let` and boolean conditions can be joined with `&&`, and each binding is visible in the conditions after it. From [allowlist.rs:115](../../plugins/safety/src/bash/allowlist.rs#L115):

```rust
if let Some(second) = cmd.args.first()
    && !second.starts_with('-')
{
    key.push(' ');
    key.push_str(&globset::escape(second));
}
```

Longer chains replace a staircase of nested `if let`s. The project-instructions cache checks four things before using a cached value. From [lib.rs:162](../../plugins/project-instructions/src/lib.rs#L162):

```rust
if let Ok(guard) = self.cache.lock()
    && let Some((sid, ws, blocks)) = guard.as_ref()
    && **sid == *cx.session_id
    && *ws == workspace
{
    return blocks.clone();
}
```

Let chains need edition 2024 and were stabilized in Rust 1.88, which is the repo's minimum version (`rust-version` in [Cargo.toml](../../Cargo.toml#L8)). In edition 2021 the same code fails with "let chains are only allowed in Rust 2024 or later".

## 9. `loop` as an expression, and labeled breaks

`loop` can return a value with `break value`. The turn loop produces the reason the turn ended and whether `turn_end` hooks already ran. From [mod.rs:383](../../crates/ch-kernel/src/session/mod.rs#L383):

```rust
let (reason, turn_end_ran) = loop {
    if cancel.is_cancelled() {
        break (TerminationReason::Cancelled, false);
    }
    if step >= max_steps {
        break (TerminationReason::StepLimit, false);
    }
    let cx = self.cx(step, cancel.clone());
    let response = match self.call_model(&cx, step, &mut reminders).await {
        Ok(r) => r,
        Err(reason) => break (reason, false),
    };
```

Every exit names its reason, and the compiler checks that all `break`s produce the same type. Without this, the code would need `let mut reason = None;` and an `unwrap` after the loop.

Labels let an inner loop break an outer one: `Cmd::of` starts with `'words: loop` and leaves it from a nested `while` with `break 'words;` ([cmd.rs:55](../../plugins/safety/src/bash/cmd.rs#L55)).

## Common mistakes

- **A reflexive `_ =>` on your own enum.** It silences the compiler exactly when a new variant needs attention. Use it only where "anything else" really has one answer.
- **Matching against a variable name.** In `match s { expected => ... }`, `expected` is a new binding that matches everything; it doesn't compare with an outer `expected`. Use a guard (`x if x == expected`), a literal, or a `const`.
- **Expecting guards to count for exhaustiveness.** `Some(n) if n > 0` and `Some(n) if n <= 0` still need a `Some(_)` arm.
- **Converting `match` to `let ... else` when the error is needed.** The `else` block can't see the value that didn't match.

## Exercises

1. Add a variant `Paused` to `TerminationReason` in [types.rs:172](../../crates/ch-kernel/src/types.rs#L172) and run `cargo check --workspace --all-targets`. List the places the compiler flags. Which matches compile anyway, and should they? Revert afterwards.
2. Write a test for [`from_status`](../../plugins/provider-anthropic/src/errors.rs#L96) checking that 503 is retryable, 401 is fatal `Auth`, and 400 with "prompt is too long" in the body is `ContextOverflow`. *Hint:* `matches!(f.error, ModelError::Auth(_))`.
3. Rewrite [`parse_approval_key`](../../plugins/frontend-repl/src/approval.rs#L115) to match `KeyCode::Char(c)` with a guard using `eq_ignore_ascii_case`, and run `cargo test -p ch-frontend-repl`. Which version is clearer?
4. Rewrite the `match` in [`ReadFile::call`](../../plugins/tools-fs/src/read.rs#L75) with `let ... else` and see why it doesn't fit. *Hint:* what would the `else` block return?
