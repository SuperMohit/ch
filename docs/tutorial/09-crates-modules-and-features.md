# 9. Crates, modules, features, lints and statics

The earlier chapters were about code inside a function or a type. This one is about how the code is organized and built: the cargo workspace, crates and their visibility rules, the cargo features that compile plugins in or out, the minimum Rust version, the lints that keep `unsafe` out, and the items that live for the whole program (`const` and `static`).

How Rust compiles and links crates, and why `ch` links plugins at compile time rather than loading them at run time, is covered in [rust-libraries-and-plugin-loading.md](../rust-libraries-and-plugin-loading.md). This chapter assumes that background.

## 1. One workspace, many crates

A **crate** is the unit the compiler builds; a **workspace** builds several crates together with one `Cargo.lock` and one `target/` directory. The kernel, the CLI and every plugin are separate crates. From [Cargo.toml:1](../../Cargo.toml#L1):

```toml
[workspace]
resolver = "3"
members = ["crates/*", "plugins/*"]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.88"
license = "MIT OR Apache-2.0"
publish = false

[workspace.lints.rust]
unsafe_code = "deny"

[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
```

The root also lists every dependency version once under `[workspace.dependencies]`. Member crates inherit settings and versions instead of repeating them. From [Cargo.toml:1](../../plugins/safety/Cargo.toml#L1) of the safety plugin:

```toml
[package]
name = "ch-safety"
description = "Path guard, file state and permission policy"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
publish.workspace = true

[lints]
workspace = true

[dependencies]
anyhow.workspace = true
ch-kernel.workspace = true
```

So every crate is built with the same edition, the same lints, and the same version of `tokio` or `serde`. Two versions of a crate in one build are possible in Rust but costly, and for crates whose types cross plugin boundaries (`serde_json::Value`, `CancellationToken`) they would be incompatible types with the same name.

Why many crates rather than one crate with modules? The crate boundary is enforced: a plugin can only use what `ch-kernel` makes public, and the kernel can't depend on any plugin at all (it isn't in its `Cargo.toml`). The architecture's main rule, "the kernel has no concrete providers, tools, prompts or UI", is checked by the compiler. Separate crates also compile in parallel and rebuild independently.

## 2. Modules and visibility

Inside a crate, `mod name;` includes `name.rs` (or `name/mod.rs`) as a child module. Items are private to their module unless marked otherwise:

| Marker | Visible in |
|---|---|
| (none) | the module and its children |
| `pub(super)` | the parent module (and its children) |
| `pub(crate)` | the whole crate |
| `pub` | everywhere the module itself is reachable |

The kernel's `lib.rs` declares its modules and re-exports the main types at the top, so plugins write `ch_kernel::Tool` instead of `ch_kernel::tool::Tool`. From [lib.rs:51](../../crates/ch-kernel/src/lib.rs#L51):

```rust
pub use tool::{Tool, ToolOutput, ToolSpec};
pub use types::{ContentBlock, Message, Role, StopReason, TerminationReason, ToolCall, Usage};

/// Re-exports plugin crates commonly need, so they agree on versions.
pub mod reexports {
    pub use async_trait::async_trait;
    pub use futures;
    pub use serde_json;
    pub use tokio_util::sync::CancellationToken;
}
```

Visibility markers enforce rules that would otherwise live in comments:

- `RegisterCx`'s fields are `pub(crate)` ([plugin.rs:91](../../crates/ch-kernel/src/plugin.rs#L91)). Only the kernel can build one, so a plugin can't fabricate a registration context for another plugin's id.
- `CancelHandle::begin` and `finish` are `pub(super)` ([cancel.rs:43](../../crates/ch-kernel/src/session/cancel.rs#L43)). Anyone with a handle can cancel, but only the `session` module can start or end a turn.
- The bash splitter in `safety` is a private module that exports three functions. From [mod.rs:11](../../plugins/safety/src/bash/mod.rs#L11):

```rust
mod allowlist;
mod cmd;
mod deny;
mod lexer;

pub use allowlist::{ask_key, segment_is_read_only};
pub use cmd::Cmd;
pub use deny::denied;
```

`safety`'s `lib.rs` declares it as plain `mod bash;`, so even these are crate-private. The lexer can change freely; nothing outside the crate can depend on it.

## 3. Cargo features: one per plugin

A cargo feature is a named switch that turns on optional dependencies and `#[cfg(feature = "...")]` code. `ch-cli` has one per built-in plugin, all on by default. From [Cargo.toml:24](../../crates/ch-cli/Cargo.toml#L24):

```toml
[features]
default = [
    "frontend-repl",
    "frontend-print",
    "provider-scripted", # off at runtime unless enabled in config; used by e2e tests
    "provider-anthropic",
    "provider-openai",
```

Each feature enables one optional dependency, such as `frontend-repl = ["dep:ch-frontend-repl"]` ([Cargo.toml:52](../../crates/ch-cli/Cargo.toml#L52)), declared as `ch-frontend-repl = { workspace = true, optional = true }` ([Cargo.toml:92](../../crates/ch-cli/Cargo.toml#L92)). The `dep:` prefix keeps cargo from also creating an implicit feature named after the dependency. The plugin list then includes a plugin only when its feature is on. From [plugins.rs:13](../../crates/ch-cli/src/plugins.rs#L13):

```rust
#[allow(clippy::vec_init_then_push)]
pub fn builtin_plugins() -> Vec<Arc<dyn Plugin>> {
    #[allow(unused_mut)]
    let mut plugins: Vec<Arc<dyn Plugin>> = Vec::new();

    // Model providers.
    #[cfg(feature = "provider-anthropic")]
    plugins.push(ch_provider_anthropic::plugin());
    #[cfg(feature = "provider-openai")]
    plugins.push(ch_provider_openai::plugin());
```

With a feature off, the `push` line doesn't exist and the crate isn't even compiled. The two `#[allow]`s cover the build with every feature off (`mut` is then unused) and clippy's suggestion to write `vec![...]`, declined so each plugin stays a one-line `push` under its own `#[cfg]`.

Code that uses a feature-gated crate needs both branches to compile. `print_options` is a function the CLI always calls; without the print frontend it returns an error. From [plugins.rs:93](../../crates/ch-cli/src/plugins.rs#L93):

```rust
#[cfg(feature = "frontend-print")]
{
    services.provide(Arc::new(ch_frontend_print::PrintOptions {
        prompt,
        output_format: output_format.parse()?,
        verbose,
    }));
    Ok(())
}
#[cfg(not(feature = "frontend-print"))]
{
    let _ = (services, prompt, output_format, verbose);
    anyhow::bail!("this ch was built without the frontend-print feature, so -p is unavailable")
}
```

`let _ = (...)` uses the parameters so the feature-off build has no unused-variable warnings, which CI would turn into errors. Feature combinations other than the default aren't built by CI, so this is where mistakes hide.

Platform conditions work the same way: `#[cfg(unix)]` selects `hard_links` on Unix and a stub elsewhere ([file_state.rs:186](../../plugins/safety/src/file_state.rs#L186)), and the kernel only depends on `nix` on Unix (`[target.'cfg(unix)'.dependencies]` in [Cargo.toml:27](../../crates/ch-kernel/Cargo.toml#L27)).

## 4. Edition and minimum Rust version

`edition = "2024"` selects the language rules (editions change syntax and defaults, never the ABI, and crates of different editions mix freely). `rust-version = "1.88"` is the **MSRV**, the oldest compiler the workspace promises to build with. Cargo refuses to build on an older one with a clear message instead of strange errors.

Day to day you build with the latest stable (`rust-toolchain.toml` pins the `stable` channel). That means you can use a std API that is newer than 1.88 without noticing. CI checks with the real 1.88 (`cargo +1.88 check --workspace --all-targets --locked`), and so does `just msrv` ([justfile:19](../../justfile#L19)). An example from this repo's own needs: `std::fs::File::try_lock` would do what the session store's file lock does, but it became stable in Rust 1.89, so on 1.88 it fails with E0658 "use of unstable library feature `file_lock`". The store uses the `nix` crate's `Flock` instead ([store.rs:44](../../crates/ch-kernel/src/store.rs#L44)).

Dependencies have MSRVs too. The workspace pins `reedline = "0.49"` with the comment "0.50+ needs rustc 1.95" ([Cargo.toml:70](../../Cargo.toml#L70)).

## 5. Lints, and staying safe without `unsafe`

`unsafe_code = "deny"` makes any `unsafe` block, `unsafe fn` or `unsafe impl` in the workspace a compile error ("usage of an `unsafe` block"). Unlike `forbid`, `deny` can be overridden on a single item with `#[allow(unsafe_code)]`; the repo has none, so that attribute is the thing to look for in review. Clippy's default lints are warnings, and CI runs with `RUSTFLAGS=-D warnings` and `clippy -- -D warnings`, so in practice every warning fails the build.

Code that needs the operating system goes through crates that wrap the unsafe calls in safe APIs. Killing a process group is a `libc::killpg` call underneath, which `nix` exposes as a function returning `Result`. From [group.rs:16](../../plugins/executor-local/src/group.rs#L16):

```rust
/// Sends `signal` to the group, ignoring "no such process".
pub(crate) fn signal_group(pgid: Pid, signal: Signal) {
    if let Err(e) = killpg(pgid, signal)
        && e != Errno::ESRCH
    {
        tracing::warn!("executor-local: killpg({pgid}, {signal}) failed: {e}");
    }
}
```

The `unsafe` code still exists, inside `nix`, where it is shared and reviewed by many users; this repo's own code stays checkable by the compiler. The other places where Rust programs typically reach for `unsafe` are covered by safe std features: catching plugin panics uses `catch_unwind` and `JoinError` ([chapter 7](07-async-with-tokio.md)), and global state uses `Once` and `const fn` constructors (section 6), never `static mut`.

Lints can be relaxed locally with `#[allow(...)]`, as in `builtin_plugins` above. Each such `allow` in the repo has a comment or an obvious reason; treat an unexplained one as a review question.

## 6. `const` and `static`

A `const` is a value, copied into every place that uses it. A `static` is a single place in memory with a fixed address for the whole run. Most of the repo's globals are `const`:

- Lookup tables: `const READ_ONLY_PROGRAMS: &[&str] = &[...]` ([allowlist.rs:5](../../plugins/safety/src/bash/allowlist.rs#L5)).
- Values from `const fn` constructors: `const CANCEL_GRACE: Duration = Duration::from_secs(3);` ([tools.rs:18](../../crates/ch-kernel/src/session/tools.rs#L18)).
- Files compiled into the binary with `include_str!`. From [tracker.rs:11](../../plugins/stuck/src/tracker.rs#L11):

```rust
const REPEAT: &str = include_str!("../../../prompts/stuck-repeat.md");
const FAILING: &str = include_str!("../../../prompts/stuck-failing.md");
const REFUSED: &str = include_str!("../../../prompts/stuck-refused.md");
```

The path is relative to the source file and the file is read at compile time, so a missing prompt is a build error, not a runtime one. `env!("CARGO_PKG_VERSION")` similarly bakes the crate version into the manifest ([lib.rs:108](../../plugins/stuck/src/lib.rs#L108)).

The deny list is a `const` table of structs holding function pointers and non-capturing closures (see [chapter 5](05-traits-and-generics.md#7-function-pointers-and-boxed-closures)). From [deny.rs:19](../../plugins/safety/src/bash/deny.rs#L19):

```rust
const DENY_LIST: &[DenyRule] = &[
    DenyRule {
        name: "rm -rf / or ~",
        check: Check::Segment(rm_recursive_root),
        reason: "Recursively deleting the filesystem root or the home directory is never \
                 allowed. Delete specific paths inside the workspace instead.",
    },
```

A `static` is needed when the item must be one shared object: something with interior mutability, or something whose address matters. The REPL installs its panic hook at most once per process. From [terminal.rs:23](../../plugins/frontend-repl/src/terminal.rs#L23):

```rust
pub fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = terminal::disable_raw_mode();
            let _ = crossterm::execute!(std::io::stdout(), cursor::Show);
            previous(info);
        }));
    });
}
```

As a `const`, every use would create a fresh `Once` and the hook would be installed on every call. A `static` must be `Sync` (all threads see it) and is initialized at compile time, so its initializer must be a `const fn` such as `Once::new()` or `tokio::sync::Mutex::const_new(())` ([run.rs:123](../../crates/ch-cli/tests/run.rs#L123)).

The repo has no `LazyLock` or `OnceLock`, which are std's tools for statics computed at run time (a compiled regex, a parsed config). Configuration and shared state reach plugins through `Cx` and the typed services instead ([chapter 5](05-traits-and-generics.md#6-type-maps-any-and-typeid)), which keeps them per session and replaceable in tests. If you need a global computed value, `static RE: LazyLock<Regex> = LazyLock::new(|| ...)` (stable since 1.80) is the tool to reach for.

## Common mistakes

- **A dependency version in a member crate.** Add it to `[workspace.dependencies]` and use `name.workspace = true`, or two crates can end up on different versions.
- **Using a std API newer than the MSRV.** Local builds on stable pass; CI's 1.88 job fails. Run `just msrv` before pushing when you use a new std method.
- **Testing only the default features.** Code under `#[cfg(not(feature = ...))]` is never compiled by CI. Build with `--no-default-features --features ...` when you change it.
- **`pub` by default.** Start private, widen to `pub(super)` or `pub(crate)` when needed, and make an item `pub` only when it is part of the crate's interface.
- **`static mut` for global state.** Every access needs `unsafe`, which the workspace denies. Use a `static` with a `Mutex`, atomic, `Once` or `LazyLock`.

## Exercises

1. Build a `ch` without `safety`: `cargo build -p ch-cli --no-default-features --features "frontend-print,provider-anthropic,executor-local,tools-fs,prompt-base"`. It should compile. Run `--list-plugins` with the binary in `target/debug/`. What fails, and why at run time rather than at compile time? *Hint:* compare `tools-fs`'s crate dependencies with its `PluginManifest::requires`.
2. Add `let x = unsafe { std::ptr::read(&1u8) };` to any function and run `cargo check` on that crate. Then find where the lint is configured and which crate setting makes it apply.
3. Try to build a `RegisterCx` in a plugin crate's test (`RegisterCx { plugin_id: ..., .. }`). Read the error and connect it to section 2.
4. Change `static ONCE` in [terminal.rs](../../plugins/frontend-repl/src/terminal.rs#L24) to `const ONCE`. It compiles, with a lint warning. Read the warning and explain what would break at run time.
