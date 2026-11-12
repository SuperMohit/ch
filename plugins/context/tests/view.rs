//! The strategy's views, called directly over hand-built histories: the invariants of
//! ADR 0004 that apply to trimming.

mod common;

use common::*;

// ---------- tests ----------

#[tokio::test]
async fn at_or_below_soft_the_history_is_sent_as_it_is() {
    let tmp = tempfile::tempdir().unwrap();
    let c = long_session();
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let size = tokens(&c.history.messages());
    // Just at `soft` of the usable window.
    let window = (size as f64 / 0.60).ceil() as u64;
    let v = view(&elide(), &c.history, window, &ccx).await;
    assert_eq!(v, c.history.messages());
    assert!(records(&tmp.path().join("s1")).is_empty());
}

#[tokio::test]
async fn above_soft_the_middle_is_trimmed_down_to_trim_to() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("s1");
    let c = long_session();
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let mut rx = ccx.cx.events.subscribe();
    let window = 25_000;
    let before = tokens(&c.history.messages());
    assert!(before as f64 > 0.60 * window as f64, "{before}");

    let v = view(&elide(), &c.history, window, &ccx).await;
    let after = tokens(&v);
    assert_invariants(&c.history, &v);
    assert!(after as f64 <= 0.40 * window as f64, "{after}");
    assert!((before - after) as f64 >= 0.10 * window as f64);
    // The recent window is word for word, apart from thinking.
    let suffix = untouched_suffix(&c.history, &v);
    assert!(suffix >= 4, "at least two steps: {suffix}");
    let recent = &v[v.len() - suffix..];
    assert!(
        tokens(recent) as f64 >= 0.30 * window as f64,
        "{}",
        tokens(recent)
    );
    // Stubs name files that hold the originals.
    assert!(assert_stubs_read_back(&c.history, &v) > 5);
    let log = records(&dir);
    assert_eq!(log.len(), 1);
    assert_eq!(log[0]["trigger"], "soft");
    assert_thinking_rule(&c.history, &v, &log);

    let events = received(&mut rx);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ContextCompacted { tokens_before, tokens_after, .. }
            if *tokens_before == before && *tokens_after == after
    )));
    let custom = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Custom { source, name, data }
                if source == "context" && name == "compaction" =>
            {
                Some(data.clone())
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(custom["trigger"], "soft");
    assert!(custom["results_elided"].as_u64().unwrap() > 5);
    assert!(custom["thinking_dropped"].as_u64().unwrap() > 5);
}

#[tokio::test]
async fn between_trims_each_view_extends_the_last_and_a_reload_renders_the_same_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = long_session();
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let ctx = elide();
    let window = 25_000;
    let first = view(&ctx, &c.history, window, &ccx).await;
    assert_eq!(records(&tmp.path().join("s1")).len(), 1);

    // A few small steps don't reach `soft` again: no new trim, and the view only grows.
    // Their thinking was written against the trimmed view, so it stays.
    for _ in 0..3 {
        c.bash(100);
    }
    let next = view(&ctx, &c.history, window, &ccx).await;
    assert_eq!(next[..first.len()], first[..]);
    let log = records(&tmp.path().join("s1"));
    assert_eq!(log.len(), 1);
    assert_thinking_rule(&c.history, &next, &log);
    let newest = next.len() - 2;
    assert!(next[newest].content.iter().any(is_thinking));

    // A new process renders the same bytes from the history and the log, also with
    // settings under which it would decide nothing: the bytes come from the log.
    let other = Context::new(ContextConfig {
        strategy: Strategy::Elide,
        soft: 0.9,
        elide_min_chars: 20_000,
        superseded_tools: Vec::new(),
        ..Default::default()
    });
    let reloaded = view(&other, &c.history, 1_000_000, &ccx).await;
    assert_eq!(reloaded, next);
    // `none` doesn't trim, but a session trimmed before keeps its bytes.
    let none = Context::new(ContextConfig {
        strategy: Strategy::None,
        ..Default::default()
    });
    assert_eq!(view(&none, &c.history, 1_000_000, &ccx).await, next);
}

#[tokio::test]
async fn compact_between_turns_drops_the_final_answer_s_thinking() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = Conv::new("Fix the parser");
    for _ in 0..6 {
        c.bash(2000);
    }
    // The model's final answer, with its reasoning; `/compact` comes after it.
    c.history.push(
        1,
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "all done".into(),
                    signature: Some("signed".into()),
                },
                ContentBlock::text("Done."),
            ],
        },
    );
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let out = compact(&elide(), &c.history, 1_000_000, &ccx, CompactReason::Manual).await;
    assert!(out.changed, "{}", out.message);
    // It was signed against the untrimmed history, so the next turn can't send it.
    c.history.push(2, Message::user("next"));
    let v = view(&elide(), &c.history, 1_000_000, &ccx).await;
    let answer = &v[v.len() - 2];
    assert_eq!(answer.content, [ContentBlock::text("Done.")]);
}

#[tokio::test]
async fn a_trim_that_frees_too_little_is_not_made() {
    let tmp = tempfile::tempdir().unwrap();
    // Many short steps: almost nothing in the middle is long enough to trim.
    let mut c = Conv::new("Fix the parser");
    for _ in 0..60 {
        c.bash(300);
    }
    c.bash(700);
    for _ in 0..10 {
        c.bash(300);
    }
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let size = tokens(&c.history.messages());
    let window = (size as f64 / 0.70) as u64;
    let v = view(&elide(), &c.history, window, &ccx).await;
    assert_eq!(v, c.history.messages());
    assert!(records(&tmp.path().join("s1")).is_empty());
    // There was something to trim, only not enough.
    let out = compact(&elide(), &c.history, window, &ccx, CompactReason::Manual).await;
    assert!(out.changed, "{}", out.message);
}

#[tokio::test]
async fn superseded_plans_go_first_and_only_the_latest_stays() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = Conv::new("Fix the parser");
    for _ in 0..6 {
        c.plan();
        c.bash(400);
    }
    for _ in 0..4 {
        c.bash(400);
    }
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let out = compact(&elide(), &c.history, 100_000, &ccx, CompactReason::Manual).await;
    assert!(out.changed, "{}", out.message);
    assert!(
        out.message.contains("5 superseded calls"),
        "{}",
        out.message
    );
    let v = view(&elide(), &c.history, 100_000, &ccx).await;
    assert_invariants(&c.history, &v);
    let plans: Vec<&Value> = v
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolUse { name, input, .. } if name == "update_plan" => Some(input),
            _ => None,
        })
        .collect();
    assert_eq!(plans.len(), 6);
    for old in &plans[..5] {
        assert_eq!(
            **old,
            json!({"superseded": "a later update_plan call replaced this"})
        );
    }
    assert_eq!(plans[5]["plan"][0]["content"], "step 11");
}

#[tokio::test]
async fn long_inputs_become_markers_and_old_reminders_are_elided_but_user_text_stays() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = Conv::new("Fix the parser");
    c.write(5000);
    c.turn("<system-reminder> is how I start every prompt; keep going");
    c.write(5000);
    // A message the user sent during the turn.
    c.history.push(c.turn, Message::assistant("Working on it."));
    c.history
        .push(c.turn, Message::user("also update the docs"));
    for _ in 0..3 {
        c.bash(100);
    }
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let out = compact(&elide(), &c.history, 100_000, &ccx, CompactReason::Manual).await;
    assert!(out.changed, "{}", out.message);
    let v = view(&elide(), &c.history, 100_000, &ccx).await;
    assert_invariants(&c.history, &v);
    let flat = format!("{v:?}");
    assert_eq!(
        flat.matches("[elided from an earlier call to save context: 5,000 characters]")
            .count(),
        2
    );
    assert!(flat.contains("<system-reminder> is how I start every prompt"));
    assert!(flat.contains("also update the docs"));
    assert!(flat.contains("[reminder elided]"));
    // The task's own reminder is part of the preamble.
    assert!(flat.contains("the plan is empty"));
}

#[tokio::test]
async fn a_spilled_result_points_at_its_spill_file_and_needs_no_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("s1");
    std::fs::create_dir_all(dir.join("spill")).unwrap();
    let spill_dir = std::fs::canonicalize(tmp.path()).unwrap().join("s1/spill");
    // Spill names a file after the call it was made for: this result's call is `c1`.
    let own = spill_dir.join("c1-bash-ab12cd34.txt");
    let other = spill_dir.join("c2-bash-ef56ab78.txt");
    let head = "x".repeat(900);
    std::fs::write(&own, format!("{head}{}", "y".repeat(5000))).unwrap();
    std::fs::write(&other, format!("{head}{}", "y".repeat(5000))).unwrap();
    let notice = |path: &Path| {
        format!(
            "{head}\n\n[… The full output is saved at {}.\nRead the rest]",
            path.display()
        )
    };
    let mut c = Conv::new("Fix the parser");
    c.call("bash", json!({"command": "make"}), notice(&own));
    // Results that name another call's spill file get a copy instead.
    c.call("bash", json!({"command": "make"}), notice(&own));
    c.call("bash", json!({"command": "make"}), notice(&other));
    for _ in 0..3 {
        c.bash(100);
    }
    let ccx = ccx(Some(tmp.path()), &["bash"]);
    compact(&elide(), &c.history, 100_000, &ccx, CompactReason::Manual).await;
    let v = view(&elide(), &c.history, 100_000, &ccx).await;
    let result = |i: usize| match &v[i].content[0] {
        ContentBlock::ToolResult { content, .. } => content.clone(),
        other => panic!("{other:?}"),
    };
    let first = result(2);
    assert!(
        first.contains(&format!(
            "The whole output, 5,900 bytes, is saved at {}",
            own.display()
        )),
        "{first}"
    );
    assert!(first.contains("a preview of 4 lines"), "{first}");
    assert!(
        first.contains("from line N on with `tail -n +N <path> | head -n 200`"),
        "{first}"
    );
    // The second names `c1`'s file but is `c2`'s result; the third is `c3`'s.
    for i in [4, 6] {
        let stub = result(i);
        assert!(stub.contains("context/elided/"), "{stub}");
    }
    assert!(dir.join("context/elided").exists());
}

#[tokio::test]
async fn without_a_session_directory_stubs_say_to_run_the_command_again() {
    let c = long_session();
    let ctx = elide();
    let ccx = ccx(None, FILE_TOOLS);
    let v = view(&ctx, &c.history, 25_000, &ccx).await;
    assert_invariants(&c.history, &v);
    let flat = format!("{v:?}");
    assert!(flat.contains("Run the command again if you need it."));
    assert!(!flat.contains("saved at"));
    // The trim is kept in memory for the rest of the session.
    assert_eq!(view(&ctx, &c.history, 25_000, &ccx).await, v);
}

#[tokio::test]
async fn an_overflow_trims_every_candidate_and_results_in_the_window_but_the_newest_step() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = Conv::new("Fix the parser");
    for _ in 0..6 {
        c.bash(2000);
    }
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    // A big window: nothing is over `soft`, and the whole session is the recent window.
    let window = 1_000_000;
    let out = compact(&elide(), &c.history, window, &ccx, CompactReason::Overflow).await;
    assert!(out.changed);
    let v = view(&elide(), &c.history, window, &ccx).await;
    assert_invariants(&c.history, &v);
    let stubbed = |m: &Message| matches!(&m.content[0], ContentBlock::ToolResult { content, .. } if content.starts_with("[Output elided"));
    let results: Vec<bool> = v.iter().skip(2).step_by(2).map(stubbed).collect();
    assert_eq!(results, [true, true, true, true, true, false]);
    assert_eq!(records(&tmp.path().join("s1"))[0]["trigger"], "overflow");

    // Nothing left to trim: the session isn't sent again.
    let again = compact(&elide(), &c.history, window, &ccx, CompactReason::Overflow).await;
    assert!(!again.changed, "{}", again.message);
}
#[tokio::test]
async fn each_session_has_its_own_log() {
    let tmp = tempfile::tempdir().unwrap();
    let c = long_session();
    let ctx = elide();
    let a = ccx_for("a", Some(tmp.path()), FILE_TOOLS);
    let b = ccx_for("b", Some(tmp.path()), FILE_TOOLS);
    let trimmed = view(&ctx, &c.history, 25_000, &a).await;
    assert_ne!(trimmed, c.history.messages());
    // Session `b` (after `/clear`, say) has no trim, even with a bigger window.
    assert_eq!(
        view(&ctx, &c.history, 1_000_000, &b).await,
        c.history.messages()
    );
    // Back in `a` (`/resume`), its log is read again.
    assert_eq!(view(&ctx, &c.history, 1_000_000, &a).await, trimmed);
}

#[tokio::test]
async fn under_none_nothing_is_trimmed_and_compact_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let c = long_session();
    let none = Context::new(ContextConfig {
        strategy: Strategy::None,
        ..Default::default()
    });
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    assert_eq!(
        view(&none, &c.history, 25_000, &ccx).await,
        c.history.messages()
    );
    let out = compact(&none, &c.history, 25_000, &ccx, CompactReason::Overflow).await;
    assert!(!out.changed);
    assert!(out.message.contains("off"), "{}", out.message);
    assert!(records(&tmp.path().join("s1")).is_empty());
}

#[tokio::test]
async fn a_result_shorter_than_its_stub_is_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    // A long session path makes the stub of a 205-character result longer than it.
    let sessions = tmp
        .path()
        .join("sessions-in-a-deeply-nested-place".repeat(4));
    std::fs::create_dir(&sessions).unwrap();
    let mut c = Conv::new("Fix the parser");
    c.call("bash", json!({"command": "ls"}), "x".repeat(205));
    c.bash(5000);
    for _ in 0..3 {
        c.bash(100);
    }
    let ccx = ccx(Some(&sessions), FILE_TOOLS);
    let ctx = Context::new(ContextConfig {
        strategy: Strategy::Elide,
        elide_min_chars: 200,
        ..Default::default()
    });
    let out = compact(&ctx, &c.history, 1_000_000, &ccx, CompactReason::Manual).await;
    assert!(out.message.contains("1 tool result"), "{}", out.message);
    let v = view(&ctx, &c.history, 1_000_000, &ccx).await;
    assert_eq!(v[2].content[0], c.history.messages()[2].content[0]);
    assert_ne!(v[4].content[0], c.history.messages()[4].content[0]);
}

#[tokio::test]
async fn the_projection_counts_the_thinking_a_trim_drops() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = Conv::new("Fix the parser");
    c.think = 1500;
    for _ in 0..30 {
        c.bash(2000);
    }
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let window = 45_000;
    let before = tokens(&c.history.messages());
    assert!(before as f64 > 0.60 * window as f64, "{before}");
    let v = view(&elide(), &c.history, window, &ccx).await;
    let after = tokens(&v);
    // Dropping the thinking frees most of what's needed: few results go, and the view
    // ends near `trim_to`, not far below it.
    assert!(after as f64 <= 0.40 * window as f64, "{after}");
    assert!(after as f64 >= 0.35 * window as f64, "{after}");
}

#[tokio::test]
async fn a_call_that_copies_a_stub_written_in_this_session_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let c = long_session();
    let ctx = elide();
    let hook = ctx.marker_hook();
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let v = view(&ctx, &c.history, 25_000, &ccx).await;
    let stub = v
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            ContentBlock::ToolResult { content, .. } if content.starts_with("[Output elided") => {
                Some(content.clone())
            }
            _ => None,
        })
        .unwrap();
    let call = |content: String| HookEvent::PreTool {
        call: ToolCall {
            id: "w1".into(),
            name: "write_file".into(),
            input: json!({"path": "notes.md", "content": content}),
        },
        read_only: false,
    };
    let copied = call(format!("The log said:\n{stub}\n"));
    assert_eq!(hook.on(&copied, &ccx.cx).await, HookDecision::deny(REFUSED));
    // Another session's stubs are not this one's; prose about stubs is fine.
    let other = ccx_for("s2", Some(tmp.path()), FILE_TOOLS);
    assert_eq!(hook.on(&copied, &other.cx).await, HookDecision::Continue);
    let prose = call("A stub looks like `[Output elided to save context: 3 lines, …]`.".into());
    assert_eq!(hook.on(&prose, &ccx.cx).await, HookDecision::Continue);
}

#[tokio::test]
async fn once_down_to_trim_to_a_trim_takes_what_lies_between_its_edits() {
    let tmp = tempfile::tempdir().unwrap();
    let c = long_session();
    let ccx = ccx(Some(tmp.path()), FILE_TOOLS);
    let v = view(&elide(), &c.history, 25_000, &ccx).await;
    let log = records(&tmp.path().join("s1"));
    let edits = log[0]["edits"].as_array().unwrap();
    let pos = |e: &Value| {
        (
            e["event"].as_u64().unwrap(),
            e["block"].as_u64().unwrap() as usize,
        )
    };
    let (first, last) = (pos(&edits[0]), pos(edits.last().unwrap()));
    // Reminders come after results in priority, so the results alone reached `trim_to`;
    // the reminders between the first and last result went too.
    let mut between = 0;
    for (event, message) in c.history.events().iter().zip(&v) {
        for (b, block) in event.message.content.iter().enumerate() {
            let at = (event.id, b);
            if let ContentBlock::Text { text } = block
                && text.starts_with("<system-reminder>")
                && first < at
                && at < last
            {
                assert_eq!(message.content[b], ContentBlock::text("[reminder elided]"));
                between += 1;
            }
        }
    }
    assert!(between > 3, "{between}");
}
