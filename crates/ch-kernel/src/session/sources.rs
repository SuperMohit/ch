//! Starting and stopping tool sources (ADR 0006 §1).

use std::sync::Arc;
use std::time::Duration;

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
    /// source with the same id is already running.
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

    /// Stops and removes a source. Its tools are withdrawn at the next model call.
    pub async fn remove_tool_source(&mut self, id: &str) -> bool {
        let Some(i) = self.sources.iter().position(|s| s.id() == id) else {
            return false;
        };
        let source = self.sources.remove(i);
        self.catalog.settle(id, SourceStatus::Stopped);
        let _ = tokio::time::timeout(STOP_GRACE, source.stop()).await;
        self.catalog.remove_source(id);
        true
    }

    /// Stops every source, giving each [`STOP_GRACE`].
    pub(super) async fn stop_sources(&mut self) {
        for task in self.source_tasks.drain(..) {
            task.abort();
        }
        let stops = self.sources.iter().map(|s| {
            let catalog = self.catalog.clone();
            async move {
                catalog.settle(s.id(), SourceStatus::Stopped);
                if tokio::time::timeout(STOP_GRACE, s.stop()).await.is_err() {
                    tracing::warn!(source = s.id(), "tool source didn't stop in time");
                }
            }
        });
        join_all(stops).await;
    }

    /// Starts `source` in a task and returns a future that waits for it until its
    /// startup timeout.
    fn spawn_source(&mut self, source: Arc<dyn ToolSource>) -> impl Future<Output = ()> + use<> {
        let id = source.id().to_string();
        self.catalog.add_source(&id);
        let sink = ToolSink::new(source.as_ref(), self.catalog.clone(), self.events.clone());
        let cx = self.cx(0, CancellationToken::new());
        let catalog = self.catalog.clone();
        let events = self.events.clone();
        let startup = source.startup_timeout();
        let task = tokio::spawn({
            let id = id.clone();
            async move {
                let started = tokio::time::timeout(BACKGROUND_START, source.start(sink, &cx)).await;
                let status = match started {
                    Ok(Ok(())) => SourceStatus::Ready { tools: 0 },
                    Ok(Err(e)) => SourceStatus::Failed {
                        reason: crate::util::printable(&format!("{e:#}")),
                    },
                    Err(_) => {
                        source.stop().await;
                        SourceStatus::Failed {
                            reason: format!(
                                "it didn't start within {} s",
                                BACKGROUND_START.as_secs()
                            ),
                        }
                    }
                };
                if catalog.settle(&id, status)
                    && let Some(status) = catalog.status(&id)
                {
                    events.emit(AgentEvent::ToolSource { source: id, status });
                }
            }
        });
        self.source_tasks.push(task.abort_handle());
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
