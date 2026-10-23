use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;

use crate::config::Config;
use crate::events::EventBus;
use crate::history::History;
use crate::registry::Registry;
use crate::store::{SessionMeta, SessionSnapshot, SessionStore};
use crate::tokens::TokenCalibrator;
use crate::tool::Tool;
use crate::types::Usage;

use super::{CancelHandle, Session};

pub struct SessionBuilder {
    registry: Arc<Registry>,
    config: Arc<Config>,
    workspace: PathBuf,
    id: Option<String>,
    approver: Option<String>,
    store: Option<SessionStore>,
    restore: Option<SessionSnapshot>,
    events: Option<EventBus>,
    model: Option<String>,
}

impl SessionBuilder {
    pub fn new(registry: Arc<Registry>, config: Arc<Config>) -> Self {
        Self {
            registry,
            config,
            workspace: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            id: None,
            approver: None,
            store: None,
            restore: None,
            events: None,
            model: None,
        }
    }

    /// Model id; overrides `model.id` and the model of a restored session.
    pub fn model(mut self, id: impl Into<String>) -> Self {
        self.model = Some(id.into());
        self
    }

    pub fn workspace(mut self, workspace: impl Into<PathBuf>) -> Self {
        self.workspace = workspace.into();
        self
    }

    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Approver id; overrides `select.approver`.
    pub fn approver(mut self, id: impl Into<String>) -> Self {
        self.approver = Some(id.into());
        self
    }

    /// Persist the session to this store after every turn.
    pub fn store(mut self, store: SessionStore) -> Self {
        self.store = Some(store);
        self
    }

    /// Continue a saved session.
    pub fn restore(mut self, snapshot: SessionSnapshot) -> Self {
        self.restore = Some(snapshot);
        self
    }

    /// Use an existing bus (so a frontend can subscribe before the session exists).
    pub fn events(mut self, bus: EventBus) -> Self {
        self.events = Some(bus);
        self
    }

    pub fn build(self) -> anyhow::Result<Session> {
        let kcfg = self.config.kernel()?;
        let registry = self.registry;
        let provider = registry.provider(kcfg.model.provider.as_deref())?;
        let configured_model = match (&kcfg.model.id, provider.default_model()) {
            (Some(id), _) => id.clone(),
            (None, Some(id)) => id.to_string(),
            (None, None) => anyhow::bail!(
                "model provider `{}` has no default model; set model.id (or pass --model)",
                provider.id()
            ),
        };
        let summ = kcfg.model.summarizer.clone().unwrap_or_default();
        let summarizer = match &summ.provider {
            Some(id) => registry.provider(Some(id))?,
            None => provider.clone(),
        };
        let summarizer_model = summ.id.unwrap_or_else(|| configured_model.clone());
        let context = registry.context(kcfg.select.context.as_deref())?;
        let executor = registry.executor(kcfg.select.executor.as_deref())?;
        let approver_id = self.approver.or(kcfg.select.approver.clone());
        let approver = registry.approver(approver_id.as_deref())?;
        let tools: HashMap<String, Arc<dyn Tool>> = registry
            .tools
            .iter()
            .map(|(_, t)| (t.spec().name, t.clone()))
            .collect();
        let tool_specs = Arc::new(registry.tool_specs());
        let hooks = registry.hooks.iter().map(|(_, h)| h.clone()).collect();
        let workspace = Arc::new(self.workspace);

        let now = Utc::now();
        let (id, history, usage, cost, created_at, model, turn) = match self.restore {
            Some(snap) => (
                snap.meta.id.clone(),
                snap.history,
                snap.usage,
                snap.cost,
                snap.meta.created_at,
                restored_model(&snap.meta, provider.id(), &configured_model),
                snap.meta.turns,
            ),
            None => (
                self.id.unwrap_or_else(new_session_id),
                History::new(),
                Usage::default(),
                0.0,
                now,
                configured_model.clone(),
                0,
            ),
        };
        let model = self.model.unwrap_or(model);
        let lock = self.store.as_ref().map(|s| s.lock(&id)).transpose()?;

        Ok(Session {
            id,
            registry,
            config: self.config,
            kcfg,
            workspace,
            history,
            events: self.events.unwrap_or_default(),
            provider,
            model,
            configured_model,
            summarizer,
            summarizer_model,
            context,
            executor,
            approver,
            tools,
            tool_specs,
            hooks,
            cancel: CancelHandle::default(),
            turn,
            usage,
            cost,
            calibrator: TokenCalibrator::default(),
            always_allow: HashSet::new(),
            store: self.store,
            _lock: lock,
            created_at,
            sink_tasks: Vec::new(),
            started: false,
            ended: false,
        })
    }
}

pub(super) fn restored_model(meta: &SessionMeta, provider: &str, configured: &str) -> String {
    match &meta.provider {
        Some(p) if p != provider => configured.to_string(),
        _ => meta.model.clone(),
    }
}

pub(super) fn new_session_id() -> String {
    let ts = Utc::now().format("%Y%m%d-%H%M%S");
    let short = uuid::Uuid::new_v4().simple().to_string();
    format!("{ts}-{}", &short[..6])
}
