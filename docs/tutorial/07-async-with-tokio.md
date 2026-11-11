# 7. Async with tokio

An agent spends most of its time waiting: for the model's stream, for a shell command, for the user's key press. Async Rust lets one thread wait on many such things at once. An `async fn` returns a **future**, a value that describes the work. Nothing runs until something **polls** it: `.await` inside another future, or a runtime such as tokio driving a top-level task.

`ch` uses tokio's multi-threaded runtime. This chapter covers the parts the code relies on: spawning tasks and what they require, cancellation with tokens and `select!`, why dropping a future is a cancellation, bounded concurrency, channels and streams.

## 1. Starting the runtime

Most programs write `#[tokio::main]`. `ch` builds the runtime by hand, so it can turn a startup failure into an exit code and bound how long shutdown waits. From [main.rs:19](../../crates/ch-cli/src/main.rs#L19):

```rust
let runtime = match tokio::runtime::Builder::new_multi_thread()
    .enable_all()
    .build()
{
    Ok(rt) => rt,
    Err(e) => {
        eprintln!("error: starting the async runtime: {e}");
        return 1;
    }
};
let result = runtime.block_on(ch_cli::run(cli, builtin_plugins()));
// Don't wait for stray background tasks (e.g. a pending stdin read).
runtime.shutdown_timeout(std::time::Duration::from_secs(1));
```

`block_on` runs one future to completion on the current thread, with the worker threads available for spawned tasks.

## 2. Spawning: why tasks need `Send + 'static`

`tokio::spawn` starts a future as an independent task, possibly on another thread, that may outlive the code that spawned it. So the future must be `Send` and `'static`: it can't borrow anything from the spawner's stack.

The kernel runs every tool call in its own task, so a panicking tool can't take the turn down. From [tools.rs:314](../../crates/ch-kernel/src/session/tools.rs#L314):

```rust
/// Runs a tool in its own task so a panic becomes an error observation. On cancellation
/// the tool gets a short grace period (it sees `cx.cancel`) before it is aborted.
async fn execute_tool(tool: Arc<dyn Tool>, input: serde_json::Value, cx: Cx) -> ToolOutput {
    let cancel = cx.cancel.clone();
    let name = tool.spec().name;
    let mut handle = tokio::spawn(async move { tool.call(input, &cx).await });
    let joined = tokio::select! {
        r = &mut handle => r,
        _ = cancel.cancelled() => {
            match tokio::time::timeout(CANCEL_GRACE, &mut handle).await {
                Ok(r) => r,
                Err(_) => {
                    handle.abort();
                    return ToolOutput::error("Cancelled by the user.");
                }
            }
        }
    };
```

Look at the signature: it takes `Arc<dyn Tool>` and `Cx` **by value**, not `&dyn Tool` and `&Cx`. `async move` moves them into the task, and inside the task `tool.call(input, &cx)` borrows from values the task now owns. A version that takes `cx: &Cx` and spawns `async move { call(cx).await }` fails with E0521, "borrowed data escapes outside of function ... argument requires that `'1` must outlive `'static`". This is why `Cx` is designed to be cheap to clone ([chapter 1](01-ownership-and-borrowing.md#4-cheap-clones-arc-and-copy)).

The `JoinHandle` reports how the task ended. A panic inside it comes back as an `Err(JoinError)` with `is_panic()` true, which the code turns into a message for the model ([tools.rs:334](../../crates/ch-kernel/src/session/tools.rs#L334)).

For blocking work (file walks, `git`, large writes) the repo uses `tokio::task::spawn_blocking`, which runs a closure on a separate thread pool so it doesn't stall the async workers. The same `'static` rule applies, which is why callers clone what they need into owned values first ([lib.rs:170](../../plugins/project-instructions/src/lib.rs#L170), and the `blocking` helper in [chapter 5](05-traits-and-generics.md#3-generics-where-the-type-is-known-at-compile-time)).

## 3. Cancellation tokens and `select!`

Every turn has a `CancellationToken` (from `tokio-util`). The REPL cancels it on Esc or Ctrl-C, and everything that waits checks it. `tokio::select!` waits for the first of several futures and drops the rest. From [retry.rs:36](../../plugins/provider-anthropic/src/retry.rs#L36):

```rust
pub async fn sleep(delay: Duration, cancel: &CancellationToken) -> Result<(), ModelError> {
    tokio::select! {
        _ = cancel.cancelled() => Err(ModelError::Cancelled),
        _ = tokio::time::sleep(delay) => Ok(()),
    }
}
```

Without the token branch, a cancelled turn would sit through a 60-second backoff before noticing.

By default `select!` picks randomly among branches that are ready at the same time, so no branch can starve the others. `biased;` makes it check them in order. The kernel uses that so a cancel always wins over a stream event that happens to be ready. From [model_call.rs:170](../../crates/ch-kernel/src/session/model_call.rs#L170):

```rust
let mut stream = tokio::select! {
    biased;
    _ = cancel.cancelled() => return Err(ModelError::Cancelled),
    s = self.provider.stream(request, cancel.clone()) => s?,
};
// Tool calls whose input is streaming: id → (name, bytes so far, last progress).
let mut inputs: HashMap<String, (String, u64, Instant)> = HashMap::new();
loop {
    let next = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(ModelError::Cancelled),
        n = stream.next() => n,
    };
```

Code that loops over many items without awaiting checks `cancel.is_cancelled()` itself; the file walkers do it once per entry ([list.rs:118](../../plugins/tools-fs/src/list.rs#L118)).

## 4. Dropping a future is cancelling it

When `select!` picks a branch, the other futures are dropped mid-flight. So is a spawned task when `abort()` is called. Any cleanup must therefore live in `Drop` impls, not in code after an `.await` that may never run.

The local executor starts shell commands in their own process group. If the future running a command is dropped (the turn was aborted, say), the processes must still die. From [group.rs:49](../../plugins/executor-local/src/group.rs#L49):

```rust
/// Kills the group with SIGKILL when dropped while armed, so dropping the `run` future
/// (for example when the caller is aborted) never leaks processes.
pub(crate) struct GroupGuard {
    pgid: Pid,
    armed: bool,
}

impl GroupGuard {
    pub(crate) fn new(pgid: Pid) -> Self {
        Self { pgid, armed: true }
    }

    /// The command finished normally; leave intentional background processes alone.
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if self.armed {
            signal_group(self.pgid, Signal::SIGKILL);
        }
    }
}
```

`run` creates the guard right after spawning the shell and disarms it on each normal exit path ([executor.rs:143](../../plugins/executor-local/src/executor.rs#L143)). Its own `select!` covers the three ways a command can end. From [executor.rs:160](../../plugins/executor-local/src/executor.rs#L160):

```rust
let ending = tokio::select! {
    status = child.wait() => Ending::Exited(status.ok()),
    _ = tokio::time::sleep(request.timeout) => Ending::TimedOut,
    _ = cancel.cancelled() => Ending::Cancelled,
};
```

A future is **cancel-safe** if dropping it at an `.await` loses nothing. `child.wait()` and `sleep` are; a future that has read half a message from a socket may not be. Check the documentation of anything you put in a `select!` inside a loop.

## 5. Polling one future many times: `tokio::pin!`

Sometimes a loop must keep waiting on the same long-running future while handling other events. The print frontend runs the turn and prints events as they arrive. From [run.rs:197](../../plugins/frontend-print/src/run.rs#L197):

```rust
let turn = {
    let fut = session.run_turn(opts.prompt.clone());
    tokio::pin!(fut);
    loop {
        tokio::select! {
            result = &mut fut => break result,
            ev = rx.recv() => match ev {
                Ok(ev) => printer.event(&ev),
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break fut.await,
            },
        }
    }
};
```

Passing `fut` to `select!` by value would move it in and drop it after the first event. `&mut fut` lends it instead, and awaiting through a `&mut` requires the future to be pinned (guaranteed not to move in memory, since it may hold pointers into itself). `tokio::pin!` pins it on the stack. `execute_tool` does the same with a `JoinHandle`, which is `Unpin` and needs no pinning.

## 6. Bounded concurrency

Read-only tool calls from one model response run in parallel, at most `max_parallel_readonly` at a time. The kernel builds one future per call and awaits them together; a semaphore limits how many run. From [tools.rs:85](../../crates/ch-kernel/src/session/tools.rs#L85):

```rust
let futs = prepared.into_iter().map(|p| {
    let sem = sem.clone();
    let mut cx = cx.clone();
    async move {
        match p {
            Prepared::Done {
                call,
                output,
                not_run,
                notes,
            } => (call, output, not_run, notes, 0u64),
            Prepared::Run { call, tool, notes } => {
                let _permit = sem.acquire_owned().await;
                cx.tool_use_id = Some(call.id.clone());
                let started = Instant::now();
                let output = execute_tool(tool, call.input.clone(), cx).await;
                let ms = started.elapsed().as_millis() as u64;
                (call, output, false, notes, ms)
            }
        }
    }
});
let done = join_all(futs).await;
```

`join_all` returns the results in the order of the input, whatever order the calls finish in, so tool results line up with the calls in the history. `_permit` releases its slot when dropped at the end of the block. Each future clones the `Arc<Semaphore>` and the `Cx` before `async move`, so it owns what it uses.

## 7. Channels

Tasks talk through channels; the repo uses three kinds.

- **broadcast**: one sender, many receivers, each sees every message. The event bus is one ([events.rs:132](../../crates/ch-kernel/src/events.rs#L132)). A slow receiver that falls more than the buffer behind gets `RecvError::Lagged(n)` and skips ahead instead of blocking the sender; the sink tasks log that and carry on ([mod.rs:264](../../crates/ch-kernel/src/session/mod.rs#L264)).
- **mpsc**: many senders, one receiver. The REPL approver sends approval requests to the REPL loop over one ([approval.rs:24](../../plugins/frontend-repl/src/approval.rs#L24)).
- **oneshot**: exactly one value, once. Each approval request carries a `oneshot::Sender` for the answer. From [approval.rs:87](../../plugins/frontend-repl/src/approval.rs#L87):

```rust
let (reply, answer) = oneshot::channel();
let pending = Pending {
    request: request.clone(),
    reply,
};
if self.bridge.tx.send(pending).is_err() {
    return not_available();
}
answer.await.unwrap_or_else(|_| not_available())
```

If the REPL drops the request without answering, `answer.await` returns an error instead of hanging forever, and the call is denied.

## 8. Streams

A `Stream` is the async version of an iterator: `next().await` yields items until `None`. Providers return one boxed, so the kernel can treat every provider alike. From [model.rs:84](../../crates/ch-kernel/src/model.rs#L84):

```rust
pub type ModelStream = Pin<Box<dyn Stream<Item = Result<ModelEvent, ModelError>> + Send>>;
```

Consuming it is a `while let` loop; `complete` ([model.rs:112](../../crates/ch-kernel/src/model.rs#L112)) drains a stream to its final event that way. Producing one by hand is easiest with `futures::stream::unfold`, which turns a state value and an async step function into a stream. The Anthropic provider keeps its parser and a queue of pending events as that state. From [stream.rs:362](../../plugins/provider-anthropic/src/stream.rs#L362):

```rust
futures::stream::unfold(state, |mut st| async move {
    loop {
        if let Some(ev) = st.pending.pop_front() {
            return Some((Ok(ev), st));
        }
        if st.done || st.asm.finished() {
            return None;
        }
```

Each step returns `Some((item, next_state))` or `None` to end the stream. The state moves in and out of every step, so no borrow has to outlive a step.

## Common mistakes

- **Blocking a runtime thread.** `std::thread::sleep`, a long `std::fs` walk or a CPU-heavy loop inside an `async fn` stalls every task on that worker. Use `tokio::time::sleep` or `spawn_blocking`.
- **Cleanup after an `.await`.** If the future is dropped there, the cleanup never runs. Put it in a `Drop` guard.
- **Fire-and-forget tasks.** A spawned task keeps running after its spawner returns. Keep the `JoinHandle` and await or abort it; `Session::end` awaits its sink tasks with a timeout.
- **Unbiased `select!` when one branch must win.** Add `biased;` and put that branch first.
- **Borrowing into `tokio::spawn`.** Clone `Arc`s or move owned values in instead.

## Exercises

1. Change [`execute_tool`](../../crates/ch-kernel/src/session/tools.rs#L316) to take `cx: &Cx` and spawn `async move { tool.call(input, cx).await }`. Read the error, then explain why cloning `Cx` before the call is cheap.
2. Write a `#[tokio::test]` in [retry.rs](../../plugins/provider-anthropic/src/retry.rs#L43) checking that `sleep(Duration::from_secs(60), &token)` returns `Err(ModelError::Cancelled)` at once when the token is already cancelled. *Hint:* check the elapsed time with `std::time::Instant`.
3. Remove `biased;` from the second `select!` in [`stream_model`](../../crates/ch-kernel/src/session/model_call.rs#L178). Describe a sequence of events where the behaviour changes.
4. In [`read_capped`](../../plugins/executor-local/src/capture.rs#L67), the `select!` races `reader.read(...)` against `stop.cancelled()` in a loop. Look up whether `AsyncReadExt::read` is cancel-safe in tokio's documentation, and explain what would go wrong if it weren't.
