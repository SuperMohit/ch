# 1. Ownership and borrowing

Every value in Rust has one owner. When the owner goes out of scope, the value is dropped. Other code can **borrow** the value (`&T` to read, `&mut T` to change) for a while, or take it over by **moving** it. The compiler checks that no borrow outlives the value and that nothing reads a value while something else may change it.

This chapter shows how `ch` decides who owns what: which functions borrow, which take ownership, where cloning is cheap, and how the code moves values out of places the borrow checker would otherwise guard.

## 1. Borrow in parameters, own in structs

The usual rule in this codebase: a function that only reads text takes `&str`; a struct that keeps text holds a `String`. Constructors that store a string take `impl Into<String>`, so callers can pass either kind.

From [tool.rs:80](../../crates/ch-kernel/src/tool.rs#L80):

```rust
pub fn ok(content: impl Into<String>) -> Self {
    Self {
        content: content.into(),
        is_error: false,
        metadata: Value::Null,
    }
}
```

`ToolOutput::ok("done")` copies the literal into a new `String` once. `ToolOutput::ok(format!(...))` moves the `String` that `format!` built, with no copy. If the parameter were `&str`, the second call would build a `String`, borrow it and copy it again. If it were `String`, every literal would need `.to_string()` at the call site.

A function that returns part of its input returns a borrow, not a new `String`. From [cmd.rs:217](../../plugins/safety/src/bash/cmd.rs#L217):

```rust
fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}
```

`/usr/bin/git` becomes `git` without allocating: the result points into the caller's string. [Chapter 2](02-lifetimes.md) explains how the compiler knows the result borrows from `word`.

## 2. When a borrow isn't possible: owned words

Sometimes the data you need doesn't exist as a slice of anything. The bash splitter in the `safety` plugin removes quotes, so `git commit -m 'a b'` has the word `a b`, which never appears contiguously in the source. So each `Segment` owns its words. From [mod.rs:32](../../plugins/safety/src/bash/mod.rs#L32):

```rust
pub struct Segment {
    /// The source text of the segment, trimmed.
    pub raw: String,
    /// Words with quotes removed (redirections excluded).
    pub words: Vec<String>,
    pub op_before: Option<Op>,
    /// Contains `$(...)`, backticks, `<(...)` or `>(...)`.
    pub has_substitution: bool,
    /// Redirects output to a file (`>`, `>>`, `>|`, `&>`, `<>` or `>&file`), unless the
    /// target is an fd, `/dev/null`, `/dev/stdout` or `/dev/stderr`.
    pub writes_file: bool,
}
```

The code that checks commands then borrows from the segment instead of copying again: `Cmd<'a>` holds `&'a str` and `&'a [String]` pointing into a `Segment`. That struct is the main example of [chapter 2](02-lifetimes.md).

The pattern is common: own the data once, at the point where it is produced, and hand out borrows to everything that only reads it.

## 3. Moving instead of cloning

A move transfers ownership; the old binding can no longer be used. Moves are cheap (a few machine words are copied; heap data stays where it is), so the kernel moves values through the turn pipeline rather than cloning them.

`Session::run_tools` takes the tool calls by value and moves each one into a batch. From [tools.rs:46](../../crates/ch-kernel/src/session/tools.rs#L46):

```rust
let mut batches: Vec<(bool, Vec<ToolCall>)> = Vec::new();
for call in calls {
    let ro = self
        .tools
        .get(&call.name)
        .is_some_and(|t| t.spec().read_only);
    match batches.last_mut() {
        Some((true, b)) if ro => b.push(call),
        _ => batches.push((ro, vec![call])),
    }
}
```

`for call in calls` consumes the `Vec`, so each `call` is an owned `ToolCall`. Note that `self.tools.get(&call.name)` only borrows `call.name`, and the borrow ends before `b.push(call)` moves it. With `for call in &calls`, `call` would be a `&ToolCall` and every push would need `call.clone()`, copying the JSON input of every call.

A neater move happens when the kernel lets hooks change a tool call. Hooks receive `&mut HookEvent`, so the call is moved into the event, the hooks run, and the call is moved back out. From [tools.rs:173](../../crates/ch-kernel/src/session/tools.rs#L173):

```rust
let mut ev = HookEvent::PreTool {
    call,
    read_only: spec.read_only,
};
let outcome = run_hooks(&self.hooks, &mut ev, cx).await;
self.emit_notes(&outcome);
let HookEvent::PreTool { call, .. } = ev else {
    unreachable!("hooks cannot change the event kind")
};
```

No clone is made, and a hook that rewrites `call.input` changes the value the kernel goes on to use. The `let ... else` destructuring is covered in [chapter 3](03-enums-and-pattern-matching.md).

## 4. Cheap clones: `Arc` and `Copy`

Some values must be shared, not moved: every tool call gets a context with the workspace, config and services. `Cx` makes that cheap by putting everything behind `Arc`. From [cx.rs:14](../../crates/ch-kernel/src/cx.rs#L14):

```rust
/// Cheap to clone; everything is behind `Arc`.
#[derive(Clone)]
pub struct Cx {
    pub session_id: Arc<str>,
    /// Workspace root. Tools must not touch anything outside it.
    pub workspace: Arc<PathBuf>,
    pub services: Arc<Services>,
    pub events: EventBus,
    pub config: Arc<Config>,
    pub executor: Option<Arc<dyn Executor>>,
    /// Specs of every registered tool (what the model sees).
    pub tools: Arc<Vec<ToolSpec>>,
```

Cloning an `Arc` increments a reference count; the data isn't copied. `run_tools` clones the `Cx` once per parallel tool call ([tools.rs:87](../../crates/ch-kernel/src/session/tools.rs#L87)), and that costs a handful of atomic increments. `Arc<str>` is used for the session id rather than `Arc<String>`: it skips one pointer hop and makes clear the id never changes.

Small plain-data types can be `Copy`, which makes every move a copy and leaves the original usable. `Usage` (four `u64` counters) derives it, from [types.rs:131](../../crates/ch-kernel/src/types.rs#L131):

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
```

so the accessor can return it by value without `.clone()` ([mod.rs:125](../../crates/ch-kernel/src/session/mod.rs#L125)). A type can only be `Copy` if all its fields are; anything holding a `String` or `Vec` can't be.

## 5. Taking a value out of a `&mut`

You can't move a field out of something you only borrow: the owner would be left with a hole. `std::mem::take` swaps in the type's default and gives you the old value. The stuck detector uses it to keep one field while resetting all others. From [tracker.rs:86](../../plugins/stuck/src/tracker.rs#L86):

```rust
if !same {
    *self = Self {
        last: Some((tool.to_string(), input.clone())),
        malformed: self.malformed,
        refused_ids: std::mem::take(&mut self.refused_ids),
        ..Self::default()
    };
}
```

Writing `refused_ids: self.refused_ids` fails with E0507, "cannot move out of `self.refused_ids` which is behind a mutable reference". The compiler suggests `.clone()`, which works but copies the whole set just before the old one is thrown away. `malformed` is a `u32`, which is `Copy`, so reading it needs no `take`.

`Option::take` is the same idea for `Option`: it leaves `None` behind. The lexer uses it to finish a word ([lexer.rs:85](../../plugins/safety/src/bash/lexer.rs#L85)), and the REPL approver uses it to hand its receiver out exactly once ([approval.rs:48](../../plugins/frontend-repl/src/approval.rs#L48)).

## 6. Walking down a tree with `&mut`

Mutable borrows can be handed on. `Config::set_value` walks `a.b.c` down nested TOML tables, creating missing ones, while holding only one `&mut` at a time. From [config.rs:88](../../crates/ch-kernel/src/config.rs#L88):

```rust
let mut table = &mut self.root;
for part in &parts[..parts.len() - 1] {
    let entry = table
        .entry(part.to_string())
        .or_insert_with(|| Value::Table(Table::new()));
    table = match entry {
        Value::Table(t) => t,
        _ => bail!("config path `{path}`: `{part}` is not a table"),
    };
}
table.insert(parts[parts.len() - 1].to_string(), value);
```

`entry` is a mutable borrow taken through `table`, and the loop then replaces `table` with a borrow of the child. That is allowed because the old `table` is never used again. If the code tried to touch the parent after descending, the compiler would reject it: two live `&mut` paths into the same tree.

## 7. `Cow`: borrow when you can, own when you must

`Cow<'a, str>` ("clone on write") holds either a `&'a str` or a `String`. Functions return it when they usually don't need to allocate but sometimes do. `String::from_utf8_lossy` is one: valid UTF-8 comes back borrowed, invalid bytes come back as an owned string with replacement characters. From [capture.rs:51](../../plugins/executor-local/src/capture.rs#L51):

```rust
let text = String::from_utf8_lossy(&self.data[start..]);
if self.dropped > 0 {
    format!(
        "[... {} bytes of earlier output dropped ...]\n{text}",
        self.dropped + start
    )
} else {
    text.into_owned()
}
```

`into_owned()` turns either case into a `String`: it copies a borrowed value and reuses an owned one. `Path::to_string_lossy` works the same way, which is why you see `.to_string_lossy().to_string()` where a `String` is stored.

Traits can return `Cow` too. The REPL's prompt (a trait from `reedline`) returns fixed strings borrowed and the search prompt owned. From [input.rs:138](../../plugins/frontend-repl/src/input.rs#L138):

```rust
fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
    Cow::Borrowed("› ")
}
fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
    Cow::Borrowed("· ")
}
fn render_prompt_history_search_indicator(&self, search: PromptHistorySearch) -> Cow<'_, str> {
    let status = match search.status {
        PromptHistorySearchStatus::Passing => "",
        PromptHistorySearchStatus::Failing => "failing ",
    };
    Cow::Owned(format!("({status}search: {}) ", search.term))
}
```

Without `Cow` the trait would have to return `String` (allocating for every redraw) or `&str` (making the formatted case impossible).

## Common mistakes

- **Parameters of type `&String` or `&Vec<T>`.** Take `&str` and `&[T]`: they accept more callers (literals, slices, `Box<str>`) and clippy's `ptr_arg` lint warns about the others.
- **Cloning to quiet the borrow checker.** A `.clone()` that the compiler suggested often hides a better move or a `mem::take`, as in section 5. Check whether the original is still needed.
- **Holding a borrow across a change to its owner.** `let first = &v[0]; v.push(x); use(first);` fails with E0502, because `push` may reallocate and leave `first` dangling. Copy out what you need, or finish using the borrow first.
- **`Copy` on types that will grow.** Deriving `Copy` is a promise; adding a `String` field later breaks every place that relied on implicit copies.

## Exercises

1. In [tools.rs:47](../../crates/ch-kernel/src/session/tools.rs#L47), change `for call in calls` to `for call in &calls` and run `cargo check -p ch-kernel`. Fix the errors with as few `.clone()` calls as you can. *Hint:* `b.push(call)` now pushes a `&ToolCall` into a `Vec<ToolCall>`.
2. In [tracker.rs:90](../../plugins/stuck/src/tracker.rs#L90), replace `std::mem::take(&mut self.refused_ids)` with `self.refused_ids` and read the error. Then try the compiler's suggestion. What does the clone cost, and why is `take` correct here?
3. Add a test to the `tests` module of [util.rs](../../crates/ch-kernel/src/util.rs) showing that `truncate_middle` counts characters, not bytes. *Hint:* build the input from `"é".repeat(500)`; each `é` is two bytes.
4. `TailBuffer::into_string` copies the bytes even when they are valid UTF-8. Rewrite it with `String::from_utf8`, falling back to the lossy conversion only on error, and keep the existing tests passing. *Hint:* `from_utf8` takes the `Vec<u8>` by value and returns it inside the error (`into_bytes`); remove the first `start` bytes with `drain` before converting.
