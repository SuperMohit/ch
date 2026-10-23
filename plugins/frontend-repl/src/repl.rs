//! The REPL loop and the turn runner.

use std::io::IsTerminal;
use std::path::Path;
use std::time::Instant;

use ch_kernel::{AgentEvent, Outcome, Session, TerminationReason};
use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::approval::{Pending, is_interrupt_key};
use crate::ask::ask;
use crate::format::{bold, dim, footer, red};
use crate::input::{Input, LineEditor, TypeAhead, normalize_input};
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
    // One receiver for the whole REPL, so events emitted outside a turn (by a slash
    // command, or by a plugin while waiting at the prompt) are shown too.
    let mut events = session.events().subscribe();
    let mut typed = TypeAhead::default();

    banner(&mut term, &session);
    if let Some(prompt) = options.initial_prompt.filter(|p| !p.trim().is_empty()) {
        term.line(&format!("{} {}", dim("›"), prompt.trim()));
        run_turn(
            &mut session,
            prompt,
            &mut events,
            approvals,
            &mut term,
            &mut typed,
            KeyPoller::start(),
        )
        .await;
    }

    let mut ctrl_c_once = false;
    let mut code = 0;
    loop {
        show_events(&mut events, &mut term);
        let input = match editor.read(typed.take()).await {
            Ok(i) => i,
            Err(e) => {
                term.line(&red(&format!("error: {e:#}")));
                code = 1;
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
        let command = run_command(
            &mut session,
            &line,
            &mut events,
            &mut term,
            &mut typed,
            KeyPoller::start(),
        )
        .await;
        if let Some(result) = command {
            match result {
                Ok(Outcome::Done) => {}
                Ok(Outcome::Message(text)) => term.write(&render_message(&text)),
                Ok(Outcome::Prompt(prompt)) => {
                    run_turn(
                        &mut session,
                        prompt,
                        &mut events,
                        approvals,
                        &mut term,
                        &mut typed,
                        KeyPoller::start(),
                    )
                    .await;
                }
                Ok(Outcome::Exit) => break,
                Err(e) => term.line(&red(&format!("error: {e:#}"))),
            }
            continue;
        }
        run_turn(
            &mut session,
            line,
            &mut events,
            approvals,
            &mut term,
            &mut typed,
            KeyPoller::start(),
        )
        .await;
    }

    editor.shutdown();
    session.end().await;
    Ok(code)
}

/// Runs a slash command with Esc and Ctrl-C cancelling it, as they do a turn, and shows
/// the events it emits. The terminal would otherwise be in cooked mode, where Ctrl-C
/// kills `ch` before the session is saved.
pub(crate) async fn run_command(
    session: &mut Session,
    line: &str,
    events: &mut Receiver<AgentEvent>,
    term: &mut Term,
    typed: &mut TypeAhead,
    mut keys: Option<KeyPoller>,
) -> Option<anyhow::Result<Outcome>> {
    let cancel = session.cancel_handle();
    let mut view = TurnView::new();
    let result = {
        let fut = session.run_command(line);
        tokio::pin!(fut);
        loop {
            tokio::select! {
                result = &mut fut => break result,
                Ok(ev) = events.recv() => term.apply(view.handle(&ev)),
                key = next_key(&mut keys) => match key {
                    Some(k) if is_interrupt_key(&k) => cancel.cancel(),
                    Some(k) => typed.push(&k),
                    None => keys = None,
                },
            }
        }
    };
    drop(keys);
    drain(events, &mut view, term);
    term.apply(view.finish());
    // A command that runs no turn leaves the cancel pending, where it would cancel
    // the next turn instead.
    cancel.clear_pending();
    result
}

/// Shows events emitted since the last turn or command.
fn show_events(events: &mut Receiver<AgentEvent>, term: &mut Term) {
    let mut view = TurnView::new();
    drain(events, &mut view, term);
    term.apply(view.finish());
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

/// Runs one turn, rendering its events, handling approvals and interrupts, and prints
/// the footer.
pub(crate) async fn run_turn(
    session: &mut Session,
    prompt: String,
    events: &mut Receiver<AgentEvent>,
    approvals: &mut UnboundedReceiver<Pending>,
    term: &mut Term,
    typed: &mut TypeAhead,
    mut keys: Option<KeyPoller>,
) {
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
                    Some(k) => typed.push(&k),
                    // The key reader died; keep going without it.
                    None => keys = None,
                },
                Some(pending) = approvals.recv() => {
                    // Render what led up to the request (its tool line) first.
                    drain(events, &mut view, term);
                    term.apply(view.finish());
                    // Keys typed before the question are type-ahead or an interrupt,
                    // never its answer.
                    let pending_keys = keys.as_mut().map(KeyPoller::take_pending);
                    for key in pending_keys.unwrap_or_default() {
                        if is_interrupt_key(&key) && !interrupted {
                            interrupted = true;
                            cancel.cancel();
                            term.line(&dim("  ⎿ interrupting…"));
                        } else {
                            typed.push(&key);
                        }
                    }
                    if interrupted {
                        // The turn is being cancelled, which answers the request.
                        continue;
                    }
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
    drain(events, &mut view, term);
    term.apply(view.finish());
    drop(keys);

    let usage = session.usage().since(&usage_before);
    let cost = (session.cost() - cost_before).max(0.0);
    term.line(&footer(&result.reason, &usage, cost, started.elapsed()));
    if matches!(result.reason, TerminationReason::Cancelled) {
        term.line(&dim(
            "  (the conversation so far is kept; continue or try again)",
        ));
    }
}

fn drain(events: &mut Receiver<AgentEvent>, view: &mut TurnView, term: &mut Term) {
    loop {
        match events.try_recv() {
            Ok(ev) => term.apply(view.handle(&ev)),
            Err(TryRecvError::Lagged(_)) => continue,
            Err(_) => break,
        }
    }
}
