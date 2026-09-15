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
  permission_mode = "acceptEdits"
  allow_tools = ["Bash"]
  deny_tools = ["Task", "Agent", "Workflow", "Team"]

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
terminal to answer a prompt. Under it the two halves of the job are gated separately, and
measuring the real CLI is the only way to see it: `permission_mode = "auto"` denies file writes
("the session currently doesn't have approval enabled for file writes"), so the worker produces
no diff at all, while `acceptEdits` (like `plan`, `manual` and `dontAsk`) writes files but denies
every Bash call, so the worker cannot run the tests it was sent to run and comes back with a
confident summary of work it never verified. The pair that works is `permission_mode =
"acceptEdits"` plus `Bash` in `allow_tools`, for anthropic workers and for `[brain]`. The
trade-off is real: an allowed Bash runs commands in the worktree without asking, which is the
same trust you extend to a CLI agent in your own shell, and a worktree is a directory, not a
sandbox. The brain keeps `deny_tools = ["Edit", "Write", "MultiEdit", "NotebookEdit"]`, so it
still cannot edit files, and `swamp doctor` warns when a worker or the brain runs in a
Bash-denying mode without Bash allowed.

`allow_tools` and `deny_tools` are tool names, not flags: Swamp merges each list into the single
`--allowed-tools` / `--disallowed-tools` flag the CLI accepts, so a raw flag in `worker.args`
would silently overwrite the other half. The suggested `deny_tools` above is about behaviour
rather than safety: a worker that can still reach the delegation tools will, given a global
CLAUDE.md that tells it to orchestrate, spawn a team of subagents and report on their work
instead of doing it. Swamp also appends a built-in worker role prompt to every worker
(`--append-system-prompt`, or the head of the prompt for codex, which has no such flag): you are
one worker, in this worktree, unattended; do the task yourself; do not spawn subagents; never
ask a question; finish with what you changed and how you verified it. Append your own text with
`providers.<p>.worker.system_prompt_file`.

Layers, lowest priority first: built-in defaults, `~/.config/swamp/config.toml`,
`<repo>/.swamp/config.toml`, `SWAMP_*` environment, `--config <file>`, command-line flags.
`swamp config show --effective` prints the merged result and where each layer came from.
`swamp.example.toml` documents every key.

## First run

```sh
swamp doctor                                       # PATH, git, tiers, account collisions
swamp run --no-brain --tier mid "fix the flaky test in tests/api.rs"   # commit first: a dirty tree is refused before the run exists
swamp trace last                                   # the run tree, with the diff summary
swamp diff last --stat                             # `last` is the run; <node> works from any run
swamp adopt last                                   # applies the patch to your checkout
```

`swamp` with no arguments, or `swamp chat`, starts an interactive session with a brain: a CLI
agent that plans, reads the repo, and dispatches workers through Swamp's own MCP tools. It never
edits files itself. `swamp run <TASK>` uses the same brain for exactly one turn: there is nobody
to answer a follow-up, so it is told to end by naming the nodes worth landing and the
`swamp adopt <node>` command for each, or to say that nothing is.

## Chat UI

`swamp chat` (and bare `swamp`) draws an inline terminal UI: finished blocks scroll into your
terminal's own scrollback, where the mouse can still select them, and only the live tail is
redrawn. Assistant text renders as markdown while it streams, and a `swamp_dispatch` call opens
a live worker board, folded from the same journal `swamp watch` reads: one row per worker with
its spinner, tier, account, model, elapsed time and cost, and, when it lands, its branch and
`+N -M`. When stdout is not a terminal the whole thing falls back to the plain transcript, so
pipes, CI and `swamp run` are unaffected.

| Key | What it does |
|---|---|
| `enter` | Send. With the popup open, complete the selected command instead. |
| `alt+enter`, `shift+enter`, trailing `\` | Newline. `shift+enter` needs the kitty keyboard protocol. |
| `esc` | Close the popup, else interrupt the turn. |
| `esc esc` | Cancel every running worker, within two seconds of the first `esc`. |
| `ctrl+c` | Clear the input; again on an empty input to leave. |
| `ctrl+d` | Leave. |
| `ctrl+l` | Clear the screen; the scrollback above is untouched. |
| `ctrl+o` | Expand the last collapsed tool result or worker board. |
| `up` / `down` | History on an empty input, otherwise move between the input's lines. |
| `/` | Open the command popup; `tab` completes, `↑↓` chooses. |
| `?` | Shortcut overlay, on an empty input. |
| `ctrl+a`, `ctrl+e`, `ctrl+k`, `ctrl+u`, `ctrl+w`, `alt+←/→` | Readline editing. |

Commands: `/help`, `/status`, `/accounts`, `/trace [node]`, `/cost`, `/tier [low|mid|high]`,
`/workers [n]`, `/cancel <node|all>`, `/diff <node>`, `/thinking [on|off]`, `/clear`,
`/resume <run>`, `/quit`.

`[ui]` settings: `chat_theme` (`auto`, `truecolor`, `ansi256`, `plain`), `collapse_lines`
(default 3), `chat_history` (default 500 entries, kept in `.swamp/chat_history`), `refresh_hz`
(default 12), `show_thinking`.

## Commands

| Command | What it does |
|---|---|
| `swamp run <TASK>` | One-shot. `--no-brain` sends the task straight to one worker. `--tier`, `--provider`, `--account`, `--workers`, `--budget`, `--detach`. |
| `swamp trace [RUN\|last\|-2]` | Render a run tree: nodes, attempts, accounts, failures, cost. `--events`, `--raw`, `--follow`, `--failed`, `--json`. |
| `swamp watch [RUN\|last]` | Live read-only TUI. Attach from a second terminal while a run is going. |
| `swamp doctor` | Health checks. `--probe` calls each account's CLI, `--schema` reports adapter drift, `--reap` removes stale worktrees and pidfiles, and sweeps `~/.swamp/sock` for sockets no process is listening on, `--fix` creates the directories and the git exclude. Exit 1 on any error, so CI can gate on it. |
| `swamp chat` | Interactive brain session. |
| `swamp runs`, `swamp resume`, `swamp cancel` | List runs, recover an interrupted one (`--plan` first, it spends nothing), stop one. |
| `swamp accounts` | Health, in-flight count, quota windows, cooldowns, lifetime spend, including the brain's. Entries for ids no longer in the config are listed under `not in config`. Also `cooldown`, `clear`, `enable`, `disable`, `reset [ID]`. |
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
