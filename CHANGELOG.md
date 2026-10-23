# Changelog

User-visible changes to `ch`, newest first. Each pull request that changes behaviour adds a line under `## Unreleased`, in the section that fits. Empty sections are dropped when a release is cut with `just release <version>`.

## Unreleased

### Breaking

- An unknown key under `[model]`, `[agent]` or `[select]` (including `[model.pricing]` and `[model.summarizer]`) stops `ch` with an error that lists the valid keys. Before, a misspelled setting such as `agent.max_step` was silently ignored. Top-level keys and `[plugins.*]` tables are unchanged.

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

- The "several context strategies are registered" error names the setting `select.context`. Before, it suggested `select.context_strategy`, which did nothing.
- In the REPL, Esc or Ctrl-C cancels a running slash command such as `/compact`. Before, Ctrl-C during a command killed `ch` without saving the session.
- A tool call whose approval prompt was interrupted is answered "Not run: the user cancelled the turn." Before, the model was told the user had denied it and not to retry it.
- The REPL exits with code 1 when its line editor fails, instead of 0.

- `ch -c --model <id>` uses the given model. Before, a resumed session kept its saved model. A session saved with a different provider from the configured one now resumes on the configured model, instead of sending the old provider's model id.
- `[model] context_window` and `pricing` apply only to `model.id`, not to a model picked with `/model` or kept from a resumed session.
- Hooks at the end of a turn run exactly once. Before, they ran twice when one of them stopped the turn, and not at all after a refusal or a blocked prompt.
- When a hook stops the turn or the user cancels it, the remaining read-only calls from the same response are no longer run.
- A note a hook adds after the model's final answer is no longer sent with the next prompt.
- Session files keep their event ids and turn numbers when they are resumed. Saving writes to a temp file of its own and flushes it to disk before replacing the session file.
- A session can only be open in one `ch` at a time. A second `ch -c` (or `/resume`) for a session another process has open now stops with a message, instead of both processes saving over each other's turns. The lock is released when `ch` exits, even after a crash.
- `/clear` starts a new session instead of emptying the current one. Before, the next turn saved over the cleared conversation, so it was lost. Now it stays saved and `/resume` brings it back.
- `edit_file` and `write_file` change the exact content the read-before-write check verified. Before, they read the file a second time after the check, so a change made between the two reads was edited over without the "changed on disk" error.

### Security

- When several hooks ask about the same tool call, "always allow" covers only the hook it was given for. Before, only the first hook's question was kept, so an "always allow" for the permission check also approved a later hook's question without asking. Several open questions are shown in one prompt that can only be allowed once.
- Commands run by `bash` no longer see the provider API keys: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, and any variable named by an `api_key_env` setting are removed from their environment. Before, `echo $ANTHROPIC_API_KEY` showed the key to the model without asking.
- Hooks get the event read-only, so a `Rewrite` decision is the only way to change a tool call. Before, a hook could edit the input in place after the permission check had approved it.
- A `bash` call with no command in it (empty, or only `;` or `&&`) and a `web_fetch` URL that can't be parsed or has no host now ask before running in `ask` and `accept-edits` modes. Before, the permission check allowed them and relied on the tool to reject them.

- A path-scoped allow rule such as `edit_file:src/**` no longer covers paths that leave its directory through `..`. Before, `src/../.git/hooks/pre-commit` was written without asking, even though it is a sensitive path. Allow rules now match only the resolved path, and for `web_fetch` the normalized URL.
- Environment variables in front of a read-only command now make it ask, except locale and display settings (`LANG`, `LC_*`, `TZ`, `NO_COLOR`, `TERM`, `COLUMNS`, `LINES`). Before, `GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.fsmonitor GIT_CONFIG_VALUE_0=<cmd> git status` ran `<cmd>` without asking. Assignments after `env` (`env LD_PRELOAD=x cat`) are now checked too.
- `env` and `printenv` ask before running, because they print the whole environment, provider API keys included.
- Commands that nest substitutions, `sh -c` or `eval` more than four levels deep are denied, because the deny list can't check the innermost parts.
