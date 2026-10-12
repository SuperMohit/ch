//! The line editor: reedline with file-backed history, multi-line input and slash
//! command completion.
//!
//! reedline blocks while reading, so it lives on its own thread and the async REPL loop
//! asks it for lines over a channel.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;

use reedline::{
    ColumnarMenu, Completer, Emacs, FileBackedHistory, KeyCode, KeyModifiers, MenuBuilder, Prompt,
    PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus, Reedline, ReedlineEvent,
    ReedlineMenu, Signal, Span, Suggestion, ValidationResult, Validator, default_emacs_keybindings,
};
use tokio::sync::oneshot;

/// What the user did at the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Line(String),
    CtrlC,
    CtrlD,
}

/// Input lines ending in `\` continue on the next line.
pub struct ContinuationValidator;

impl Validator for ContinuationValidator {
    fn validate(&self, line: &str) -> ValidationResult {
        if line.ends_with('\\') {
            ValidationResult::Incomplete
        } else {
            ValidationResult::Complete
        }
    }
}

/// Turns the editor buffer into the prompt text: `\` line continuations become plain
/// newlines.
pub fn normalize_input(buffer: &str) -> String {
    buffer
        .replace("\\\r\n", "\n")
        .replace("\\\n", "\n")
        .trim()
        .to_string()
}

/// Completes slash command names at the start of the line.
pub struct SlashCompleter {
    /// `(name, description)` without the slash.
    commands: Vec<(String, String)>,
}

impl SlashCompleter {
    pub fn new(mut commands: Vec<(String, String)>) -> Self {
        commands.sort();
        Self { commands }
    }
}

impl Completer for SlashCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        slash_suggestions(&self.commands, line, pos)
    }
}

/// Suggestions for `line` with the cursor at byte `pos`.
pub fn slash_suggestions(commands: &[(String, String)], line: &str, pos: usize) -> Vec<Suggestion> {
    let Some(before) = line.get(..pos) else {
        return Vec::new();
    };
    let Some(prefix) = before.strip_prefix('/') else {
        return Vec::new();
    };
    if prefix.contains(char::is_whitespace) {
        return Vec::new();
    }
    commands
        .iter()
        .filter(|(name, _)| name.starts_with(prefix))
        .map(|(name, desc)| Suggestion {
            value: format!("/{name}"),
            display_override: None,
            description: (!desc.is_empty()).then(|| desc.clone()),
            style: None,
            extra: None,
            span: Span::new(0, pos),
            append_whitespace: true,
            match_indices: None,
        })
        .collect()
}

/// The prompt: `› ` with `· ` on continuation lines.
pub struct ReplPrompt;

impl Prompt for ReplPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("› ")
    }
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("· ")
    }
    fn render_prompt_history_search_indicator(&self, search: PromptHistorySearch) -> Cow<'_, str> {
        let status = match search.status {
            PromptHistorySearchStatus::Passing => "",
            PromptHistorySearchStatus::Failing => "failing ",
        };
        Cow::Owned(format!("({status}search: {}) ", search.term))
    }
}

fn build_editor(history: Option<(PathBuf, usize)>, commands: Vec<(String, String)>) -> Reedline {
    let mut keys = default_emacs_keybindings();
    keys.add_binding(
        KeyModifiers::ALT,
        KeyCode::Enter,
        ReedlineEvent::Edit(vec![reedline::EditCommand::InsertNewline]),
    );
    keys.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion_menu".into()),
            ReedlineEvent::MenuNext,
        ]),
    );
    keys.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::BackTab,
        ReedlineEvent::MenuPrevious,
    );
    let menu = ColumnarMenu::default().with_name("completion_menu");
    let mut editor = Reedline::create()
        .with_validator(Box::new(ContinuationValidator))
        .with_completer(Box::new(SlashCompleter::new(commands)))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu)))
        .with_edit_mode(Box::new(Emacs::new(keys)));
    if let Some((path, size)) = history {
        match FileBackedHistory::with_file(size, path.clone()) {
            Ok(h) => editor = editor.with_history(Box::new(h)),
            Err(e) => tracing::warn!("history file {} unavailable: {e}", path.display()),
        }
    }
    editor
}

type Request = oneshot::Sender<std::io::Result<Input>>;

/// The line editor on its own thread.
pub struct LineEditor {
    tx: Option<std_mpsc::Sender<Request>>,
    thread: Option<JoinHandle<()>>,
}

impl LineEditor {
    /// Starts the editor thread. `history` is the history file and its size.
    pub fn spawn(history: Option<(PathBuf, usize)>, commands: Vec<(String, String)>) -> Self {
        let (tx, rx) = std_mpsc::channel::<Request>();
        let thread = std::thread::Builder::new()
            .name("ch-repl-editor".into())
            .spawn(move || {
                let mut editor = build_editor(history, commands);
                let prompt = ReplPrompt;
                while let Ok(reply) = rx.recv() {
                    let result = editor.read_line(&prompt).map(|sig| match sig {
                        Signal::Success(line) => Input::Line(line),
                        Signal::CtrlD => Input::CtrlD,
                        _ => Input::CtrlC,
                    });
                    let _ = reply.send(result);
                }
                // Dropping the editor writes the history file.
            })
            .ok();
        Self {
            tx: thread.as_ref().map(|_| tx),
            thread,
        }
    }

    /// Reads one entry from the user.
    pub async fn read(&self) -> anyhow::Result<Input> {
        let Some(tx) = &self.tx else {
            anyhow::bail!("the line editor could not be started");
        };
        let (reply, answer) = oneshot::channel();
        tx.send(reply)
            .map_err(|_| anyhow::anyhow!("the line editor stopped"))?;
        Ok(answer
            .await
            .map_err(|_| anyhow::anyhow!("the line editor stopped"))??)
    }

    /// Stops the editor thread, saving history.
    pub fn shutdown(&mut self) {
        self.tx.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for LineEditor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmds() -> Vec<(String, String)> {
        vec![
            ("help".into(), "Show help".into()),
            ("history".into(), String::new()),
            ("exit".into(), "Quit".into()),
        ]
    }

    #[test]
    fn continuation_lines() {
        assert!(matches!(
            ContinuationValidator.validate("abc \\"),
            ValidationResult::Incomplete
        ));
        assert!(matches!(
            ContinuationValidator.validate("abc"),
            ValidationResult::Complete
        ));
        assert_eq!(normalize_input("first \\\nsecond\n"), "first \nsecond");
        assert_eq!(normalize_input("  hi  "), "hi");
    }

    #[test]
    fn completes_slash_commands() {
        let s = slash_suggestions(&cmds(), "/h", 2);
        let names: Vec<&str> = s.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(names, vec!["/help", "/history"]);
        assert_eq!(s[0].description.as_deref(), Some("Show help"));
        assert_eq!(s[1].description, None);
        assert_eq!(s[0].span, Span::new(0, 2));
        assert_eq!(slash_suggestions(&cmds(), "/", 1).len(), 3);
        // Not at the start, or past the command name: nothing.
        assert!(slash_suggestions(&cmds(), "hello /h", 8).is_empty());
        assert!(slash_suggestions(&cmds(), "/help me", 8).is_empty());
    }
}
