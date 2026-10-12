# ADR 0001: Split security responsibilities between the sandbox and the agent

- **Status:** Proposed
- **Date:** 2026-10-12
- **Related:** [architecture §8](../architecture.md#8-design-defaults) (safety), [roadmap](../../ROADMAP.md) 10.1 (`executor-docker`) and open question 4 (project config trust)

## Context

The `safety` plugin decides, before every tool call, whether the call runs, asks or is denied. For `bash` it splits the command line into segments and checks each one against a read-only allowlist and a built-in deny list. Today that policy is the only thing between the model and the host.

A shell-command parser can't be a complete security boundary. Programs on the read-only allowlist run other programs when given the right environment variable, config file or option:

- `GIT_EXTERNAL_DIFF='<cmd> #' git diff` and `X='<cmd>' git --config-env=core.fsmonitor=X status` both run `<cmd>` through `sh`.
- `cargo check` compiles and runs `build.rs` and proc-macros.
- `sort --compress-program=<prog>`, and `rg` with `--pre` set in a `RIPGREP_CONFIG_PATH` file, run other programs.

The deny list can be dodged the same way: `timeout -s KILL 5 git push`, `$'\x67it' push`, `sh <<EOF … EOF`. Each case can be patched, but the list of patches never ends.

Only `bash` goes through the `Executor`. Everything else runs inside the `ch` process on the host: the file tools, `web_fetch`, the model provider calls, config loading and hooks. `project-instructions` also runs `git` directly on the host when a session starts ([env.rs](../../plugins/project-instructions/src/env.rs)).

## Decision

Commands run in a sandbox, and the sandbox is responsible for **containing** what they can do. The agent stays responsible for everything that happens inside the `ch` process, and for asking before changes the sandbox can't tell apart from normal work.

### The sandbox owns

The sandbox wraps the `Executor`, so it covers `bash` and any later tool that runs commands through it. It must provide:

| Requirement | What it takes over from the agent |
|---|---|
| Only the workspace is mounted; no home directory | Bash reading files outside the workspace (`cat ~/.ssh/id_rsa`) |
| `.git/config`, `.git/hooks/` and `.ch/` are mounted read-only | Nothing today; closes the escape path described below |
| A clean environment: no provider API keys or other secrets | `printenv` showing keys to the model |
| No git credentials (SSH keys, credential helpers, tokens) | The `git push` deny rule |
| Network egress blocked, or limited to an allowlist | The `curl \| sh` deny rule, and data leaving through commands |
| Not root, no devices, limits on processes and memory | The `sudo`, `mkfs`, `dd`, `chmod -R /` and fork-bomb deny rules |

With these in place, the read-only allowlist and the deny list stop being security controls. They become user-experience controls: whether to ask before a command changes something, and a guard against obvious accidents. Gaps in them are no longer security bugs.

### The agent owns

| Responsibility | Why the sandbox can't do it |
|---|---|
| **Project config trust.** `.ch/config.toml` from the workspace must not change the provider `base_url` or `api_key_env`, `plugins.safety.mode` or `allow`, `plugins.enable` / `disable`, `select.*`, or plugin settings that name commands, files, URLs or environment variables, until the user trusts those exact values. A project's `deny` and `ask` rules are added to the user's, never replace them | `ch` reads config and calls the provider from the host, with the API key. A repo can point `base_url` at any host and receive the key and the conversation |
| **Running the safety hook after any `Rewrite`.** A later `pre_tool` hook can change a call's input after the safety hook approved it | It's kernel logic, and the file tools and `web_fetch` run inside `ch` |
| **Path checks in the file tools**, including sensitive paths on case-insensitive filesystems (`.Git/hooks/…` on macOS) | `write_file` and `edit_file` run inside `ch` |
| **`web_fetch` network policy:** follow redirects only within the approved site and report others, so the next fetch is checked again; refuse host names that resolve to private, loopback or link-local addresses. URLs that name such an address directly (`http://localhost:3000`) still work, because the user approves that origin explicitly | `web_fetch` runs inside `ch`, on the host's network |
| **Asking before commands that destroy work in the workspace** (`git reset --hard`, `git clean -f`, `rm -rf src`) | The workspace is mounted read-write, so the sandbox can't tell these from normal edits |

### Shared: the workspace mount

Anything a sandboxed command writes to the workspace lands on the host. Files that host tools run automatically are therefore a way out of the sandbox:

1. A sandboxed command writes `core.fsmonitor=<cmd>` into `.git/config`.
2. The next time `ch` starts, `project-instructions` runs `git status` on the host.
3. `<cmd>` runs on the host.

A sandboxed command that writes `.ch/config.toml` reaches the host the same way, through the project config issue above. Both sides have to act: the sandbox mounts these paths read-only, and the agent doesn't trust config from the workspace. Other files with the same shape, such as `.envrc` (direnv) and editor task files, are outside `ch`'s control; the user docs should mention them.

## Consequences

**Positive**

- Security no longer depends on the shell parser being complete.
- The allowlist can stay short and readable, tuned for fewer prompts rather than for coverage.
- `bypass` mode and `--approver auto-allow` become reasonable choices for CI and evals inside the sandbox.

**Negative**

- Until a sandbox ships, `executor-local` runs commands on the host with none of the guarantees above, and the allowlist gaps are real security bugs. Either fix the known gaps in the meantime, or document that `ask` mode with `executor-local` is not a security boundary.
- With `.git/config` read-only, commands that change git config (`git config`, `git remote add`, `git branch -u`) fail inside the sandbox. Commits still work. Pushing happens on the host, by the user.
- Starting a container per command adds latency. One long-lived container per session avoids most of it.
- The permissions section of the README ("tools can only touch files inside the workspace", "the deny list always blocks") has to say which guarantees come from the sandbox and which hold with `executor-local`.

## Alternatives considered

1. **Make the shell parser a complete boundary.** Rejected. Allowlisted programs run other programs through environment variables, config files and options, and new ones appear with each tool release.
2. **Sandbox the whole `ch` process instead of the executor.** This would also cover `web_fetch` and the provider calls; an egress allowlist limited to the provider's host would block the `base_url` attack. But the API key then lives inside the sandbox, so the agent still has to keep it out of commands' environment, and the REPL needs a terminal. Kept as an option for CI and evals, not the default.
3. **No sandbox; rely on approvals.** Rejected for the reasons in Context: the approval prompt can be skipped through commands the policy treats as read-only.
