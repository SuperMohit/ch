//! `PlanStore`: the current plan of the session, kept outside the history.

use std::sync::{Mutex, MutexGuard};

use ch_kernel::Cx;

use crate::plan::Plan;

/// The plan file in the session's directory, so a resumed session keeps its plan.
const FILE: &str = "plan.json";

/// The plan of the current session. Only `update_plan` changes it.
///
/// It holds one session at a time: a different session id (after `/clear` or
/// `/resume`) reads that session's plan from its directory again, so the plan always
/// matches the session that was loaded.
#[derive(Default)]
pub struct PlanStore {
    slot: Mutex<Option<Slot>>,
}

struct Slot {
    session: String,
    /// `None` if the session has no plan.
    plan: Option<Plan>,
    /// The turn and step of the first request that showed the model the current plan:
    /// the one after its latest `update_plan` call, or one with a plan reminder. Only in
    /// memory, since each turn starts with a reminder.
    shown: Option<(u32, u32)>,
    /// Refreshes since the model last updated the plan or a turn began. Each one doubles
    /// the wait for the next.
    ignored: u32,
}

impl PlanStore {
    /// The plan of `cx`'s session, if `update_plan` was ever called in it.
    pub fn get(&self, cx: &Cx) -> Option<Plan> {
        self.current(cx).as_ref().and_then(|slot| slot.plan.clone())
    }

    /// Replaces the plan of `cx`'s session and saves it next to the session.
    pub(crate) fn set(&self, cx: &Cx, plan: Plan) {
        let mut slot = self.lock();
        save(cx, &plan);
        *slot = Some(Slot {
            session: cx.session_id.to_string(),
            plan: Some(plan),
            shown: Some((cx.turn, cx.step + 1)),
            ignored: 0,
        });
    }

    /// Records that the first request of a turn shows the plan.
    pub(crate) fn turn_started(&self, cx: &Cx) {
        if let Some(slot) = self.current(cx).as_mut() {
            slot.shown = Some((cx.turn, cx.step));
            slot.ignored = 0;
        }
    }

    /// Whether `cx`'s request should show the plan again: `every` requests after it was
    /// last shown, doubled for each earlier refresh the model didn't follow with an
    /// update. If so, records the refresh and returns the requests since the plan was
    /// shown.
    pub(crate) fn refresh_due(&self, cx: &Cx, every: u32) -> Option<u32> {
        let mut guard = self.current(cx);
        let slot = guard.as_mut()?;
        let since = match slot.shown {
            Some((turn, step)) if turn == cx.turn => cx.step.saturating_sub(step),
            _ => cx.step,
        };
        let wait = u64::from(every) << slot.ignored.min(32);
        if u64::from(since) < wait {
            return None;
        }
        slot.shown = Some((cx.turn, cx.step));
        slot.ignored += 1;
        Some(since)
    }

    fn current(&self, cx: &Cx) -> MutexGuard<'_, Option<Slot>> {
        let mut slot = self.lock();
        if slot.as_ref().is_none_or(|s| *s.session != *cx.session_id) {
            *slot = Some(Slot {
                session: cx.session_id.to_string(),
                plan: load(cx),
                shown: None,
                ignored: 0,
            });
        }
        slot
    }

    fn lock(&self) -> MutexGuard<'_, Option<Slot>> {
        // Every update is a single assignment, so a poisoned lock is still consistent.
        self.slot
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

    fn plan(content: &str) -> Plan {
        vec![PlanItem {
            content: content.into(),
            status: Status::InProgress,
            active_form: format!("{content}ing"),
        }]
    }

    #[test]
    fn another_session_has_its_own_plan() {
        let store = PlanStore::default();
        store.set(&cx("a", None), plan("Fix"));
        assert_eq!(store.get(&cx("a", None)), Some(plan("Fix")));
        assert_eq!(store.get(&cx("b", None)), None);
    }

    #[test]
    fn switching_sessions_reads_the_saved_plan_again() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (cx("a", Some(tmp.path())), cx("b", Some(tmp.path())));
        let store = PlanStore::default();
        store.set(&a, plan("Fix"));
        assert!(tmp.path().join("a").join(FILE).is_file());

        assert_eq!(store.get(&b), None);
        // Another process changed session a's plan meanwhile.
        PlanStore::default().set(&a, plan("Test"));
        assert_eq!(store.get(&a), Some(plan("Test")));
    }

    #[test]
    fn an_unreadable_plan_file_means_no_plan() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("a")).unwrap();
        std::fs::write(tmp.path().join("a").join(FILE), "not json").unwrap();
        assert_eq!(PlanStore::default().get(&cx("a", Some(tmp.path()))), None);
    }
}
