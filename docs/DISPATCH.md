# Dispatches: listing, inspecting, cancelling

A dispatch is one `swamp_dispatch` call (or the single task of `swamp run --no-brain`): an id
(`dsp_…`), the caller that asked, and the logical id every task got before anything ran. Both the
brain and the user can list, inspect and cancel dispatches, from any process and across resumes,
because everything below is folded from the run's journal (schema 2, DESIGN §7.2).

## Surfaces

| who | list | one dispatch or node | cancel |
|---|---|---|---|
| user | `swamp dispatches [RUN\|last] [--failed] [--follow] [--json]` | `swamp dispatch <ID> [--json]`, `swamp trace --dispatch <ID>`, `swamp trace --group-by dispatch` | `swamp cancel <dispatch\|node\|run>` |
| chat | `/dispatches [--failed]` | `/trace <node\|dispatch>` | `/cancel <node\|all>` |
| brain | `swamp_status { dispatch? }` | `swamp_inspect { dispatch }`, `swamp_inspect { node }` | `swamp_cancel { dispatch?, nodes? }` |

A dispatch id resolves like a node id: in full (`dsp_01K…`), without its prefix, by the short id
the list prints (its last six characters), or by any unique prefix, searched across every run
newest first. A prefix two dispatches share names neither, and the error lists both. `swamp cancel`
resolves runs first, then nodes and dispatches together; `nd_` and `dsp_` prefixes pick one kind.

A schema-1 run has no dispatch events. Its nodes are listed in one bucket named `legacy`, with no
call seq and no caller; the bucket settles when the run finishes.

## Cancelling

Every surface cancels through one helper (`dispatch::cancel::cancel_node`), so the journal looks
the same whoever asked:

1. `cancel/<logical_id>` is written in the run directory with who asked (`user` or `brain`).
2. One durable `NodeStateChanged { to: Cancelled { by } }` is journaled on the task's logical id.
   A task that already ended is left alone: nothing is journaled out of a terminal state.
3. The live attempt is stopped. In the supervising process its cancellation token fires and the
   executor kills the process group; from any other process the group named by the attempt's
   pidfile is killed (SIGTERM, then SIGKILL after `limits.grace_period`; `--signal kill` skips
   the grace period).

The supervising run watches for the marker, so a task cancelled from another terminal is never
retried and its dispatch settles with the task counted as cancelled. `swamp_cancel` only stops
tasks of dispatches the brain itself issued in the current run. `swamp cancel` exits 0 when it
cancelled at least one node, 1 when there was nothing left to cancel.

## JSON

Every document carries `"schema": 2`, the journal schema it is folded from. Node ids are strings
with their `nd_` prefix and dispatch ids with `dsp_` (or `legacy`); times are RFC 3339 UTC;
durations are `_s` (seconds) or `_ms` (milliseconds). Fields are only ever added.

### `swamp dispatches --json`

```json
{
  "schema": 2,
  "run": "run_01ARZ3NDEKTSV4RRFFQ69G5F00",
  "dispatches": [ <summary>, ... ]
}
```

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
| `wait`, `max_wait_s` | bool, int \| null | how the call was made |
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
  "refused": [{ "node": "nd_…", "reason": "not dispatched by you" }],
  "nodes": [ <NodeResult>, ... ]
}
```

`nodes` are the cancelled tasks' results once they settle, the same shape `swamp_await` returns.

### `swamp_result` for a node this process did not dispatch

After a resume, or for any node the live results map does not hold, `swamp_result` reads the
latest finished attempt's `nodes/<attempt>/result.json` (or, without one, what the journal alone
knows) and marks it `"source": "journal"` with the `attempt` it came from. `swamp_worker_diff`
falls back to that attempt's `patch.diff` the same way.
