# 5. Traits, generics and trait objects

A **trait** names a set of methods a type can implement. Traits are how `ch` is extensible: every extension point (tool, hook, provider, approver, frontend, ...) is a trait in `ch-kernel`, and every plugin implements some of them. The table in [architecture.md](../architecture.md#2-extension-points) lists them all.

Code that uses a trait can do so in two ways. **Generics** (`impl Trait`, `<T: Trait>`) are resolved at compile time: the compiler makes a copy of the function for each concrete type. **Trait objects** (`dyn Trait`) are resolved at run time through a table of function pointers (a vtable). This chapter shows where the repo uses each and why.

## 1. A trait as an extension point

From [tool.rs:103](../../crates/ch-kernel/src/tool.rs#L103):

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    /// Runs the tool. Return errors as `ToolOutput::error`; a panic is also caught and
    /// turned into an error observation, but don't rely on that. `cx.cancel` is
    /// cancelled when the user interrupts the turn.
    async fn call(&self, input: Value, cx: &Cx) -> ToolOutput;

    /// One-line human summary of a call for frontends, e.g. `src/main.rs` for
    /// `read_file`. Defaults to the compact JSON input.
    fn summarize(&self, input: &Value) -> String {
        let s = input.to_string();
        crate::util::truncate_middle(&s, 120)
    }
}
```

Three things to notice:

- **Supertraits.** `Tool: Send + Sync` means every tool must also be `Send` and `Sync`, so the kernel can share tools between async tasks. Putting the bound on the trait reports a non-thread-safe tool where it is implemented, not deep in the kernel. [Chapter 6](06-shared-state.md) explains the two traits.
- **Default methods.** `summarize` has a body, so implementors may skip it.
- **`&self` everywhere.** No method takes `&mut self`. Tools are shared, possibly by parallel calls, so any state they keep needs interior mutability (chapter 6).

The `#[async_trait]` attribute is explained in section 5.

## 2. Static dispatch in, dynamic dispatch stored

Plugins register tools through a generic method that stores a trait object. From [plugin.rs:128](../../crates/ch-kernel/src/plugin.rs#L128):

```rust
pub fn add_tool(&mut self, t: impl Tool + 'static) {
    self.builder
        .tools
        .push((self.plugin_id.clone(), Arc::new(t)));
}

pub fn add_tool_arc(&mut self, t: Arc<dyn Tool>) {
    self.builder.tools.push((self.plugin_id.clone(), t));
}
```

`impl Tool` in argument position is shorthand for a generic parameter. The caller writes `cx.add_tool(ReadFile::new(ctx))` and doesn't box anything. Inside, `Arc::new(t)` creates an `Arc<ReadFile>`, which coerces to `Arc<dyn Tool>` when pushed. The registry stores lists of those. From [registry.rs:23](../../crates/ch-kernel/src/registry.rs#L23):

```rust
type Tagged<T> = Vec<(String, Arc<T>)>;

/// Mutable state during registration. Use [`RegistryBuilder::build`].
#[derive(Default)]
pub struct RegistryBuilder {
    pub(crate) enabled: Vec<String>,
    pub(crate) providers: Tagged<dyn ModelProvider>,
    pub(crate) tools: Tagged<dyn Tool>,
    pub(crate) hooks: Tagged<dyn Hook>,
```

Trait objects are needed here because the list holds different types, chosen at run time by config. A `Vec<T>` holds one type only. The cost is an indirect call per method, which is nothing next to a model call, and that the compiler can't inline across it.

`Arc<dyn Tool>` rather than `Box<dyn Tool>` because the same tool is handed to several places at once: the session's tool map, and each spawned task that runs a call.

## 3. Generics where the type is known at compile time

When the type is fixed at each call site and performance or flexibility matters, generics are the better fit. The print frontend writes to any `Write`r, so tests can pass a `Vec<u8>` and inspect the output. From [run.rs:169](../../plugins/frontend-print/src/run.rs#L169):

```rust
pub async fn run_with<O, E>(
    mut session: Session,
    opts: &PrintOptions,
    out: &mut O,
    err: &mut E,
) -> anyhow::Result<PrintResult>
where
    O: Write + Send,
    E: Write + Send,
{
```

The `where` clause lists the bounds. `Write` is what the body needs. `Send` is a promise to the caller: the print frontend runs this inside an `async_trait` method, whose future must be `Send`, and this future holds the writers across `.await`s. The helper struct is generic too, and its `impl` block carries the bounds: `impl<O: Write, E: Write> Printer<'_, O, E>` ([run.rs:83](../../plugins/frontend-print/src/run.rs#L83)).

Closures are a common generic parameter, since every closure has its own unnamed type. From [common.rs:180](../../plugins/tools-fs/src/common.rs#L180):

```rust
pub async fn blocking<F>(f: F) -> ToolOutput
where
    F: FnOnce() -> ToolOutput + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(out) => out,
        Err(e) => ToolOutput::error(format!("The tool failed internally: {e}. Try again.")),
    }
}
```

Each bound is there because `spawn_blocking` needs it: `FnOnce` because it is called once, `Send` because it runs on another thread, `'static` because that thread may outlive the caller's stack frame.

## 4. `?Sized`: generics over trait objects

A generic parameter `T` implicitly requires `T: Sized`, meaning its size is known at compile time. `dyn Tool` isn't sized (different tools have different sizes), so a plain `<T>` can't be `dyn Tool`. The registry's lookup helper opts out. From [registry.rs:325](../../crates/ch-kernel/src/registry.rs#L325):

```rust
fn pick<T: ?Sized>(
    kind: &str,
    key: &str,
    items: &Tagged<T>,
    id_of: impl Fn(&T) -> &str,
    wanted: Option<&str>,
) -> anyhow::Result<Option<Arc<T>>> {
```

`pick` works for executors, approvers, frontends and context strategies, each a `Tagged<dyn ...>`. Without `?Sized`, calling it fails with E0277, "the size for values of type `dyn ...` cannot be known at compilation time", and the compiler suggests exactly this fix. That's possible because `pick` only handles `T` behind pointers (`&T`, `Arc<T>`).

## 5. Async methods in traits: `async_trait`

Rust supports `async fn` in traits natively since 1.75, but a trait with a native `async fn` can't be used as `dyn Trait`: each implementation returns a different future type, so there is no single vtable entry. A standalone example, compiled to check:

```rust
trait Tool {
    async fn call(&self) -> u32;
}

fn run(_t: &dyn Tool) {} // E0038
```

The error is E0038, "the trait `Tool` is not dyn compatible ... because method `call` is `async`". The kernel needs `dyn Tool`, so it uses the `async-trait` crate. `#[async_trait]` rewrites each `async fn` into a normal method returning `Pin<Box<dyn Future<Output = T> + Send + 'a>>`, where `'a` covers the borrowed arguments. Every implementation now returns the same type, so the trait is dyn compatible. The price is one heap allocation per call, and the future is required to be `Send`. That last part matters in [chapter 6](06-shared-state.md#5-std-mutex-vs-tokio-mutex).

You can see the same transformation done by hand where a library trait expects a boxed future. From [fetch.rs:105](../../plugins/tool-web-fetch/src/fetch.rs#L105):

```rust
impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs = resolve_public(&host).await?;
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}
```

`Resolving` is `reqwest`'s alias for a pinned, boxed, `Send` future. The kernel defines its own such alias for model streams: `ModelStream` ([model.rs:84](../../crates/ch-kernel/src/model.rs#L84)).

Plugins import the macro from the kernel (`use ch_kernel::reexports::async_trait;`). The kernel's [`reexports`](../../crates/ch-kernel/src/lib.rs#L54) module exists so that every plugin uses the same version of `async_trait`, `futures`, `serde_json` and `CancellationToken` as the kernel's traits.

## 6. Type maps: `Any` and `TypeId`

Plugins share state through typed services: `safety` provides a `PathGuard`, `tools-fs` asks for one. The kernel doesn't know these types, so it stores them as `dyn Any` keyed by their `TypeId`. From [services.rs:14](../../crates/ch-kernel/src/services.rs#L14):

```rust
#[derive(Default, Clone)]
pub struct Services {
    map: HashMap<TypeId, (&'static str, Arc<dyn Any + Send + Sync>)>,
}

impl Services {
    /// Registers a service. Returns `false` (and keeps the old one) if a service of the
    /// same type already exists.
    pub fn provide<T: Any + Send + Sync>(&mut self, service: Arc<T>) -> bool {
        let id = TypeId::of::<T>();
        if self.map.contains_key(&id) {
            return false;
        }
        self.map.insert(id, (type_name::<T>(), service));
        true
    }

    pub fn get<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.map
            .get(&TypeId::of::<T>())
            .and_then(|(_, s)| s.clone().downcast::<T>().ok())
    }
```

`Arc<T>` coerces to `Arc<dyn Any + Send + Sync>` on insert, and `Arc::downcast` checks the type id and gives back an `Arc<T>`. Callers write `cx.services.get::<PathGuard>()` and get a typed value or `None`; no cast can go wrong. `T: Any` requires `T: 'static`, so a service can't hold borrowed data.

`dyn Any` doesn't implement `Debug`, so `#[derive(Debug)]` won't work on `Services`. The file implements `Debug` by hand to print the stored type names instead ([services.rs:48](../../crates/ch-kernel/src/services.rs#L48)).

## 7. Function pointers and boxed closures

Not every callback needs a trait object. The bash deny list stores plain function pointers. From [deny.rs:3](../../plugins/safety/src/bash/deny.rs#L3):

```rust
/// What a deny-list entry looks at.
enum Check {
    /// One simple command; every top-level and nested segment is checked.
    Segment(fn(&Cmd<'_>) -> bool),
    /// The whole command line. The check must look into `parsed.nested` itself.
    Command(fn(&str, &Parsed) -> bool),
}
```

A `fn` pointer is one machine word with no allocation, and it can live in a `const` table. Closures that capture nothing coerce to `fn` pointers, which is why the table can write `Check::Segment(|c| matches!(c.program, "sudo" | "doas" | "su"))`. A closure that captures a variable can't: the compiler says "closures can only be coerced to `fn` types if they do not capture any variables".

When a callback must capture state, it goes in a `Box<dyn Fn>`. The CLI passes the REPL a hook to run after a rule is saved: `pub type OnRuleSaved = Box<dyn Fn(&str) + Send + Sync>;` ([plugins.rs:110](../../crates/ch-cli/src/plugins.rs#L110)).

## 8. Standard traits worth implementing

- `Default`: derived when zeros and empties are right, written by hand otherwise (`ModelConfig` defaults `max_output_tokens` to 16,384, [config.rs:180](../../crates/ch-kernel/src/config.rs#L180)). It also enables `..Default::default()` in struct literals, which tests use heavily.
- `FromStr`: makes `"json".parse::<OutputFormat>()` work ([lib.rs:63](../../plugins/frontend-print/src/lib.rs#L63)). The CLI hands the print frontend its format as a string and parses it with `.parse()?` ([plugins.rs:97](../../crates/ch-cli/src/plugins.rs#L97)), so the CLI crate needs no knowledge of the enum's variants.
- `Drop`: runs cleanup when a value goes out of scope; see `GroupGuard` in [chapter 7](07-async-with-tokio.md).
- `From`: makes `?` convert errors; usually derived through `thiserror`'s `#[from]` (chapter 4).

## Common mistakes

- **`dyn Trait` where a generic would do.** If each call site uses one concrete type, a generic is simpler and faster. Reach for `dyn` when you need a mixed collection or a choice made at run time.
- **Forgetting `Send + Sync` on a shared trait.** Without the supertraits, the error shows up far away, where an `Arc<dyn Trait>` crosses into a spawned task.
- **A native `async fn` in a trait you need as `dyn`.** Use `#[async_trait]`, or return a boxed future yourself.
- **`<T>` for something used with `dyn`.** Add `?Sized` when the function only touches `T` behind a pointer.

## Exercises

1. Write a test-only tool (a struct implementing `Tool`) in a crate's `tests/` directory and check it with `ch_testkit::assert_tool_conforms`. Make it fail first, then fix it. *Hint:* the description needs `When:`, `Protocol:`, `Error modes:` and `Side effects:` in that order.
2. Remove `?Sized` from [`pick`](../../crates/ch-kernel/src/registry.rs#L325) and read the error. Which call is reported?
3. In a scratch crate, put three types implementing a trait with an `async fn` into one `Vec<Box<dyn ...>>`. Do it first without and then with `#[async_trait]`.
4. Add a test to [services.rs](../../crates/ch-kernel/src/services.rs#L54) that provides two different types and checks `names()`. Then provide the same type twice through `RegisterCx::provide_service` and read the error message. *Hint:* `type_name` returns full paths such as `ch_kernel::services::tests::Counter`.
