//! Terminal side effects: writing (with raw-mode newline handling), the spinner, and
//! key polling during turns.
//!
//! During a turn the terminal is in raw mode so Esc and Ctrl-C arrive as key presses.
//! Raw mode is always undone: by [`KeyPoller`]'s `Drop`, and by a panic hook installed
//! with [`install_panic_hook`].

use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEvent, KeyEventKind};
use crossterm::{cursor, terminal};
use indicatif::{ProgressBar, ProgressStyle};
use tokio::sync::mpsc;
use unicode_width::UnicodeWidthStr;

use crate::view::Action;

/// Restores the terminal before the default panic message is printed.
pub fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = terminal::disable_raw_mode();
            let _ = crossterm::execute!(std::io::stdout(), cursor::Show);
            previous(info);
        }));
    });
}

/// Writes to stdout and owns the spinner.
pub struct Term {
    /// Raw mode is on, so `\n` must be written as `\r\n`.
    raw: bool,
    spinner: Option<ProgressBar>,
    spinner_message: Option<String>,
}

impl Default for Term {
    fn default() -> Self {
        Self::new()
    }
}

impl Term {
    pub fn new() -> Self {
        Self {
            raw: false,
            spinner: None,
            spinner_message: None,
        }
    }

    pub fn set_raw(&mut self, raw: bool) {
        self.raw = raw;
    }

    /// Writes `text`, stopping the spinner first.
    pub fn write(&mut self, text: &str) {
        self.stop_spinner();
        let mut out = std::io::stdout().lock();
        let _ = if self.raw {
            out.write_all(text.replace('\n', "\r\n").as_bytes())
        } else {
            out.write_all(text.as_bytes())
        };
        let _ = out.flush();
    }

    /// Writes `text` followed by a newline.
    pub fn line(&mut self, text: &str) {
        self.write(&format!("{text}\n"));
    }

    pub fn apply(&mut self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Write(t) => self.write(&t),
                Action::Restyle { raw, styled } => self.restyle(&raw, &styled),
                Action::Spinner(Some(msg)) => self.start_spinner(msg),
                Action::Spinner(None) => self.stop_spinner(),
            }
        }
    }

    /// Redraws the just-completed raw line as `styled`. Falls back to ending the line
    /// when the raw text's on-screen height can't be known.
    fn restyle(&mut self, raw: &str, styled: &str) {
        self.stop_spinner();
        let Ok((cols, rows)) = terminal::size() else {
            self.write("\n");
            return;
        };
        let height = screen_rows(raw, cols);
        if height >= rows as usize {
            self.write("\n");
            return;
        }
        let mut out = std::io::stdout().lock();
        if height > 1 {
            let up = u16::try_from(height - 1).unwrap_or(u16::MAX);
            let _ = crossterm::queue!(out, cursor::MoveUp(up));
        }
        let _ = crossterm::queue!(
            out,
            cursor::MoveToColumn(0),
            terminal::Clear(terminal::ClearType::FromCursorDown)
        );
        drop(out);
        self.write(&format!("{styled}\n"));
    }

    fn start_spinner(&mut self, msg: String) {
        if !std::io::stderr().is_terminal() {
            return;
        }
        if let Some(sp) = &self.spinner {
            if self.spinner_message.as_deref() != Some(msg.as_str()) {
                sp.set_message(msg.clone());
                self.spinner_message = Some(msg);
            }
            return;
        }
        let style = ProgressStyle::with_template("{spinner:.cyan} {msg:.dim} {elapsed:.dim}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner())
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ");
        let sp = ProgressBar::new_spinner().with_style(style);
        sp.set_message(msg.clone());
        sp.enable_steady_tick(Duration::from_millis(80));
        self.spinner = Some(sp);
        self.spinner_message = Some(msg);
    }

    pub fn stop_spinner(&mut self) {
        if let Some(sp) = self.spinner.take() {
            sp.finish_and_clear();
        }
        self.spinner_message = None;
    }
}

/// How many terminal rows `text` takes when printed from column 0 at width `cols`.
pub fn screen_rows(text: &str, cols: u16) -> usize {
    let cols = usize::from(cols.max(1));
    let width = UnicodeWidthStr::width(text);
    width.div_ceil(cols).max(1)
}

/// Reads key presses on a background thread while a turn runs, with the terminal in
/// raw mode. Dropping it stops the thread and restores the terminal.
pub struct KeyPoller {
    rx: mpsc::UnboundedReceiver<KeyEvent>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl KeyPoller {
    /// Starts polling, or returns `None` when stdin isn't a terminal or raw mode can't
    /// be enabled.
    pub fn start() -> Option<Self> {
        if !std::io::stdin().is_terminal() || terminal::enable_raw_mode().is_err() {
            return None;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("ch-repl-keys".into())
            .spawn(move || {
                while !flag.load(Ordering::SeqCst) {
                    match event::poll(Duration::from_millis(30)) {
                        Ok(true) => match event::read() {
                            Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => {
                                if tx.send(k).is_err() {
                                    break;
                                }
                            }
                            Ok(_) => {}
                            Err(_) => break,
                        },
                        Ok(false) => {}
                        Err(_) => break,
                    }
                }
            });
        match thread {
            Ok(handle) => Some(Self {
                rx,
                stop,
                thread: Some(handle),
            }),
            Err(_) => {
                let _ = terminal::disable_raw_mode();
                None
            }
        }
    }

    /// The next key press; `None` if the reader stopped.
    pub async fn next(&mut self) -> Option<KeyEvent> {
        self.rx.recv().await
    }
}

impl Drop for KeyPoller {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = terminal::disable_raw_mode();
    }
}

/// The next key from an optional poller; never resolves when there is none.
pub async fn next_key(keys: &mut Option<KeyPoller>) -> Option<KeyEvent> {
    match keys {
        Some(k) => k.next().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_for_wrapped_text() {
        assert_eq!(screen_rows("", 80), 1);
        assert_eq!(screen_rows("abc", 80), 1);
        assert_eq!(screen_rows(&"x".repeat(80), 80), 1);
        assert_eq!(screen_rows(&"x".repeat(81), 80), 2);
        // Wide characters take two columns.
        assert_eq!(screen_rows(&"漢".repeat(41), 80), 2);
    }
}
