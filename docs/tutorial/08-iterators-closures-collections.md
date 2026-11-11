# 8. Iterators, closures and collections

Rust code that transforms data is mostly iterator chains and closures, and the result usually lands in one of a handful of standard collections. These pieces compile to plain loops, so the readable form costs nothing at run time.

This chapter covers how iterator chains work, which closure traits exist and how `move` changes what a closure captures, and how `ch` picks between `HashMap`, `BTreeMap`, `HashSet` and the rest.

## 1. Iterator chains are lazy; `collect` decides the result

Adapters such as `filter` and `map` build a new iterator and do nothing until something consumes it. `collect` consumes it, and its target type, written on the binding, chooses what is built. From [registry.rs:141](../../crates/ch-kernel/src/registry.rs#L141):

```rust
let enabled: HashSet<String> = statuses
    .iter()
    .filter(|s| s.enabled)
    .map(|s| s.manifest.id.clone())
    .collect();
```

The same chain with `Vec<String>` on the left would build a vector. `collect` can build any type that implements `FromIterator`: `Vec`, `HashSet`, `HashMap` (from pairs), `String` (from `char`s or `&str`s), and `Result<Vec<_>, E>` (see [chapter 4](04-option-result-and-errors.md#3-collecting-into-a-result)).

Pairs collect into a map. The registry builds its id-to-plugin lookup by zipping two lists. From [registry.rs:164](../../crates/ch-kernel/src/registry.rs#L164):

```rust
let by_id: HashMap<String, Arc<dyn Plugin>> = manifests
    .iter()
    .zip(plugins)
    .map(|(m, p)| (m.id.clone(), p))
    .collect();
```

`manifests.iter()` borrows, while `zip(plugins)` consumes `plugins` (a `Vec`) by value, so each `p` is an owned `Arc` moved into the map with no clone.

## 2. Adapters you will see often

`filter_map` filters and transforms in one step: return `Some(x)` to keep `x`, `None` to drop the item. It is the natural way to pick one enum variant out of a list. From [types.rs:91](../../crates/ch-kernel/src/types.rs#L91):

```rust
pub fn tool_calls(&self) -> Vec<ToolCall> {
    self.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, input } => Some(ToolCall {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            }),
            _ => None,
        })
        .collect()
}
```

Other common ones in the repo:

- `any` / `all` / `find` / `position` stop at the first item that decides the answer.
- `chain` joins two iterators: `Parsed::all` yields top-level then nested segments ([mod.rs:57](../../plugins/safety/src/bash/mod.rs#L57)).
- `rev` walks backwards, if the iterator is double-ended: the path resolver collects `steps(path).rev()` into a `Vec` used as a stack, so `pop` yields the first step ([path_guard.rs:177](../../plugins/safety/src/path_guard.rs#L177)).
- `enumerate` pairs items with their index.
- `sum` adds up numbers; it sometimes needs a type hint, written as a turbofish.

From [model_call.rs:38](../../crates/ch-kernel/src/session/model_call.rs#L38):

```rust
let fixed_chars: usize = system.iter().map(String::len).sum::<usize>()
    + reminders.iter().map(String::len).sum::<usize>()
    + specs_chars(&self.tool_specs);
```

Without `::<usize>`, the compiler can't tell what type the first `sum` should produce before the `+`, and fails with E0283 ("type annotations needed").

## 3. Functions and methods as closures

Anywhere a closure is expected, you can pass a function or method path. `.map(String::len)` above is short for `.map(|s| s.len())`. The same trick checks a JSON list. From [tool.rs:54](../../crates/ch-kernel/src/tool.rs#L54):

```rust
if let Some(required) = obj.get("required") {
    let strings = required
        .as_array()
        .is_some_and(|list| list.iter().all(Value::is_string));
```

The plan validator passes a private function: `text.chars().any(is_unsafe)` ([plan.rs:73](../../plugins/planning/src/plan.rs#L73)). This only works when the function's signature matches what the adapter passes. `iter()` over a `Vec<String>` yields `&String`, so `.map(str::len)` doesn't fit, while `.map(String::len)` does.

## 4. Closure traits: `Fn`, `FnMut`, `FnOnce`

Every closure implements one or more of three traits, based on what it does with what it captures:

- `Fn`: only reads its captures, so it can be called any number of times, even concurrently.
- `FnMut`: changes its captures, so calls need exclusive access.
- `FnOnce`: may move a capture out, so it can be called only once.

Every closure is `FnOnce`; those that don't move out are also `FnMut`; those that don't mutate are also `Fn`. A function that accepts a closure should ask for the *least* it needs, which means the most permissive trait for callers. `resolve_print_prompt` calls its stdin reader at most once, so it asks for `FnOnce`. From [app.rs:64](../../crates/ch-cli/src/app.rs#L64):

```rust
pub fn resolve_print_prompt(
    cli: &Cli,
    stdin_piped: bool,
    read_stdin: impl FnOnce(StdinUse) -> anyhow::Result<String>,
) -> anyhow::Result<String> {
```

That lets one caller hand over a string it already has, moving it out of the closure. From [app.rs:310](../../crates/ch-cli/src/app.rs#L310):

```rust
let prompt = match stdin {
    Stdin::Process => {
        resolve_print_prompt(&cli, !std::io::stdin().is_terminal(), read_process_stdin)?
    }
    Stdin::Text(text) => resolve_print_prompt(&cli, true, move |_| Ok(text))?,
    Stdin::None => resolve_print_prompt(&cli, false, |_| Ok(String::new()))?,
};
```

`move |_| Ok(text)` returns the captured `String` itself, so it is only `FnOnce`. Had the parameter been `impl Fn`, this line would fail with E0507, "cannot move out of `text`, a captured variable in an `Fn` closure". The first arm passes a plain function, the third a closure that captures nothing. The parameter also makes the function testable: the tests pass closures that panic if stdin is read when it shouldn't be ([args.rs:126](../../crates/ch-cli/tests/args.rs#L126)).

The registry's `pick` takes `id_of: impl Fn(&T) -> &str` instead ([registry.rs:329](../../crates/ch-kernel/src/registry.rs#L329)), because it calls it once per item.

## 5. `move` and what a closure captures

By default a closure captures each variable in the least demanding way its body allows: by shared reference, by mutable reference, or by value. `move` forces capture by value. You need it when the closure outlives the current function, as with threads, spawned tasks, or a callback stored inside a returned object. From [common.rs:148](../../plugins/tools-fs/src/common.rs#L148):

```rust
.filter_entry(move |entry| {
    let skip = entry.depth() > 0
        && entry.file_type().is_some_and(|t| t.is_dir())
        && entry
            .file_name()
            .to_str()
            .is_some_and(|n| SKIP_DIRS.contains(&n));
    if skip && let Some(s) = &skipped {
        let mut set = s.lock().unwrap_or_else(|p| p.into_inner());
        set.insert(entry.file_name().to_string_lossy().into_owned());
    }
    !skip
});
```

The walker returned by this function keeps the closure, so the closure can't borrow the function's local `skipped`. `move` puts the `Option<Arc<Mutex<...>>>` inside the closure. Because `skipped` holds an `Arc`, the caller can keep its own clone and read the set after the walk.

A common pattern is to clone just before a `move` closure, so the original stays usable: `let sem = sem.clone(); let mut cx = cx.clone(); async move { ... }` in the parallel tool runner ([tools.rs:86](../../crates/ch-kernel/src/session/tools.rs#L86)).

## 6. Choosing a collection

| Need | Type | Example |
|---|---|---|
| Lookup by key, order irrelevant | `HashMap` | the session's tools by name ([mod.rs:71](../../crates/ch-kernel/src/session/mod.rs#L71)) |
| Lookup by key, iterate in key order | `BTreeMap` | streamed tool calls by index ([stream.rs:54](../../plugins/provider-openai/src/stream.rs#L54)) |
| Membership, order irrelevant | `HashSet` | rules the user allowed "always" ([mod.rs:79](../../crates/ch-kernel/src/session/mod.rs#L79)) |
| Membership, sorted output | `BTreeSet` | skipped directory names shown to the model ([common.rs:132](../../plugins/tools-fs/src/common.rs#L132)) |
| Queue: push back, pop front | `VecDeque` | stream events not yet returned ([stream.rs:344](../../plugins/provider-anthropic/src/stream.rs#L344)) |
| Everything else | `Vec` | almost everywhere |

The `BTreeMap` choice matters for correctness. OpenAI-style streams send tool-call fragments keyed by an index, and the assembled message must list the calls in index order, every time. `HashMap` iteration order is randomized per process, so the history, the snapshots and the order tools run in would change from run to run. `BTreeMap` iterates in key order ([stream.rs:193](../../plugins/provider-openai/src/stream.rs#L193)).

The same reasoning applies to any `HashMap` whose contents end up in text. When the kernel tells the model which tools exist, it sorts the names first ([tools.rs:149](../../crates/ch-kernel/src/session/tools.rs#L149)).

## 7. The entry API

`entry(key)` looks a key up once and returns a handle that can insert if it's missing. The stream assembler finds or creates the pending call for each fragment. From [stream.rs:122](../../plugins/provider-openai/src/stream.rs#L122):

```rust
for (pos, tc) in calls.iter().enumerate() {
    let index = tc["index"].as_u64().unwrap_or(pos as u64);
    let entry = self.calls.entry(index).or_default();
    if let Some(id) = tc["id"].as_str().filter(|s| !s.is_empty()) {
        entry.id = id.to_string();
    }
    let f = &tc["function"];
    if let Some(name) = f["name"].as_str().filter(|s| !s.is_empty()) {
        entry.name.push_str(name);
    }
    let args = f["arguments"].as_str().unwrap_or("");
    entry.arguments.push_str(args);
```

`or_default()` inserts `PendingCall::default()` if needed, and both cases give back `&mut PendingCall`. The alternative, `if !map.contains_key(&k) { map.insert(k, ..) }` followed by `map.get_mut(&k).unwrap()`, hashes three times and has an `unwrap` that the reader must check. Use `or_insert_with(|| ...)` when the default needs arguments, as `Config::set_value` does for missing tables ([config.rs:90](../../crates/ch-kernel/src/config.rs#L90)).

`insert` itself returns useful information: `HashMap::insert` returns the old value, and `HashSet::insert` returns whether the value was new. Both make duplicate checks one line. From [registry.rs:276](../../crates/ch-kernel/src/registry.rs#L276):

```rust
let mut seen: HashMap<String, &String> = HashMap::new();
for (plugin, name) in items {
    if let Some(other) = seen.insert(name.clone(), plugin) {
        bail!("{kind} `{name}` is registered by both `{other}` and `{plugin}`");
    }
}
```

## 8. Sorting and filtering in place

- `sort_by_key` is **stable**: equal keys keep their order. The registry relies on that so hooks with the same priority run in registration order ([registry.rs:253](../../crates/ch-kernel/src/registry.rs#L253)).
- `sort_unstable` is faster and allocates nothing, fine when equal elements are indistinguishable, such as plain strings.
- `retain` removes elements that fail a predicate without building a new `Vec` ([registry.rs:206](../../crates/ch-kernel/src/registry.rs#L206)).
- `drain(..)` empties a collection while yielding owned items, as `Session::end` does with its sink tasks ([mod.rs:301](../../crates/ch-kernel/src/session/mod.rs#L301)).

## Common mistakes

- **Iterating a `HashMap` into output that must be stable.** Prompts, error messages and snapshots change from run to run. Sort first, or use a `BTreeMap`.
- **An unconsumed iterator.** `items.iter().map(|x| do_something(x));` does nothing; the compiler warns that iterators are lazy. Use a `for` loop for side effects.
- **`collect::<Vec<_>>()` only to iterate again.** Chain the next adapter directly, unless you need the items twice or need to release a borrow.
- **Asking for `Fn` when `FnOnce` would do.** Callers then can't move captured values out.
- **`move` on a closure that should share a value.** Clone the `Arc` first, then move the clone.

## Exercises

1. Rewrite [`Registry::enabled_plugin_ids`](../../crates/ch-kernel/src/registry.rs#L431) as a `for` loop, then back. Which do you find clearer?
2. Change `Assembler::calls` in [stream.rs:54](../../plugins/provider-openai/src/stream.rs#L54) to a `HashMap` and run `cargo test -p ch-provider-openai` a few times. Then write a test with several tool calls that would fail with a `HashMap`. *Hint:* start from the existing test `tool_calls_assemble_by_index`, send the first fragments of the calls out of index order, and check the order of `message.tool_calls()` in the completed response.
3. Change the parameter of [`resolve_print_prompt`](../../crates/ch-cli/src/app.rs#L67) to `impl Fn(...)` and read the error. Then change it to `impl FnMut(...)`. Which call sites break, and why?
4. In [`human_size`](../../plugins/tools-fs/src/common.rs#L165), replace the `while` loop with `std::iter::successors` and `take_while`, keeping the tests green. Is the result easier to read?
