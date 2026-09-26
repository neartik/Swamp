# Dispatches: listing, inspecting, cancelling

A dispatch is one `swamp_dispatch` call (or the single task of `swamp run --no-brain`): an id
(`dsp_…`), the caller that asked, and the logical id every task got before anything ran. Both the
brain and the user can list, inspect and cancel dispatches, from any process and across resumes,
because everything below is folded from the run's journal (schema 2, DESIGN §7.2).

## Surfaces

| who | list | one dispatch or node | cancel |
|---|---|---|---|
| user | `swamp dispatches [RUN\|last] [--failed] [--follow] [--json]` | `swamp dispatch <ID> [--run RUN] [--json]`, `swamp trace --dispatch <ID>`, `swamp trace --group-by dispatch` | `swamp cancel <dispatch\|node\|run>` |
| chat | `/dispatches [--failed]` | `/trace <node\|dispatch>` | `/cancel <node\|all>` |
| brain | `swamp_status { dispatch? }` | `swamp_inspect { dispatch }`, `swamp_inspect { node }` | `swamp_cancel { dispatch?, nodes? }` |

A dispatch id resolves like a node id: in full (`dsp_01K…`), without its prefix, by the short id
the list prints (its last six characters), or by any unique prefix, searched across every run
newest first. A prefix two dispatches share names neither, and the error lists both. `--run` on
`swamp dispatch` (or the RUN argument of `swamp trace`) limits the search to one run, which is how
`legacy` is named once several schema-1 runs exist. `swamp cancel` resolves runs first, then nodes
and dispatches together; `nd_` and `dsp_` prefixes pick one kind.

A schema-1 run has no dispatch events. Its nodes are listed in one bucket named `legacy`, with no
call seq and no caller; the bucket settles when the run finishes.

## Delegation

A brain that reads the repo itself instead of dispatching spends the most expensive context in
the run. `RunView::brain_self_work` measures it from the journal: the tool calls the brain made
itself before its first `swamp_dispatch`, and the brain's share of the run's cost. swamp's own
MCP tools are never counted. The cut is the brain's `swamp_dispatch` call in its own stream
order, because the brain's stdout is read while the MCP call is served and a read it made first
can reach the journal after the `dispatch_issued` line; a stream that never names the call
(schema 1, or a CLI that hides MCP calls) falls back to the journal's first dispatch. Until then
every call counts, and the line says `no dispatch yet`.

The budget is `limits.brain_read_budget` (default 8), the same number the system prompt gives
the brain. Every surface prints it:

- `swamp trace` and `swamp dispatches`, under the rows, and `/status` and `/dispatches` in chat:
  `brain  3/8 calls before the first dispatch, 12% of the cost`. Past the budget the line ends
  `: over limits.brain_read_budget`. The share is left out until the brain has reported a cost,
  and reads `of the known cost` while some node has reported none: the total is then a floor and
  the brain's share only a ceiling.
- `swamp board`, in the header: `brain 3/8 (12%)`, `(~12%)` when the cost is incomplete, the
  share dropped at the narrow tier. Within
  budget it is the first header cell to give way; past it the cell turns red, reads
  `brain 11/8 over`, and outlasts the queued count and the cost. The header shows the tailed run
  whose brain made the most calls.
- `--json`: `swamp dispatches --json` and `swamp trace --json` (without `--dispatch` or `--node`)
  carry a top-level `brain`, and each run of `swamp board --json` carries one, `null` for a run
  without a brain:

```json
"brain": {
  "calls": 3, "budget": 8, "over_budget": false, "dispatched": true,
  "brain_usd": 0.42, "total_usd": 3.5, "cost_share": 0.12, "cost_complete": true
}
```

`brain_usd` and `cost_share` are null until the brain has reported a cost. `cost_complete` is
false when some node reported no cost, which makes `total_usd` a floor and `cost_share` a
ceiling.

## Cancelling

Every surface cancels through one helper (`dispatch::cancel::cancel_node`), so the journal looks
the same whoever asked:

1. `cancel/<logical_id>` is written in the run directory with who asked (`user` or `brain`).
2. One durable `NodeStateChanged { to: Cancelled { by } }` is journaled on the task's logical id.
   A task that already ended is left alone: nothing is journaled out of a terminal state, and the
   first terminal state journaled for a task is the one every reader folds.
3. The live attempt is stopped. In the supervising process its cancellation token fires and the
   executor kills the process group; from any other process the group named by the attempt's
   pidfile is killed (SIGTERM, then SIGKILL after `limits.grace_period`; `--signal kill` skips
   the grace period). `swamp cancel` also kills an attempt still running under our pidfile when
   its task already ended, such as a worker whose supervisor died before stopping it.

The supervising run watches for the marker, so a task cancelled from another terminal is never
retried and its dispatch settles with the task counted as cancelled. `swamp_cancel` only stops
tasks of dispatches the brain itself issued in the current run. `swamp cancel` exits 0 when it
cancelled at least one node, 1 when there was nothing left to cancel.

## Journal events

Every surface above is a fold over `journal.jsonl`, one JSON object per line: `seq`, `at`, `run`,
an optional `node` (the line node), and the event, tagged by `ev`. These are every `ev` a schema-2
journal can hold; `tests/docs_drift.rs` fails when this list and `JournalEvent` disagree.

| `ev` | line node | what it records |
|---|---|---|
| `run_started` | none | the run header: swamp version, `schema`, argv, cwd, repo, base, config hash, task. A resume appends a second one; the first is the run's origin |
| `node_spawned` | the node | a full `NodeRecord` snapshot under `record`, before the process starts; everything after is a delta |
| `account_selected` | the task | the account a lease went to, its exec, the policy and why, and the accounts excluded |
| `model_resolved` | the task | the tier's model for that account, with its extra flags |
| `process_started` | the attempt | pid, process group, argv, env overrides, cwd |
| `session_bound` | the node | the CLI session handle, for `--resume` |
| `node_event` | the node | one normalized `WorkerEvent` and the byte offset in `stream.jsonl` it came from |
| `node_usage` | the node | tokens and cost so far |
| `node_files` | the attempt | the files git says the attempt changed |
| `node_blocked` | the task | no account can take the task until `until`: `why`, and each account's refusal |
| `node_retry` | the attempt | the attempt failed with `reason` and the task retries, rotating account or not |
| `provider_switch` | the task | cross-provider failover moved the task to another provider |
| `worktree_created` | the task | the attempt's worktree, branch and base |
| `diff_captured` | the attempt | the attempt's patch and its size |
| `node_finished` | the node | terminal state, exit, usage, cost, work, summary, files, unparsed lines |
| `account_health` | none | an account's health, cooldown and quota snapshot changed |
| `account_usage` | the node | an account's window and lifetime token counters, on every commit and window roll |
| `brain_turn` | the brain | a user or assistant turn of the brain conversation |
| `brain_tool_call` | the caller | one swamp MCP tool call: tool, argument hash and file, `call_seq`, and the dispatch a `swamp_dispatch` issued |
| `dispatch_issued` | the caller | the `DispatchRecord`: id, caller, call seq, wait, and every task's logical id, before any task starts |
| `task_queued` | the task | the task waits for a lease: title, tier, depth, dispatch |
| `dispatch_rejected` | the task | a hard limit refused the task; it is never queued |
| `node_state_changed` | the task or attempt | a phase transition, `from` a payload-free phase `to` a full state, with why |
| `process_exited` | the attempt | the process is gone, with its exit code or signal |
| `dispatch_settled` | the caller | every task of the dispatch has ended: counts and cost |
| `adopted` | the node | the node's work landed: branch, commit, conflicts |
| `note` | any | free text from the user, the brain (`swamp_note`) or swamp |
| `run_finished` | none | graceful shutdown; its absence marks a run interrupted |

A schema-1 journal has none of `dispatch_issued`, `task_queued`, `dispatch_rejected`,
`node_state_changed`, `process_exited` or `dispatch_settled`, and still folds: its workers land in
the `legacy` bucket.

## JSON

The `swamp dispatches`, `swamp dispatch` and `swamp_inspect` documents carry `"schema": 2`, the
journal schema they are folded from; the `swamp_cancel` and `swamp_result` replies do not. Node ids are strings
with their `nd_` prefix and dispatch ids with `dsp_` (or `legacy`); times are RFC 3339 UTC;
durations are `_s` (seconds) or `_ms` (milliseconds). Fields are only ever added.

### `swamp dispatches --json`

```json
{
  "schema": 2,
  "run": "run_01ARZ3NDEKTSV4RRFFQ69G5F00",
  "dispatches": [ <summary>, ... ],
  "brain": <self work> | null
}
```

`brain` is the delegation metric of [Delegation](#delegation), null for a run without a brain.

With `--follow --json`, one `<summary>` per line, printed whenever a dispatch's state, counts or
cost change.

`<summary>`:

| field | type | meaning |
|---|---|---|
| `id` | string | `dsp_…`, or `legacy` |
| `short` | string | what the text list prints |
| `state` | `open` \| `settled` | settled once every task has ended |
| `call_seq` | int \| null | the n-th brain tool call of the run; null outside a tool call |
| `caller` | object \| null | `{ node, kind: "brain" \| "task", task }`: who dispatched; `task` is the caller's logical id when a worker dispatched |
| `at` | time \| null | when the dispatch was issued (the run start for `legacy`) |
| `age_s` | int \| null | seconds since `at` |
| `wait`, `max_wait_s` | bool \| null, int \| null | how the call was made; `wait` is null for `legacy` |
| `tasks` | int | number of tasks |
| `counts` | object | every task under its current phase: `queued`, `blocked`, `leased`, `running`, `succeeded`, `failed`, `cancelled`, `rejected` |
| `cost` | `<rollup>` | the tasks and all their attempts |

`<rollup>`: `{ usd, complete, usage, nodes, failed, rejected }`. `complete` is false when an
attempt reported no cost, so `usd` is a lower bound; `usage` is token counts
(`input_tokens`, `cached_input_tokens`, `cache_write_tokens`, `output_tokens`,
`reasoning_tokens`); `nodes` counts tasks that were not rejected.

### `swamp dispatch <ID> --json` and `swamp_inspect { dispatch }`

```json
{
  "schema": 2,
  "run": "run_…",
  "dispatch": <summary>,
  "tasks": [ <task>, ... ]
}
```

### `swamp_inspect { node }`

```json
{ "schema": 2, "task": <task> }
```

`node` takes the logical id or any attempt's, in full, by short id or by prefix.

`<task>`:

| field | type | meaning |
|---|---|---|
| `node` | string | the logical id |
| `title`, `tier` | string | |
| `dispatch` | string | the dispatch it belongs to |
| `parent` | string \| null | the caller |
| `depth` | int \| null | nesting depth below the run root; null in schema 1 |
| `state` | phase | `queued`, `blocked`, `leased`, `running`, `succeeded`, `failed`, `cancelled`, `rejected` |
| `detail` | object | the full state with its payload, e.g. `{ "state": "running", "pid": 4242, "pgid": 4242, "since": … }` |
| `elapsed_ms` | int \| null | first attempt start to last attempt end, or to now while live |
| `account`, `model`, `provider` | string \| null | of the latest attempt |
| `pid`, `pgid` | int \| null | of the latest attempt while it runs |
| `attempts` | `[<attempt>]` | oldest first |
| `cost` | `<rollup>` | the task, its attempts and everything it dispatched in turn |
| `blocked` | object \| null | while blocked: `{ until, why, ineligible: [{ account, reason }] }`, `reason` being `disabled`, `auth_broken`, `cooling`, `provider_stop`, `credits_depleted`, `spend_control`, `at_capacity` or `quota_stop` |
| `rejected` | failure \| null | why a hard limit refused the task |
| `failure` | failure \| null | why the task failed |
| `transitions` | `[{ from, to, why }]` | the task's journaled phase changes |
| `dispatches` | `[string]` | dispatches this task's attempts issued |

`<attempt>`: `{ node, attempt, state, account, model, provider, pid, pgid, started_at,
ended_at, elapsed_ms, usage, cost, exit, failure }`, where `cost` is `{ usd, basis }` or null and
`exit` is `{ code, signal, duration_ms }` or null.

A failure is the journal's `Failure` (`{ "kind": "worker_error", "subtype": …, "detail": … }`
and so on). Through MCP its `detail` and `evidence` arrive wrapped in a `<worker-output>`
envelope, like every other worker-derived string.

### `swamp_cancel`

```json
{
  "cancelled": ["nd_…"],
  "ended": [{ "node": "nd_…", "state": "succeeded" }],
  "refused": [{ "node": "nd_…", "reason": "not dispatched by you" },
              { "dispatch": "dsp_…", "reason": "not dispatched by you" }],
  "nodes": [ <NodeResult>, ... ]
}
```

A `refused` entry names either a `node` or a whole `dispatch`; its `reason` is free text, such as
`not dispatched by you` or the error the cancel hit. `nodes` are the cancelled tasks' results once
they settle, the same shape `swamp_await` returns.

### `swamp_result` for a node this process did not dispatch

After a resume, or for any node the live results map does not hold, `swamp_result` reads the
latest finished attempt's `nodes/<attempt>/result.json` (or, without one, what the journal alone
knows) and marks it `"source": "journal"` with the `attempt` it came from. When the task's
journaled state differs from that attempt's (cancelled while queued for a retry, or still
retrying), `state` and `ok` follow the task, and so does `failure` once the task has ended.
`swamp_worker_diff` falls back to that attempt's `patch.diff` the same way.

### `swamp trace --json`

The document carries a top-level `brain` (see [Delegation](#delegation)) unless `--dispatch` or
`--node` narrows it. `--node <ID> --json` prints only that node's `NodeRecord`: no `brain`, `tree`,
`totals` or `events`. With `--dispatch <ID>`, `tree`, `nodes` and `events` hold only that dispatch's tasks and what they
dispatched in turn, `totals` is the dispatch's rollup, and a `dispatch` field names it.
`--group-by dispatch` has no JSON form (use `swamp dispatches --json`) and does not combine with
`--follow`; `--dispatch` does not combine with `--node`.
