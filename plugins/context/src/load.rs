//! Reading a session's compaction log back, and what to do when it doesn't match.

use std::io;

use ch_kernel::{ContextCx, History};

use crate::log::{self, Applied, Kind, Lines, Record};
use crate::strategy::{Loaded, warn};

/// The session's records, applied up to the first one that doesn't match `history`.
///
/// - A line an append left unfinished, or a trim or summary decided on events the
///   history doesn't have or that the model never answered after (the log is ahead of a
///   save that a crash prevented), affects no reply in the history: it and the records
///   after it are dropped from the log.
/// - Any other record that doesn't match, or a reset that doesn't, means a view that was
///   sent can't be rendered again. The records from it on are dropped and a fresh reset
///   takes their place: every thinking block so far is left out, which the API accepts,
///   and the log keeps that so later requests repeat it.
///
/// When the log can't be rewritten, compactions stop for the session: it may still hold
/// records that weren't applied.
pub(crate) async fn load(history: &History, ccx: &ContextCx) -> Loaded {
    let mut loaded = Loaded {
        records: Vec::new(),
        frozen: None,
    };
    let Some(dir) = ccx.cx.session_dir.as_deref().cloned() else {
        return loaded;
    };
    let read = {
        let dir = dir.clone();
        tokio::task::spawn_blocking(move || log::load(&dir))
            .await
            .unwrap_or_else(|e| Err(e.to_string()))
    };
    let (Lines { entries, torn }, unreadable) = match read {
        Ok(lines) => (lines, false),
        Err(why) => (
            Lines {
                entries: vec![Err(why)],
                torn: false,
            },
            true,
        ),
    };
    let count = entries.len();
    let later_reset = |from: usize| {
        entries[from..]
            .iter()
            .any(|e| e.as_ref().is_ok_and(|r| r.kind == Kind::Reset))
    };
    let mut applied = Applied::default();
    let mut cut = None;
    for (i, entry) in entries.iter().enumerate() {
        if torn && i + 1 == count {
            cut = Some((i, true, "its last line is unfinished".to_string()));
            break;
        }
        let why = match entry {
            Err(why) => why.clone(),
            Ok(record) => match record.check(history, &applied) {
                Ok(()) => {
                    applied.add(record);
                    loaded.records.push(record.clone());
                    continue;
                }
                Err(why) => {
                    let harmless = record.kind != Kind::Reset
                        && !later_reset(i + 1)
                        && !log::answered_after(history, record.at_event);
                    cut = Some((i, harmless && !unreadable, why));
                    break;
                }
            },
        };
        cut = Some((i, false, why));
        break;
    }
    let Some((i, harmless, why)) = cut else {
        return loaded;
    };
    if harmless {
        warn(
            ccx,
            format!(
                "ignoring compaction records from {} on: {why}; they are newer than the \
                 saved session",
                i + 1
            ),
        );
    } else {
        warn(
            ccx,
            format!(
                "the compaction log doesn't match this session (record {}: {why}); the \
                 model's earlier reasoning is left out from now on",
                i + 1
            ),
        );
        loaded.records.extend(Record::reset(history));
    }
    let records = loaded.records.clone();
    let rewritten = tokio::task::spawn_blocking(move || log::rewrite(&dir, &records))
        .await
        .unwrap_or_else(|e| Err(io::Error::other(e)));
    if let Err(e) = rewritten {
        let why = format!("couldn't rewrite the compaction log: {e}");
        warn(ccx, format!("no more compactions in this session: {why}"));
        loaded.frozen = Some(why);
    }
    loaded
}
