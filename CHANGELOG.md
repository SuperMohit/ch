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

- `ch -p "task"` no longer hangs when stdin is an open pipe that nothing is written to, as under `ssh` or a parent process. Input piped alongside a prompt is waited for at most 3 seconds; after that the task runs without it, with a note on stderr.
- `ch -p fix tests` is an error that shows the quoted form, `ch -p "fix tests"`. Before, the second word was silently dropped.
- After you save a second "always allow" rule in the same REPL session, the project stays trusted. Before, only the first saved rule kept trust, so the next run asked again and headless runs dropped the project's settings.
- `CH_HOME` also moves the user-level `CH.md` and `commands/` directory. Before, those were always read from `~/.ch`.
- Recording trust in `trusted-projects.toml` replaces the file atomically, so a crash can't leave a damaged file that blocks trusting any project.
- `ch` starts without the Anthropic provider. With `model.provider` unset, it uses the first enabled provider (Anthropic when it is there), and with `model.id` unset, that provider's default model. Before, the kernel's built-in `anthropic` / `claude-opus-5-5` defaults made a build with only OpenAI, or a config with `provider-anthropic` disabled, fail with "no model provider `anthropic`". OpenAI has no default model, so it asks for `model.id`.
- The "several context strategies are registered" error names the setting `select.context`. Before, it suggested `select.context_strategy`, which did nothing.
- In the REPL, Esc or Ctrl-C cancels a running slash command such as `/compact`. Before, Ctrl-C during a command killed `ch` without saving the session.
- A tool call whose approval prompt was interrupted is answered "Not run: the user cancelled the turn." Before, the model was told the user had denied it and not to retry it.
- The REPL exits with code 1 when its line editor fails, instead of 0.
- Text typed in the REPL while a turn or a slash command runs is kept and fills the next prompt, unsent, so you can edit it or press Enter. Before, it was thrown away. Keys typed before an approval question appears no longer answer it, and an Esc pressed just before the question still interrupts the turn.
- The REPL shows warnings and other events emitted outside a turn, such as by a slash command like `/compact` or by a plugin while `ch` waits for input. Before, it only listened during a turn, so those were never shown.

- `ch -c --model <id>` uses the given model. Before, a resumed session kept its saved model. A session saved with a different provider from the configured one now resumes on the configured model, instead of sending the old provider's model id.
- `[model] context_window` and `pricing` apply only to `model.id`, not to a model picked with `/model` or kept from a resumed session.
- A turn stops with an error after the model's tool calls are cut off by the output limit three responses in a row. Before, each cut-off call was retried until `agent.max_steps` (300 by default) was used up.
- A plugin that registers a tool model APIs would reject (a name that isn't 1 to 64 letters, digits, `_` or `-`, or an input schema that isn't a `"type": "object"` schema) stops `ch` at startup with an error naming the plugin. Before, the bad tool was sent with every request, and every model call failed.
- Hooks at the end of a turn run exactly once. Before, they ran twice when one of them stopped the turn, and not at all after a refusal or a blocked prompt.
- When a hook stops the turn or the user cancels it, the remaining read-only calls from the same response are no longer run.
- A note a hook adds after the model's final answer is no longer sent with the next prompt.
- Session files keep their event ids and turn numbers when they are resumed. Saving writes to a temp file of its own and flushes it to disk before replacing the session file.
- A session can only be open in one `ch` at a time. A second `ch -c` (or `/resume`) for a session another process has open now stops with a message, instead of both processes saving over each other's turns. The lock is released when `ch` exits, even after a crash.
- `/clear` starts a new session instead of emptying the current one. Before, the next turn saved over the cleared conversation, so it was lost. Now it stays saved and `/resume` brings it back.
- `edit_file` and `write_file` change the exact content the read-before-write check verified. Before, they read the file a second time after the check, so a change made between the two reads was edited over without the "changed on disk" error.

### Security

- The `bash` permission check looks inside heredocs, whose `$(...)` and backticks the shell runs when the delimiter is unquoted (`<<EOF`, not `<<'EOF'`). Before, `cat <<EOF` with `$(rm -rf ~)` in the body ran without asking, as a read-only `cat`, and the deny list never saw the `rm`.
- The trust prompt shows a project's setting names and values with control and bidi characters escaped. Before, a quoted key holding escape codes could move the cursor and redraw the prompt over the settings you were being asked to trust.
- `ch` refuses a `.ch/config.toml` that is a symlink to a file outside the project, and a parse error in the project config gives the line number without quoting the line. Before, a project could make `ch` print a line of any readable file, such as a credentials file, to the terminal and the logs.
- A project's `.ch/config.toml` can no longer get around the trust check. Settings now need trust unless they are in `[model]` or `[agent]`, are `plugins.safety.deny` or `ask`, or are declared safe by their plugin. Before, the check went by key name and looked only inside tables, so a project could write `[plugins] safety = ["bypass", ["bash"]]` to turn on bypass mode and erase your deny rules, disable the `safety` plugin, re-enable plugins you turned off, or send your API key to another server, all without being asked.
- `edit_file` and `write_file` refuse to change a file that has more than one hard link (on Unix). A hard link is the same file under another name, possibly outside the workspace, such as a dotfile or a package manager's shared store, so editing it changed that file too. Reading a hard-linked file still works.
- When several hooks ask about the same tool call, "always allow" covers only the hook it was given for. Before, only the first hook's question was kept, so an "always allow" for the permission check also approved a later hook's question without asking. Several open questions are shown in one prompt that can only be allowed once.
- Commands run by `bash` no longer see the provider API keys: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, and any variable named by an `api_key_env` setting are removed from their environment. Before, `echo $ANTHROPIC_API_KEY` showed the key to the model without asking.
- Hooks get the event read-only, so a `Rewrite` decision is the only way to change a tool call. Before, a hook could edit the input in place after the permission check had approved it.
- A `bash` call with no command in it (empty, or only `;` or `&&`) and a `web_fetch` URL that can't be parsed or has no host now ask before running in `ask` and `accept-edits` modes. Before, the permission check allowed them and relied on the tool to reject them.

- A path-scoped allow rule such as `edit_file:src/**` no longer covers paths that leave its directory through `..`. Before, `src/../.git/hooks/pre-commit` was written without asking, even though it is a sensitive path. Allow rules now match only the resolved path, and for `web_fetch` the normalized URL.
- Environment variables in front of a read-only command now make it ask, except locale and display settings (`LANG`, `LC_*`, `TZ`, `NO_COLOR`, `TERM`, `COLUMNS`, `LINES`). Before, `GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.fsmonitor GIT_CONFIG_VALUE_0=<cmd> git status` ran `<cmd>` without asking. Assignments after `env` (`env LD_PRELOAD=x cat`) are now checked too.
- `env` and `printenv` ask before running, because they print the whole environment, provider API keys included.
- Commands that nest substitutions, `sh -c` or `eval` more than four levels deep are denied, because the deny list can't check the innermost parts.
