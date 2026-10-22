//! The REPL loop and the turn runner.

use std::io::IsTerminal;
use std::path::Path;
use std::time::Instant;

use ch_kernel::{Outcome, Session, TerminationReason, Usage};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::approval::{Pending, is_interrupt_key};
use crate::ask::ask;
use crate::format::{bold, dim, footer, red};
use crate::input::{Input, LineEditor, normalize_input};
use crate::markdown::render_message;
use crate::terminal::{KeyPoller, Term, install_panic_hook, next_key};
use crate::view::TurnView;
use crate::{ReplConfig, ReplOptions, RuleSaved};

/// Runs the interactive loop until the user exits.
pub(crate) async fn run_repl(
    mut session: Session,
    approvals: &mut UnboundedReceiver<Pending>,
    config: &ReplConfig,
    options: ReplOptions,
) -> anyhow::Result<i32> {
    if !std::io::stdin().is_terminal() {
        session.end().await;
        anyhow::bail!(
            "the REPL needs a terminal on stdin; for scripted runs use `ch -p \"<task>\"`"
        );
    }
    install_panic_hook();

    let commands: Vec<(String, String)> = session
        .registry()
        .commands
        .iter()
        .map(|(_, c)| (c.name().to_string(), c.description().to_string()))
        .collect();
    let history = config.history_path().map(|p| {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        (p, config.history_size)
    });
    let mut editor = LineEditor::spawn(history, commands);
    let mut term = Term::new();

    banner(&mut term, &session);
    if let Some(prompt) = options.initial_prompt.filter(|p| !p.trim().is_empty()) {
        term.line(&format!("{} {}", dim("›"), prompt.trim()));
        run_turn(
            &mut session,
            prompt,
            approvals,
            &mut term,
            KeyPoller::start(),
        )
        .await;
    }

    let mut ctrl_c_once = false;
    loop {
        let input = match editor.read().await {
            Ok(i) => i,
            Err(e) => {
                term.line(&red(&format!("error: {e:#}")));
                break;
            }
        };
        let line = match input {
            Input::CtrlD => break,
            Input::CtrlC => {
                if ctrl_c_once {
                    break;
                }
                ctrl_c_once = true;
                term.line(&dim("(press Ctrl-C again or Ctrl-D to exit)"));
                continue;
            }
            Input::Line(l) => {
                ctrl_c_once = false;
                normalize_input(&l)
            }
        };
        if line.is_empty() {
            continue;
        }
        if let Some(result) = session.run_command(&line).await {
            match result {
                Ok(Outcome::Done) => {}
                Ok(Outcome::Message(text)) => term.write(&render_message(&text)),
                Ok(Outcome::Prompt(prompt)) => {
                    run_turn(
                        &mut session,
                        prompt,
                        approvals,
                        &mut term,
                        KeyPoller::start(),
                    )
                    .await;
                }
                Ok(Outcome::Exit) => break,
                Err(e) => term.line(&red(&format!("error: {e:#}"))),
            }
            continue;
        }
        run_turn(&mut session, line, approvals, &mut term, KeyPoller::start()).await;
    }

    editor.shutdown();
    session.end().await;
    Ok(0)
}

fn banner(term: &mut Term, session: &Session) {
    let workspace = shorten_home(session.workspace());
    term.line(&format!(
        "{} {}",
        bold("ch"),
        dim(&format!("· {} · {workspace}", session.model()))
    ));
    if session.turn() > 0 {
        term.line(&dim(&format!(
            "resumed session {} ({} turns)",
            session.id(),
            session.turn()
        )));
    }
    term.line(&dim(
        "/help for commands · Alt-Enter or a trailing \\ for a new line · Esc interrupts · \
         Ctrl-D exits",
    ));
}

fn shorten_home(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

fn usage_since(before: &Usage, after: &Usage) -> Usage {
    Usage {
        input_tokens: after.input_tokens.saturating_sub(before.input_tokens),
        output_tokens: after.output_tokens.saturating_sub(before.output_tokens),
        cache_read_tokens: after
            .cache_read_tokens
            .saturating_sub(before.cache_read_tokens),
        cache_write_tokens: after
            .cache_write_tokens
            .saturating_sub(before.cache_write_tokens),
    }
}

/// Runs one turn, rendering its events, handling approvals and interrupts, and prints
/// the footer.
pub(crate) async fn run_turn(
    session: &mut Session,
    prompt: String,
    approvals: &mut UnboundedReceiver<Pending>,
    term: &mut Term,
    mut keys: Option<KeyPoller>,
) {
    let mut events = session.events().subscribe();
    let cancel = session.cancel_handle();
    let workspace = session.workspace().clone();
    let on_saved = session.registry().services.get::<RuleSaved>();
    let usage_before = session.usage();
    let cost_before = session.cost();
    let started = Instant::now();

    let mut view = TurnView::new();
    let mut interrupted = false;

    let result = {
        let fut = session.run_turn(prompt);
        tokio::pin!(fut);
        loop {
            tokio::select! {
                result = &mut fut => break result,
                ev = events.recv() => match ev {
                    Ok(ev) => term.apply(view.handle(&ev)),
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => break fut.await,
                },
                key = next_key(&mut keys) => match key {
                    Some(k) if is_interrupt_key(&k) && !interrupted => {
                        interrupted = true;
                        cancel.cancel();
                        term.apply(view.finish());
                        term.line(&dim("  ⎿ interrupting…"));
                    }
                    Some(_) => {}
                    // The key reader died; keep going without it.
                    None => keys = None,
                },
                Some(pending) = approvals.recv() => {
                    // Render what led up to the request (its tool line) first.
                    drain(&mut events, &mut view, term);
                    term.apply(view.finish());
                    let answer =
                        ask(&pending.request, &mut keys, term, &workspace, on_saved.as_deref()).await;
                    if answer.cancel_turn {
                        interrupted = true;
                        cancel.cancel();
                    }
                    let _ = pending.reply.send(answer.approval);
                }
            }
        }
    };
    drain(&mut events, &mut view, term);
    term.apply(view.finish());
    drop(keys);

    let usage = usage_since(&usage_before, &session.usage());
    let cost = (session.cost() - cost_before).max(0.0);
    term.line(&footer(&result.reason, &usage, cost, started.elapsed()));
    if matches!(result.reason, TerminationReason::Cancelled) {
        term.line(&dim(
            "  (the conversation so far is kept; continue or try again)",
        ));
    }
}

fn drain(
    events: &mut tokio::sync::broadcast::Receiver<ch_kernel::AgentEvent>,
    view: &mut TurnView,
    term: &mut Term,
) {
    loop {
        match events.try_recv() {
            Ok(ev) => term.apply(view.handle(&ev)),
            Err(TryRecvError::Lagged(_)) => continue,
            Err(_) => break,
        }
    }
}
