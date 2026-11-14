# Evals

`ch-eval` runs a suite of tasks with `ch -p` across a matrix of configs and reports how
the configs compare. Each run happens in a fresh Docker container that holds only a copy
of the task's repo; the diff and the task's hidden check then run in further containers
with no network. The design is in [ADR 0005](../docs/adr/0005-eval-runner.md).

```
eval/
  docker/Dockerfile           # ch-eval-base: the default task image; also runs the diff and the proxy
  docker/builder.Dockerfile   # ch-eval-builder, which builds static ch and ch-eval
  tasks/<id>/                 # the starter suite
```

> **Every run calls the model and costs money.** Check a matrix's size with `--dry-run`
> and cap it with `--max-cost`. The model's commands run unattended in the container.
> The container's only way out is a credential proxy that forwards model calls and adds
> the API key, so the key never enters it; still, use a dedicated key with a spending
> limit.

## Running a matrix

You need Docker and git. From the repository root:

```sh
cargo run -p ch-eval -- build      # once: the images and static ch and ch-eval (a few minutes)
```

Write a base config with the model, provider and pricing. It becomes each run's
`config.toml`; don't put secrets in it, since it is copied into every run directory.

```toml
# eval.toml
[model]
provider = "anthropic"
id = "..."          # required: the proxy forwards only the models the config names

# USD per million tokens, from your provider's price list. --max-cost needs a price for
# every model the cells use, and without one its calls cost 0. The numbers here are
# placeholders.
[model.pricing]
input = 3.0
output = 15.0
cache_read = 0.3
cache_write = 3.75

# Prices for other models: a summarizer's, or a model a cell switches to with
# --vary model.id=... (otherwise [model.pricing] follows the configured model, as in ch).
# [eval.pricing."other-model"]
# input = 1.0
# output = 5.0
```

Then:

```sh
export ANTHROPIC_API_KEY=...       # the dedicated key; only the proxy's container gets it
cargo run -p ch-eval -- run --config eval.toml --out eval-out/planning \
  --vary planning=on,off --repeat 3 --max-cost 20 --dry-run
# the same without --dry-run to run it
```

- `--vary KEY=V1,V2` adds a dimension. Each value is a TOML literal passed to `ch` as
  `--set KEY=V`: `--vary plugins.context.strategy='"none"','"staged"'`,
  `--vary plugins.planning.refresh_after_steps=0,10`. Keys start with `model`, `agent`,
  `plugins` or `select`. A key without a dot is a plugin, switched `on` or `off`:
  `--vary planning=on,off`, `--vary tools-fs=on,off`. The cells are every combination;
  with no `--vary` there is one cell, `base`.
- `--repeat N` runs each task in each cell N times, since model runs vary. `-j N` runs N
  at once (default 2).
- `--max-cost USD` caps what the proxy records for this `--out`: past it, model calls
  are refused and no new run starts. Runs it cut off are made again on resume.
- `--max-attempts N` (default 3): a run that ends because the model API failed (rate
  limits, overloads, server errors) isn't recorded; it is tried again later, with a
  wait that doubles with each try, and the matrix stops when one run has failed this
  way N times.
- `--task ID` and `--tag TAG` pick tasks; `--suite DIR` uses another suite;
  `--profile NAME` applies a profile from the base config to every run.
- `--ch PATH` uses a given static Linux `ch` instead of the one built from this tree.
  Otherwise `run` rebuilds the binaries when the source changed.

The key comes from the variable the provider's `api_key_env` names (by default
`ANTHROPIC_API_KEY`, or `OPENAI_API_KEY` for the OpenAI provider); if it isn't set,
`ch-eval run` says so before anything runs. Only the proxy gets it, on its stdin. The
proxy forwards to the provider's `base_url` from the base config, so a gateway or a
local OpenAI-compatible server works as the upstream.

Running again with the same `--out` resumes: runs with a `run.json` are kept, as long as
the `ch` binary, image, base config, cell settings and task are the same as when they
were made; otherwise `ch-eval` refuses and names what changed. After an interruption,
`ch-eval report OUT` writes the report again from what was recorded. Ctrl-C removes the
containers and the network.

Each run has a network of its own whose only other member is the proxy: `web_fetch`,
package installs and other downloads fail inside evals, and runs can't reach each
other. A run that says nothing about the agent (its container didn't start, the API
refused the key, none of its model calls succeeded) stops the matrix with an error, which
quotes the API's last answer, instead of being recorded as a failure.

A cell that varies `model.id` should have the new model's prices under
`[eval.pricing."<model>"]`; otherwise `ch-eval run` warns that the base config's
`[model.pricing]` prices it.

Docker Desktop can't cap what the agent writes into a run's `work/` and `ch-home/`,
which are mounted from the host; a run can fill the disk. The logs `ch-eval` keeps are
capped.

To measure a change compiled into `ch`, such as a prompt, run the suite once per build
into two output directories (`--ch` takes another build) and compare them:

```sh
cargo run -p ch-eval -- compare eval-out/before eval-out/after --to eval-out/compared
```

`compare` refuses directories whose tasks differ, and, unless you pass
`--allow-different-config`, whose base configs or cells differ, naming what differs.
The two directories' runs were made at different times, so a change in the model API
between them looks like a change in `ch`; the report says so.

## What you get

```
OUT/report.md                  # per cell, per task, and paired comparisons
OUT/results.json               # the same numbers for tools
OUT/proxy/                     # the proxy's ledger of every model call, and its log
OUT/runs/<task>/<cell>/<repeat>/
  run.json                     # the record: pass, how it ended, tokens and cost, signals
  events.jsonl                 # every event of the run (ch -p --output-format stream-json)
  stderr.log  diff.patch  verify.log
  work/                        # the workspace after the run, with the check's files
  baseline.git/                # the starting commit (every .git under work/ and ch-home/ is removed)
  ch-home/                     # the run's config, session and logs
```

Per cell, the report gives the pass rate with a 95% Wilson interval over runs; mean and
median cost, model calls and tokens (cache included) as the proxy counted them, steps as
`ch` reported them, and wall time; how runs and checks ended (`done`, `step_limit`,
`timeout`, ...; checks that failed, timed out or couldn't run); and per run the tool
calls by tool, tool errors by failure reason, denied calls, compactions, plan refreshes,
hook denials and warnings. Each pair of cells is compared per task over the repeats both
have, with an exact sign test over the tasks where they differ: tasks are the unit,
since repeats of one task aren't independent evidence. With fewer than six differing
tasks the sign test can't reach p < 0.05.

The event stream is `ch`'s own account, and the agent can write to it, so steps,
reasons and signals are as `ch` reported them. Cost and tokens come from the proxy.

## Writing a task

A task is a directory in the suite:

```
tasks/<id>/
  task.toml
  repo/        # the starting workspace; it gets `git init` and a baseline commit
  verify/      # hidden check files, copied over the workspace after the run
```

```toml
instruction = "The total in report.py is off by one for empty input. Fix it."
verify = "python3 -I test_hidden.py"   # a shell command; exit 0 is a pass
image = "ch-eval-base"      # optional; must exist locally (ch-eval never pulls)
timeout_secs = 900          # optional; the agent is stopped after this
verify_timeout_secs = 600   # optional
max_steps = 80              # optional; sets agent.max_steps
tags = ["bugfix", "python"] # optional
```

- **Make it unambiguous.** The instruction should say what done looks like, so a correct
  solution passes and nothing else does.
- **Keep the check hidden.** Put it in `verify/`, not `repo/`. Its files replace whatever
  the agent left at those paths, so editing or deleting visible tests can't make it pass.
  Don't make the check depend on files in `repo/` that the agent may reasonably change.
- **Keep the workspace out of the check's own tools.** The check runs in the agent's
  workspace, next to code the agent wrote: a `unittest/` directory there shadows
  Python's, for example. The starter tasks run `python3 -I` (and add the workspace at the
  end of the path, only to import the code under test), use a `.cjs` file and `node:`
  modules, and put a recording `bash` stub first on `PATH`. A check that imports the
  agent's code can still be tampered with by that code; no check is tamper-proof, and
  the report measures what the checks measure.
- **Check it both ways.** The check must fail on the starting repo and pass on a correct
  fix. `ch-eval run` tries it on the starting repo first and stops if it passes there or
  can't run (exit 124 to 127). Try the fix yourself in the task's image, then throw it
  away: it must not be in the repo.
- **The check is any shell command,** and `ch-eval` knows nothing about languages. The
  image must have the tools it uses; `ch-eval-base` has git, Python 3 and Node.js. It
  runs with no network, and so does the agent.
- **Add a `.gitignore`** for build output (such as `__pycache__/`) so it stays out of
  `diff.patch`.

A misspelled key inside a plugin's table, such as
`--vary plugins.context.stratgy='"none"'`, isn't caught yet: most plugins accept unknown
settings, so those cells are the base config. Check the spelling against the plugin's
docs; once plugins reject unknown settings (PR #48), `ch` refuses such a config, so
the cell's first run stops the matrix.

The starter tasks: `py-duration-hours` (a Python bug fix), `node-csv-quotes` (a small
Node.js feature) and `sh-logsum-posix` (porting a bash script to POSIX sh).
