//! `PlanStore`: the current plan of each session, kept outside the history.

use std::collections::HashMap;
use std::sync::Mutex;

use ch_kernel::Cx;

use crate::plan::Plan;

/// The plan file in the session's directory, so a resumed session keeps its plan.
const FILE: &str = "plan.json";

/// Plans by session id. `/clear` starts a new session id, so it starts without a plan.
#[derive(Default)]
pub struct PlanStore {
    /// `None` once looked up and no plan was found.
    plans: Mutex<HashMap<String, Option<Plan>>>,
}

impl PlanStore {
    /// The plan of `cx`'s session, if `update_plan` was ever called in it.
    pub fn get(&self, cx: &Cx) -> Option<Plan> {
        self.lock()
            .entry(cx.session_id.to_string())
            .or_insert_with(|| load(cx))
            .clone()
    }

    /// Replaces the plan of `cx`'s session and saves it next to the session.
    pub fn set(&self, cx: &Cx, plan: Plan) {
        save(cx, &plan);
        self.lock().insert(cx.session_id.to_string(), Some(plan));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Option<Plan>>> {
        // Every update is a single insert, so a poisoned lock is still consistent.
        self.plans
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn load(cx: &Cx) -> Option<Plan> {
    let path = cx.session_dir.as_ref()?.join(FILE);
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes)
        .inspect_err(|e| tracing::warn!("ignoring {}: {e}", path.display()))
        .ok()
}

/// A plan that can't be saved still works for this run, so failures are only logged.
fn save(cx: &Cx, plan: &Plan) {
    let Some(dir) = &cx.session_dir else {
        return;
    };
    let path = dir.join(FILE);
    let saved = std::fs::create_dir_all(dir.as_path()).and_then(|()| {
        let json = serde_json::to_vec_pretty(plan).expect("a plan serializes");
        ch_kernel::util::write_atomic(&path, &json)
    });
    if let Err(e) = saved {
        tracing::warn!("saving the plan to {} failed: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::plan::{PlanItem, Status};

    fn cx(session: &str, dir: Option<&std::path::Path>) -> Cx {
        let mut cx = Cx::detached("/ws");
        cx.session_id = Arc::from(session);
        cx.session_dir = dir.map(|d| Arc::new(d.join(session)));
        cx
    }

    fn plan() -> Plan {
        vec![PlanItem {
            content: "Fix it".into(),
            status: Status::InProgress,
            active_form: "Fixing it".into(),
        }]
    }

    #[test]
    fn plans_are_per_session() {
        let store = PlanStore::default();
        store.set(&cx("a", None), plan());
        assert_eq!(store.get(&cx("a", None)), Some(plan()));
        assert_eq!(store.get(&cx("b", None)), None);
    }

    #[test]
    fn a_new_store_reads_the_saved_plan() {
        let tmp = tempfile::tempdir().unwrap();
        PlanStore::default().set(&cx("a", Some(tmp.path())), plan());
        assert!(tmp.path().join("a").join(FILE).is_file());

        let resumed = PlanStore::default();
        assert_eq!(resumed.get(&cx("a", Some(tmp.path()))), Some(plan()));
        assert_eq!(resumed.get(&cx("b", Some(tmp.path()))), None);
    }

    #[test]
    fn an_unreadable_plan_file_means_no_plan() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("a")).unwrap();
        std::fs::write(tmp.path().join("a").join(FILE), "not json").unwrap();
        assert_eq!(PlanStore::default().get(&cx("a", Some(tmp.path()))), None);
    }
}
