# 4. Option, Result and errors

Rust has no null and no exceptions. A value that may be missing is an `Option<T>`; an operation that may fail returns a `Result<T, E>`. Both are ordinary enums, so the previous chapter's patterns work on them, but most code uses their methods (**combinators**) and the `?` operator instead.

This chapter covers those tools, then how `ch` reports errors: `anyhow` where errors only need to be shown, `thiserror` where callers branch on them, plain values where the reader is the model, and panics for bugs.

## 1. Combinators instead of nested matches

A combinator transforms an `Option` or `Result` without unpacking it. `FileState::record_read` uses four in a row. From [file_state.rs:70](../../plugins/safety/src/file_state.rs#L70):

```rust
let hash = content_hash(content);
let mut map = self.lock();
let prev = map.get(path).filter(|e| e.hash == hash);
let full_read = full || prev.is_some_and(|e| e.full_read);
let seen = prev.map(|e| e.seen.clone()).unwrap_or_default();
```

- `filter` keeps `Some` only if the predicate holds: an entry for older content counts as no entry.
- `is_some_and` asks "is it `Some`, and does the value pass?" in one call. `is_none_or` (used in [store.rs:38](../../plugins/planning/src/store.rs#L38)) is its mirror.
- `map` transforms the value inside, and `unwrap_or_default` supplies an empty `Vec` for `None`.

The same with `match` would take a dozen lines and a mutable variable.

`bool` has two conversions into `Option`. From [cmd.rs:166](../../plugins/safety/src/bash/cmd.rs#L166):

```rust
let attached = match a.strip_prefix("--split-string=") {
    Some(value) => Some(value),
    None if a == "--split-string" => Some(""),
    None => value_option(ENV_VALUE_OPTIONS, a)
        .and_then(|(c, rest)| (c == 'S').then_some(rest)),
};
```

`cond.then_some(x)` is `if cond { Some(x) } else { None }`. It evaluates `x` first, which is fine for a slice that already exists. When building the value costs something, use `then` with a closure, as the stuck detector does for its stop message ([tracker.rs:136](../../plugins/stuck/src/tracker.rs#L136)): the `format!` only runs when the condition is true. `and_then` chains a step that can itself return `None`.

Two more conversions show up often: `Option::transpose` turns `Option<Result<T, E>>` into `Result<Option<T>, E>`, so `?` can apply to an optional fallible step (`self.store.as_ref().map(|s| s.lock(&id)).transpose()?` in [builder.rs:138](../../crates/ch-kernel/src/session/builder.rs#L138)); and `as_deref` turns `&Option<String>` into `Option<&str>`.

## 2. `?` on `Option` and `Result`

`expr?` unwraps a success and returns early with the failure. In a function returning `Option`, it works on `Option`s. From [config.rs:102](../../crates/ch-kernel/src/config.rs#L102):

```rust
pub fn get_value(&self, path: &str) -> Option<&Value> {
    let mut parts = path.split('.');
    let mut current = self.root.get(parts.next()?)?;
    for part in parts {
        current = current.as_table()?.get(part)?;
    }
    Some(current)
}
```

Each `?` is a place where the path can fail to exist, and there is no `if` in sight. Mixing kinds needs an explicit conversion: `.ok()` turns a `Result` into an `Option` (dropping the error), and `.ok_or(e)` / `.ok_or_else(|| e)` go the other way. The plan store reads its file that way, logging rather than failing. From [store.rs:52](../../plugins/planning/src/store.rs#L52):

```rust
fn load(cx: &Cx) -> Option<Plan> {
    let path = cx.session_dir.as_ref()?.join(FILE);
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes)
        .inspect_err(|e| tracing::warn!("ignoring {}: {e}", path.display()))
        .ok()
}
```

`?` also works inside closures that return `Option`. The retry code reads a header in one line ([retry.rs:26](../../plugins/provider-anthropic/src/retry.rs#L26)): `let get = |name: &str| headers.get(name)?.to_str().ok()?.trim().parse::<f64>().ok();`.

On a `Result`, `?` converts the error with `From::from` before returning it. That is how `?` on an `io::Error` works in a function returning `anyhow::Result`, and how the spill plugin unwraps two layers at once. From [lib.rs:181](../../plugins/spill/src/lib.rs#L181):

```rust
async fn save(
    session_dir: PathBuf,
    call_id: String,
    tool: String,
    content: String,
    max_bytes: u64,
) -> anyhow::Result<PathBuf> {
    let task = tokio::task::spawn_blocking(move || {
        store::save(&session_dir, &call_id, &tool, &content, max_bytes)
    });
    Ok(task.await??)
}
```

`task.await` is `Result<anyhow::Result<PathBuf>, JoinError>`. The first `?` handles the task failing (a panic, say) and converts the `JoinError` into `anyhow::Error`; the second returns the error from `store::save`.

## 3. Collecting into a `Result`

An iterator of `Result`s can be collected into `Result<Vec<_>, _>`. It stops at the first error and returns it. From [rules.rs:93](../../plugins/safety/src/rules.rs#L93):

```rust
pub fn parse_rules(list: &[String], field: &str) -> anyhow::Result<Vec<Rule>> {
    list.iter()
        .map(|r| Rule::parse(r).with_context(|| format!("plugins.safety.{field}")))
        .collect()
}
```

The return type tells `collect` what to build. The same works for `Option<Vec<_>>`.

## 4. `anyhow`: errors that only need to be reported

Most failures in `ch` end as a message to the user: a bad config file, a plugin that can't start. For those, the code returns `anyhow::Result<T>`, which holds any error type plus a chain of context messages. From [lib.rs:78](../../plugins/safety/src/lib.rs#L78):

```rust
fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
    let config: SafetyConfig = cx.config().context("invalid [plugins.safety] config")?;
    let guard = Arc::new(PathGuard::new(cx.workspace()).with_context(|| {
        format!(
            "workspace root `{}` can't be resolved",
            cx.workspace().display()
        )
    })?);
```

- `.context(msg)` wraps an error with a message saying what was being done.
- `.with_context(|| ...)` does the same lazily; use it whenever the message needs `format!`, so nothing is allocated on success.
- `bail!(...)` returns an error built from a format string; `anyhow!(...)` builds one without returning.
- `Context` also works on `Option`: `.context("no model provider plugin is enabled")` turns `None` into an error ([registry.rs:388](../../crates/ch-kernel/src/registry.rs#L388)).

The registry adds its own layer, ``registering plugin `{id}` `` ([registry.rs:185](../../crates/ch-kernel/src/registry.rs#L185)). How the chain prints depends on the format. From [main.rs:32](../../crates/ch-cli/src/main.rs#L32):

```rust
match result {
    Ok(code) => code,
    Err(e) => {
        tracing::error!("{e:#}");
        eprintln!("error: {e:#}");
        1
    }
}
```

- `{e}` prints only the outermost message: ``registering plugin `safety` ``. True, but useless.
- `{e:#}` prints the whole chain on one line, joined by `: `, ending with the underlying TOML error that names the bad value.
- `{e:?}` prints the chain on several lines under "Caused by:", plus a backtrace when enabled.

That is why every user-facing error in the repo uses `{e:#}`. A config test relies on it to check that the inner message names the misspelled key ([config.rs:359](../../crates/ch-kernel/src/config.rs#L359)).

## 5. `thiserror`: errors that callers branch on

When the caller needs to decide what to do, a string isn't enough. The kernel treats a context overflow differently from a cancellation and from a network failure, so providers return a typed error. From [model.rs:66](../../crates/ch-kernel/src/model.rs#L66):

```rust
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("context window exceeded: {0}")]
    ContextOverflow(String),
    #[error("rate limited: {0}")]
    RateLimited(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("provider unavailable: {0}")]
    Unavailable(String),
    #[error("cancelled")]
    Cancelled,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
```

`thiserror` generates the `Display` and `std::error::Error` impls from the attributes. `#[from]` also generates `From<anyhow::Error>`, so `?` on an `anyhow::Result` inside a provider produces `ModelError::Other`. `#[error(transparent)]` displays the inner error unchanged. The kernel then maps the variants to how the turn ends. From [model_call.rs:129](../../crates/ch-kernel/src/session/model_call.rs#L129):

```rust
let response = self
    .stream_model(request, cancel)
    .await
    .map_err(|e| match e {
        ModelError::Cancelled => TerminationReason::Cancelled,
        ModelError::ContextOverflow(message) => {
            TerminationReason::ContextOverflow { message }
        }
        e => TerminationReason::Error {
            message: e.to_string(),
        },
    })?;
```

You can also write the two impls by hand, as `tool-web-fetch` does for small errors it passes to `reqwest` ([fetch.rs:72](../../plugins/tool-web-fetch/src/fetch.rs#L72)): a `Display` impl plus an empty `impl std::error::Error for TooManyRedirects {}`. The same file names `Box<dyn std::error::Error + Send + Sync>` as `BoxError` ([fetch.rs:13](../../plugins/tool-web-fetch/src/fetch.rs#L13)), the type `reqwest`'s resolver interface requires. `Send + Sync` lets the error cross threads.

A rule of thumb that fits this repo: `anyhow` in application code and at plugin registration, `thiserror` enums at interfaces where the caller matches on the kind.

## 6. Errors as values for the model

A tool error isn't a failure of `ch`; it is information for the model, which should read it and try something else. So `Tool::call` doesn't return a `Result` at all. From [tool.rs:107](../../crates/ch-kernel/src/tool.rs#L107):

```rust
/// Runs the tool. Return errors as `ToolOutput::error`; a panic is also caught and
/// turned into an error observation, but don't rely on that. `cx.cancel` is
/// cancelled when the user interrupts the turn.
async fn call(&self, input: Value, cx: &Cx) -> ToolOutput;
```

Inside the tools, helpers return `Result<T, ToolOutput>`, so the error is already the finished observation, written to say how to fix the problem. From [common.rs:59](../../plugins/tools-fs/src/common.rs#L59):

```rust
pub fn parse_input<T: DeserializeOwned>(
    tool: &str,
    input: Value,
    usage: &str,
) -> Result<T, ToolOutput> {
    serde_json::from_value(input)
        .map_err(|e| ToolOutput::error(format!("Invalid input for {tool}: {e}. Expected {usage}.")))
}
```

The safety services do the same with `Result<_, String>`: `FileState::check_writable` returns a sentence like "You haven't read `x` yet. Read the whole file with read_file before editing it." The type says "this message goes to a reader", not "propagate me".

## 7. Panics are for bugs

A panic unwinds the thread and is meant for states that can't happen if the code is correct. The repo uses `expect` with a message saying why it can't fail (`serde_json::to_vec_pretty(plan).expect("a plan serializes")` in [store.rs:67](../../plugins/planning/src/store.rs#L67)) and `unreachable!` after moving a value through hooks that can't change its variant ([tools.rs:118](../../crates/ch-kernel/src/session/tools.rs#L118)).

Because plugins are third-party code from the kernel's point of view, the kernel also survives their bugs: it runs each tool in its own task and turns a panic into an error observation ([tools.rs:332](../../crates/ch-kernel/src/session/tools.rs#L332), covered in [chapter 7](07-async-with-tokio.md)). Panics also interact with locks; see poisoning in [chapter 6](06-shared-state.md).

## Common mistakes

- **`.unwrap()` on input you don't control.** A config value, a file or a model response can always be wrong. Return an error with context.
- **`.context(format!(...))`.** The string is built even on success. Use `.with_context(|| format!(...))`.
- **Printing an `anyhow::Error` with `{e}`.** You get the outermost context and lose the cause. Use `{e:#}`.
- **`then_some(expensive())`.** The argument is evaluated before `then_some` is called. Use `then(|| expensive())`.
- **`String` errors where callers must branch.** Matching on message text breaks when someone rewords it. Use an enum.

## Exercises

1. Write a test for [`Config::get_value`](../../crates/ch-kernel/src/config.rs#L102) where a middle segment isn't a table (`a.b.c` with `a.b = 1`). Which `?` returns `None`?
2. Write an integration test in `plugins/safety/tests/plugin.rs` that builds a registry with `RegistryBuilder::build` and the config `[plugins.safety]\nmode = "nope"`, and prints the error with `{}`, `{:#}` and `{:?}`. *Hint:* `ch_testkit::build_registry` panics on errors, so call the kernel directly; `ch-kernel` is already a dependency of the crate.
3. Add a variant `ModelError::Quota(String)` in [model.rs](../../crates/ch-kernel/src/model.rs#L66). Which `match` in the kernel handles it without changes, and how does it end the turn? Revert afterwards.
4. Rewrite [`load`](../../plugins/planning/src/store.rs#L52) with `match` and no `?`, then compare the two versions for length and for how easy it is to see every failure case.
