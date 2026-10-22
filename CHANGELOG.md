# Changelog

User-visible changes to `ch`, newest first. Each pull request that changes behaviour adds a line under `## Unreleased`, in the section that fits. Empty sections are dropped when a release is cut with `just release <version>`.

## Unreleased

### Breaking

### New

- **Interactive REPL.** `ch` streams the model's text as it arrives, shows one line per tool call and coloured diffs for edits, and takes multi-line input. Esc or Ctrl-C interrupts a turn and keeps the history.
- **Headless mode.** `ch -p "task"` runs one task without interaction, reading the prompt from stdin with `-p -`. `--output-format` is `text`, `json` or `stream-json`, and the exit code says how the run ended (done, error, step limit, context overflow, stopped by a hook, prompt blocked, cancelled).
- **Model providers.** The Anthropic Messages API, with streaming, retries, prompt caching and extended thinking, and OpenAI-compatible chat completions (OpenAI, OpenRouter, vLLM, SGLang).
- **Tools.** `read_file`, `edit_file`, `write_file`, `list_files`, `glob_files`, `grep_text`, `bash` and `web_fetch`. With the file tools turned off, `bash` switches to a bash-only description.
- **Permissions.** Three modes (`ask`, `accept-edits`, `bypass`) and `allow`, `ask` and `deny` rules of the form `tool` or `tool:glob`. Each part of a compound shell command is checked separately, read-only commands run without asking, and a built-in deny list blocks commands such as `sudo` and `git push`. Answering "always" can save the rule to `.ch/config.toml`.
- **File safety.** The file tools only touch files inside the workspace, including through `..` and symlinks. A file must be read in full before it is edited, and an edit is rejected if the file changed on disk since that read.
- **`web_fetch` safety.** Redirects are followed only within the approved site, and host names that resolve to private or local addresses are refused.
- **Sessions.** Saved after every turn. `ch -c` continues the last session in the project, and `ch -r` or `/resume` picks an earlier one.
- **Configuration.** Layered TOML (`~/.ch/config.toml`, `.ch/config.toml`, profiles, `--set key=value`). Project settings that could loosen permissions, run commands or send your API key elsewhere apply only after you trust them, and `--trust-project` trusts them for one headless run.
- **Slash commands.** `/help`, `/clear`, `/compact`, `/model`, `/cost`, `/tools`, `/plugins`, `/config`, `/resume` and `/exit`. Markdown files in `.ch/commands/` or `~/.ch/commands/` become custom commands, with `$ARGUMENTS` replaced by what you type after them.
- **Project instructions.** `CH.md` and `AGENTS.md` from the git root down to the workspace, plus `~/.ch/CH.md`, are added to the system prompt with the working directory, OS, date and git status.
- **Plugins.** Providers, tools, safety, prompts, commands and frontends are all plugins and can be turned off with `plugins.disable`. `ch --list-plugins` shows what is loaded and why, and `ch --config-path` shows where config, sessions and logs live.
- **Cost.** `/cost` shows token usage and, with `[model.pricing]` set, the cost of the session.

### Improved

### Fixed

### Security

- A path-scoped allow rule such as `edit_file:src/**` no longer covers paths that leave its directory through `..`. Before, `src/../.git/hooks/pre-commit` was written without asking, even though it is a sensitive path. Allow rules now match only the resolved path, and for `web_fetch` the normalized URL.
- Environment variables in front of a read-only command now make it ask, except locale and display settings (`LANG`, `LC_*`, `TZ`, `NO_COLOR`, `TERM`, `COLUMNS`, `LINES`). Before, `GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.fsmonitor GIT_CONFIG_VALUE_0=<cmd> git status` ran `<cmd>` without asking. Assignments after `env` (`env LD_PRELOAD=x cat`) are now checked too.
- `env` and `printenv` ask before running, because they print the whole environment, provider API keys included.
- Commands that nest substitutions, `sh -c` or `eval` more than four levels deep are denied, because the deny list can't check the innermost parts.
