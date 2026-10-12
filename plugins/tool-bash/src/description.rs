//! The two description variants of the `bash` tool.

/// Used when the dedicated file tools (`tools-fs`) are enabled.
pub(crate) fn with_file_tools(default_timeout: u64, max_timeout: u64) -> String {
    format!(
        "Run a shell command with `/bin/sh -c` in the workspace and return its output.

When: building, running tests, linters and scripts, git, package managers, and other \
terminal tasks. Do NOT use it to read, search or edit files: use read_file, grep_text, \
glob_files, list_files, edit_file and write_file instead. They are faster, their output \
is easier to work with, and their edits are checked.

Protocol:
- Each call runs in a fresh non-interactive shell starting at the workspace root, or at \
`cwd` (relative to the workspace). `cd` and exported variables do not persist between \
calls; chain dependent steps with `&&`.
- `timeout_seconds` defaults to {default_timeout} and is capped at {max_timeout}.
- There is no TTY and no stdin. Use non-interactive flags (`-y`, `--yes`, `--no-edit`, \
`CI=1`); pagers are already disabled.
- Long-running servers or watchers: start them in the background with their output \
redirected (`cmd > /tmp/server.log 2>&1 &`), or the call waits until the timeout.
- Output is stdout, then stderr under `[stderr]`, then a final status line such as \
`[exit code: 0]`. Very long output is truncated in the middle, so narrow it with \
`head`, `tail`, `grep` or quieter flags.

Error modes:
- Nonzero exit: returned as an error with the output. Read stderr, fix the cause, then \
retry; don't repeat an identical failing command.
- Timeout: the whole process group is killed and the partial output is returned. Raise \
`timeout_seconds`, make the command faster, or run it in the background.
- `cwd` missing or outside the workspace: rejected. Pass an existing directory inside \
the workspace.
- The permission policy may deny the command or ask the user first. If denied, don't \
retry it verbatim; choose another approach or ask the user.

Side effects: whatever the command does. It can change files, install packages and use \
the network. Files changed through bash must be read again with read_file before \
editing them."
    )
}

/// Used when `tools-fs` is disabled: the shell is the only way to touch files.
pub(crate) fn bash_only(default_timeout: u64, max_timeout: u64) -> String {
    format!(
        "Run a shell command with `/bin/sh -c` in the workspace and return its output.

Working with only a shell: there are no dedicated file tools in this session, so this \
tool is how you read, search, create and edit files, as well as how you build, test \
and run git.

When: every task that touches the workspace: reading files (`cat -n`, `sed -n \
'10,40p'`), searching (`rg`/`grep -rn`, `find`), listing (`ls`, `find -maxdepth`), \
editing, running tests and builds, git, and package managers.

Protocol:
- Each call runs in a fresh non-interactive shell starting at the workspace root, or at \
`cwd` (relative to the workspace). `cd` and exported variables do not persist between \
calls; chain dependent steps with `&&`.
- Read only what you need: line ranges with `sed -n 'A,Bp'` or `head`/`tail`, and \
search with `rg -n` or `grep -rn` before opening large files.
- Prefer small, targeted edits over rewriting whole files: a `python3 - <<'EOF'` script \
that replaces one exact string (and fails if it's missing or ambiguous), or `sed -i` \
for simple one-line substitutions. Create new files with `cat > path <<'EOF'`.
- Nothing validates your edits: no tool checks that a replacement matched or that the \
file still parses. After changing a file, read the changed region back (and run the \
tests or a syntax check) before moving on.
- `timeout_seconds` defaults to {default_timeout} and is capped at {max_timeout}.
- There is no TTY and no stdin. Use non-interactive flags (`-y`, `--yes`, `--no-edit`, \
`CI=1`); pagers are already disabled.
- Long-running servers or watchers: start them in the background with their output \
redirected (`cmd > /tmp/server.log 2>&1 &`), or the call waits until the timeout.
- Output is stdout, then stderr under `[stderr]`, then a final status line such as \
`[exit code: 0]`. Very long output is truncated in the middle, so narrow it with \
`head`, `tail`, `grep` or quieter flags.

Error modes:
- Nonzero exit: returned as an error with the output. Read stderr, fix the cause, then \
retry; don't repeat an identical failing command.
- An edit that silently did nothing (pattern not found) still exits 0 with `sed`; that \
is why you read the file back.
- Timeout: the whole process group is killed and the partial output is returned. Raise \
`timeout_seconds`, make the command faster, or run it in the background.
- `cwd` missing or outside the workspace: rejected. Pass an existing directory inside \
the workspace.
- The permission policy may deny the command or ask the user first. If denied, don't \
retry it verbatim; choose another approach or ask the user.

Side effects: whatever the command does. It can change files, install packages and use \
the network. Keep changes inside the workspace."
    )
}
