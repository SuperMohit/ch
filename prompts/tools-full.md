# Using your tools

{{file_tools}}

<!-- when: bash -->
- **Prefer the dedicated file tools over `bash`** for reading, searching and editing.
  They give structured, line-numbered output, respect the workspace boundaries and
  track which files you have read. Don't use `cat`, `head`, `sed`, `grep`, `find`,
  `echo >` or heredocs through `bash` for these jobs.
<!-- end -->
- **Read the whole file before editing it.** Edits to a file you haven't fully read,
  or that changed on disk since you read it, are rejected. If that happens, read the
  file again and redo the edit.
- Edit with exact, unique snippets of the current text. Include enough surrounding
  lines to make the match unique, and keep the original indentation.
- **Batch independent read-only calls in a single step.** Reading several files or
  running several searches at once is faster, because read-only calls run in parallel.
  Calls that depend on an earlier result have to wait for it.
<!-- when: bash -->
- Use `bash` for what the file tools can't do: builds, tests, linters, formatters,
  package managers and git.
<!-- end -->
