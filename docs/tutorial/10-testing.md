# 10. Testing

Rust has testing built into the language and cargo: `#[test]` marks a test function, `cargo test` builds a test binary and runs every test in it, in parallel threads. There are three places tests can live, and the choice decides what a test can see. This chapter covers those three, async tests, the fakes the repo uses to test the kernel without a model, and the helpers in `ch-testkit`.

Snapshot tests (`insta`) have their own guide, [snapshot-testing.md](../snapshot-testing.md); this chapter only shows where they fit.

## 1. Unit tests next to the code

A unit test lives in a `tests` module inside the file it tests. From [util.rs:70](../../crates/ch-kernel/src/util.rs#L70):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_strings_unchanged() {
        assert_eq!(truncate_middle("hello", 10), "hello");
    }
```

- `#[cfg(test)]` compiles the module only for `cargo test`. Normal builds don't include it, and it can use `[dev-dependencies]` such as `tempfile` and `insta`, which normal builds don't have. Without the attribute, a `use tempfile` in the module fails to resolve outside tests.
- `use super::*;` imports everything from the parent module, private items included. A child module can see its parent's private items, so unit tests can test private functions directly.

Test modules can be long. When they are, the repo moves them to their own file and declares that file test-only. From [lib.rs:41](../../plugins/safety/src/lib.rs#L41):

```rust
mod bash;
#[cfg(test)]
mod bash_pin;
```

[`bash_pin.rs`](../../plugins/safety/src/bash_pin.rs#L1) runs a corpus of shell commands through the private `bash` module and snapshots every decision, so a refactor of the splitter can't silently change what gets run. Because `bash` is private, this test has to be inside the crate. The REPL does the same with `#[cfg(test)] mod turn_tests;` ([lib.rs:37](../../plugins/frontend-repl/src/lib.rs#L37)).

## 2. Table-driven tests

Many tests in the repo loop over a table of inputs and expected outputs, and pass the input as the assertion message so a failure says which case broke. From [cmd.rs:247](../../plugins/safety/src/bash/cmd.rs#L247):

```rust
#[test]
fn wrapper_option_values_are_not_the_program() {
    let program = |cmd: &str| {
        let p = parse(cmd);
        Cmd::of(&p.segments[0]).program.to_string()
    };
    for (cmd, want) in [
        ("timeout -s KILL 5 rm -rf /", "rm"),
        ("timeout --signal KILL -k 1 5 rm x", "rm"),
        ("timeout -sKILL 5 rm x", "rm"),
        ("env -u X sudo ls", "sudo"),
        ("env -iu X sudo ls", "sudo"),
        ("env -C /tmp sudo ls", "sudo"),
        ("nice -n 5 rm x", "rm"),
        ("nice -n5 rm x", "rm"),
        ("xargs -I {} rm {}", "rm"),
        ("xargs -n 1 -P 4 rm", "rm"),
        ("stdbuf -o L grep x", "grep"),
        ("exec -a name sudo ls", "sudo"),
        ("env -S 'sudo ls'", "env"),
        ("timeout -s KILL", "timeout"),
    ] {
        assert_eq!(program(cmd), want, "{cmd}");
    }
}
```

The `program` closure returns an owned `String` (`.to_string()`), because `Cmd::of(...).program` borrows `p`, a local of the closure that is dropped when it returns ([chapter 2](02-lifetimes.md)). Adding a case is one line. The trade-off: the first failing case stops the test, so later cases aren't reported.

## 3. Integration tests in `tests/`

Each `.rs` file in a crate's `tests/` directory is compiled as a **separate crate** that depends on the crate under test. It sees only the public API, exactly as a plugin would. That makes `tests/` the right place to check that the public interface is enough.

The kernel's integration tests use fake plugins only, which proves the kernel doesn't depend on any real one (the testing strategy in [architecture.md](../architecture.md#9-testing-strategy)). A fake model provider replays scripted responses. A whole fake plugin can be built from a closure. From [pipeline.rs:229](../../crates/ch-kernel/tests/pipeline.rs#L229):

```rust
/// A plugin assembled from closures, for tests.
struct TestPlugin {
    manifest: PluginManifest,
    #[allow(clippy::type_complexity)]
    register: Box<dyn Fn(&mut RegisterCx<'_>) -> anyhow::Result<()> + Send + Sync>,
}

impl Plugin for TestPlugin {
    fn manifest(&self) -> PluginManifest {
        self.manifest.clone()
    }
    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()> {
        (self.register)(cx)
    }
}

fn plugin(
    id: &str,
    f: impl Fn(&mut RegisterCx<'_>) -> anyhow::Result<()> + Send + Sync + 'static,
) -> Arc<dyn Plugin> {
    Arc::new(TestPlugin {
        manifest: PluginManifest::new(id, "test"),
        register: Box::new(f),
    })
}
```

A test can then write `plugin("tools", |cx| { cx.add_tool(EchoTool); Ok(()) })`. The closure is boxed as `dyn Fn` because the struct can't be generic over each test's closure type and still be one `TestPlugin` type. The parentheses in `(self.register)(cx)` matter: they call the closure stored in the field. `self.register(cx)` would call the *method* `Plugin::register`, that is, this function itself, and recurse forever.

Because `tests/` files only see public items, anything a test needs from inside has to be public or tested from a unit test instead. The repo uses both: `ch-kernel` exposes `Cx::detached` for tests and offline use ([cx.rs:62](../../crates/ch-kernel/src/cx.rs#L62)), while the bash splitter is tested from inside its crate.

## 4. Async tests

A test that awaits needs a runtime. `#[tokio::test]` wraps the function in one (single-threaded by default). From [pipeline.rs:383](../../crates/ch-kernel/tests/pipeline.rs#L383):

```rust
#[tokio::test]
async fn panicking_tool_becomes_error_observation() {
    let mut h = harness(
        vec![
            Ok(tool_reply(&[("t1", "boom", json!({}))])),
            Ok(text_reply("ok")),
        ],
        vec![],
        config(),
    );
    let r = h.session.run_turn("go").await;
    assert_eq!(r.reason, TerminationReason::Done);
    let results = tool_results(&h.session.history().events()[2].message);
    assert!(results[0].2);
    assert!(results[0].1.contains("kaboom"));
}
```

Plain `#[test]` on an `async fn` fails with "async functions cannot be used for tests". The test above also shows the kernel guarantee from [chapter 7](07-async-with-tokio.md): a tool that panics with `kaboom` doesn't end the turn; the model gets the panic message as an error result and the turn finishes `Done`.

Tests in one binary run in parallel threads. Tests that touch shared process state must be serialized; the CLI tests use a `static` tokio mutex for that ([run.rs:123](../../crates/ch-cli/tests/run.rs#L123), explained in [chapter 6](06-shared-state.md#5-std-mutex-vs-tokio-mutex)).

## 5. Tests that need the network

Tests against the real model APIs are marked `#[ignore]` with a reason, so `cargo test` skips them and CI needs no API keys. From [live.rs:25](../../plugins/provider-anthropic/tests/live.rs#L25):

```rust
#[tokio::test]
#[ignore = "needs ANTHROPIC_API_KEY and network"]
async fn streamed_tool_use_round_trip() {
```

Run them with `cargo test -p ch-provider-anthropic --test live -- --ignored`. Everything before `--` goes to cargo; everything after goes to the test binary.

## 6. Doc tests

Code blocks in `///` comments are compiled and run as tests by `cargo test`, so examples in the documentation can't go stale. From [lib.rs:167](../../crates/ch-testkit/src/lib.rs#L167):

```rust
/// Builds a [`Cx`] for tests.
///
/// ```
/// use std::sync::Arc;
/// use ch_testkit::TestCx;
///
/// struct MyService;
/// let cx = TestCx::new("/tmp/workspace")
///     .service(Arc::new(MyService))
///     .build();
/// assert!(cx.services.get::<MyService>().is_some());
/// ```
```

A doc test is compiled like an integration test, against the public API, and needs the full `use` paths.

## 7. Shared conformance checks: `ch-testkit`

Every tool must follow the same rules: a snake_case name, a valid schema, a description with the four sections, and bad input returned as an error observation rather than a panic. `ch-testkit` checks them for any tool, so each plugin's tests call one function instead of repeating the checks. The panic check has to catch a panic inside an async call. From [lib.rs:126](../../crates/ch-testkit/src/lib.rs#L126):

```rust
pub async fn assert_errors_are_observations(tool: &dyn Tool, bad_inputs: &[Value], cx: &Cx) {
    let name = tool.spec().name;
    for input in bad_inputs {
        let result = AssertUnwindSafe(tool.call(input.clone(), cx))
            .catch_unwind()
            .await;
```

`catch_unwind` (here the `futures` version for futures) turns a panic into an `Err`, so the helper can report which input caused it instead of just failing. `AssertUnwindSafe` is the caller's promise that observing state after a panic is fine; the compiler can't prove that for an arbitrary future holding references.

## 8. Snapshots and temporary directories

Large outputs, such as the system prompt for a given set of tools, are checked with `insta` snapshots. From [lib.rs:215](../../plugins/prompt-base/src/lib.rs#L215):

```rust
#[test]
fn snapshot_full_tools() {
    insta::assert_snapshot!("tools_full", rules_for(FULL));
}
```

The workflow (reviewing, accepting, keeping snapshots deterministic) is in [snapshot-testing.md](../snapshot-testing.md).

Tests that touch the file system create a fresh directory with `tempfile::tempdir()`, which deletes itself when the returned value is dropped. Keep the value in a variable for the whole test: `let tmp = tempfile::tempdir().unwrap();` ([store.rs:107](../../plugins/planning/src/store.rs#L107)). Writing `let path = tempfile::tempdir().unwrap().path().to_path_buf();` deletes the directory at the end of that statement, and the test then works on a path that no longer exists.

## Common mistakes

- **A `tests` module without `#[cfg(test)]`.** It is compiled into the normal build, where dev-dependencies don't exist.
- **Testing internals from `tests/`.** Integration tests see only `pub` items. Test private code from a unit test instead of making it `pub` for the test.
- **`#[test]` on an `async fn`.** Use `#[tokio::test]`.
- **Output that depends on `HashMap` order, the clock, the home directory or the temp path.** The test passes locally and fails elsewhere. Sort, inject, or replace with placeholders, as the e2e snapshot does with `[WS]`.
- **Dropping the `TempDir` too early.** Bind it to a variable that lives as long as the test.

## Exercises

1. Add a table-driven unit test for `is_root_or_home` in [deny.rs](../../plugins/safety/src/bash/deny.rs#L130), covering `/`, `~/`, `$HOME/*`, `/tmp` and the empty string.
2. Add an integration test to `plugins/safety/tests/plugin.rs` checking that `git -C repo push origin main` is denied. *Hint:* copy `hook_applies_the_policy` and its `verdict` helper.
3. Write a doc test for [`first_line`](../../crates/ch-kernel/src/util.rs#L26) and run it with `cargo test -p ch-kernel --doc`.
4. Move the `services.rs` unit test ([services.rs:54](../../crates/ch-kernel/src/services.rs#L54)) into `crates/ch-kernel/tests/` as an integration test. What do you have to change, and what can't the test see any more?
