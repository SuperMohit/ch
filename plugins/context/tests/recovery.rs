//! Reading the compaction log back: records bound to their history, a log ahead of a
//! crashed save, damage that leads to a reset, and saves that fail.

mod common;

use common::*;

#[tokio::test]
async fn a_record_that_no_longer_matches_stops_the_log_and_resets_the_reasoning() {
    let tmp = tempfile::tempdir().unwrap();
    let (c, _, log) = trimmed_twice(tmp.path()).await;
    let path = tmp.path().join("s1/context/log.jsonl");
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);

    // The first record's block changed (the history was edited), though the model
    // answered after it: what was sent can't be rendered again.
    let mut lines: Vec<Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    lines[0]["edits"][0]["hash"] = json!("0000");
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(&path, text).unwrap();
    let mut rx = ccx.cx.events.subscribe();
    let v = view(&elide(), &c.history, 1_000_000, &ccx).await;
    let w = warnings(&mut rx);
    assert_eq!(w.len(), 1, "{w:#?}");
    assert!(
        w[0].contains("record 1") && w[0].contains("has changed"),
        "{}",
        w[0]
    );
    // Neither record applies, the later one included; no thinking is sent.
    assert_eq!(thinking_left(&v), 0);
    let stripped: Vec<Message> = c.history.messages().iter().map(without_thinking).collect();
    assert_eq!(v, stripped);
    // The reset is in the log, so the next process sends the same bytes, quietly.
    let after = records(&tmp.path().join("s1"));
    assert_eq!(after.len(), 1);
    assert_eq!(after[0]["kind"], "reset");
    let mut rx = ccx.cx.events.subscribe();
    assert_eq!(view(&elide(), &c.history, 1_000_000, &ccx).await, v);
    assert!(warnings(&mut rx).is_empty());
}

#[tokio::test]
async fn a_line_that_doesn_t_parse_resets_unless_it_is_the_torn_end_of_the_log() {
    let tmp = tempfile::tempdir().unwrap();
    let (c, trimmed, log) = trimmed_twice(tmp.path()).await;
    let path = tmp.path().join("s1/context/log.jsonl");
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);

    // A crash while appending: the last line is cut off. Nothing was sent with it.
    std::fs::write(&path, format!("{log}{{\"kind\":\"eli")).unwrap();
    let mut rx = ccx.cx.events.subscribe();
    assert_eq!(
        view(&elide(), &c.history, 1_000_000, &ccx).await[..trimmed.len()],
        trimmed[..]
    );
    assert!(warnings(&mut rx)[0].contains("newer than the saved session"));
    assert_eq!(
        records(&tmp.path().join("s1")).len(),
        2,
        "the torn line is gone"
    );

    // Garbage in the middle of the log.
    let first = log.lines().next().unwrap();
    std::fs::write(&path, format!("not json\n{first}\n")).unwrap();
    let v = view(&elide(), &c.history, 1_000_000, &ccx).await;
    assert_eq!(thinking_left(&v), 0);
    assert_eq!(records(&tmp.path().join("s1"))[0]["kind"], "reset");
}

#[tokio::test]
async fn an_edit_to_a_block_already_edited_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let (c, _, log) = trimmed_twice(tmp.path()).await;
    let path = tmp.path().join("s1/context/log.jsonl");
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let mut lines: Vec<Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    // The second record also edits the first record's first block.
    let twice = lines[0]["edits"][0].clone();
    lines[1]["edits"].as_array_mut().unwrap().insert(0, twice);
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(&path, text).unwrap();
    let mut rx = ccx.cx.events.subscribe();
    view(&elide(), &c.history, 1_000_000, &ccx).await;
    let w = warnings(&mut rx);
    assert!(
        w[0].contains("record 2") && w[0].contains("edited twice"),
        "{w:#?}"
    );
}

#[tokio::test]
async fn records_from_before_a_crash_are_dropped_and_don_t_come_back_when_ids_are_reused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("s1");
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    // The process trims at the start of a turn, then stops before the session is saved:
    // the history on disk ends before that turn, but the log has the trim.
    let mut lost = long_session();
    lost.turn("now the docs");
    view(&elide(), &lost.history, 25_000, &ccx).await;
    assert_eq!(records(&dir).len(), 1);

    // The next process reuses the turn's event id for another prompt.
    let mut resumed = long_session();
    resumed.turn("something else entirely");
    let mut rx = ccx.cx.events.subscribe();
    let v = view(&elide(), &resumed.history, 1_000_000, &ccx).await;
    assert_eq!(v, resumed.history.messages(), "the lost trim isn't applied");
    assert!(warnings(&mut rx)[0].contains("newer than the saved session"));
    assert!(records(&dir).is_empty(), "it is gone from the log");

    // A process after that, with the model having answered since: still nothing comes
    // back, and nothing is reset.
    for _ in 0..3 {
        resumed.bash(100);
    }
    let mut rx = ccx.cx.events.subscribe();
    let v = view(&elide(), &resumed.history, 1_000_000, &ccx).await;
    assert_eq!(v, resumed.history.messages());
    assert!(warnings(&mut rx).is_empty());
}

#[tokio::test]
async fn a_copy_left_by_a_crash_is_reused_and_a_foreign_file_is_not_trusted() {
    let tmp = tempfile::tempdir().unwrap();
    let c = long_session();
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let expected = view(&elide(), &c.history, 25_000, &ccx).await;
    // As if the process stopped after saving the copies but before the record.
    let dir = tmp.path().join("s1");
    std::fs::remove_file(dir.join("context/log.jsonl")).unwrap();
    let elided = dir.join("context/elided");
    let first: PathBuf = std::fs::read_dir(&elided)
        .unwrap()
        .map(|e| e.unwrap().path())
        .min()
        .unwrap();
    std::fs::write(&first, "something else").unwrap();
    let mut rx = ccx.cx.events.subscribe();
    let v = view(&elide(), &c.history, 25_000, &ccx).await;
    assert_ne!(v, expected, "one stub can't point at the changed file");
    assert!(format!("{v:?}").contains("Run the command again"));
    let warned = received(&mut rx).into_iter().any(
        |e| matches!(e, AgentEvent::Warning { message } if message.contains("a different file")),
    );
    assert!(warned);
    assert_eq!(std::fs::read_to_string(&first).unwrap(), "something else");
    // The record holds the stub that was sent, so a reload renders the same view.
    assert_eq!(view(&elide(), &c.history, 25_000, &ccx).await, v);
}

#[cfg(unix)]
#[tokio::test]
async fn a_trim_whose_record_can_t_be_saved_isn_t_made() {
    use std::os::unix::fs::PermissionsExt as _;
    let tmp = tempfile::tempdir().unwrap();
    let c = long_session();
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let log = tmp.path().join("s1/context/log.jsonl");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, "").unwrap();
    std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o400)).unwrap();
    let mut rx = ccx.cx.events.subscribe();
    let ctx = elide();
    let v = view(&ctx, &c.history, 25_000, &ccx).await;
    assert_eq!(v, c.history.messages(), "the view goes out unchanged");
    let w = warnings(&mut rx);
    assert!(w[0].contains("a compaction wasn't made"), "{w:#?}");
    let out = compact(&ctx, &c.history, 25_000, &ccx, CompactReason::Manual).await;
    assert!(!out.changed);
    assert!(out.message.contains("couldn't be saved"), "{}", out.message);
    // Nothing is half-applied: the next view decides again.
    std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o600)).unwrap();
    let v = view(&ctx, &c.history, 25_000, &ccx).await;
    assert_ne!(v, c.history.messages());
    assert_eq!(view(&elide(), &c.history, 25_000, &ccx).await, v);
}

#[tokio::test]
async fn a_reset_survives_a_crash_before_the_session_is_saved() {
    let tmp = tempfile::tempdir().unwrap();
    let (c, _, log) = trimmed_twice(tmp.path()).await;
    let path = tmp.path().join("s1/context/log.jsonl");
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let mut lines: Vec<Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    lines[0]["edits"][0]["hash"] = json!("0000");
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(&path, text).unwrap();

    // A new process starts a turn: the damaged log is replaced by a reset.
    let mut first = c.history.clone();
    first.push(2, Message::user("go on"));
    let v = view(&elide(), &first, 1_000_000, &ccx).await;
    assert_eq!(thinking_left(&v), 0);
    assert_eq!(records(&tmp.path().join("s1"))[0]["kind"], "reset");

    // It stops before the turn ends, so that prompt is never saved, and the next
    // process reuses its event id for another one. The reset still applies.
    let mut second = c.history.clone();
    second.push(2, Message::user("something else"));
    let mut rx = ccx.cx.events.subscribe();
    let v = view(&elide(), &second, 1_000_000, &ccx).await;
    assert_eq!(thinking_left(&v), 0);
    assert!(warnings(&mut rx).is_empty());

    // A reset that doesn't match is never dropped as harmless: a fresh one replaces it.
    let last = second.last().unwrap().id;
    let stale = json!({"kind": "reset", "trigger": "recovery", "at_event": last,
                       "at_blocks": 1, "at_hash": "0000"});
    std::fs::write(&path, format!("{stale}\n")).unwrap();
    let v = view(&elide(), &second, 1_000_000, &ccx).await;
    assert_eq!(thinking_left(&v), 0);
    let after = records(&tmp.path().join("s1"));
    assert_eq!(after.len(), 1);
    assert_eq!(after[0]["kind"], "reset");
    assert_ne!(after[0]["at_hash"], "0000");

    // Nor is a record that a reset follows, though the model never answered after it.
    let mut ahead = lines[1].clone();
    ahead["at_event"] = json!(last);
    ahead["at_hash"] = json!("0000");
    std::fs::write(&path, format!("{ahead}\n{}\n", after[0])).unwrap();
    let v = view(&elide(), &second, 1_000_000, &ccx).await;
    assert_eq!(thinking_left(&v), 0);
}

#[tokio::test]
async fn a_trim_still_matches_after_the_call_s_reminders_are_added() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = long_session();
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let trimmed = view(&elide(), &c.history, 25_000, &ccx).await;
    // The session adds the call's reminders to the newest message after the view, and
    // the model answers.
    c.history
        .append_to_last_user(1, vec![reminder("remember the plan")]);
    c.bash(100);
    let mut rx = ccx.cx.events.subscribe();
    let v = view(&elide(), &c.history, 1_000_000, &ccx).await;
    assert!(warnings(&mut rx).is_empty());
    let n = trimmed.len() - 1;
    assert_eq!(v[..n], trimmed[..n]);
    assert_eq!(records(&tmp.path().join("s1"))[0]["kind"], "elided");
    assert!(v[v.len() - 2].content.iter().any(is_thinking));
}

#[tokio::test]
async fn when_the_log_can_t_be_rewritten_no_more_trims_are_made() {
    let tmp = tempfile::tempdir().unwrap();
    let c = long_session();
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let context = tmp.path().join("s1/context");
    std::fs::create_dir_all(&context).unwrap();
    // A torn line to drop, and something in the way of the new file.
    std::fs::write(context.join("log.jsonl"), "{\"kind\":\"eli").unwrap();
    std::fs::create_dir(context.join("log.jsonl.tmp")).unwrap();
    let ctx = elide();
    let mut rx = ccx.cx.events.subscribe();
    let v = view(&ctx, &c.history, 25_000, &ccx).await;
    assert_eq!(v, c.history.messages(), "no trim, though over `soft`");
    let w = warnings(&mut rx);
    assert!(
        w.iter()
            .any(|m| m.contains("no more compactions in this session")),
        "{w:#?}"
    );
    let out = compact(&ctx, &c.history, 25_000, &ccx, CompactReason::Manual).await;
    assert!(!out.changed);
    assert!(
        out.message.contains("compactions are off"),
        "{}",
        out.message
    );
}
