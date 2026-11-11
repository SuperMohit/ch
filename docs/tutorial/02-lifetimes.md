# 2. Lifetimes

A **lifetime** is the compiler's name for the stretch of code during which a borrow is valid. Every reference has one. Most are inferred, and you only write them when the compiler can't tell from a signature which input a returned borrow comes from, or when a struct holds a borrow.

Lifetimes never change how long a value lives. They describe constraints, and the compiler checks the code against them. A lifetime annotation that is too strict makes callers fail to compile; one that is too loose is rejected inside the function.

## 1. Elision: the rules that let you leave them out

The compiler fills in lifetimes in function signatures by three rules:

1. Each reference parameter gets its own lifetime.
2. If there is exactly one input lifetime, every output reference gets it.
3. If one of the inputs is `&self` or `&mut self`, outputs get the lifetime of `self`.

Rule 2 covers small string helpers. From [util.rs:26](../../crates/ch-kernel/src/util.rs#L26):

```rust
pub fn first_line(s: &str) -> &str {
    s.trim().lines().next().unwrap_or("").trim_end()
}
```

Written out, this is `fn first_line<'a>(s: &'a str) -> &'a str`: the result is a slice of `s` and can't outlive it. Rule 3 covers accessors such as `Session::model(&self) -> &str` ([mod.rs:116](../../crates/ch-kernel/src/session/mod.rs#L116)), which return a borrow of a field.

## 2. Two inputs: say which one you borrow from

With two reference parameters and no `self`, rule 2 doesn't apply, and the compiler won't guess. From [cmd.rs:205](../../plugins/safety/src/bash/cmd.rs#L205):

```rust
fn value_option<'w>(short: &str, word: &'w str) -> Option<(char, &'w str)> {
    let cluster = word.strip_prefix('-').filter(|c| !c.starts_with('-'))?;
    let (i, c) = cluster.char_indices().find(|&(_, c)| short.contains(c))?;
    Some((c, &cluster[i + c.len_utf8()..]))
}
```

The returned `&str` is a slice of `word`, so only `word` gets the named lifetime. `short` keeps its own anonymous one: the caller can pass a temporary for it.

Without `<'w>` the function fails with E0106: "this function's return type contains a borrowed value, but the signature does not say whether it is borrowed from `short` or `word`". The compiler then suggests giving both parameters the same `'a`. That compiles, but it is stricter than necessary: callers would have to keep `short` alive as long as they use the result, although the result never points into it. Name only the input you actually return from.

## 3. Structs that hold borrows

A struct that holds a reference needs a lifetime parameter, and an instance can't outlive what it borrows. `Cmd` is a view of a `Segment` (see [chapter 1](01-ownership-and-borrowing.md#2-when-a-borrow-isnt-possible-owned-words)) that skips wrappers such as `env` and `timeout`. From [cmd.rs:40](../../plugins/safety/src/bash/cmd.rs#L40):

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd<'a> {
    /// The program's base name (`/usr/bin/git` → `git`); empty when the segment has
    /// none (`fi`, `''`, a lone redirection).
    pub program: &'a str,
    pub args: &'a [String],
    /// Leading `VAR=value` assignments.
    pub assignments: Vec<&'a str>,
}
```

It is built with `Cmd::of(seg: &'a Segment) -> Cmd<'a>` ([cmd.rs:51](../../plugins/safety/src/bash/cmd.rs#L51)). Building a `Cmd` copies no strings: `args` is a slice of the segment's words. The deny list builds one for every segment of every command the model sends, so this matters little for speed, but it keeps one source of truth: no copy can drift from the segment.

## 4. Returning `'a` instead of `'_`

Methods on a borrowing struct have a choice. A result can borrow from `self` (the `Cmd` value) or from what `self` borrows (the `Segment`). From [cmd.rs:128](../../plugins/safety/src/bash/cmd.rs#L128):

```rust
pub(super) fn git_subcommand(&self) -> Option<(&'a str, &'a [String])> {
    let i = self.git_globals().1?;
    Some((self.args[i].as_str(), &self.args[i + 1..]))
}
```

By elision rule 3, `-> Option<(&str, &[String])>` would tie the result to `&self`. Writing `'a` ties it to the segment instead, so the result stays valid after the `Cmd` is gone. The difference shows when the `Cmd` is a temporary. A standalone version, compiled to check it:

```rust
struct Segment {
    words: Vec<String>,
}

struct Cmd<'a> {
    args: &'a [String],
}

impl<'a> Cmd<'a> {
    fn of(seg: &'a Segment) -> Cmd<'a> {
        Cmd { args: &seg.words[1..] }
    }
    fn sub_tied(&self) -> Option<&str> {
        self.args.first().map(String::as_str)
    }
    fn sub_free(&self) -> Option<&'a str> {
        self.args.first().map(String::as_str)
    }
}

fn main() {
    let seg = Segment { words: vec!["git".into(), "push".into()] };
    let free = Cmd::of(&seg).sub_free(); // fine
    let tied = Cmd::of(&seg).sub_tied(); // E0716
    println!("{free:?} {tied:?}");
}
```

The `tied` line fails with E0716, "temporary value dropped while borrowed": the temporary `Cmd` dies at the end of the statement, and the result is tied to it. The same reasoning lets the fields be used directly: `fn program_of(seg: &Segment) -> &str { Cmd::of(seg).program }` compiles, because `program` is a `&'a str` that borrows `seg`, not the temporary.

## 5. Context objects that borrow: `RegisterCx<'_>`

During registration a plugin gets a context that borrows the config, the registry being built and the workspace path. From [plugin.rs:85](../../crates/ch-kernel/src/plugin.rs#L85):

```rust
pub trait Plugin: Send + Sync {
    fn manifest(&self) -> PluginManifest;
    fn register(&self, cx: &mut RegisterCx<'_>) -> anyhow::Result<()>;
}

/// Handed to [`Plugin::register`]. Everything a plugin adds is tagged with its id.
pub struct RegisterCx<'a> {
    pub(crate) plugin_id: String,
    pub(crate) config: &'a Config,
    pub(crate) builder: &'a mut RegistryBuilder,
    pub(crate) workspace: &'a Path,
}
```

The lifetime is the point of the design: a plugin can't keep the context after `register` returns, because it borrows the builder, which the kernel turns into the immutable `Registry` afterwards. A plugin that tried to store `cx` in a field of a long-lived value would fail to compile. What a plugin needs later, it copies out (`cx.config::<T>()` returns an owned `T`).

`'_` is the anonymous lifetime. In `RegisterCx<'_>` it says "this type borrows something; the signature doesn't need to name it". The same goes for returned lock guards. From [file_state.rs:48](../../plugins/safety/src/file_state.rs#L48):

```rust
fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Entry>> {
    // A panic while holding the lock can't leave the map half-updated (every
    // update is a single insert), so a poisoned lock is still usable.
    self.entries
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
```

Writing `MutexGuard<HashMap<...>>` without `'_` compiles, but current compilers warn ("hiding a lifetime that's elided elsewhere is confusing", lint `mismatched_lifetime_syntaxes`), and CI builds with `-D warnings`, so the warning would fail the build.

## 6. Returning a `&mut` from one of two inputs

Lifetimes work the same for mutable borrows. `child_table` returns a mutable reference into the table it was given, creating the child if needed. From [rules.rs:38](../../plugins/frontend-repl/src/rules.rs#L38):

```rust
fn child_table<'a>(table: &'a mut Table, key: &str) -> anyhow::Result<&'a mut Table> {
    match table
        .entry(key)
        .or_insert_with(|| Value::Table(Table::new()))
    {
        Value::Table(t) => Ok(t),
        _ => bail!("`{key}` in the config file is not a table; fix it or add the rule by hand"),
    }
}
```

Because the result borrows only `table`, calls chain: `let plugins = child_table(&mut root, "plugins")?; let safety = child_table(plugins, "safety")?;` ([rules.rs:23](../../plugins/frontend-repl/src/rules.rs#L23)). `root` stays mutably borrowed until `safety` is last used.

## 7. `'static`

`&'static T` is a reference valid for the whole program: string literals, and references into `const` and `static` items. The deny list is a `const` table, so looking up a rule can return a reference into it with no lifetime tied to the caller's data. From [deny.rs:122](../../plugins/safety/src/bash/deny.rs#L122):

```rust
/// The first deny-list entry `command` hits.
pub fn denied(command: &str, parsed: &Parsed) -> Option<&'static DenyRule> {
    DENY_LIST.iter().find(|rule| match rule.check {
        Check::Segment(f) => parsed.all().any(|s| f(&Cmd::of(s))),
        Check::Command(f) => f(command, parsed),
    })
}
```

Methods like `HookPoint::as_str(&self) -> &'static str` ([hook.rs:62](../../crates/ch-kernel/src/hook.rs#L62)) return literals; spelling out `'static` overrides elision rule 3, so the result isn't tied to `self`.

`'static` as a **bound** means something else. In `add_tool(&mut self, t: impl Tool + 'static)` ([plugin.rs:128](../../crates/ch-kernel/src/plugin.rs#L128)), `T: 'static` means "`T` holds no borrows that could expire", not "lives forever". A tool owning `String`s and `Arc`s qualifies; a tool holding a `&Config` doesn't. The bound is required because the tool goes into an `Arc<dyn Tool>`, and a trait object type without a lifetime defaults to `dyn Tool + 'static`.

## 8. `impl Trait` that borrows its input

A function can return an iterator over borrowed data. From [path_guard.rs:163](../../plugins/safety/src/path_guard.rs#L163):

```rust
fn steps(path: &Path) -> impl DoubleEndedIterator<Item = Step> + '_ {
    path.components().filter_map(|c| match c {
        Component::Prefix(_) | Component::RootDir => Some(Step::Root(c.as_os_str().into())),
        Component::CurDir => None,
        Component::ParentDir => Some(Step::Parent),
        Component::Normal(n) => Some(Step::Name(n.into())),
    })
}
```

The returned iterator holds `path.components()`, which borrows `path`. `+ '_` says so. The repo uses edition 2024, where a returned `impl Trait` captures all input lifetimes automatically, so `+ '_` is optional here; in edition 2021 leaving it out was an error (E0700, "hidden type ... captures lifetime that does not appear in bounds"). You will still see both styles. [`Parsed::all`](../../plugins/safety/src/bash/mod.rs#L57) returns `impl Iterator<Item = &Segment>` with no `+ '_`.

## Common mistakes

- **Adding lifetime parameters to fix an ownership problem.** If a struct field keeps failing with lifetime errors, it may need to own its data (`String`, not `&str`). `Segment` owns its words for that reason.
- **Tying every reference to one `'a`.** The compiler's suggestion for E0106 does exactly that. It compiles, but forces callers to keep inputs alive that the result never borrows.
- **Reading `T: 'static` as "lives forever".** It means "contains no short-lived borrows". `String` is `'static`; a `String` you drop a line later is still `'static`.
- **Returning a reference to a local.** `fn name() -> &str { let s = format!(..); &s }` can't work, whatever lifetimes you write. Return the `String`.

## Exercises

1. Change [`git_subcommand`](../../plugins/safety/src/bash/cmd.rs#L128) to return `Option<(&str, &[String])>`. Does `cargo check -p ch-safety --tests` still pass? Then add a test that calls it on a temporary `Cmd`. *Hint:* look at how the callers in `deny.rs` and `allowlist.rs` hold the `Cmd`.
2. Remove `<'w>` from [`value_option`](../../plugins/safety/src/bash/cmd.rs#L205), read E0106, and apply the compiler's suggestion. Write a call that compiled before and fails now. *Hint:* pass a `String` built inline as `short` and use the result after that statement.
3. In [`steps`](../../plugins/safety/src/path_guard.rs#L163), replace `+ '_` with `+ use<>` (capture no lifetimes) and read the error. Then try `+ use<'_>`.
4. Try to keep the registration context: in a test plugin, add a field `saved: std::sync::Mutex<Option<&'static Config>>` and store `cx.full_config()` in it from `register`. Read the error and explain it in terms of section 5.
