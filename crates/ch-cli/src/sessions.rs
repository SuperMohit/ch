//! Choosing a saved session for `--continue` and `--resume`.

use std::fmt::Write as _;

use anyhow::bail;
use ch_kernel::{SessionMeta, SessionSnapshot, SessionStore};

/// Which saved session to load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resume {
    /// Start a new session.
    New,
    /// The most recently updated session (`--continue`).
    Latest,
    /// A specific session (`--resume <ID>`).
    Id(String),
    /// Ask the user to pick one (`--resume`).
    Pick,
}

impl Resume {
    pub fn from_flags(continue_last: bool, resume: Option<Option<String>>) -> Self {
        match (continue_last, resume) {
            (_, Some(Some(id))) => Self::Id(id),
            (_, Some(None)) => Self::Pick,
            (true, None) => Self::Latest,
            (false, None) => Self::New,
        }
    }
}

/// The numbered list printed for `--resume` without an id.
pub fn format_session_list(sessions: &[SessionMeta]) -> String {
    let mut out = String::new();
    for (i, s) in sessions.iter().enumerate() {
        let title = if s.title.is_empty() {
            "(no prompt)"
        } else {
            &s.title
        };
        let _ = writeln!(
            out,
            "{:>3}. {}  {}  {} turn{}  {}",
            i + 1,
            s.updated_at.format("%Y-%m-%d %H:%M"),
            s.id,
            s.turns,
            if s.turns == 1 { "" } else { "s" },
            title
        );
    }
    out
}

/// Parses the user's choice (1-based) from a list of `count` sessions; returns the
/// 0-based index.
pub fn parse_choice(input: &str, count: usize) -> anyhow::Result<usize> {
    let input = input.trim();
    match input.parse::<usize>() {
        Ok(n) if (1..=count).contains(&n) => Ok(n - 1),
        _ => bail!("`{input}` is not a session number between 1 and {count}"),
    }
}

/// Loads the session `resume` asks for. `pick` is called with the list when the user
/// must choose, and returns the chosen 0-based index.
pub fn load(
    store: &SessionStore,
    resume: &Resume,
    pick: impl FnOnce(&[SessionMeta]) -> anyhow::Result<usize>,
) -> anyhow::Result<Option<SessionSnapshot>> {
    match resume {
        Resume::New => Ok(None),
        Resume::Latest => match store.latest()? {
            Some(meta) => Ok(Some(store.load(&meta.id)?)),
            None => Ok(None),
        },
        Resume::Id(id) => {
            if !store.path_for(id).exists() {
                let known: Vec<String> = store.list()?.into_iter().map(|m| m.id).take(5).collect();
                bail!(
                    "no session `{id}` in this project{}",
                    if known.is_empty() {
                        String::new()
                    } else {
                        format!(" (recent: {})", known.join(", "))
                    }
                );
            }
            Ok(Some(store.load(id)?))
        }
        Resume::Pick => {
            let sessions = store.list()?;
            if sessions.is_empty() {
                bail!("there are no saved sessions for this project yet");
            }
            let i = pick(&sessions)?;
            let meta = sessions
                .get(i)
                .ok_or_else(|| anyhow::anyhow!("no session number {}", i + 1))?;
            Ok(Some(store.load(&meta.id)?))
        }
    }
}
