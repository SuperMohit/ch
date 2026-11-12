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

use ch_kernel::util::printable;

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
    out: Box<dyn Write + Send>,
    size: fn() -> Option<(u16, u16)>,
    spinner_allowed: bool,
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
            out: Box::new(std::io::stdout()),
            size: || terminal::size().ok(),
            spinner_allowed: true,
            spinner: None,
            spinner_message: None,
        }
    }

    /// A terminal of `size` that writes to `out` and never shows a spinner.
    #[cfg(test)]
    pub fn capture(out: impl Write + Send + 'static, size: fn() -> Option<(u16, u16)>) -> Self {
        Self {
            out: Box::new(out),
            size,
            spinner_allowed: false,
            spinner: None,
            spinner_message: None,
        }
    }

    /// Writes `text`, stopping the spinner first. In raw mode (while a [`KeyPoller`]
    /// runs) `\n` alone doesn't return to column 0, so it is written as `\r\n`.
    pub fn write(&mut self, text: &str) {
        let text = &guard(text);
        self.stop_spinner();
        let out = &mut self.out;
        let _ = if terminal::is_raw_mode_enabled().unwrap_or(false) {
            out.write_all(text.replace('\n', "\r\n").as_bytes())
        } else {
            out.write_all(text.as_bytes())
        };
        let _ = out.flush();
    }

    /// Erases the character before the cursor, for line editing.
    pub fn backspace(&mut self) {
        let _ = self.out.write_all(b"\x08 \x08");
        let _ = self.out.flush();
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
        let Some((cols, rows)) = (self.size)() else {
            self.write("\n");
            return;
        };
        let height = screen_rows(raw, cols);
        if height >= rows as usize {
            self.write("\n");
            return;
        }
        let out = &mut self.out;
        if height > 1 {
            let up = u16::try_from(height - 1).unwrap_or(u16::MAX);
            let _ = crossterm::queue!(out, cursor::MoveUp(up));
        }
        let _ = crossterm::queue!(
            out,
            cursor::MoveToColumn(0),
            terminal::Clear(terminal::ClearType::FromCursorDown)
        );
        self.write(&format!("{styled}\n"));
    }

    fn start_spinner(&mut self, msg: String) {
        if !self.spinner_allowed || !std::io::stderr().is_terminal() {
            return;
        }
        let msg = printable(&msg);
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

/// `text` with every control character escaped except newline, tab and colour codes
/// (SGR: `ESC [`, digits and `;`, then `m`), the only escape sequences the REPL's own
/// output uses. Text from the model, tools and hooks is made printable before it is
/// formatted; this catches any that wasn't, so nothing written can move the cursor or
/// redraw the screen. Colour codes from such text still pass, so they are removed
/// where the text is formatted.
pub fn guard(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let len = match c {
            '\u{1b}' => sgr_len(&rest[1..]).map_or(0, |n| 1 + n),
            '\n' | '\t' => c.len_utf8(),
            _ => 0,
        };
        if len > 0 {
            out.push_str(&rest[..len]);
            rest = &rest[len..];
        } else {
            out.push_str(&printable(&rest[..c.len_utf8()]));
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// The length of an SGR sequence after an `ESC`, if `seq` starts with one.
fn sgr_len(seq: &str) -> Option<usize> {
    let body = seq.strip_prefix('[')?;
    let end = body.find(|c: char| !(c.is_ascii_digit() || c == ';'))?;
    body[end..].starts_with('m').then_some(end + 2)
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

    /// A poller fed by `rx` instead of the terminal, without raw mode.
    #[cfg(test)]
    pub fn from_keys(rx: mpsc::UnboundedReceiver<KeyEvent>) -> Self {
        Self {
            rx,
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
        }
    }

    /// The next key press; `None` if the reader stopped.
    pub async fn next(&mut self) -> Option<KeyEvent> {
        self.rx.recv().await
    }

    /// Key presses already read but not yet taken.
    pub fn take_pending(&mut self) -> Vec<KeyEvent> {
        std::iter::from_fn(|| self.rx.try_recv().ok()).collect()
    }
}

impl Drop for KeyPoller {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
            let _ = terminal::disable_raw_mode();
        }
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
    use crate::format::{dim, red};
    use crate::markdown::{MarkdownLines, render_message};

    #[test]
    fn the_guard_passes_styles_and_escapes_everything_else() {
        let styled = format!("{} {}\n\t", red("x"), dim("y"));
        assert_eq!(guard(&styled), styled);
        assert_eq!(guard("\u{1b}[2Kx\r\u{8}"), "\\u{1b}[2Kx\\u{d}\\u{8}");
        assert_eq!(guard("\u{1b}]0;title\u{7}"), "\\u{1b}]0;title\\u{7}");
        assert_eq!(guard("\u{1b}[1;31"), "\\u{1b}[1;31");
        assert_eq!(guard("\u{1b}[?25l"), "\\u{1b}[?25l");
        assert_eq!(guard("ab\u{202e}c"), "ab\\u{202e}c");
    }

    #[test]
    fn the_repls_own_output_passes_the_guard() {
        let md = "# Title\n\n- **bold** and `code`\n\n```rust\nfn main() {}\n```\n\n\
                  | a | b |\n|---|---|\n| 1 | 2 |\n\n> quoted *text*\n";
        let message = render_message(md);
        assert_eq!(guard(&message), message);
        let mut lines = MarkdownLines::default();
        for line in md.lines() {
            if let Some(styled) = lines.render(line) {
                assert_eq!(guard(&styled), styled);
            }
        }
    }

    #[test]
    fn writes_go_through_the_guard() {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        struct Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for Shared {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut term = Term::capture(Shared(buf.clone()), || None);
        term.line("ok\r\u{1b}[2Kfake");
        term.backspace();
        let written = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert_eq!(written, "ok\\u{d}\\u{1b}[2Kfake\n\u{8} \u{8}");
    }

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
