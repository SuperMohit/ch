# Learning Rust from `ch`

A Rust tutorial that uses this repository as its textbook. Each chapter takes one area of the language, shows where `ch` uses it with code copied from the repo, and explains why the code is shaped that way: what the compiler would say otherwise, which bug the shape prevents, and what it costs.

It is written for a programmer who already reads and reviews this code and wants to understand its Rust in depth. It doesn't teach programming, and it doesn't replace [The Rust Programming Language](https://doc.rust-lang.org/book/); read the book's chapter on a topic first if it is new to you, then the chapter here to see it in real code.

Every snippet links to the file and line it came from. Each chapter ends with common mistakes and with exercises that change real files and ask what the compiler says. Do the exercises on a scratch branch and revert them afterwards.

## Reading order

Read chapters 1 to 4 in order: ownership, lifetimes, enums and errors are used everywhere after that. Chapters 5 to 8 build on them, and 6 and 7 are best read together, since shared state and async meet in the same code. Chapters 9 and 10 stand on their own and can be read at any point.

If you are reviewing a change, the chapter on the area it touches is a good refresher: plugins and extension points (5), hooks with state (6), providers and executors (7), tests (10).

## Chapters

| # | Chapter | What it covers |
|---|---|---|
| 1 | [Ownership and borrowing](01-ownership-and-borrowing.md) | Moves, `&str` vs `String`, cheap clones with `Arc`, `mem::take`, `Cow` |
| 2 | [Lifetimes](02-lifetimes.md) | Elision, borrowing structs such as `Cmd<'a>`, `'_`, `'static`, `impl Trait` captures |
| 3 | [Enums and pattern matching](03-enums-and-pattern-matching.md) | Exhaustive `match`, guards, or-patterns, slices, `matches!`, `let ... else`, let chains |
| 4 | [Option, Result and errors](04-option-result-and-errors.md) | Combinators, `?`, `anyhow` and `{e:#}`, `thiserror`, errors as observations, panics |
| 5 | [Traits, generics and trait objects](05-traits-and-generics.md) | Extension traits, `impl Trait` vs `dyn Trait`, `?Sized`, `async_trait`, `Any` |
| 6 | [Shared state](06-shared-state.md) | `Send` and `Sync`, `Arc`, `Mutex` and poisoning, std vs tokio locks, atomics |
| 7 | [Async with tokio](07-async-with-tokio.md) | Spawning, cancellation tokens, `select!`, drop as cancellation, channels, streams |
| 8 | [Iterators, closures and collections](08-iterators-closures-collections.md) | Lazy chains, `Fn`/`FnMut`/`FnOnce`, `move`, `BTreeMap` for order, the entry API |
| 9 | [Crates, modules and features](09-crates-modules-and-features.md) | Workspace, visibility, one cargo feature per plugin, MSRV, `unsafe_code`, `const` and `static` |
| 10 | [Testing](10-testing.md) | Unit and integration tests, `#[tokio::test]`, `#[ignore]`, doc tests, `ch-testkit` |

Related background: [rust-libraries-and-plugin-loading.md](../rust-libraries-and-plugin-loading.md) (crates, ABIs, why plugins are linked at compile time) and [snapshot-testing.md](../snapshot-testing.md) (`insta`).

## How this stays current

The snippets are copied from the code, so they drift when the code changes. Each pull request that adds or changes Rust code updates the chapters for the concepts it uses:

- If a snippet's lines moved or changed, update the snippet and its line anchor.
- If the change uses a concept no chapter covers, or is a clearer example than the current one, add it to the chapter for that concept.
- If code a chapter cites is deleted, replace the example or remove the section.

A wrong line anchor is worse than none, so check every anchor you touch against the file.
