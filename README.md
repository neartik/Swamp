# Swamp

Swamp runs coding agents in parallel over the CLI subscriptions you already pay for. It drives
the vendor CLIs (`claude`, `codex`) as child processes, gives every worker its own git worktree
and branch, records everything it does in an append-only journal, and rotates between
subscriptions when one hits its rate limit.

No API keys. No network client of its own: every provider byte comes from a CLI's stdout.

- One process per worker, launched detached, with stdout and stderr redirected to files. A dead
  supervisor does not kill the work, and `swamp resume` picks it back up where the parser stopped.
- One git worktree per attempt, outside the repo. Your checkout is never written to until you
  run `swamp adopt`.
- One journal per run. Every view (`trace`, `watch`, `replay`, the brain's status tool) is a fold
  over the same lines.

## Install

```sh
cargo install --path . --bin swamp
swamp --version
```

Requires git and at least one vendor CLI on PATH.

## Accounts: one wrapper executable per subscription

Swamp never sees a credential. A Swamp account is a name plus an executable on PATH, and the
executable is a wrapper script that points the vendor CLI at one subscription's config directory.
Two accounts that exec the same binary with the same config dir are one subscription: dispatch
would double-spend a single quota and failover between them would be a silent no-op. `swamp
doctor` refuses that setup by name.

`~/bin/claude-main`:

```sh
#!/bin/sh
export CLAUDE_CONFIG_DIR="$HOME/.claude-main"
exec claude "$@"
```

`~/bin/claude-alt`:

```sh
#!/bin/sh
export CLAUDE_CONFIG_DIR="$HOME/.claude-alt"
exec claude "$@"
```

`~/bin/codex-main`:

```sh
#!/bin/sh
export CODEX_HOME="$HOME/.codex-main"
exec codex "$@"
```

```sh
chmod +x ~/bin/claude-main ~/bin/claude-alt ~/bin/codex-main
CLAUDE_CONFIG_DIR=~/.claude-main claude          # log in once per subscription
CLAUDE_CONFIG_DIR=~/.claude-alt  claude
CODEX_HOME=~/.codex-main         codex
```

The env map in `[[accounts]]` sets the same variables at launch, so the wrapper and the config
agree. Keep both: the wrapper is what you use by hand, the env map is what Swamp guarantees.

## Configure

`~/.config/swamp/config.toml` (or `<repo>/.swamp/config.toml`, which wins; `swamp config init`
writes a starter). Model ids live only here.

```toml
version = 1

[limits]
max_parallel = 4
worker_timeout = "25m"

[dispatch]
default_provider = "anthropic"
default_tier = "mid"

[providers.anthropic]
models = { high = "<model>", mid = "<model>", low = "<model>" }

  [providers.anthropic.worker]
  permission_mode = "auto"

[[accounts]]
id = "main"
provider = "anthropic"
exec = "claude-main"
max_concurrency = 2
env = { CLAUDE_CONFIG_DIR = "~/.claude-main" }

[[accounts]]
id = "alt"
provider = "anthropic"
exec = "claude-alt"
max_concurrency = 2
env = { CLAUDE_CONFIG_DIR = "~/.claude-alt" }
```

Workers and the brain are launched with `--permission-prompts none`, because nobody is at the
terminal to answer a prompt. Under it, `permission_mode = "acceptEdits"` (and `plan`, `manual`,
`dontAsk`) auto-denies every Bash call, so a worker cannot run the tests or the build it was sent
to run and comes back with a confident summary of work it never did; `permission_mode = "auto"`
is therefore the recommendation for anthropic workers and for `[brain]`. The trade-off is real:
`auto` lets a worker run arbitrary commands in its worktree without asking, which is the same
trust you extend to a CLI agent in your own shell, and a worktree is a directory, not a sandbox.
The brain keeps `deny_tools = ["Edit", "Write", "MultiEdit", "NotebookEdit"]` either way, so it
still cannot edit files, and `swamp doctor` warns when a worker is configured with a mode that
denies Bash.

Layers, lowest priority first: built-in defaults, `~/.config/swamp/config.toml`,
`<repo>/.swamp/config.toml`, `SWAMP_*` environment, `--config <file>`, command-line flags.
`swamp config show --effective` prints the merged result and where each layer came from.
`swamp.example.toml` documents every key.

## First run

```sh
swamp doctor                                       # PATH, git, tiers, account collisions
swamp run --no-brain --tier mid "fix the flaky test in tests/api.rs"   # commit first: a dirty tree is refused
swamp trace last                                   # the run tree, with the diff summary
swamp diff last --stat                             # `last` is the run; <node> works from any run
swamp adopt last                                   # applies the patch to your checkout
```

`swamp` with no arguments, or `swamp chat`, starts an interactive session with a brain: a CLI
agent that plans, reads the repo, and dispatches workers through Swamp's own MCP tools. It never
edits files itself.

## Commands

| Command | What it does |
|---|---|
| `swamp run <TASK>` | One-shot. `--no-brain` sends the task straight to one worker. `--tier`, `--provider`, `--account`, `--workers`, `--budget`, `--detach`. |
| `swamp trace [RUN\|last\|-2]` | Render a run tree: nodes, attempts, accounts, failures, cost. `--events`, `--raw`, `--follow`, `--failed`, `--json`. |
| `swamp watch [RUN\|last]` | Live read-only TUI. Attach from a second terminal while a run is going. |
| `swamp doctor` | Health checks. `--probe` calls each account's CLI, `--schema` reports adapter drift, `--reap` removes stale worktrees and sockets, `--fix` creates the directories and the git exclude. Exit 1 on any error, so CI can gate on it. |
| `swamp chat` | Interactive brain session. |
| `swamp runs`, `swamp resume`, `swamp cancel` | List runs, recover an interrupted one (`--plan` first, it spends nothing), stop one. |
| `swamp accounts` | Health, in-flight count, quota windows, cooldowns, lifetime spend. Also `cooldown`, `clear`, `enable`, `disable`, `reset`. |
| `swamp diff`, `swamp adopt`, `swamp worktrees` | Inspect a worker's patch, land it, manage the worktrees. A node is named by its full id, either short id (the attempt's, printed by `swamp trace`, or the logical one in the branch name) or a prefix, searched across every run; `last` and `-2` name a run and resolve to its node. |
| `swamp gc`, `swamp replay`, `swamp config`, `swamp completions` | Housekeeping, re-render or re-derive a recorded run, inspect config, shell completions. |

Exit codes: `0` ok, `1` generic, `2` config invalid, `3` no capacity (all accounts cooling),
`4` node failed, `5` conflict, `6` cancelled, `7` budget exceeded.

## Layout

Swamp adds `/.swamp/` to `.git/info/exclude`, never to a tracked `.gitignore`.

```
<repo>/.swamp/
  config.toml                  optional project overrides
  swamp.log                    tracing output, never the journal
  runs/<run_id>/
    run.json                   header: cwd, git HEAD, config hash, version, argv
    journal.jsonl              the run tree, append-only, one JSON object per line
    ctl.sock                   MCP control socket, 0600 in a 0700 directory
    tools/                     the arguments of every brain tool call
    nodes/<node_short>/
      prompt.md                the exact bytes fed to the worker's stdin
      stream.jsonl             raw provider stdout, verbatim, never rewritten
      stderr.log
      noise.log                non-JSON lines (banners, npm warnings)
      last-message.txt         codex -o
      result.json              the node's result
      patch.diff
      pid
  last -> runs/<run_id>

~/.swamp/
  accounts.json                cross-run, cross-repo quota and cooldown state (file-locked)
  worktrees/<repo>-<hash8>/<run_short>/<node_short>-<attempt>/
```

Worker branches are `swamp/<run_short>/<node_short>-<attempt>`, where `<node_short>` is the
LOGICAL node id: it is stable across retries, so every attempt of one task lands on a branch of
the same family. The node directory under `runs/<run_id>/nodes/` is named after the ATTEMPT id
instead, which is what `swamp trace` prints in its leading column. `swamp diff` and `swamp adopt`
accept both: a logical short id resolves to that node's last finished attempt.

## A worktree is a directory, not a sandbox

Worker isolation is a git worktree: a separate directory on the same filesystem, with the same
user, the same network and the same credentials as your shell. A worker can read your whole
machine and write outside its worktree. Swamp bounds concurrency, quota, budget and depth; it
does not contain a process. Run workers on code you would run yourself, and use
`--isolation readonly` or a container if you need more than that.

A `--dangerously-*` flag in `providers.*.worker.args` makes the whole configuration invalid
unless `limits.unsafe_ack = true` is set: every command, `swamp doctor` included, exits 2 until
one or the other changes.

## Development

```sh
cargo build
cargo clippy --all-targets -- -D warnings
cargo test
```

The end-to-end suite (`tests/e2e_*.rs`) runs the real binary against the fake CLIs in
`tests/support/`, which are extra `[[bin]]` targets driven by scenario files and replaying the
recorded streams in `docs/ref/`. They are test fixtures, which is why the install line above
names `--bin swamp`.
There is no network access anywhere in the test suite, and `tests/e2e_smoke.rs` asserts that the
dependency tree contains no HTTP client.
