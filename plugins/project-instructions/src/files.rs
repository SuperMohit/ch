//! Finding and loading `CH.md` / `AGENTS.md` instruction files.

use std::path::{Path, PathBuf};

/// One instruction file that was found.
#[derive(Debug, Clone, PartialEq)]
pub struct InstructionFile {
    pub path: PathBuf,
    pub content: String,
}

/// The nearest directory at or above `start` that contains `.git` (a directory, or a
/// file for worktrees and submodules).
pub fn git_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Directories to search, outermost first: from the git root down to `workspace`, or
/// just `workspace` when it isn't inside a git repository.
pub fn search_dirs(workspace: &Path) -> Vec<PathBuf> {
    let Some(root) = git_root(workspace) else {
        return vec![workspace.to_path_buf()];
    };
    let mut dirs: Vec<PathBuf> = workspace
        .ancestors()
        .take_while(|d| d.starts_with(&root))
        .map(Path::to_path_buf)
        .collect();
    dirs.reverse();
    dirs
}

/// Loads the instruction files in order: `user_file` first (if any), then `names` in
/// each directory from the git root down to `workspace`. Files that resolve to the same
/// real path (symlinks, case-insensitive file systems) are loaded once. Unreadable files
/// are skipped with a warning.
pub fn load(workspace: &Path, user_file: Option<&Path>, names: &[String]) -> Vec<InstructionFile> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(u) = user_file {
        candidates.push(u.to_path_buf());
    }
    for dir in search_dirs(workspace) {
        for name in names {
            candidates.push(dir.join(name));
        }
    }

    let mut seen: Vec<PathBuf> = Vec::new();
    let mut out = Vec::new();
    for path in candidates {
        if !path.is_file() {
            continue;
        }
        let real = path.canonicalize().unwrap_or_else(|_| path.clone());
        if seen.contains(&real) {
            continue;
        }
        seen.push(real);
        match std::fs::read(&path) {
            Ok(bytes) => {
                let content = String::from_utf8_lossy(&bytes).trim().to_string();
                if !content.is_empty() {
                    out.push(InstructionFile { path, content });
                }
            }
            Err(e) => tracing::warn!(path = %path.display(), "skipping instruction file: {e}"),
        }
    }
    out
}

/// Splits `max_chars` between `files`, giving inner (later, more specific) files
/// priority. Returns, for each file, how many characters of it to keep.
pub fn allocate(files: &[InstructionFile], max_chars: usize) -> Vec<usize> {
    let mut remaining = max_chars;
    let mut keep = vec![0; files.len()];
    for (i, f) in files.iter().enumerate().rev() {
        let len = f.content.chars().count();
        keep[i] = len.min(remaining);
        remaining -= keep[i];
    }
    keep
}

/// Shows `path` with the home directory as `~`.
pub fn display_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

/// Renders the instructions block, or `None` when there are no files.
pub fn render(files: &[InstructionFile], max_chars: usize) -> Option<String> {
    if files.is_empty() {
        return None;
    }
    let keep = allocate(files, max_chars);
    let mut out = String::from(
        "# Project instructions\n\n\
         The user and the project's maintainers wrote these instructions. Follow them; they \
         take precedence over your default behaviour. Files are listed from the most general \
         to the most specific, and when two disagree the later (more specific) one wins.",
    );
    for (file, &n) in files.iter().zip(&keep) {
        let label = display_path(&file.path);
        let total = file.content.chars().count();
        out.push_str(&format!("\n\n<instructions path=\"{label}\">\n"));
        if n == total {
            out.push_str(&file.content);
        } else if n == 0 {
            out.push_str(&format!(
                "[omitted: this file ({total} characters) did not fit in the instructions \
                 budget. Read {label} if you need it.]"
            ));
        } else {
            let head: String = file.content.chars().take(n).collect();
            out.push_str(head.trim_end());
            out.push_str(&format!(
                "\n[truncated: showing the first {n} of {total} characters. Read {label} for \
                 the rest.]"
            ));
        }
        out.push_str("\n</instructions>");
    }
    Some(out)
}
