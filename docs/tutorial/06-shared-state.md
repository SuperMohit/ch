# 6. Shared state: `Send`, `Sync`, `Arc` and `Mutex`

`ch` runs on a multi-threaded tokio runtime: tool calls run in parallel, sinks run in their own tasks, the REPL reads keys on a separate thread. Any value touched from more than one place must be safe to share, and the compiler checks that through two marker traits.

- A type is **`Send`** if a value of it can be moved to another thread.
- A type is **`Sync`** if a `&T` can be shared between threads, that is, if `&T` is `Send`.

Both are implemented automatically when all of a type's fields have them. `Rc` is neither; `RefCell` is `Send` but not `Sync`; `Arc` and `Mutex` are both, as long as their contents allow it. You never implement them by hand in this repo; you choose types that have them.

## 1. Who needs `Send + Sync`, and why

The kernel shares each tool between the session and the tasks that run its calls, so tools sit behind `Arc<dyn Tool>`. An `Arc<T>` is only `Send` if `T: Send + Sync`: several threads may hold clones and call `&self` methods at once. That is why every extension trait has the supertraits, as in `pub trait Tool: Send + Sync` ([tool.rs:104](../../crates/ch-kernel/src/tool.rs#L104)).

The kernel never needs a lock around a tool or around the registry. `Registry` is "Immutable once built" ([registry.rs:50](../../crates/ch-kernel/src/registry.rs#L50)) and shared as `Arc<Registry>`. Data that nobody changes needs only `Arc`. Locks are for data that changes.

## 2. Interior mutability behind `&self`

Hooks get `&self`, but the stuck detector must count calls. It keeps its state in a `Mutex`, which hands out mutable access through a shared reference, one caller at a time. From [lib.rs:126](../../plugins/stuck/src/lib.rs#L126):

```rust
/// The `user_prompt`, `after_model`, `pre_tool` and `post_tool` hook.
pub struct StuckHook {
    config: StuckConfig,
    tracker: Mutex<Tracker>,
}

impl StuckHook {
    pub fn new(config: StuckConfig) -> Self {
        Self {
            config,
            tracker: Mutex::default(),
        }
    }

    fn tracker(&self) -> MutexGuard<'_, Tracker> {
        // Every update leaves the counts consistent, so a poisoned lock is still usable.
        self.tracker.lock().unwrap_or_else(|p| p.into_inner())
    }
}
```

The pieces:

- `Mutex<Tracker>` is `Sync` when `Tracker` is `Send`, so `StuckHook` meets the `Hook: Send + Sync` bound. A `RefCell<Tracker>` gives the same `&self` mutation but isn't `Sync`, and the `impl Hook` would fail to compile.
- `lock()` returns a `MutexGuard`, which derefs to `&mut Tracker` and unlocks when dropped. Returning the guard from a helper keeps the poisoning policy in one place.
- The guard borrows the mutex (the `'_`, see [chapter 2](02-lifetimes.md)), so it can't outlive the hook.

The kernel gives hooks only `&self`; it never hands out `&mut`. That keeps the hook list simple (`Vec<Arc<dyn Hook>>`) and leaves the locking decision to the plugins that have state.

`FileState` is the same shape with a map inside: `entries: Mutex<HashMap<PathBuf, Entry>>` ([file_state.rs:29](../../plugins/safety/src/file_state.rs#L29)). It is a service shared by `safety` and `tools-fs`, used by tool calls that may run in parallel.

## 3. Short critical sections

A guard held longer than needed makes other callers wait. The usual trick is to let the guard be a temporary, dropped at the end of the statement. From [file_state.rs:164](../../plugins/safety/src/file_state.rs#L164):

```rust
pub fn record_write(&self, path: &Path, content: &[u8]) {
    self.lock().insert(
        path.to_path_buf(),
        Entry {
            hash: content_hash(content),
            full_read: true,
            seen: Vec::new(),
        },
    );
}
```

`record_read` computes the SHA-256 of the file *before* taking the lock ([file_state.rs:70](../../plugins/safety/src/file_state.rs#L70)), so hashing a large file doesn't block other tools.

A `std::sync::Mutex` is not reentrant. Locking it again on the same thread while a guard is alive deadlocks (or panics). `self.lock().get(p).map(|_| self.lock().remove(p))` would do exactly that, because the first guard lives until the end of the statement.

## 4. Poisoning

If a thread panics while holding a `std::sync::Mutex`, the mutex is marked **poisoned**, and later `lock()` calls return `Err`. The idea is that the data may be half-updated. Code can recover the guard with `into_inner()` on the error, which is what the repo does where every update is a single assignment or insert and so can't be left half-done. Each such place says so in a comment, as `StuckHook::tracker` above and `FileState::lock` ([file_state.rs:48](../../plugins/safety/src/file_state.rs#L48)) do.

Not every lock site recovers. `CancelHandle` uses `.expect("cancel lock")` ([cancel.rs:23](../../crates/ch-kernel/src/session/cancel.rs#L23)): its critical sections only set flags and can't panic, so the lock can't be poisoned, and if it ever were, something would be badly wrong. In tests, plain `.lock().unwrap()` is common and fine.

## 5. std `Mutex` vs tokio `Mutex`

Most locks in `ch` are `std::sync::Mutex`, even in async code. That is fine as long as **the guard is never held across an `.await`**. Holding it across an await can deadlock: the task is suspended with the lock held, and another task that tries to lock blocks its whole worker thread. With `async_trait` it doesn't even compile, because `MutexGuard` isn't `Send`. The core of a standalone example, compiled to check:

```rust
#[async_trait::async_trait]
impl Hook for Counter {
    async fn on(&self) -> u32 {
        let mut n = self.n.lock().unwrap();
        *n += 1;
        tokio::task::yield_now().await;
        *n
    }
}
```

The error is "future cannot be sent between threads safely", with the note "future is not `Send` as this value is used across an await". `StuckHook::on` takes the guard and calls only synchronous code. When async work sits between two uses, release the lock first. The project-instructions plugin checks its cache, drops the guard, does slow blocking work, then locks again to store the result. From [lib.rs:160](../../plugins/project-instructions/src/lib.rs#L160):

```rust
async fn system_blocks(&self, cx: &Cx) -> Vec<String> {
    let workspace = cx.workspace().to_path_buf();
    if let Ok(guard) = self.cache.lock()
        && let Some((sid, ws, blocks)) = guard.as_ref()
        && **sid == *cx.session_id
        && *ws == workspace
    {
        return blocks.clone();
    }

    let config = self.config.clone();
    let user = self.user.clone();
    let ws = workspace.clone();
    let blocks =
        tokio::task::spawn_blocking(move || Self::compute(&config, user.as_deref(), &ws))
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("project instructions failed: {e}");
                Vec::new()
            });
    if let Ok(mut guard) = self.cache.lock() {
        *guard = Some((cx.session_id.to_string(), workspace, blocks.clone()));
    }
    blocks
}
```

`guard` only lives inside the first `if`, and the compiler confirms it: if the guard were still alive at the `.await` below, this `async_trait` method wouldn't compile. Two concurrent calls may both miss the cache and both compute; that costs a little work, and it's the price of not holding a lock during I/O.

When you really must hold a lock across `.await`, use `tokio::sync::Mutex`, whose `lock()` is itself async and whose guard is `Send`. The CLI tests use one to run tests one at a time that share process-wide state. From [run.rs:121](../../crates/ch-cli/tests/run.rs#L121):

```rust
/// `ch_cli::run_with` hands the print frontend its task through process-wide options, so
/// runs in this file must not overlap.
static RUN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
```

Each test starts with `let _guard = RUN_LOCK.lock().await;` and holds it while the whole run is awaited ([run.rs:131](../../crates/ch-cli/tests/run.rs#L131)). The guard is named `_guard`, not `_`: a plain `_` pattern drops the value immediately, which would unlock at once.

## 6. Shareable handles: `Arc<Mutex<T>>`

To give several owners the same mutable state, wrap the mutex in an `Arc`. `CancelHandle` is a cheap, cloneable handle on the cancellation state of the current turn. From [cancel.rs:11](../../crates/ch-kernel/src/session/cancel.rs#L11):

```rust
#[derive(Clone, Default)]
pub struct CancelHandle(Arc<Mutex<CancelState>>);

#[derive(Default)]
struct CancelState {
    token: CancellationToken,
    active: bool,
    pending: bool,
}
```

The print frontend clones it into a task that waits for Ctrl-C, while the session keeps its own copy ([lib.rs:116](../../plugins/frontend-print/src/lib.rs#L116)). The three fields change together (a cancel that arrives between turns must be remembered as `pending`), so one lock around all of them is simpler than three atomics.

The file tools use the same shape to collect skipped directory names from a walker callback: `pub type Skipped = Arc<Mutex<BTreeSet<String>>>;` ([common.rs:132](../../plugins/tools-fs/src/common.rs#L132)).

## 7. Atomics for single flags

A lone flag or counter doesn't need a mutex. The REPL approver must know whether the REPL is running and can answer. From [approval.rs:56](../../plugins/frontend-repl/src/approval.rs#L56):

```rust
pub fn set_active(&self, on: bool) {
    self.active.store(on, Ordering::SeqCst);
}

fn is_active(&self) -> bool {
    self.active.load(Ordering::SeqCst)
}
```

`AtomicBool` reads and writes without a lock. The `Ordering` argument says what other memory effects become visible with it; `SeqCst` is the strongest and the right default when you don't need to reason about anything weaker.

The same struct holds `rx: Mutex<Option<mpsc::UnboundedReceiver<Pending>>>` ([approval.rs:25](../../plugins/frontend-repl/src/approval.rs#L25)). A receiver has one owner, so the frontend takes it out with `Option::take` when it starts and puts it back when it stops ([approval.rs:46](../../plugins/frontend-repl/src/approval.rs#L46)). `Mutex<Option<T>>` is the standard way to hand an owned value out of a shared struct exactly once.

## 8. Releasing resources by dropping

A lock is one kind of resource released by `Drop`. The repo uses the same pattern for others. `SessionLock` holds an operating-system file lock, and dropping it releases the lock. From [store.rs:36](../../crates/ch-kernel/src/store.rs#L36):

```rust
/// Held while a process has a session open; see [`SessionStore::lock`].
pub struct SessionLock {
    #[cfg(unix)]
    _lock: nix::fcntl::Flock<fs::File>,
}
```

The session keeps it in a field named `_lock` ([mod.rs:81](../../crates/ch-kernel/src/session/mod.rs#L81)); the leading underscore says "held only for its `Drop`". Replacing the field (as `/clear` and `/resume` do) drops the old lock at that moment.

## Common mistakes

- **Holding a `std::sync::MutexGuard` across `.await`.** Release it first, or use `tokio::sync::Mutex` if the critical section really includes async work.
- **`let _ = mutex.lock()`.** The `_` pattern doesn't bind, so the guard is dropped immediately. Use `let _guard = ...`.
- **Locking twice in one expression.** `std::sync::Mutex` isn't reentrant; the second lock deadlocks or panics.
- **A mutex around data that never changes after setup.** `Arc<T>` alone is enough, and cheaper to read.
- **Swallowing poisoning without a reason.** `unwrap_or_else(|p| p.into_inner())` is right only when every update leaves the data consistent. Say so in a comment, as the repo does.

## Exercises

1. In the [`PreTool` arm of `StuckHook::on`](../../plugins/stuck/src/lib.rs#L180), change the arm to take the guard into a variable, then `std::future::ready(()).await;`, then call `check`. Run `cargo check -p ch-stuck` and read the error. Then move the await before the line that takes the guard, and check again.
2. Add a `#[cfg(test)] mod tests` to [cancel.rs](../../crates/ch-kernel/src/session/cancel.rs) that checks a cancel requested before `begin()` cancels the next turn's token, and that `clear_pending` prevents it. *Hint:* `begin` is `pub(super)`, which a child module of `cancel` can still call.
3. Replace `AtomicBool` in [`ApprovalBridge`](../../plugins/frontend-repl/src/approval.rs#L23) with `Mutex<bool>`. What changes in the call sites, and what would you gain or lose?
4. Change `FileState::entries` to `RefCell<HashMap<PathBuf, Entry>>` (and `lock()` to return `self.entries.borrow_mut()`), then run `cargo check -p ch-safety`. Read the error from the `provide_service` call in the safety plugin. Which bound fails, and where is it declared?
