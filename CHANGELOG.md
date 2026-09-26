# Changelog

Notable changes to Swamp. The journal schema, the `--json` shapes in `docs/DISPATCH.md` and the
config keys only ever change additively; anything else that would break a user is called out.

## Unreleased

### Delegation (P1, P5)

- The brain is told to delegate early: at most `limits.brain_read_budget` (default 8) tool calls
  of its own before its first `swamp_dispatch`, investigation sent out as a low tier task, and a
  tier rubric that starts at low (mid for normal changes, high only for design and review).
- The delegation is measured. `RunView::brain_self_work` counts the brain's own tool calls before
  its first dispatch, in its stream order, and its share of the run's cost. `swamp trace`,
  `swamp dispatches`, `/status` and `/dispatches` print a `brain` line, the board header a
  `brain 3/8 (12%)` cell, and `swamp dispatches --json`, `swamp trace --json` (without
  `--dispatch` or `--node`) and each run of `swamp board --json` carry a `brain` object; each
  warns past the budget.

### Dispatch lineage, journal schema 2 (P2)

- Every dispatch, logical task, attempt, rejection and state change is a durable journal fact:
  `dispatch_issued`, `task_queued`, `dispatch_rejected`, `node_state_changed`, `process_exited`
  and `dispatch_settled`. `node_blocked` carries each account's refusal and `brain_tool_call` its
  call sequence and the dispatch it issued. A schema-1 journal still folds, its workers in a
  `legacy` bucket.
- The fold answers lineage, state and cost from those facts: dispatches, tasks, transitions,
  `state_of`, `attempts` and rollups by dispatch, task or subtree. Queued and rejected tasks are
  tree rows.
- `swamp run --no-brain` is a one-task dispatch, `replay --reparse` keeps the dispatch lines,
  `swamp resume` closes stranded tasks and settles open dispatches, and a dispatcher built for an
  existing run continues its depths, node budget and call sequence.

### Dispatch surface (P3)

- `swamp dispatches [RUN]` (`--failed`, `--follow`, `--json`) and `swamp dispatch <ID>`
  (`--run`, `--json`) list and inspect dispatches; `swamp trace --dispatch <ID>` narrows a trace
  and `--group-by dispatch` sections it. Dispatch ids resolve in full, by short id or by prefix
  across every run.
- The brain gains `swamp_inspect` and `swamp_cancel`, `swamp_status` takes a dispatch, and
  `swamp_result` / `swamp_worker_diff` answer from the journal for nodes a previous process
  dispatched.
- One cancel path (`dispatch::cancel::cancel_node`) for every surface: a marker the supervising
  run honours instead of retrying, one durable `Cancelled` transition, and the process group
  killed from any process. `swamp cancel` takes run, node and dispatch ids. Another process may
  append to a live journal under a lock.
- `docs/DISPATCH.md` documents the surface and every field of its JSON.

### Board and keys (P4)

- `swamp board` groups each run by dispatch, ranks tasks failures first, names why a blocked task
  waits, and draws on three declared layout tiers (Narrow, Medium, Wide) at any pane size.
- The board cancels a task or a dispatch with `k`, after a `y` / `n`, behind `ui.board_actions`.
- `swamp board --once` draws at `$COLUMNS` when set, so a script can pick the tier; an account
  with no quota window leaves its bars blank.
- One key table (`ui::keys`) drives every surface's hints, overlays and the README Keys table;
  `swamp watch` asks before it cancels.
- Chat dispatch blocks render from the dispatch their call issued, queued, blocked and rejected
  tasks included; the status line counts open dispatches and running, stuck and queued tasks.

### Doctor (P1, P5)

- `swamp chat` and `swamp run` warn at startup when a permission mode would deny Bash.
- `swamp doctor --schema` also prints the journal schema of every recent run and checks the
  configured claude permission modes against the `--permission-mode` choices `claude --help`
  lists, naming the right spelling.

### Fixes

- A task cancelled while it waited for a lease no longer launches when the slot it waited on
  frees (P5).
- `swamp adopt --dry-run` no longer reports a conflict with no paths when git refuses
  `merge-tree` (git older than 2.40); it says which git the dry run needs (P5).
- Ctrl+D on an empty chat input interrupts, cancels and quits like an armed Ctrl+C instead of
  hanging for up to 15 minutes; brain shutdown waits at most `limits.grace_period` (P1).
- The default anthropic worker runs with `acceptEdits` plus an explicit allowed-tools list,
  Bash included, so it can run the tests it was sent to run; never `bypassPermissions` (P1).
- `limits.brain_turn_timeout` had no effect and is no longer a default; it still parses (P1).

### Internal (P5)

- `src/doctor.rs` is split into `doctor/{env,accounts,permissions,workspace,reap}`, with the
  public API unchanged and the output pinned by a snapshot.
- One `worktree_root` and one repo hash in `workspace`; the production `DirectRunner` in
  `dispatch::runner` backs both the dispatcher and `swamp run --no-brain`; the pidfile liveness
  check is `RunPaths::is_live`.
- CI runs `cargo fmt --check`, `clippy -D warnings` and `cargo test` on Linux and macOS and
  uploads pending `insta` snapshots on failure. `tests/docs_drift.rs` checks the `ev` names in
  `docs/DISPATCH.md` and DESIGN §7.2 against `JournalEvent`, and that every subcommand has a row
  in the README command table.
