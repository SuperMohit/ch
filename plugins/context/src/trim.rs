//! Trims: deciding one, saving the copies of the results it shortens, and its record.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use ch_kernel::util::{ReadTools, anchored, create_private, private_dir, thousands};
use ch_kernel::{AgentEvent, Budget, CompactionKind, ContextCx, History, Message};
use serde_json::json;

use crate::log::{self, Applied, EditKind, Record, Replacement, Trigger};
use crate::plan::{self, Files, Original};
use crate::render::render;
use crate::strategy::{Context, ID, Made, Step, thinking_count, warn};
use crate::stub::{self, Saved};

/// A trim that has been decided but not saved.
pub(crate) struct Decision {
    record: Record,
    originals: Vec<Original>,
    before: u64,
    /// The size of the view with the trim.
    pub(crate) after: u64,
    /// How the stubs tell the model to read the files, for a copy that fails.
    tools: ReadTools,
}

impl Context {
    /// Trims the view of `records`: above `soft`, down to `trim_to` and only if that
    /// frees `min_free`; on `/compact` or an overflow, every candidate.
    pub(crate) async fn trim(
        &self,
        history: &History,
        records: &[Record],
        budget: &Budget,
        ccx: &ContextCx,
        trigger: Trigger,
    ) -> Step {
        match self.decide(history, records, budget, ccx, trigger) {
            Some(decision) => self.commit(history, records, decision, budget, ccx).await,
            None => Step::Nothing("nothing to trim before the last steps".into()),
        }
    }

    /// Plans a trim; `None` when there is nothing to trim or, above `soft`, when it
    /// wouldn't free `min_free`. Writes nothing.
    pub(crate) fn decide(
        &self,
        history: &History,
        records: &[Record],
        budget: &Budget,
        ccx: &ContextCx,
        trigger: Trigger,
    ) -> Option<Decision> {
        let files = &Files {
            base: ccx.cx.session_dir.as_deref().and_then(|d| anchor(d)),
            tools: ReadTools::of(&ccx.cx),
        };
        let rendered = render(history, records);
        let applied = Applied::of(records);
        let plan = plan::plan(
            history,
            &rendered,
            &applied.taken,
            budget,
            &self.config,
            trigger,
            files,
        );
        if plan.edits.is_empty() {
            return None;
        }
        let record = Record::elided(trigger, history, plan.edits)?;
        let before = budget.measure(&rendered.view());
        let mut all = records.to_vec();
        all.push(record.clone());
        let after = budget.measure(&render(history, &all).view());
        let freed = before.saturating_sub(after);
        if trigger == Trigger::Soft
            && (freed as f64) < self.config.min_free * budget.usable() as f64
        {
            return None;
        }
        Some(Decision {
            record,
            originals: plan.originals,
            before,
            after,
            tools: files.tools,
        })
    }

    /// Saves a decided trim: the copies of its results, then its record. With a
    /// session directory it takes effect only once its record is durable, so a restart
    /// renders every view that was sent.
    pub(crate) async fn commit(
        &self,
        history: &History,
        records: &[Record],
        decision: Decision,
        budget: &Budget,
        ccx: &ContextCx,
    ) -> Step {
        let Decision {
            mut record,
            originals,
            before,
            tools,
            ..
        } = decision;
        let failed = match ccx.cx.session_dir.as_deref() {
            Some(dir) => save_originals(dir.clone(), originals).await,
            None => Vec::new(),
        };
        for (original, why) in &failed {
            warn(
                ccx,
                format!("couldn't save {}: {why}", original.path.display()),
            );
            if let Some(edit) = record.edits.iter_mut().find(|e| e.pos() == original.pos) {
                edit.replacement =
                    Replacement::Text(stub::result(&original.content, Saved::Nowhere, tools));
            }
        }
        if let Err(why) = self.persist(ccx, &record).await {
            return Step::NotSaved(why);
        }
        let before_view = render(history, records).view();
        let mut all = records.to_vec();
        all.push(record.clone());
        let view = render(history, &all).view();
        let after = budget.measure(&view);
        report(ccx, &record, &before_view, &view, before, after);
        Step::Made(Made {
            message: describe(&record, before, after),
            view,
            records: vec![record],
        })
    }
}

/// The session directory anchored as the file tools check it, also before it exists;
/// `None` if it is a symlink.
pub(crate) fn anchor(dir: &Path) -> Option<PathBuf> {
    match fs::symlink_metadata(dir) {
        Ok(_) => anchored(dir).ok(),
        Err(_) => Some(fs::canonicalize(dir.parent()?).ok()?.join(dir.file_name()?)),
    }
}

/// Saves the originals of trimmed results off the async runtime: they can be large.
/// The copies and their directories are made durable before the record that names them
/// is written. Returns those that couldn't be saved, and why.
pub(crate) async fn save_originals(
    session_dir: PathBuf,
    originals: Vec<Original>,
) -> Vec<(Original, String)> {
    if originals.is_empty() {
        return Vec::new();
    }
    let all = originals.clone();
    let task = tokio::task::spawn_blocking(move || {
        let dir = match private_dir(&session_dir, "context/elided") {
            Ok(dir) => dir,
            Err(e) => {
                let why = e.to_string();
                return originals.into_iter().map(|o| (o, why.clone())).collect();
            }
        };
        let mut failed: Vec<(Original, String)> = Vec::new();
        for o in originals {
            let Some(name) = o.path.file_name() else {
                continue;
            };
            if let Err(e) = save_original(&dir.join(name), &o.content, &|f, b| f.write_all(b)) {
                failed.push((o, e.to_string()));
            }
        }
        // `elided/`, `context/` and the session directory may all be new.
        let synced = log::sync_dir(&dir)
            .and_then(|()| dir.parent().map_or(Ok(()), log::sync_dir))
            .and_then(|()| {
                dir.parent()
                    .and_then(Path::parent)
                    .map_or(Ok(()), log::sync_dir)
            });
        if let Err(e) = synced {
            tracing::warn!("syncing {}: {e}", dir.display());
        }
        failed
    });
    match task.await {
        Ok(failed) => failed,
        Err(e) => all.into_iter().map(|o| (o, e.to_string())).collect(),
    }
}

/// Writes a new file readable only by the user, made durable; a failed write leaves no
/// file. One already there (from a trim that crashed before its record) is kept if it
/// is private and holds the same text.
fn save_original(
    path: &Path,
    content: &str,
    write: &dyn Fn(&mut fs::File, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    match create_private(path) {
        Ok(mut file) => write(&mut file, content.as_bytes())
            .and_then(|()| file.sync_all())
            .inspect_err(|_| {
                drop(file);
                let _ = fs::remove_file(path);
            }),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(path)?;
            #[cfg(unix)]
            let private = {
                use std::os::unix::fs::PermissionsExt as _;
                meta.permissions().mode() & 0o077 == 0
            };
            #[cfg(not(unix))]
            let private = true;
            if meta.is_file() && private && fs::read(path)? == content.as_bytes() {
                Ok(())
            } else {
                Err(io::Error::other("a different file is already there"))
            }
        }
        Err(e) => Err(e),
    }
}

/// Emits the trim's events.
fn report(
    ccx: &ContextCx,
    record: &Record,
    before_view: &[Message],
    view: &[Message],
    before: u64,
    after: u64,
) {
    let count = |kind| record.edits.iter().filter(|e| e.kind == kind).count();
    ccx.cx.emit(AgentEvent::ContextCompacted {
        kind: CompactionKind::Elided,
        tokens_before: before,
        tokens_after: after,
    });
    ccx.cx.emit_custom(
        ID,
        "compaction",
        json!({
            "kind": "elided",
            "trigger": record.trigger,
            "results_elided": count(EditKind::Result),
            "inputs_shortened": count(EditKind::Input),
            "reminders_elided": count(EditKind::Reminder),
            "superseded_calls": count(EditKind::Superseded),
            "thinking_dropped": thinking_count(before_view).saturating_sub(thinking_count(view)),
            "first_edited_event": record.edits[0].event,
            "at_event": record.at_event,
            "tokens_before": before,
            "tokens_after": after,
        }),
    );
}

/// `/compact`'s answer.
fn describe(record: &Record, before: u64, after: u64) -> String {
    let count = |kind| record.edits.iter().filter(|e| e.kind == kind).count();
    let parts: Vec<String> = [
        (EditKind::Result, "tool result"),
        (EditKind::Input, "tool input"),
        (EditKind::Reminder, "reminder"),
        (EditKind::Superseded, "superseded call"),
    ]
    .into_iter()
    .filter_map(|(kind, name)| match count(kind) {
        0 => None,
        1 => Some(format!("1 {name}")),
        n => Some(format!("{n} {name}s")),
    })
    .collect();
    let list = match parts.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    };
    format!(
        "Trimmed {list}: {} → {} tokens.",
        thousands(before),
        thousands(after)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_that_fails_part_way_leaves_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("3-0.txt");
        let half = |f: &mut fs::File, bytes: &[u8]| {
            f.write_all(&bytes[..bytes.len() / 2])?;
            Err(io::Error::other("disk full"))
        };
        assert!(save_original(&path, "a long result", &half).is_err());
        assert!(!path.exists());
        save_original(&path, "a long result", &|f, b| f.write_all(b)).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "a long result");
    }

    #[cfg(unix)]
    #[test]
    fn a_copy_left_behind_is_reused_only_if_it_is_private_and_the_same() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("3-0.txt");
        let write = |f: &mut fs::File, b: &[u8]| f.write_all(b);
        save_original(&path, "text", &write).unwrap();
        assert!(save_original(&path, "text", &write).is_ok());
        assert!(save_original(&path, "other", &write).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(save_original(&path, "text", &write).is_err());
    }
}
