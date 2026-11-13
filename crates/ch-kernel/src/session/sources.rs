//! Starting and stopping tool sources (ADR 0006 §1).

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt as _;
use futures::future::join_all;
use tokio_util::sync::CancellationToken;

use crate::events::AgentEvent;
use crate::tool_source::{BACKGROUND_START, SourceStatus, ToolSink, ToolSource};

use super::Session;

/// How long a source gets to stop when the session ends.
const STOP_GRACE: Duration = Duration::from_secs(5);

impl Session {
    /// Starts every source concurrently and waits for each until it reports its tools,
    /// fails, or reaches its startup timeout. One that is still starting then goes on in
    /// the background, and its tools arrive like any other change.
    pub(super) async fn start_sources(&mut self) {
        let sources = self.sources.clone();
        let waits: Vec<_> = sources.into_iter().map(|s| self.spawn_source(s)).collect();
        join_all(waits).await;
        if !self.ledger.is_started() {
            self.tool_specs = Arc::new(
                (self.catalog.offered().0.into_iter())
                    .map(|o| o.spec)
                    .collect(),
            );
        }
    }

    /// Adds a source while the session runs (`/plugins reload`) and waits for it as at
    /// session start. Its tools reach the model at the next model call. Fails if a
    /// source with the same id is already running. A source removed earlier under the
    /// same id takes its old place in the precedence order.
    pub async fn add_tool_source(&mut self, source: Arc<dyn ToolSource>) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.sources.iter().any(|s| s.id() == source.id()),
            "a tool source with the id `{}` is already running",
            source.id()
        );
        self.sources.push(source.clone());
        if self.started && !self.ended {
            self.spawn_source(source).await;
        }
        Ok(())
    }

    /// Stops and removes a source. Its tools are withdrawn at the next model call, and
    /// anything it reports from now on is ignored.
    pub async fn remove_tool_source(&mut self, id: &str) -> bool {
        let Some(i) = self.sources.iter().position(|s| s.id() == id) else {
            return false;
        };
        let source = self.sources.remove(i);
        self.catalog.stop_source(id);
        if let Some(task) = self.source_tasks.remove(id) {
            task.abort();
        }
        let _ = tokio::time::timeout(STOP_GRACE, source.stop()).await;
        true
    }

    /// Stops every source, giving each [`STOP_GRACE`].
    pub(super) async fn stop_sources(&mut self) {
        for (_, task) in self.source_tasks.drain() {
            task.abort();
        }
        let stops = self.sources.iter().map(|s| {
            let catalog = self.catalog.clone();
            async move {
                catalog.stop_source(s.id());
                if tokio::time::timeout(STOP_GRACE, s.stop()).await.is_err() {
                    tracing::warn!(source = s.id(), "tool source didn't stop in time");
                }
            }
        });
        join_all(stops).await;
    }

    /// Starts `source` in a task and returns a future that waits for it until its
    /// startup timeout. The future owns everything it uses, so several can run at once.
    fn spawn_source(&mut self, source: Arc<dyn ToolSource>) -> impl Future<Output = ()> + use<> {
        let id = source.id().to_string();
        let generation = self.catalog.start_source(&id);
        let sink = ToolSink::new(
            source.as_ref(),
            generation,
            self.catalog.clone(),
            self.events.clone(),
        );
        let cx = self.cx(0, CancellationToken::new());
        let catalog = self.catalog.clone();
        let events = self.events.clone();
        let startup = source.startup_timeout();
        // The background limit never cuts a configured startup timeout short.
        let limit = startup.max(BACKGROUND_START);
        let task = tokio::spawn({
            let id = id.clone();
            async move {
                let start = AssertUnwindSafe(source.start(sink, &cx)).catch_unwind();
                let status = match tokio::time::timeout(limit, start).await {
                    Ok(Ok(Ok(()))) => SourceStatus::Ready { tools: 0 },
                    Ok(Ok(Err(e))) => SourceStatus::Failed {
                        reason: crate::util::printable(&format!("{e:#}")),
                    },
                    Ok(Err(_panic)) => SourceStatus::Failed {
                        reason: "it panicked while starting".into(),
                    },
                    Err(_) => {
                        // Only this start's own process: a newer start or a stop
                        // owns the source now.
                        if catalog.is_current(&id, generation) {
                            source.stop().await;
                        }
                        SourceStatus::Failed {
                            reason: format!("it didn't start within {} s", limit.as_secs()),
                        }
                    }
                };
                if catalog.settle(&id, generation, status)
                    && let Some(status) = catalog.status(&id)
                {
                    events.emit(AgentEvent::ToolSource { source: id, status });
                }
            }
        });
        if let Some(old) = self.source_tasks.insert(id.clone(), task.abort_handle()) {
            old.abort();
        }
        let events = self.events.clone();
        let catalog = self.catalog.clone();
        async move {
            if tokio::time::timeout(startup, task).await.is_err()
                && catalog.status(&id) == Some(SourceStatus::Starting)
            {
                events.emit(AgentEvent::Warning {
                    message: format!(
                        "`{id}` is still starting; its tools will be added when it's ready"
                    ),
                });
            }
        }
    }
}

impl Drop for Session {
    /// A session dropped without [`Session::end`] (an error path, a panic) still stops
    /// its sources: start tasks are aborted, and each source is asked to stop if a
    /// runtime is there to run it.
    fn drop(&mut self) {
        for (_, task) in self.source_tasks.drain() {
            task.abort();
        }
        if self.ended || !self.started || self.sources.is_empty() {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        for source in self.sources.drain(..) {
            self.catalog.stop_source(source.id());
            rt.spawn(async move {
                let _ = tokio::time::timeout(STOP_GRACE, source.stop()).await;
            });
        }
    }
}
