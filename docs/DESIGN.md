# Swamp - Design

Swamp is a Rust CLI/shell that orchestrates coding agents. A principal agent (the brain) runs as
an official coding-CLI subprocess and fans work out to worker subprocesses, each isolated in a git
worktree, each drawn from a pool of per-account executables. Every run produces a persistent,
inspectable execution tree.

Status: v1 design. Every CLI flag named here was checked against `claude --help`,
`codex --help` and `codex exec --help` in `docs/ref/`. Flags that do not appear in those
option lists are not used; see "Flag verification notes".

---

## 1. Goals and non-goals

### Goals

1. One binary, `swamp`. Subcommands: `chat` (default), `run`, `trace`, `watch`, `runs`,
   `accounts`, `doctor`, `diff`, `adopt`, `gc`, `replay`, `config`.
2. The brain decomposes work and dispatches sub-tasks. Workers are the official CLIs in
   non-interactive mode, so they keep their full tool harness (file edits, shell, MCP).
3. Tier (high/mid/low) maps to a model per provider. Model ids live only in config. No model id
   appears in the source.
4. Multi-account: a pool of executables per provider, one executable per subscription. Swamp never
   handles credentials. Selection is round-robin, least-loaded or quota-aware. Rate-limit and auth
   failures fail over to the next account.
5. Subscriptions only. An API key is optional and is never required for the core flow. Swamp makes
   zero network calls of its own.
6. Traceability is first class. Every node records parent, prompt, provider, executable, model,
   tier, start/end, usage, cost when available, exit status, files touched, and the verbatim worker
   event stream. Storage is JSONL under `.swamp/`.
7. Parallel workers are isolated in git worktrees. Results surface as branches plus patches.
8. Clean extension points for new providers.

### Non-goals for v1 (explicit cuts, with the seam that admits them later)

| Cut | Seam |
|---|---|
| Direct-API brain | `trait Brain` has a third `#[cfg(feature = "api-brain")]` impl stub |
| DAG dependencies between sub-tasks | `TaskRequest.deps: Vec<NodeId>` is parsed and rejected |
| Automatic merge / conflict resolution | `swamp adopt` is a user action; `MergeStrategy` enum exists |
| Worker follow-up turns (multi-turn workers) | `SessionHandle` + `--resume` plumbing is built and tested |
| Quota-aware as the default policy | `SelectionPolicy::QuotaAware` is implemented but not default |
| Windows support | All platform code confined to `worker/spawn.rs`, `worker/liveness.rs`, `mcp/server.rs` |
| Raw stream compression | `gc` deletes rather than compresses |

### Smallest runnable thing

```
swamp run "fix the flaky test in tests/api.rs" --no-brain --tier mid
```

One process, one worktree, one node, one diff. It exercises config, account pool, adapter, spawn,
parse, classify, journal and workspace with no brain and no MCP. Build this first (see PLAN.md).

---

## 2. Brain mode

**Decision: the brain is an official CLI driven headlessly, with Swamp's dispatch tools injected as
an MCP stdio server. Direct API is feature-gated and off by default.**

### Why not direct API

The hard constraint is "must work on subscriptions only; an API key must be optional." A direct-API
brain requires `ANTHROPIC_API_KEY` / `OPENAI_API_KEY`. That alone disqualifies it as the default.
Three secondary reasons:

- The multi-account story is "one executable per subscription". A direct-API brain would need a
  second, incompatible credential mechanism, so the user would configure accounts twice.
- A direct-API brain has no tools until we write them. It cannot read the repo to decide how to
  decompose a task without re-implementing Read/Grep/Glob/Bash. Decomposition quality depends on
  repo context, so this is not optional.
- We would own context management, compaction, retries and tool-loop mechanics. All of that already
  exists, tested, in the CLIs.

### Why CLI-as-brain works

- Auth is the CLI's problem. `claude-main` / `claude-alt` wrappers set `CLAUDE_CONFIG_DIR`;
  `codex-main` sets `CODEX_HOME`. The brain is just another entry in the same account pool, selected
  by the same code path, journaled with the same node schema.
- The brain inherits a full tool harness: it can Read/Grep the repo, run `git log`, inspect a failing
  test, and then decide the decomposition and the tier.
- Custom tools are deliverable. Both CLIs accept MCP stdio servers non-interactively:
  `claude --mcp-config <json-or-file> --strict-mcp-config`, and codex `-c mcp_servers.<name>.command=...`.

Cost of the choice: we do not control the brain's system prompt beyond `--append-system-prompt`, and
we inherit vendor stream-schema churn. Both are mitigated (section 4.7, section 10).

### Topology: in-process MCP server plus a thin stdio bridge

The MCP tools must share live state with the dispatcher (account pool, journal writer, running
worker handles), so the server is in-process: Swamp hosts JSON-RPC 2.0 over a Unix domain socket at
`~/.swamp/sock/<run_short>.sock`. The socket does NOT live under the run directory: macOS caps a
`sockaddr_un` path at 104 bytes (SUN_LEN) and a repo can sit arbitrarily deep, so a run inside a
long path would fail to bind before anything ran. The real path is recorded in `run.json`. The CLIs
only spawn stdio children, so Swamp passes itself as that child:

```json
{"mcpServers":{"swamp":{"command":"/abs/path/to/swamp",
  "args":["mcp-bridge","--socket","/Users/me/.swamp/sock/<run_short>.sock"]}}}
```

`swamp mcp-bridge` is a hidden subcommand: a byte pump between stdin/stdout and the socket. No
protocol logic, no state. One source of truth, identical for both providers. The generated
`mcp.json` uses `std::env::current_exe()`, never the bare name `swamp`.

An HTTP/SSE MCP server on loopback would remove the bridge but adds an HTTP stack, a bearer-token
scheme and a listening port. Not worth it for v1.

### Session shape per provider

- **Anthropic brain (the happy path).** One long-lived process,
  `claude -p --input-format stream-json --output-format stream-json --verbose`. Swamp writes user
  turns as JSON lines to stdin and reads events from stdout. True interactive chat, one process for
  the session, prompt cache stays warm. This is the one node with a live stdin pipe and is therefore
  the one node that is not detach-survivable.
- **OpenAI brain.** `codex exec` has no streaming stdin format, so each user turn is a fresh
  `codex exec resume <thread_id> --json` (turn 1 is plain `codex exec`). Continuity comes from
  Codex's own thread persistence. Slower per turn, functionally equivalent.

`swamp run` drives the same brain for exactly one turn, and is told so: a one-shot paragraph in
the system prompt (`BrainMode::OneShot`) requires the last turn to name the nodes worth landing
and the exact `swamp adopt <node>` command, or to say that nothing is. Without it the brain ends
a non-interactive run with an offer ("say the word and I'll merge") that nobody can accept.
`swamp chat` keeps the interactive text unchanged.

`BrainTransport { Persistent, ResumePerTurn }` absorbs the asymmetry as data rather than as two
hand-written classes. `codex app-server` / `exec-server` are experimental long-lived transports and
are the stated upgrade path for a third variant.

### Brain resilience

The brain's `session_uuid` is pre-assigned and journaled before launch, so a crashed brain is
resumed with `claude -r <id>` / `codex exec resume <thread>` plus a journal-derived state preamble.
Workers keep running throughout, because they are detached (section 4.1). Losing the brain never
loses worker work.

`reserve_brain_slot = true` (default) keeps one healthy account of the brain's provider out of the
worker pool, unless that provider has only one account, in which case nothing is held back: holding
the only account would leave the workers with none. Without the reservation the system deadlocks:
the brain blocks in `swamp_await`, workers hold every account, and nothing can finish because the
brain never gets to run an integration node.

The brain's permit comes from a dedicated one-slot semaphore, never from `max_parallel`. The
reservation already subtracts a worker permit; charging the brain a second one out of the same
budget leaves `max_parallel - 2` workers and deadlocks outright at `max_parallel <= 2`.

### MCP tool surface

| tool | behaviour |
|---|---|
| `swamp_dispatch` | create 1..N worker nodes `{title, prompt, tier, provider?, isolation?}`; returns node ids; `wait` defaults true with `max_wait_s` |
| `swamp_await` | block on node ids with a timeout; returns `NodeResult` per node |
| `swamp_status` | the run tree rendered compactly for an LLM, byte-budgeted |
| `swamp_result` | one node: final text, files, usage, cost, failure class |
| `swamp_worker_diff` | a node's patch, truncated, with a path to the full file |
| `swamp_note` | write an annotation into the journal (the brain's reasoning, preserved) |

Caps are enforced in the dispatcher, never in the prompt: `max_parallel_dispatch`,
`max_nodes_per_run`, `max_high_tier_concurrent`, `max_depth`, and a pre-spawn USD/token budget check.
A confused brain must not be able to fork-bomb a subscription.

`swamp_dispatch` always returns within `max_wait_s` with partial results marked `"running"` rather
than blocking forever. Structurally, `mcp/tools.rs` depends on `dispatch/` and `journal/` and on
nothing in `brain/`, which is what keeps the dispatch tool from deadlocking against the brain process
it serves.

---

## 3. Module tree

Single binary crate. At this size a workspace costs more ceremony than it returns. The three seams
that become crates later (`worker/`, `journal/`, `mcp/`) are already dependency-clean: they depend
on `model/` and `config/` only, never on each other.

```
Swamp/
  Cargo.toml
  rust-toolchain.toml
  swamp.example.toml
  docs/{DESIGN.md,PLAN.md,ref/}
  src/
    main.rs             tokio::main, tracing init, config load, clap dispatch to cmd::*
    lib.rs              module declarations + pub use of the cross-package surface
    cli.rs              clap derive: Cli, Command, every flag. Nothing else.
    error.rs            SwampError (thiserror); exit-code mapping
    ids.rs              RunId/NodeId (ULID newtypes), NodeIds (ULID + session UUID pair)
    doctor.rs           health checks, shared by cmd::doctor and startup warnings

    model/
      mod.rs            re-exports
      core.rs           Provider, Tier, NodeKind, NodeState, Usage, Cost, FileChange, RateLimit*
      failure.rs        Failure taxonomy, Detector, rotates_account/retries_same_account/is_terminal
      node.rs           NodeRecord: the one record type the whole tool agrees on
      event.rs          WorkerEvent: provider-neutral normalized event
      result.rs         NodeResult / TaskRequest: the brain-facing contract

    config/
      mod.rs            re-exports, Config::load
      schema.rs         serde structs mirroring swamp.toml exactly
      load.rs           layered merge: defaults <- ~/.config <- ./.swamp <- env <- flags
      resolve.rs        tier x provider -> model; account -> exec/env; path expansion
      validate.rs       all errors reported together, with the offending key

    journal/
      mod.rs            Journal::open -> (JournalHandle, JoinHandle); single writer task
      record.rs         JournalLine { seq, at, run, node, event } + JournalEvent
      writer.rs         owns the fd; torn-tail repair; barrier fsync policy
      reader.rs         streaming line reader, byte offsets, tolerant of a torn last line
      fold.rs           RunView: fold(JournalLine) -> tree; Projection trait
      raw.rs            RawSink: verbatim per-node stream + stderr + noise sinks
      paths.rs          .swamp and ~/.swamp layout, run discovery, `last` symlink

    worker/
      mod.rs            re-exports
      adapter.rs        ProviderAdapter trait, LaunchSpec, ParseState, Capability, ExitContext
      spawn.rs          detached spawn: process group, file-backed fd0/1/2, pidfile
      follow.rs         tail stream.jsonl from a byte offset, drive the parser, emit events
      liveness.rs       pid + process start time (PID-reuse safe), killpg, reap
      claude.rs         ClaudeAdapter: argv + wire types + parse + classify
      codex.rs          CodexAdapter: argv + wire types + parse + classify
      classify.rs       shared layered classifier, pattern sets, Detector bookkeeping
      prompt.rs         the built-in worker role, plus worker.system_prompt_file

    dispatch/
      mod.rs            Dispatcher: owns pool + semaphores; dispatch_batch/dispatch_one
      account.rs        Account, AccountState, Health
      pool.rs           AccountPool, Lease, acquire/release/report; Notify-based waiting
      policy.rs         SelectionPolicy scoring
      cooldown.rs       cooldown math, clamping, circuit breaker
      persist.rs        ~/.swamp/accounts.json, fs4 lock + atomic rename
      retry.rs          run_node(): the attempt loop; where failover policy lives

    workspace/
      mod.rs            WorkspaceManager: create/finalize/gc, global git gate
      git.rs            thin async wrapper over the `git` CLI
      worktree.rs       worktree add/remove/prune, branch naming, seed paths
      diff.rs           git diff --numstat / --name-status -> Vec<FileChange>; patch emission
      adopt.rs          apply / merge / cherry-pick into the user's checkout

    mcp/
      mod.rs            McpServer: UDS listener, per-connection loop, tool registry
      jsonrpc.rs        hand-rolled JSON-RPC 2.0 over NDJSON
      server.rs         initialize / tools/list / tools/call
      tools.rs          swamp_* tools as plain async fns + hand-written JSON Schemas
      bridge.rs         `swamp mcp-bridge`: stdio <-> UDS byte pump, zero logic

    brain/
      mod.rs            Brain / BrainSession traits, BrainEvent, factory from config
      claude.rs         persistent stream-json session over stdin/stdout
      codex.rs          resume-per-turn session
      prompt.rs         the Swamp system prompt: tool contract, tier rubric, worktree semantics

    ui/
      mod.rs            re-exports
      fmt.rs            durations, token counts, cost, status glyphs, truncation
      trace.rs          static tree renderer; --events, --raw, --json, --follow
      watch.rs          ratatui live TUI: tree pane / node pane / account footer
      chat.rs           rustyline REPL rendering BrainEvent plus inline worker progress

    cmd/
      mod.rs            one module per subcommand, each a thin `async fn run(cfg, args)`
      run.rs chat.rs trace.rs watch.rs runs.rs accounts.rs doctor.rs
      diff.rs adopt.rs gc.rs replay.rs config.rs mcp_bridge.rs

  tests/
    parse_claude.rs     golden test against docs/ref/claude-stream-sample.jsonl
    parse_codex.rs      golden test against docs/ref/codex-stream-sample.jsonl
    journal_fold.rs     fold determinism, torn tail, retry chains
    dispatch_failover.rs fake adapter returning RateLimited; asserts rotation + cooldown
    e2e_smoke.rs        fake CLI on PATH; full `swamp run --no-brain` path
```

Dependency direction is strictly downward:

```
cmd -> {brain, dispatch, ui, journal, mcp} -> {worker, workspace} -> {model, config, ids, error}
```

`mcp/tools.rs` holds `Arc<Dispatcher>` and a journal reader. It never calls into `brain/`.

---

## 4. Core types

### 4.0 ids.rs

```rust
use serde::{Deserialize, Serialize};
use ulid::Ulid;

macro_rules! ulid_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Ulid);

        impl $name {
            pub fn new() -> Self { Self(Ulid::new()) }
            /// Last 6 chars: stable, typeable, unique enough within a run.
            pub fn short(&self) -> String { self.0.to_string()[20..].to_ascii_lowercase() }
        }
        impl Default for $name { fn default() -> Self { Self::new() } }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}{}", $prefix, self.0)
            }
        }
        impl std::str::FromStr for $name {
            type Err = ulid::DecodeError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(Ulid::from_string(s.trim_start_matches($prefix))?))
            }
        }
    };
}
ulid_id!(RunId, "run_");
ulid_id!(NodeId, "nd_");

/// `claude --session-id` requires a valid UUID, so every node carries a paired UUID
/// alongside its ULID. Both are journaled; neither is derived from the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeIds { pub id: NodeId, pub session_uuid: uuid::Uuid }
```

ULIDs are lexicographically time-sortable, so `ls .swamp/runs` is chronological and a `BTreeMap`
keyed by id is in creation order.

### 4.1 model/core.rs

```rust
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider { Anthropic, Openai }

/// Ordered so `Tier::High > Tier::Low` works for policy checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier { Low, Mid, High }

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind { Root, Brain, Worker }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum NodeState {
    Queued,
    /// Every eligible account is cooling; the scheduler sleeps until `until`.
    Blocked { #[serde(with = "time::serde::rfc3339")] until: OffsetDateTime, why: String },
    Leased { account: AccountId },
    Running { pid: i32, pgid: i32, #[serde(with = "time::serde::rfc3339")] since: OffsetDateTime },
    /// Process outlived a supervisor crash, or vice versa. Recovery decides adopt vs resume.
    Orphaned { pid: i32, stream_offset: u64 },
    Succeeded,
    Failed { failure: Failure },
    Cancelled { by: CancelSource },
}

impl NodeState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed { .. } | Self::Cancelled { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelSource { User, Brain, Budget, Timeout, Shutdown }

/// A resume handle. ALWAYS carried with the account that minted it: a claude session id
/// created under CLAUDE_CONFIG_DIR=A does not exist under B. If this invariant breaks,
/// resume silently starts a fresh conversation while Swamp believes it has context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHandle {
    pub account: AccountId,
    pub id: String,        // claude session_id (uuid) | codex thread_id
    pub preassigned: bool, // true when Swamp chose it before launch
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceRef {
    /// Fresh `git worktree` on a `swamp/<run>/<node>` branch. Default for anything that writes.
    Worktree { path: Utf8PathBuf, branch: String, base: String },
    /// The user's real checkout, serialized behind a repo-wide write mutex. Opt-in.
    Shared { path: Utf8PathBuf },
    /// The checkout with provider-level write denial. Used for the brain and review nodes.
    ReadOnly { path: Utf8PathBuf },
}

impl WorkspaceRef {
    pub fn path(&self) -> &camino::Utf8Path {
        match self { Self::Worktree { path, .. } | Self::Shared { path } | Self::ReadOnly { path } => path }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)] pub input_tokens: u64,
    #[serde(default)] pub cached_input_tokens: u64,
    #[serde(default)] pub cache_write_tokens: u64,
    #[serde(default)] pub output_tokens: u64,
    #[serde(default)] pub reasoning_tokens: u64,
}

impl Usage {
    pub fn absorb(&mut self, o: &Usage) {
        self.input_tokens += o.input_tokens;
        self.cached_input_tokens += o.cached_input_tokens;
        self.cache_write_tokens += o.cache_write_tokens;
        self.output_tokens += o.output_tokens;
        self.reasoning_tokens += o.reasoning_tokens;
    }
    pub fn billable(&self) -> u64 { self.input_tokens + self.cache_write_tokens + self.output_tokens }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Cost { pub usd: f64, pub basis: CostBasis }

/// `Reported` = the CLI told us (claude `total_cost_usd`). Note that on a subscription this is
/// list-price equivalence, not money billed - the sample shows `costBasis: "list"`. The UI
/// therefore renders reported cost with a leading `~` too.
/// `Estimated` = we multiplied tokens by the `[pricing]` table because the CLI reports none.
/// Absent cost renders as `-`, never `$0.00`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostBasis { Reported, Estimated }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind { Add, Modify, Delete, Rename }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: Utf8PathBuf,
    pub kind: ChangeKind,
    #[serde(default)] pub added: u32,
    #[serde(default)] pub removed: u32,
    /// Git is authoritative. EventStream is a live-progress estimate and may be wrong.
    pub source: EvidenceSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource { Git, EventStream }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitScope { FiveHour, SevenDay, Minute, Unknown }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitStatus { Allowed, Warning, Rejected }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LimitWindow {
    pub scope: LimitScope,
    /// 0.0 ..= 1.0
    pub utilization: f64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub resets_at: Option<OffsetDateTime>,
}

/// The real sample carries five_hour = 0.06 AND seven_day = 0.64 in the same event.
/// Collapsing to one window throws away the one that is actually near exhaustion,
/// so every window is kept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitSnapshot {
    pub status: LimitStatus,
    pub windows: Vec<LimitWindow>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub resets_at: Option<OffsetDateTime>,
}

impl RateLimitSnapshot {
    pub fn worst_utilization(&self) -> f64 {
        self.windows.iter().map(|w| w.utilization).fold(0.0, f64::max)
    }
    pub fn soonest_reset(&self) -> Option<OffsetDateTime> {
        self.windows.iter().filter_map(|w| w.resets_at).min().or(self.resets_at)
    }
    pub fn worst_scope(&self) -> LimitScope {
        self.windows.iter()
            .max_by(|a, b| a.utilization.total_cmp(&b.utilization))
            .map_or(LimitScope::Unknown, |w| w.scope)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FinalSummary {
    pub ok: bool,
    /// Raw provider terminal marker: claude `result.subtype`, codex `turn.completed|failed`.
    pub subtype: String,
    pub text: Option<String>,
    pub usage: Usage,
    pub cost: Option<Cost>,
    pub api_error_status: Option<i64>,
    #[serde(default)] pub num_turns: u32,
    /// Under `--permission-prompts none` anything that would prompt is denied. A worker then
    /// writes a confident summary of work it never did. Non-empty is a failure signal.
    #[serde(default)] pub permission_denials: u32,
}
```

### 4.2 model/failure.rs

The single most important policy surface in Swamp.

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Failure {
    /// Provider quota exhausted for this account. Cool it, try the next one.
    RateLimited {
        #[serde(with = "time::serde::rfc3339::option")] resets_at: Option<OffsetDateTime>,
        scope: LimitScope,
        detected_by: Detector,
        /// The literal matching line, truncated. Makes a misclassification diagnosable.
        evidence: String,
    },
    /// Credentials for this account are dead. Out of rotation until a human fixes it.
    AuthExpired { detail: String, detected_by: Detector },
    /// Transient upstream capacity problem (429-adjacent 529/503). Back off on the SAME account.
    Overloaded { detail: String },
    /// Our own guard tripped. Do NOT fail over: another account would spend too.
    BudgetExceeded { limit_usd: f64, spent_usd: f64 },
    Timeout { after_s: u64 },
    /// The task itself failed. NEVER rotate: a bad prompt would burn every subscription.
    WorkerError { subtype: String, detail: String },
    /// Tools were auto-denied because nobody could answer a prompt.
    PermissionDenied { denials: u32 },
    /// Process died abnormally (signal, OOM, supervisor kill).
    Crashed { signal: Option<i32> },
    /// The stream ended with no terminal event and the pid is gone.
    Truncated { offset: u64 },
    /// No usable account at all.
    NoCapacity { detail: String },
}

/// Which layer of the classifier fired. Journaled so `swamp doctor --schema` can report
/// "12 of the last 40 rate-limit detections used the regex fallback", which is the early
/// warning that a CLI changed its wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Detector { Telemetry, StructuredResult, Pattern, ExitCode }

impl Failure {
    /// Burn a different subscription on the same work.
    pub fn rotates_account(&self) -> bool {
        matches!(self, Self::RateLimited { .. } | Self::AuthExpired { .. })
    }
    /// Retry here with backoff, resuming the same session so the retry does not repay context.
    pub fn retries_same_account(&self) -> bool {
        matches!(self, Self::Overloaded { .. } | Self::Crashed { .. } | Self::Truncated { .. })
    }
    /// Stop. The answer is the answer.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::WorkerError { .. } | Self::BudgetExceeded { .. }
            | Self::Timeout { .. } | Self::PermissionDenied { .. } | Self::NoCapacity { .. })
    }
}
```

### 4.3 model/node.rs

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeRecord {
    pub id: NodeId,
    pub run_id: RunId,
    pub parent: Option<NodeId>,
    /// Stable across retries: attempt 2 is a new NodeId but the same `logical`.
    /// The tree collapses attempts into one row with an attempt chain.
    pub logical: NodeId,
    pub attempt: u32,
    /// Set when this node is a failover retry of a sibling.
    pub retry_of: Option<NodeId>,
    pub kind: NodeKind,
    pub title: String,

    /// The prompt is on disk, not inline: it can be large and it is the exact bytes fed to fd0.
    pub prompt_path: Utf8PathBuf,
    pub prompt_sha256: String,

    // Provenance: which subscription actually did the work.
    pub provider: Provider,
    pub account: Option<AccountId>,
    pub exec: Option<String>,
    pub argv: Vec<String>,
    pub model: Option<String>,
    pub tier: Tier,

    pub workspace: WorkspaceRef,
    pub session: Option<SessionHandle>,
    pub state: NodeState,

    #[serde(with = "time::serde::rfc3339")] pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")] pub started_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")] pub ended_at: Option<OffsetDateTime>,

    pub usage: Usage,
    pub cost: Option<Cost>,
    pub exit: Option<ExitInfo>,
    pub files: Vec<FileChange>,
    pub work: Option<WorkResultRef>,
    pub summary: Option<String>,
    /// Byte offset consumed so far in nodes/<id>/stream.jsonl. Restart resumes exactly here.
    pub stream_offset: u64,
    pub unparsed_lines: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitInfo { pub code: Option<i32>, pub signal: Option<i32>, pub duration_ms: u64 }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkResultRef {
    pub head: String,
    pub branch: String,
    pub patch: Utf8PathBuf,
    pub insertions: u32,
    pub deletions: u32,
    pub empty: bool,
}
```

### 4.4 model/event.rs

```rust
use serde_json::value::RawValue;

/// Provider-neutral. Every adapter normalizes into this; the journal, the TUI and the brain
/// only ever see this shape. Raw provider lines live untouched in nodes/<id>/stream.jsonl.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "e", rename_all = "snake_case")]
pub enum WorkerEvent {
    SessionStarted { session: String, model: Option<String>, auth_hint: Option<String> },
    AssistantText { text: String },
    Thinking { text: String },
    ToolCall { id: String, name: String, summary: String },
    ToolResult { id: String, ok: bool, summary: String },
    FileChanged { path: Utf8PathBuf, kind: ChangeKind },
    Usage(Usage),
    RateLimit(RateLimitSnapshot),
    Final(FinalSummary),
    /// Never dropped and never fatal: a CLI schema change degrades instead of breaking.
    Unknown { raw: Box<RawValue> },
}
```

### 4.5 model/result.rs - the brain-facing contract

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct TaskRequest {
    pub title: String,
    pub prompt: String,
    #[serde(default)] pub tier: Option<Tier>,
    #[serde(default)] pub provider: Option<Provider>,
    #[serde(default)] pub isolation: Option<IsolationMode>,
    #[serde(default)] pub account: Option<AccountId>,
    /// Parsed and rejected in v1 with a clear message. The seam for DAG dispatch.
    #[serde(default)] pub deps: Vec<NodeId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum IsolationMode { Worktree, Shared, ReadOnly }

#[derive(Debug, Clone, Serialize)]
pub struct NodeResult {
    pub node: NodeId,
    pub title: String,
    pub ok: bool,
    pub state: &'static str,        // "succeeded" | "failed" | "running" | "cancelled"
    pub tier: Tier,
    pub provider: Provider,
    pub account: Option<AccountId>,
    pub model: Option<String>,
    pub attempts: u32,
    /// Truncated to limits.max_result_bytes and wrapped by the MCP layer in an explicit
    /// untrusted-content envelope. Worker output is data, never instruction.
    pub summary: Option<String>,
    pub files: Vec<FileChange>,
    pub branch: Option<String>,
    pub patch: Option<Utf8PathBuf>,
    pub insertions: u32,
    pub deletions: u32,
    pub usage: Usage,
    pub cost: Option<Cost>,
    pub duration_ms: u64,
    pub failure: Option<Failure>,
    pub permission_denials: u32,
}
```

### 4.6 error.rs

```rust
#[derive(Debug, thiserror::Error)]
pub enum SwampError {
    #[error("no {provider:?} account available: {excluded} excluded by failover, {cooling} cooling down")]
    NoAccountAvailable { provider: Provider, excluded: usize, cooling: usize },
    #[error("no model configured for {provider:?} tier {tier:?}; set providers.<p>.models.<t> in swamp.toml")]
    TierUnmapped { provider: Provider, tier: Tier },
    #[error("executable `{exec}` for account `{id}` not found in PATH")]
    ExecNotFound { id: String, exec: String },
    #[error("all {attempts} attempts exhausted for task `{title}`")]
    ExhaustedAttempts { title: String, attempts: u32 },
    #[error("not a git repository: {0} (worktree isolation requires git; use isolation = \"shared\")")]
    NotAGitRepo(camino::Utf8PathBuf),
    #[error("refusing to run: working tree is dirty. Commit, stash, or pass --include-dirty")]
    DirtyTree,
    #[error("config invalid:\n{0}")]
    ConfigInvalid(String),
    #[error("swamp is already running for this repo (pid {pid})")]
    AlreadyRunning { pid: i32 },
    #[error(transparent)] Io(#[from] std::io::Error),
    #[error(transparent)] Json(#[from] serde_json::Error),
}

/// Scripts branch on these. 3 vs 4 is "try again in an hour" vs "your task is broken".
pub fn exit_code(e: &anyhow::Error) -> i32 {
    match e.downcast_ref::<SwampError>() {
        Some(SwampError::ConfigInvalid(_)) => 2,
        Some(SwampError::NoAccountAvailable { .. }) => 3,
        Some(SwampError::ExhaustedAttempts { .. }) => 4,
        _ => 1,
    }
}
// 5 = merge conflict, 6 = cancelled, 7 = budget exceeded: set by the cmd layer.
```

---

## 5. Worker protocol

### 5.1 Launch model: zero live pipes

Workers are launched fully detached. All three stdio fds are ordinary files, never pipes:

```
fd0 <- .swamp/runs/<run>/nodes/<node>/prompt.md      O_RDONLY
fd1 -> .swamp/runs/<run>/nodes/<node>/stream.jsonl   O_WRONLY|O_APPEND|O_CREAT
fd2 -> .swamp/runs/<run>/nodes/<node>/stderr.log     O_WRONLY|O_APPEND|O_CREAT
```

The child gets its own process group. Swamp then *tails* `stream.jsonl` instead of reading a pipe.
Five consequences, all of which we want:

1. A supervisor crash or `kill -9 swamp` leaves workers running and loses nothing.
2. Restart re-opens `stream.jsonl`, seeks to the journaled `stream_offset`, and keeps folding.
   Nothing lost, nothing double-counted.
3. The classic two-pipe deadlock (drain stdout to EOF before touching stderr) disappears
   structurally rather than by careful coding.
4. Prompt-via-file sidesteps `ARG_MAX` entirely. macOS caps argv near 1 MB and prompts with pasted
   context exceed it. Neither adapter ever puts the prompt on argv.
5. "Raw bytes are persisted before they are parsed" is free rather than an invariant the pump has
   to maintain. The raw file is the primary artifact; the journal is derived from it.

Cancellation and timeout are `killpg(pgid, SIGTERM)`, grace period, then `SIGKILL`, which also reaps
the worker's own shell grandchildren.

```rust
// worker/spawn.rs
pub struct Detached { pub pid: i32, pub pgid: i32, pub started_at: OffsetDateTime }

pub struct NodeIo {
    pub node: NodeId,
    pub prompt: Utf8PathBuf,
    pub stdout: Utf8PathBuf,
    pub stderr: Utf8PathBuf,
    pub pidfile: Utf8PathBuf,
    pub depth: u32,
}

pub fn spawn_detached(
    argv: &[std::ffi::OsString],
    env: &[(std::ffi::OsString, std::ffi::OsString)],
    cwd: &camino::Utf8Path,
    io: &NodeIo,
) -> anyhow::Result<Detached> {
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .envs(env.iter().cloned())
        .env("SWAMP_DEPTH", (io.depth + 1).to_string())  // recursion guard
        .env("SWAMP_NODE", io.node.to_string())
        .stdin(std::fs::File::open(&io.prompt)?)
        .stdout(open_append(&io.stdout)?)
        .stderr(open_append(&io.stderr)?)
        .process_group(0)      // own pgid; survives our death
        .kill_on_drop(false);
    let child = cmd.spawn()?;
    let pid = child.id().ok_or_else(|| anyhow::anyhow!("child exited before id"))? as i32;
    write_pidfile(&io.pidfile, pid)?;   // pid + process start time
    Ok(Detached { pid, pgid: pid, started_at: OffsetDateTime::now_utc() })
}
```

`liveness.rs` pairs the pid with its process start time (`sysinfo` on macOS, `/proc/<pid>/stat`
field 22 on Linux), so recovery never mistakes a recycled pid for our worker.

### 5.2 Adapter trait

```rust
// worker/adapter.rs
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub node: NodeIds,
    pub provider: Provider,
    pub exec: String,
    pub env: BTreeMap<String, String>,
    pub model: String,
    pub tier: Tier,
    pub cwd: Utf8PathBuf,
    pub isolation: IsolationMode,
    pub session: SessionPlan,
    pub kind: NodeKind,                   // Worker or Brain; changes the argv
    pub permission_mode: String,          // from config, e.g. "acceptEdits"
    pub sandbox: String,                  // codex, e.g. "workspace-write"
    pub budget_usd: Option<f64>,
    pub append_system_prompt: Option<String>,
    pub allow_tools: Vec<String>,
    pub deny_tools: Vec<String>,
    pub mcp: Option<McpAttach>,
    pub last_message_path: Utf8PathBuf,
    pub extra_args: Vec<String>,
    pub attempt: u32,
}

#[derive(Debug, Clone)]
pub enum SessionPlan {
    New { preassigned: Option<String> },
    Resume(SessionHandle),
}

#[derive(Debug, Clone)]
pub struct McpAttach { pub command: Utf8PathBuf, pub args: Vec<String> }

#[derive(Debug, Default)]
pub struct ParseState {
    pub session: Option<String>,
    pub model: Option<String>,
    pub usage: Usage,
    pub last_rate_limit: Option<RateLimitSnapshot>,
    pub last_final: Option<FinalSummary>,
    pub tool_names: HashMap<String, String>,   // tool_use_id -> name, to label results
    pub files: Vec<FileChange>,
    pub stderr_tail: VecDeque<String>,         // last 64 lines
    pub unparsed: u32,
}

/// One input line can fan out to several normalized events (a claude assistant message
/// routinely carries a text block AND a tool_use block). Returning Option would drop the rest.
pub struct ParseOutput { pub events: SmallVec<[WorkerEvent; 4]>, pub noise: bool }

pub struct ExitContext<'a> {
    pub exit: Option<ExitInfo>,
    pub state: &'a ParseState,
    pub patterns: &'a FailurePatterns,   // compiled RegexSets from config, field-upgradeable
    pub deadline_hit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    Resume, PreassignedSession, McpStdio, NativeBudget,
    StreamingStdin, ReportedCost, QuotaTelemetry, ToolPolicyFlags,
}

/// Deliberately sync and object-safe: an adapter is a pure argv builder plus a line parser
/// plus a classifier. All async lives in `spawn.rs` / `follow.rs`, which makes adapters
/// trivially testable against the recorded sample streams with no tokio runtime.
///
/// Adding a provider = implement this + a config block. Nothing in journal/, dispatch/,
/// workspace/, mcp/ or ui/ changes.
pub trait ProviderAdapter: Send + Sync + 'static {
    fn provider(&self) -> Provider;
    fn supports(&self, cap: Capability) -> bool;
    fn build_argv(&self, spec: &LaunchSpec) -> anyhow::Result<Vec<OsString>>;
    fn env(&self, spec: &LaunchSpec) -> Vec<(OsString, OsString)>;
    fn parse_line(&self, line: &str, st: &mut ParseState) -> ParseOutput;
    fn classify(&self, cx: &ExitContext<'_>) -> Option<Failure>;   // None == success
    fn brain_transport(&self) -> BrainTransport;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrainTransport { Persistent, ResumePerTurn }
```

### 5.3 Anthropic argv

Worker:

```
<exec> -p
  --output-format stream-json
  --verbose                        # required alongside stream-json in print mode
  --model <models[tier]>           # from config; never hardcoded
  --session-id <uuid>              # pre-generated, journaled BEFORE spawn -> resume always possible
  [--permission-mode acceptEdits]  # from config, and ONLY when it sets one: the flag has a
                                   # closed choice list, so an empty argument is an argv error
                                   # that kills the CLI before it emits one stream line.
                                   # Measured under --permission-prompts none: "auto" DENIES
                                   # file writes, so the worker produces no diff at all;
                                   # acceptEdits/plan/manual/dontAsk write files but deny every
                                   # Bash call that is not allowed by name. acceptEdits plus
                                   # Bash in worker.allow_tools is the recommendation.
                                   # Never bypassPermissions unless opted in.
  --permission-prompts none        # nobody is at the keyboard; prompts are denied, not hung
  --strict-mcp-config              # with no --mcp-config: workers get exactly zero MCP servers
  [--max-budget-usd <n>]           # when a per-node budget is set
  --append-system-prompt <text>    # the worker role (worker/prompt.rs) plus, if set,
                                   # providers.<p>.worker.system_prompt_file, read as TEXT
  [--allowed-tools ...providers.<p>.worker.allow_tools]
  [--disallowed-tools Edit Write MultiEdit NotebookEdit   # IsolationMode::ReadOnly
                      ...providers.<p>.worker.deny_tools]
  [--resume <session-id>]          # same-account retry, keeps the cache warm
  [--effort <level>]               # optional per-tier knob from providers.*.tier_extra
  [...providers.anthropic.worker.args]
```

No prompt argument. `--input-format` defaults to `text` and `-p` reads stdin, which is our
prompt file. The worker role prompt is not optional: without it a worker inherits the operator's
own `CLAUDE.md`, and a worker told by it to orchestrate will spawn subagents, ask questions
nobody can answer, and return boilerplate instead of a report. `worker.allow_tools` and
`worker.deny_tools` are merged into one flag each exactly as the brain's lists are, so a raw
`--allowedTools` in `worker.args` is never the right way to spell them. cwd is the worktree. Env is `account.env` merged over the inherited env (typically
`CLAUDE_CONFIG_DIR`); Swamp never reads or writes a credential file.

Brain adds, and removes `--strict-mcp-config`-without-config:

```
  --input-format stream-json
  --mcp-config '<inline json>'  --strict-mcp-config
  --allowed-tools mcp__swamp__swamp_dispatch ... mcp__swamp__swamp_note [...brain.allow_tools]
  --include-partial-messages     # smooth streaming in the chat UI; off for workers
```

The allow list is generated from the tool registry in `mcp/tools.rs`, never written out by
hand: the brain runs with `--permission-prompts none`, so a tool the list forgets is denied
automatically and the brain loses the only way it has to do anything. `--allowed-tools` and
`--disallowed-tools` are variadic, so a repeat overwrites rather than accumulates: the
read-only denials and `brain.deny_tools` are merged into one flag, and so are the MCP names
and `brain.allow_tools`. A read-only brain therefore still calls every Swamp tool while Edit,
Write, MultiEdit and NotebookEdit stay denied.

`--include-partial-messages` is deliberately off for workers: roughly 10x the raw volume for no
benefit, since nobody reads a worker's stream token by token. The brain gets it only when
`brain.include_partial_messages` is set, and the `stream_event` lines it produces are parsed to
nothing: they are partial chunks, not unparsable noise, and journaling one event per chunk would
inflate a run's journal tenfold for deltas no renderer consumes.

### 5.4 OpenAI argv

Worker:

```
<exec> exec
  --json
  -m <models[tier]>
  -C <worktree>
  [-s workspace-write]                     # "read-only" for IsolationMode::ReadOnly; omitted
                                           # entirely when the config names no sandbox, since
                                           # `-s ''` is a clap error and exits 2
  -o <node_dir>/last-message.txt           # the vendor's own "final answer", parse-independent
  -c approval_policy="never"
  [-c model_reasoning_effort="high"]       # from providers.openai.tier_extra
  [--skip-git-repo-check]                  # only when the cwd is not a git repo
  [--output-schema <file>]
  [...providers.openai.worker.args]
  -                                        # read the prompt from stdin (our prompt file)
```

**`codex exec` has no `-a/--ask-for-approval`.** That flag exists only on the top-level `codex`
command. Approval policy on `exec` must go through `-c approval_policy="never"`. Using `-a` here
makes every OpenAI worker die at argv parsing.

`codex exec` has no `--append-system-prompt`, so the worker role rides at the head of the
prompt file instead, ahead of the task and separated from it by a rule. `Capability::SystemPromptFlag`
is what the attempt loop asks, so a provider that grows the flag later moves without a code change
elsewhere.

Resume: `<exec> exec resume <thread_id> --json ... -`.

Brain adds:

```
  -c mcp_servers.swamp.command="<abs path to swamp>"
  -c 'mcp_servers.swamp.args=["mcp-bridge","--socket","<~/.swamp/sock/<run_short>.sock>"]'
```

`-c` takes a dotted TOML path, and `codex mcp` manages "external MCP servers for Codex", so
`mcp_servers.*` is the right key. No allow list is needed on this side: `approval_policy`
gates "when the model requires human approval before executing a command", and an MCP tool
call is not a command, so `approval_policy="never"` leaves the swamp tools callable. `doctor --probe` verifies this end to end rather than assuming it
(section 9).

`--dangerously-skip-permissions` (claude) and `--dangerously-bypass-approvals-and-sandbox` (codex)
are refused unless `limits.unsafe_ack = true`, print a warning on every run, and are flagged by
`doctor`.

### 5.5 Stream parsing

Follow loop, shared by both providers:

```rust
// worker/follow.rs
pub async fn follow(
    node: NodeId,
    path: &Utf8Path,
    mut offset: u64,
    adapter: Arc<dyn ProviderAdapter>,
    st: &mut ParseState,
    sink: &mut RawSink,
    out: mpsc::Sender<(NodeId, WorkerEvent, u64)>,
    alive: impl Fn() -> bool,
) -> anyhow::Result<()> {
    let mut f = tokio::fs::File::open(path).await?;
    f.seek(SeekFrom::Start(offset)).await?;
    let mut rdr = BufReader::new(f);
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = rdr.read_line(&mut buf).await?;
        if n == 0 {
            if !alive() { break; }
            tokio::time::sleep(POLL).await;   // 150ms
            continue;
        }
        // A partial trailing write is held back until its newline arrives.
        if !buf.ends_with('\n') { rdr.seek(SeekFrom::Start(offset)).await?; tokio::time::sleep(POLL).await; continue; }
        offset += n as u64;
        // Cap: a 40 MB base64 blob on one line must degrade, not OOM.
        let line = truncate_line(buf.trim_end(), MAX_LINE);
        let po = adapter.parse_line(line, st);
        if po.noise { st.unparsed += 1; sink.noise(line).await; }
        for ev in po.events { out.send((node, ev, offset)).await?; }
    }
    Ok(())
}
```

`offset` travels with every event, so the journal always holds a consistent (event, byte position)
pair. That is exactly what makes restart resumption exact rather than approximate.

#### Claude wire types (matched against `docs/ref/claude-stream-sample.jsonl`)

```rust
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClaudeLine {
    System(SystemLine),
    Assistant(MsgEnvelope),
    User(MsgEnvelope),
    RateLimitEvent { rate_limit_info: RateLimitInfo },
    Result(Box<ResultLine>),
    #[serde(other)] Other,
}

#[derive(Deserialize)]
struct SystemLine {
    subtype: String,
    #[serde(default)] session_id: Option<String>,
    #[serde(default)] model: Option<String>,
    /// "none" proves subscription auth. doctor asserts it.
    #[serde(default, rename = "apiKeySource")] api_key_source: Option<String>,
}

#[derive(Deserialize)]
struct RateLimitInfo {
    status: String,                                            // "allowed" | ...
    #[serde(default, rename = "resetsAt")] resets_at: Option<i64>,
    #[serde(default, rename = "rateLimitType")] kind: Option<String>,
    #[serde(default, rename = "unifiedWindows")] windows: BTreeMap<String, Window>,
}
#[derive(Deserialize, Clone, Copy)]
struct Window { utilization: f64, #[serde(rename = "resetsAt")] resets_at: i64 }

#[derive(Deserialize)]
struct MsgEnvelope { message: Msg, #[serde(default)] parent_tool_use_id: Option<String> }
#[derive(Deserialize)]
struct Msg {
    #[serde(default)] model: Option<String>,
    #[serde(default)] content: Vec<Block>,
    #[serde(default)] usage: Option<ClaudeUsage>,
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block {
    Text { text: String },
    Thinking { #[serde(default)] thinking: String },
    ToolUse { id: String, name: String, input: serde_json::Value },
    ToolResult { tool_use_id: String, #[serde(default)] is_error: bool },
    #[serde(other)] Other,
}

#[derive(Deserialize, Default)]
struct ClaudeUsage {
    #[serde(default)] input_tokens: u64,
    #[serde(default)] cache_read_input_tokens: u64,
    #[serde(default)] cache_creation_input_tokens: u64,
    #[serde(default)] output_tokens: u64,
    #[serde(default)] output_tokens_details: Option<OutDetails>,
}
#[derive(Deserialize, Default)]
struct OutDetails { #[serde(default)] thinking_tokens: u64 }

#[derive(Deserialize)]
struct ResultLine {
    subtype: String,   // success | error_during_execution | error_max_turns | error_max_budget_usd
    #[serde(default)] is_error: bool,
    #[serde(default)] result: Option<String>,
    #[serde(default)] total_cost_usd: Option<f64>,
    #[serde(default)] usage: ClaudeUsage,
    #[serde(default)] api_error_status: Option<i64>,
    #[serde(default)] num_turns: u32,
    #[serde(default)] session_id: Option<String>,
    #[serde(default)] permission_denials: Vec<serde_json::Value>,
    #[serde(default)] duration_ms: u64,
}
```

Mapping:

| line | normalized |
|---|---|
| `system` / `init` | `SessionStarted { session, model, auth_hint: apiKeySource }` |
| `rate_limit_event` | `RateLimit(snapshot)` from every entry in `unifiedWindows`, plus top-level `status` |
| `assistant` blocks | one event per block: `AssistantText` / `Thinking` / `ToolCall`, plus `Usage` |
| `user` tool_result blocks | `ToolResult { ok: !is_error }`, name looked up in `tool_names` |
| `result` | `Final` with `Cost { basis: Reported }` from `total_cost_usd`, plus `permission_denials.len()` |

`Edit` / `Write` / `MultiEdit` / `NotebookEdit` tool inputs yield `FileChanged` with
`EvidenceSource::EventStream`. Advisory only; git is authoritative at finalize.

`total_cost_usd` is present on subscription runs, but `modelUsage[*].costBasis` is `"list"`, so it
is list-price equivalence and not money billed. The UI prefixes it with `~`.

#### Codex wire types (matched against `docs/ref/codex-stream-sample.jsonl`)

The real sample's first stdout line is **not JSON**: `Reading additional input from stdin...`.
Any line not starting with `{` returns `noise: true`, is appended to `noise.log`, and never fails
the node. Expect more of this: the stream is a human-oriented channel that happens to carry JSON.

```rust
#[derive(Deserialize)]
#[serde(tag = "type")]
enum CodexLine {
    #[serde(rename = "thread.started")]  ThreadStarted { thread_id: String },
    #[serde(rename = "turn.started")]    TurnStarted {},
    #[serde(rename = "turn.completed")]  TurnCompleted { usage: CodexUsage },
    #[serde(rename = "turn.failed")]     TurnFailed { error: CodexError },
    #[serde(rename = "item.started")]    ItemStarted { item: CodexItem },
    #[serde(rename = "item.updated")]    ItemUpdated { item: CodexItem },
    #[serde(rename = "item.completed")]  ItemCompleted { item: CodexItem },
    #[serde(rename = "error")]           Error { message: String },
    #[serde(other)]                      Other,
}

#[derive(Deserialize, Default, Clone, Copy)]
struct CodexUsage {
    #[serde(default)] input_tokens: u64,
    #[serde(default)] cached_input_tokens: u64,
    #[serde(default)] cache_write_input_tokens: u64,
    #[serde(default)] output_tokens: u64,
    #[serde(default)] reasoning_output_tokens: u64,
}

#[derive(Deserialize)]
struct CodexError { #[serde(default)] code: Option<String>, message: String }

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CodexItem {
    AgentMessage { id: String, #[serde(default)] text: String },
    Reasoning { id: String, #[serde(default)] text: String },
    CommandExecution { id: String, #[serde(default)] command: String,
                       #[serde(default)] exit_code: Option<i32>,
                       #[serde(default)] status: Option<String> },
    FileChange { id: String, #[serde(default)] changes: Vec<CodexChange> },
    McpToolCall { id: String, #[serde(default)] server: String, #[serde(default)] tool: String },
    WebSearch { id: String, #[serde(default)] query: String },
    TodoList { id: String },
    #[serde(other)] Other,
}
#[derive(Deserialize)]
struct CodexChange { path: String, kind: String }   // add | delete | update
```

`thread.started` yields the `SessionHandle` and is journaled the instant it arrives, so the window
in which a codex session is unrecoverable is one line of output wide. `turn.completed.usage` maps
`reasoning_output_tokens` to `Usage.reasoning_tokens`. Codex reports **no cost** and **no quota
telemetry**: `Cost` is `None` unless a `[pricing]` row exists, and then it is stamped
`CostBasis::Estimated`. Swamp never presents an estimate as a measurement.

Only `item.completed` produces durable events; `item.started` / `item.updated` produce live progress
events that are journaled but folded idempotently by item id.

### 5.6 Exit handling and failure classification

Layered, cheapest and most reliable first. Every classification journals its `Detector` and its
evidence string.

```rust
// worker/classify.rs
pub fn classify(cx: &ExitContext<'_>) -> Option<Failure> {
    // Layer 1: structured telemetry. Free, and fires before any error text exists.
    if let Some(rl) = &cx.state.last_rate_limit {
        if rl.status == LimitStatus::Rejected {
            return Some(Failure::RateLimited {
                resets_at: rl.soonest_reset(), scope: rl.worst_scope(),
                detected_by: Detector::Telemetry, evidence: "rate_limit_event status".into(),
            });
        }
    }
    // Layer 2: the terminal result event.
    if let Some(f) = &cx.state.last_final {
        if f.api_error_status == Some(429) {
            return Some(Failure::RateLimited { resets_at: None, scope: LimitScope::Unknown,
                detected_by: Detector::StructuredResult, evidence: "api_error_status 429".into() });
        }
        if matches!(f.api_error_status, Some(529) | Some(503)) {
            return Some(Failure::Overloaded { detail: format!("{:?}", f.api_error_status) });
        }
        match f.subtype.as_str() {
            "error_max_budget_usd" => return Some(Failure::BudgetExceeded {
                limit_usd: 0.0, spent_usd: f.cost.map_or(0.0, |c| c.usd) }),
            "success" if f.ok && f.permission_denials == 0 => return None,
            _ => {}
        }
        // A writing worker that "succeeded" with denials did not do the work it claims.
        if f.permission_denials > 0 {
            return Some(Failure::PermissionDenied { denials: f.permission_denials });
        }
        let t = f.text.as_deref().unwrap_or("");
        if let Some(m) = cx.patterns.rate_limit_match(t) {
            return Some(Failure::RateLimited { resets_at: None, scope: LimitScope::Unknown,
                detected_by: Detector::Pattern, evidence: truncate(m, 400) });
        }
        if let Some(m) = cx.patterns.auth_match(t) {
            return Some(Failure::AuthExpired { detail: truncate(m, 200), detected_by: Detector::Pattern });
        }
        if cx.patterns.overloaded_match(t).is_some() {
            return Some(Failure::Overloaded { detail: truncate(t, 200) });
        }
        return Some(Failure::WorkerError { subtype: f.subtype.clone(), detail: truncate(t, 400) });
    }
    // Layer 3: no terminal event at all. stderr tail, then exit status.
    if cx.deadline_hit { return Some(Failure::Timeout { after_s: 0 }); }
    let tail = cx.state.stderr_tail.iter().cloned().collect::<Vec<_>>().join("\n");
    if let Some(m) = cx.patterns.rate_limit_match(&tail) {
        return Some(Failure::RateLimited { resets_at: None, scope: LimitScope::Unknown,
            detected_by: Detector::Pattern, evidence: truncate(m, 400) });
    }
    if let Some(m) = cx.patterns.auth_match(&tail) {
        return Some(Failure::AuthExpired { detail: truncate(m, 200), detected_by: Detector::Pattern });
    }
    match cx.exit {
        Some(ExitInfo { signal: Some(s), .. }) => Some(Failure::Crashed { signal: Some(s) }),
        Some(ExitInfo { code: Some(0), .. }) => Some(Failure::Truncated { offset: 0 }),
        Some(ExitInfo { code: Some(127), .. }) => Some(Failure::AuthExpired {
            detail: "exec not found (127)".into(), detected_by: Detector::ExitCode }),
        _ => Some(Failure::Crashed { signal: None }),
    }
}
```

Proactive quota stop is separate from failure: when a live `RateLimit` event reports
`worst_utilization() >= quota_stop_at`, the pool cools the account immediately so no *new* node is
routed to it, while running nodes finish normally. Avoiding the failure is much better than
recovering from it, and the telemetry is free.

Default pattern sets live in config (`[failure.anthropic]`, `[failure.openai]`) so a vendor wording
change can be patched in the field without shipping a new binary.

### 5.7 Results back to the caller

Git is the ground truth for "files touched", not the event stream. After the process exits,
`workspace::diff::collect(&worktree, base)` runs `git diff --numstat` and `--name-status`, commits
to `swamp/<run_short>/<node_short>` when `commit_on_success`, and writes `patches/<node>.patch`.
This makes the result identical across providers and correct even when a worker edits files through
a shell heredoc. Event-stream `FileChanged` entries are kept as a live-progress signal and are
replaced at finalize by the git-sourced list, in the journal AND in the `NodeResult` the brain
reads: a worker that announced no edit at all still has a patch, and `swamp_result` must not
report an empty file list for it.

The attempt loop writes that `NodeResult` to `nodes/<attempt>/result.json` as the node finishes,
so the node directory is self-describing even if the journal is lost or the supervisor dies
between the finish and the next fold.

---

## 6. Dispatch and accounts

### 6.1 Model

An account is a name plus an executable plus an optional non-secret env overlay.

```rust
#[derive(Debug, Clone)]
pub struct Account {
    pub id: AccountId,
    pub provider: Provider,
    pub exec: String,                    // claude-main, codex-alt, ...
    pub env: BTreeMap<String, String>,   // e.g. CLAUDE_CONFIG_DIR; never a credential
    pub weight: u32,
    pub max_concurrency: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountState {
    pub inflight: usize,
    pub health: Health,
    #[serde(with = "time::serde::rfc3339::option")] pub cooldown_until: Option<OffsetDateTime>,
    pub consecutive_infra_failures: u32,
    pub quota: Option<RateLimitSnapshot>,
    #[serde(with = "time::serde::rfc3339::option")] pub last_used: Option<OffsetDateTime>,
    pub lifetime_nodes: u64,
    pub lifetime_cost_usd: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    #[default] Healthy,
    Degraded,    // utilization past quota_warn_at: deprioritize, still usable
    Cooling,     // cooldown_until in the future
    AuthBroken,  // out of rotation until a human fixes it
    Disabled,    // config, or `swamp accounts disable`
}
```

Swamp resolves `exec` through PATH, records its absolute path, and execs it. It never reads, writes,
sets or forwards a credential, and it never sets `CLAUDE_CONFIG_DIR` / `CODEX_HOME` /
`ANTHROPIC_API_KEY` / `OPENAI_API_KEY` itself unless the user put it in `accounts[].env`. The
wrapper owns auth:

```sh
#!/bin/sh
exec env CLAUDE_CONFIG_DIR="$HOME/.claude-alt" /opt/homebrew/bin/claude "$@"
```

**Account state is cross-run and cross-repo**: `~/.swamp/accounts.json`, guarded by an `fs4`
advisory lock with temp-write-and-rename. Rate limits are per account, not per repo, so a cooldown
learned in project A must be honoured in project B, and must survive a restart. In-memory-only state
walks straight back into a limited account after every restart.

### 6.2 Tier mapping

Pure config lookup, loud on miss. No model id appears anywhere in the source.

```rust
impl Config {
    pub fn model_for(&self, p: Provider, t: Tier, account: Option<&AccountId>) -> Result<String, SwampError> {
        // Per-account override first: a plan without Opus can map high -> sonnet.
        if let Some(a) = account.and_then(|a| self.account(a)) {
            if let Some(m) = a.models.get(&t) { return Ok(m.clone()); }
        }
        self.providers.get(&p).and_then(|pc| pc.models.get(&t)).cloned()
            .ok_or(SwampError::TierUnmapped { provider: p, tier: t })
    }
    /// Extra per-tier flags: `--effort` (claude) or `-c model_reasoning_effort=` (codex).
    /// A tier encodes which model AND how hard it thinks.
    pub fn tier_extra(&self, p: Provider, t: Tier) -> BTreeMap<String, String>;
}
```

`ModelResolved { tier, model, extra }` is journaled, so a trace read six months from now still says
exactly which model ran, even after the config changed. `swamp doctor` prints the full resolved
`(provider, tier) -> model` matrix.

### 6.3 Concurrency

Three levels of backpressure, all real semaphores:

1. **Global**: `limits.max_parallel` caps total live worker processes. Four `claude` processes each
   running `cargo build` in its own worktree melt a laptop long before they exhaust a quota.
2. **Per account**: `accounts[].max_concurrency` (default 2). Subscriptions throttle on concurrent
   sessions, not only on tokens, and blowing past that gets you limited faster than the token budget
   would.
3. **Per dispatch batch**: `swamp_dispatch` tasks queue against the above and the tool returns when
   all complete or `max_wait_s` elapses.

`reserve_brain_slot = true` holds one permit and one healthy account of the brain's provider out of
the worker pool. The held permit is the subtraction from `max_parallel`; the brain's own lease is
taken from a separate one-slot semaphore, so the reservation costs exactly one slot. `brain.account`
pins which account the brain leases, and a pin that cannot be leased is an error, never a silent
fallback to another account.

### 6.4 Selection

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SelectionPolicy { RoundRobin, LeastLoaded, QuotaAware }

impl AccountPool {
    /// Lower is better. None means ineligible.
    fn score(&self, a: &Account, s: &AccountState, now: OffsetDateTime) -> Option<f64> {
        match s.health {
            Health::Disabled | Health::AuthBroken => return None,
            Health::Cooling if s.cooldown_until.is_some_and(|t| t > now) => return None,
            _ => {}
        }
        if s.inflight >= a.max_concurrency { return None; }
        let util = s.quota.as_ref().map_or(0.0, |q| q.worst_utilization());
        if util >= self.cfg.quota_stop_at { return None; }     // proactive, before any error
        let load = s.inflight as f64 / a.max_concurrency.max(1) as f64;
        let idle = s.last_used.map_or(f64::MAX, |t| (now - t).as_seconds_f64());
        Some(match self.policy {
            SelectionPolicy::RoundRobin  => -idle,
            SelectionPolicy::LeastLoaded => load - 0.01 * a.weight as f64,
            SelectionPolicy::QuotaAware  => 0.65 * util + 0.35 * load
                                            - 0.05 * a.weight as f64
                                            - 0.02 * (idle / 3600.0).min(1.0),
        })
    }
}
```

A score alone leaves two idle accounts exactly tied, and the tie then fell to the map order, so
every sequential single-worker run burned the same subscription. Equal scores are therefore
settled in order by fewest `lifetime_nodes`, then lowest `lifetime_cost_usd` (both persisted in
`~/.swamp/accounts.json`, so the rotation survives the process), then by a per-pool cursor that
rotates the candidate list once per selection. Load always outranks history: a busy account is
never preferred to an idle one.

`LeastLoaded` is the **v1 default**. `QuotaAware` is implemented and selectable, but it is not the
default because the telemetry that feeds it exists for Anthropic only: claude emits
`rate_limit_event.unifiedWindows.*.utilization`, codex emits nothing comparable in
`codex exec --json`. A default policy that works for half the pool is worse than an honest one. With
no telemetry `util` is 0.0 and `QuotaAware` degrades into `LeastLoaded` anyway.

`acquire` never busy-spins: it waits on a `Notify` (a lease came back) or sleeps until the earliest
journaled reset time.

```rust
pub struct Lease { pub account: AccountId, pub exec: String,
                   pub env: BTreeMap<String, String>, _permit: OwnedSemaphorePermit, pool: Arc<AccountPool> }
// Drop decrements inflight and notifies waiters, so a panicking node cannot leak a slot.

pub enum NoCapacity {
    AllCooling { retry_at: OffsetDateTime },
    Saturated,
    Exhausted { reason: String },
}

impl AccountPool {
    pub async fn acquire(self: &Arc<Self>, provider: Provider,
                         exclude: &HashSet<AccountId>, deadline: Instant) -> Result<Lease, NoCapacity>;
    pub fn report(&self, id: &AccountId, failure: Option<&Failure>, cost: Option<Cost>);
    /// Fed from every live WorkerEvent::RateLimit, while the worker is still running.
    pub fn observe_quota(&self, id: &AccountId, snap: RateLimitSnapshot);
    pub fn snapshot(&self) -> Vec<(Provider, AccountId, AccountState)>;
}
```

### 6.5 Cooldown and recovery

```
RateLimited  -> cooldown = resets_at when the provider told us, else backoff * 2^(consecutive-1),
                clamped into [cooldown.min, cooldown.max]. Clock skew and past timestamps fall
                back to cooldown.default.
AuthExpired  -> Health::AuthBroken for the run, plus a loud note. Re-auth is a human action.
Overloaded   -> short cooldown (30s), no consecutive penalty.
Crashed x N  -> circuit breaker: after `breaker_threshold` consecutive infra failures, park it.
success      -> consecutive_infra_failures = 0, cooldown cleared, Health recomputed from quota.
```

### 6.6 The attempt loop: where failover policy lives

```rust
// dispatch/retry.rs
pub async fn run_node(cx: &NodeCtx, mut spec: LaunchSpec, task: &TaskRequest) -> NodeOutcome {
    let mut excluded: HashSet<AccountId> = HashSet::new();
    let mut providers = cx.provider_order.iter().copied();
    let mut provider = providers.next().expect("at least one provider");
    let mut backoff = Duration::from_secs(2);
    let mut prev: Option<NodeId> = None;
    let logical = NodeId::new();

    for attempt in 1..=cx.max_attempts {
        let lease = match cx.pool.acquire(provider, &excluded, cx.deadline).await {
            Ok(l) => l,
            Err(NoCapacity::AllCooling { retry_at }) => {
                // Cross-provider failover happens only here, and only if opted in.
                if cx.cross_provider { if let Some(next) = providers.next() {
                    provider = next; excluded.clear(); continue;
                }}
                cx.journal.emit(JournalEvent::NodeBlocked { until: retry_at, why: "all accounts cooling".into() });
                tokio::time::sleep_until(instant_of(retry_at)).await;
                continue;
            }
            Err(e) => return NodeOutcome::failed(Failure::NoCapacity { detail: format!("{e:?}") }),
        };

        // A session handle is only valid for the account that minted it.
        spec.session = match &spec.session {
            SessionPlan::Resume(h) if h.account == lease.account => spec.session.clone(),
            SessionPlan::Resume(_) => SessionPlan::New { preassigned: cx.new_session_id(provider) },
            s => s.clone(),
        };
        spec.model = match cx.cfg.model_for(provider, spec.tier, Some(&lease.account)) {
            Ok(m) => m, Err(e) => return NodeOutcome::failed(Failure::NoCapacity { detail: e.to_string() }),
        };
        spec.attempt = attempt;

        // A FRESH worktree per attempt. A rate-limited worker may have already half-edited its
        // tree; retrying on top of partial edits is how you produce plausible-looking corruption
        // that no test catches. A new worktree is simpler and safer than resetting the old one,
        // and the failed attempt's tree is retained for inspection when keep_on_failure is set.
        let wt = match cx.workspace.create(logical, attempt).await {
            Ok(w) => w, Err(e) => return NodeOutcome::failed(Failure::NoCapacity { detail: e.to_string() }),
        };
        spec.cwd = wt.path.clone();
        spec.node = cx.new_node_ids();

        // Journal the node BEFORE spawning, so a crash still leaves a Running node with full
        // provenance and a resume handle.
        cx.journal.emit_durable(JournalEvent::NodeSpawned { /* full NodeRecord */ }).await;

        let out = cx.exec.run(&spec, &wt).await;    // detached spawn + follow to terminal event
        cx.pool.report(&lease.account, out.failure.as_ref(), out.cost);

        match &out.failure {
            None => return NodeOutcome::ok(out),
            Some(f) if f.rotates_account() => {
                excluded.insert(lease.account.clone());
                prev = Some(spec.node.id);
                cx.journal.emit(JournalEvent::NodeRetry { attempt, reason: f.clone(), rotate: true });
                spec.session = SessionPlan::New { preassigned: cx.new_session_id(provider) };
                continue;
            }
            Some(f) if f.retries_same_account() => {
                cx.journal.emit(JournalEvent::NodeRetry { attempt, reason: f.clone(), rotate: false });
                // Resume the same session on the same account so the retry does not repay context.
                if let Some(h) = out.session.clone() { spec.session = SessionPlan::Resume(h); }
                tokio::time::sleep(jitter(backoff)).await;
                backoff = (backoff * 2).min(Duration::from_secs(120));
                continue;
            }
            // Terminal: the TASK failed, not the infrastructure. Stop.
            Some(_) => return NodeOutcome::from(out),
        }
    }
    NodeOutcome::failed(Failure::WorkerError {
        subtype: "attempts_exhausted".into(), detail: format!("{} attempts", cx.max_attempts) })
}
```

Four rules, stated plainly, because they are the difference between an orchestrator and something
that destroys a user's quota in ten minutes:

1. **Only `RateLimited` and `AuthExpired` rotate accounts.** A failing test suite, a malformed
   prompt or a worker that gave up must never cause Swamp to retry the same doomed task on every
   subscription. This is the single most important guard against a config typo draining everything.
2. **`Overloaded` / `Crashed` / `Truncated` retry the same account** with exponential backoff and
   jitter, resuming the same session.
3. **Each attempt is its own node**, linked by `retry_of` and grouped by `logical`. The tree shows
   exactly what `claude-main` did before `claude-alt` picked it up, both raw streams preserved.
4. **Cross-provider failover is off by default.** Silently moving an Anthropic task to a Codex model
   changes the result in ways the user did not ask for. It is a per-tier `provider_order` opt-in and
   fires only when every account of the current provider is cooling.

### 6.7 `swamp accounts`

```
PROVIDER   ACCOUNT  EXEC         HEALTH    INFLIGHT  5H    7D    COOLDOWN  NODES   $
anthropic  main     claude-main  degraded     2/3    0.06  0.91  -          214  ~12.40
anthropic  alt      claude-alt   cooling      0/2    1.00  0.72  in 41m      88   ~4.10
openai     main     codex-main   healthy      0/2     -     -    -           31  ~1.90 est
```

The 5H/7D columns are blank for OpenAI because `codex exec --json` reports no quota telemetry. That
absence is exactly why `QuotaAware` is not the v1 default.

The brain is an account's tenant like any worker: it reports its spend per turn, its rate-limit
snapshots as they stream in, and itself as exactly one node per run when it shuts down. A brain
that spent a run's planning on `claude-main` and left it at NODES 0 / $0 would also leave the
history tie-break of `LeastLoaded` and `QuotaAware` blind to the single most expensive node.

`accounts.json` is machine-wide, so it holds entries for ids this repo does not configure: another
repo may still own them. They are listed under a `not in config` note and dropped only when asked,
with `swamp accounts reset <id>`. Nothing deletes them silently.

---

## 7. Journal and run tree

### 7.1 File layout

```
<repo>/.swamp/                            # added to .git/info/exclude, never to tracked .gitignore
  config.toml                             # optional project overrides
  runs/<run_id>/
    run.json                              # header: cwd, git HEAD, config hash, swamp version, argv
    journal.jsonl                         # THE tree: append-only JournalLine stream
    mcp.json                              # generated MCP config handed to the brain
    brain.md                              # the system prompt actually used, verbatim
    swamp.pid
    nodes/<node_short>/                     # the ATTEMPT id; the branch keeps the LOGICAL one
      prompt.md                           # the exact bytes fed to fd0
      stream.jsonl                        # RAW provider stdout, verbatim, never rewritten
      stderr.log
      noise.log                           # non-JSON lines (codex banner, npm warnings)
      last-message.txt                    # codex -o
      result.json                         # NodeResult
      patch.diff
      pid                                 # pid + process start time
  last -> runs/<run_id>

~/.swamp/
  accounts.json                           # cross-run, cross-repo quota state (fs4-locked)
  worktrees/<repo-name>-<hash8>/<run_short>/<node_short>-<attempt>/   # LOGICAL node short id
```

Worktrees live **outside** the repo. Inside, every worker's `rg` / `find` / `cargo` walks its
siblings' trees, which is quadratic noise and genuinely confuses agents, and one careless
`rm -rf .swamp` would delete uncommitted worker output. `[workspace] root` can move them back for
users who prefer it.

Raw streams and the normalized journal are separate files on purpose. The journal stays small,
ordered and instant to fold; the raw stream is large and append-only. A chatty worker emits
megabytes, and keeping that out of `journal.jsonl` is what makes `swamp trace` fast on a big run.

### 7.2 Schema

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalLine {
    pub seq: u64,
    #[serde(with = "time::serde::rfc3339")] pub at: OffsetDateTime,
    pub run: RunId,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub node: Option<NodeId>,
    #[serde(flatten)] pub event: JournalEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum JournalEvent {
    RunStarted { swamp_version: String, schema: u32, argv: Vec<String>,
                 cwd: Utf8PathBuf, repo: Option<Utf8PathBuf>, base: Option<String>,
                 config_sha256: String, task: Option<String> },
    /// Full snapshot at creation. Everything after is a delta.
    NodeSpawned { node: Box<NodeRecord> },
    AccountSelected { account: AccountId, exec: String, policy: SelectionPolicy,
                      reason: String, excluded: Vec<AccountId> },
    ModelResolved { tier: Tier, model: String, extra: BTreeMap<String, String> },
    ProcessStarted { pid: i32, pgid: i32, argv: Vec<String>,
                     env_overrides: BTreeMap<String, String>, cwd: Utf8PathBuf },
    SessionBound { session: SessionHandle },
    /// Normalized event plus the byte offset in stream.jsonl it was parsed from.
    /// (last offset, raw file) is a complete crash-safe resume point for the parser.
    NodeEvent { offset: u64, event: WorkerEvent },
    NodeUsage { usage: Usage, cost: Option<Cost> },
    /// Git's list: paths relative to the worktree root, counts from `--numstat`. The event
    /// stream's absolute, count-free list is only the fallback when there is no diff.
    NodeFiles { files: Vec<FileChange> },
    NodeBlocked { #[serde(with = "time::serde::rfc3339")] until: OffsetDateTime, why: String },
    NodeRetry { attempt: u32, reason: Failure, rotate: bool },
    ProviderSwitch { from: Provider, to: Provider },
    WorktreeCreated { path: Utf8PathBuf, branch: String, base: String },
    DiffCaptured { patch: Utf8PathBuf, head: String, files: u32, insertions: u32, deletions: u32 },
    NodeFinished { state: NodeState, exit: Option<ExitInfo>, usage: Usage, cost: Option<Cost>,
                   work: Option<WorkResultRef>, summary: Option<String>,
                   files: Vec<FileChange>, unparsed_lines: u32 },
    AccountHealth { account: AccountId, health: Health,
                    #[serde(with = "time::serde::rfc3339::option")] cooldown_until: Option<OffsetDateTime>,
                    quota: Option<RateLimitSnapshot> },
    BrainTurn { role: TurnRole, text: String },
    BrainToolCall { tool: String, args_sha256: String, args_path: Utf8PathBuf },
    Adopted { into: String, commit: String, conflicts: Vec<Utf8PathBuf> },
    Note { author: NoteAuthor, text: String },
    /// Written on graceful shutdown. Its ABSENCE is what marks a run interrupted.
    RunFinished { state: NodeState, nodes: u32, usage: Usage, cost_usd: Option<f64> },
}
```

`schema: u32` in `RunStarted` is the compatibility marker. Once users have traces, `JournalEvent`
cannot break: every new field gets `#[serde(default)]`, every enum gets `#[serde(other)]` on the read
path, and changes are additive only. Renaming a variant is a breaking change and is treated as one.

### 7.3 Writer

One tokio task owns the `journal.jsonl` fd and is the only writer in the process. Everything else
holds a cloned `JournalHandle` (an `mpsc::UnboundedSender`).

```rust
#[derive(Clone)]
pub struct JournalHandle { /* tx, run, paths */ }

impl JournalHandle {
    /// Fire-and-forget, sync, non-blocking. Journalling must never fail a run and must never
    /// stall a worker's stdout pump on a slow disk.
    pub fn emit(&self, node: Option<NodeId>, event: JournalEvent);
    /// Await durability. Used only at the handful of points where a record must survive a power
    /// cut BEFORE an irreversible side effect: spawning a process, adopting a branch,
    /// finishing a node.
    pub async fn emit_durable(&self, node: Option<NodeId>, event: JournalEvent) -> anyhow::Result<u64>;
    pub fn run(&self) -> RunId;
    pub fn paths(&self) -> &RunPaths;
}

pub enum FsyncPolicy { Always, Barrier, Interval(Duration), Never }
// Barrier is the default: RunStarted / NodeSpawned / ProcessStarted / WorktreeCreated /
// NodeFinished / Adopted / RunFinished sync immediately; high-frequency text batches at
// 64 records or 250ms.
```

Startup repairs a torn tail: a crash mid-write leaves a partial last line, so `open` seeks back to
the last newline and truncates. `seq` is rebuilt from the tail so a restart continues the sequence
rather than forking it.

### 7.4 Fold

Every view is a fold over the same lines. `swamp trace` is `replay()`, the TUI is `tail()`,
`swamp_status` is the same fold rendered with a byte budget, and crash recovery is a fold plus an
append. There is no live view that can disagree with the recorded view because there is only one
fold.

```rust
pub trait Projection { type Out; fn apply(&mut self, l: &JournalLine); fn finish(self) -> Self::Out; }

#[derive(Debug, Default)]
pub struct RunView {
    pub header: Option<RunHeader>,
    pub nodes: BTreeMap<NodeId, NodeRecord>,
    pub children: BTreeMap<NodeId, Vec<NodeId>>,
    pub roots: Vec<NodeId>,
    pub by_logical: BTreeMap<NodeId, Vec<NodeId>>,   // attempt chains, collapsed in the tree view
    pub accounts: BTreeMap<AccountId, AccountState>,
    pub events: BTreeMap<NodeId, Vec<WorkerEvent>>,  // only when with_events
    pub totals: Usage,
    pub cost_usd: f64,
    pub cost_complete: bool,     // false if any node's cost is unknown
    pub last_seq: u64,
    pub finished: bool,
}

impl RunView {
    /// Pure and idempotent: replaying the same prefix always yields the same state.
    pub fn apply(&mut self, l: &JournalLine);
    pub fn load(dir: &Utf8Path, with_events: bool) -> anyhow::Result<Self>;
    /// Running nodes with no NodeFinished and a dead pid become Orphaned.
    pub fn mark_orphans(&mut self);
    pub fn tree(&self) -> Vec<TreeRow>;
}

/// Live tail: yields historical lines first, then new ones. Byte-offset poll at 150ms.
/// No filesystem-watcher dependency for a file whose path and writer we already know.
pub struct Tailer { /* path, offset, view */ }
```

`replay()` tolerates a truncated final line, reports it, and continues. A torn last line after a
crash is expected, not an error.

### 7.5 Crash recovery

```rust
pub enum Recovery {
    /// pid + start time still match: re-attach the tailer at stream_offset. The common case,
    /// because workers are detached and never noticed we died.
    Adopt { node: NodeId, pid: i32, offset: u64 },
    /// Process gone, stream has a terminal event: finalize from what is on disk.
    Finalize { node: NodeId },
    /// Process gone, stream truncated, session handle known: relaunch with --resume.
    ResumeSession { node: NodeId, session: SessionHandle, continuation: String },
    /// Process gone, no session handle: rerun from the original prompt.
    Rerun { node: NodeId },
    /// Worktree has uncommitted work but the node cannot continue: hand it to the user.
    Salvage { node: NodeId, path: Utf8PathBuf },
}

pub fn plan(view: &RunView, paths: &RunPaths) -> Vec<Recovery>;
```

`swamp` on startup scans for runs with no `RunFinished`, prints them, and offers `swamp resume`.
Nothing auto-resumes, because relaunching workers spends quota.

### 7.6 `swamp trace`

```
$ swamp trace last
run_01JZQ8  ~/projects/api  base 9f3c1ad  started 14:02:11  4m12s  ~$1.84  1.2M tok

* a13f70  brain            anthropic/main  opus     4m12s  214k  ~$0.71  ok
  |
  +- e2qgr7  [mid ] migrate user model                    1m48s  118k  ~$0.42  ok
  |    attempt 1  9c02d1  anthropic/main   sonnet   rate_limited (seven_day, telemetry) resets 21:55  12s
  |    attempt 2  e2qgr7  anthropic/alt    sonnet   ok
  |    branch swamp/01jzq8/b73e10-2   +214 -37   6 files
  |
  +- c81e00  [low ] update changelog   openai/main  astra     31s   14k       -  ok
  |    branch swamp/01jzq8/c81e00-1   +12 -0     1 file
  |
  +- f40a92  [high] audit auth middleware  anthropic/main opus  2m04s  183k  ~$0.50  failed
       WorkerError(error_during_execution): 2 tests still failing
       no failover (task-level failure)

usage  in 1.2M  out 84.1k  cache-read 9.4M  cache-write 220k
cost   ~$1.84   (1 node reported no cost data)
```

The leading column is the ATTEMPT node id: it names `nodes/<node_short>/` and is what `swamp
diff` and `swamp adopt` are given, so what is on screen always resolves. The account cell holds
`<provider>/<account>`, and when the two together do not fit it is the provider prefix that goes:
which subscription ran the node is the whole point of the column, and a real account id is a
wrapper name like `claude-main`, not `main`. The branch keeps the
LOGICAL id, which is stable across attempts. Both spellings resolve; a logical short id resolves
to the node's last finished attempt. Glyphs and columns are in `ui/fmt.rs`. `-` in the cost column means the provider reported nothing;
never `$0.00`. `~` means list-price or estimated, never money billed.

Flags: `--node <id>`, `--events`, `--raw` (verbatim `stream.jsonl`), `--stderr`, `--follow`,
`--json` (the folded `RunView`), `--depth <n>`, `--failed`, `--since <dur>`.

### 7.7 `swamp watch`

Read-only ratatui view, fed by the journal tail, with an optional subscription to the supervisor's
broadcast channel when it happens to be the same process. It never requires the supervisor to be
alive, so an interrupted run is inspectable with the same tool, and `swamp watch` from a second
terminal works against a run started elsewhere.

- Left 40%: the run tree, spinner and elapsed timer and live token counter on running nodes.
- Right 60%: the selected node's normalized event log; `r` toggles the raw JSONL view.
- Footer: per-account utilization gauges coloured by `Health`, with cooldown countdowns.

Keys: up/down select, `r` raw toggle, `d` open the node diff in `$PAGER`, `k` cancel node, `a`
accounts pane, `q` quit (detaches; never kills the run).

`crossterm::event::EventStream` and the journal tail are joined in one `tokio::select!`, redraw
capped at `[ui] refresh_hz`. A `TerminalGuard` `Drop` impl and a panic hook both restore the
terminal, so a crash never leaves a wrecked tty.

The chat UI is deliberately *not* the TUI in v1. Keeping the observer read-only and out-of-process
removes an entire class of state-sharing bugs and costs the user one extra terminal.

---

## 8. CLI surface

```
swamp [OPTIONS] [COMMAND]

Global:
  -C, --cd <DIR>          Project root (default: cwd, walked up to the git root)
      --config <FILE>     Extra config layer, highest priority
      --profile <NAME>    Named profile from [profiles.<name>]
      --json              Machine-readable output where applicable
  -v, --verbose...        -v info, -vv debug, -vvv trace; logs to .swamp/swamp.log
  -q, --quiet
      --no-color

swamp / swamp chat                          Interactive session with the brain
      --brain <anthropic|openai>            Override [brain].provider
      --account <ID>                        Pin the brain to one account
      --tier <high|mid|low>                 Brain tier (default high)
      --resume <RUN|last>                   Relaunch the brain with --resume + a state preamble
      --workers <N>                         Override limits.max_parallel
      --budget <USD>
      --dry-run                             Boots a real brain whose dispatch tools journal and
                                            return a fake success. Iterate the system prompt,
                                            which is the actual product surface, without spending.

swamp run <TASK...>                         One-shot, non-interactive
      --no-brain                            Dispatch TASK to a single worker. The v1 smoke path.
      --tier <TIER>                         Default mid
      --provider <P>  --account <ID>        --account pins and disables failover (warns)
      --workers <N>
      --isolation <worktree|shared|readonly>
      --base <REF>                          Branch workers from this ref instead of HEAD
      --include-dirty                       Base from `git stash create` so uncommitted work is visible
      --timeout <DUR>                       Default 25m
      --budget <USD>  --max-attempts <N>
      --wait | --detach
      --json | -q                           -q prints only the run id
      -                                     Read TASK from stdin; @file also accepted

swamp trace [RUN|last]                      RUN accepts a full id, a unique prefix, the printed
                                            short id (the ULID's last 6 chars, case-insensitive),
                                            `last`, or `-2`
      --node <ID> --events --raw --stderr --follow --json --depth <N> --failed --since <DUR>
                                            With no RUN, --node searches every run, so a node of
                                            an older run renders that run

swamp watch [RUN|last]                      Live TUI, read-only, attachable from another terminal

swamp runs                                  List runs, newest first
      --all --interrupted --limit <N> --json

swamp resume <RUN|last>                     Recover an interrupted run
      --plan                                Print the Recovery plan and exit, spending nothing
      --only <NODE>... --rerun-failed --no-brain

swamp accounts [list]                       Health, inflight, 5h/7d, cooldown, nodes, spend
  check [--account <ID>]                    Run `<exec> --version` + a 1-token probe per account
  cooldown <ID> <DUR> | clear <ID> | enable <ID> | disable <ID> | reset [ID]
                                            `reset` with an ID drops that one entry, which is
                                            how an account that left the config leaves the file

swamp diff <NODE> [--stat|--name-only|--patch]
                                            --stat renders the captured patch git-style
swamp adopt <NODE>...                       Land a worker's work in the user's tree
                                            NODE is a full id, either short id (the ATTEMPT's,
                                            which trace prints and which names the node dir, or
                                            the LOGICAL one, which names the branch and resolves
                                            to the last finished attempt) or a prefix, searched
                                            across every run newest first and refused when the
                                            prefix matches nodes in more than one; `last` and
                                            `-2` name a run and resolve to its only node, or to
                                            its most recently finished one
      --strategy <apply|merge|cherry-pick>  Default apply
      --into <BRANCH> --dry-run --force     Refuses on a dirty tree unless --force

swamp worktrees [ls|prune|open <NODE>]
swamp cancel <RUN|NODE>... [--all] [--signal term|kill]
swamp gc [--older-than <DUR>] [--keep <N>] [--dry-run] [--force]
swamp replay <RUN> [--reparse]              --reparse re-derives the journal from the retained raw
                                            streams with the CURRENT adapters. This is the real
                                            answer to vendor schema drift: fix the adapter, then
                                            recover history instead of losing it.
swamp doctor [--probe] [--fix] [--schema] [--reap]
swamp config [show [--effective] | path | validate | init]
swamp completions <SHELL>
swamp mcp-bridge --socket <PATH>            Hidden. stdio <-> UDS pump, spawned by the brain CLI.
```

Exit codes: `0` ok, `1` generic, `2` config invalid, `3` no capacity (all accounts cooling),
`4` node failed, `5` conflict, `6` cancelled, `7` budget exceeded.

A refusal that happens before anything is launched - a dirty working tree is the common one - is
checked before the run is created: one line on stderr, the exit code, no run directory and no
node. A run that exists is a run that spent something.

---

## 9. `swamp doctor`

```
$ swamp doctor
swamp 0.1.0 - rustc 1.98.0 - darwin arm64

environment
  ok   git 2.47.1                    worktree support present
  ok   repo ~/projects/api           HEAD a3f91c2, clean
  ok   .swamp/ writable              listed in .git/info/exclude
  ok   ~/.swamp state dir            free 412 GiB
  ok   swamp resolvable              /usr/local/bin/swamp (needed for the MCP bridge)
  ok   not inside a worker           SWAMP_DEPTH unset

providers.anthropic (claude-cli)
  ok   claude-main -> ~/bin/claude-main   wrapper -> claude 2.x
         auth: subscription (apiKeySource=none)   CLAUDE_CONFIG_DIR=~/.claude
         tiers: high=opus  mid=sonnet  low=haiku   probe 1.4s
  ok   claude-alt  -> ~/bin/claude-alt    CLAUDE_CONFIG_DIR=~/.claude-alt
  WARN claude-work -> /opt/homebrew/bin/claude
         not a wrapper and CLAUDE_CONFIG_DIR is unset: this is the SAME account as
         claude-main. Dispatch would double-spend one quota while reporting two
         healthy accounts, and failover between them would be a silent no-op.

providers.openai (codex-cli)
  ok   codex-main  -> ~/bin/codex-main    wrapper -> codex-cli 0.15.x
         auth: ChatGPT subscription   CODEX_HOME=~/.codex-main
  note codex exec reports no cost and no quota telemetry.
         [pricing] is set, so OpenAI node costs render as estimates.

protocol
  ok   claude stream-json fixtures parse   (5/5 golden lines)
  ok   codex  --json fixtures parse        (5/5, 1 known non-JSON preamble)
  ok   mcp round trip                      brain spawned with --mcp-config called swamp_ping

config
  ok   ~/.config/swamp/config.toml         valid
  WARN [workspace] link is empty but ./target is 3.1 GiB
         fresh worktrees will rebuild from scratch; consider link = ["target"]
  WARN providers.anthropic.worker.permission_mode = "acceptEdits"
         denies every Bash call under --permission-prompts none and Bash is not
         allowed, so workers cannot run tests, a build or git. Add "Bash" to
         providers.anthropic.worker.allow_tools. ("auto" is not the fix: it denies
         the file writes instead.) The same check covers [brain].

3 warnings, 0 errors.
```

The most valuable check is the third provider line. Two "accounts" that resolve to the same config
dir is silent, expensive, and otherwise only discovered when one quota dies twice as fast as
expected. `doctor` resolves each `exec`, runs it with a probe, compares effective config dirs, and
errors when two accounts collide.

`--probe` sends a one-token prompt through each configured account and asserts the stream still
yields `SessionStarted` + `Final` with non-zero usage. Run it after every CLI upgrade; it is the
real defence against upstream format drift. `--schema` reports the `Detector::Pattern` fallback rate
and the unparsed-line ratio across recent runs, which is the early warning that a vendor changed
wording or shape. `--reap` removes stale worktrees, sockets and pidfiles and runs `git worktree prune`.

---

## 10. Config format

Layered, lowest to highest: built-in defaults, `~/.config/swamp/config.toml`,
`<repo>/.swamp/config.toml`, `SWAMP_*` env, `--config`, CLI flags.
`swamp config show --effective` prints the merged result with the origin of every key.

```toml
# ~/.config/swamp/config.toml  (or <repo>/.swamp/config.toml)
version = 1

# ---------------------------------------------------------------- limits
[limits]
max_parallel          = 4          # machine-wide concurrent worker processes
max_nodes_per_run     = 32
max_parallel_dispatch = 6          # per single swamp_dispatch call
max_high_tier_concurrent = 2       # `high` is expensive and the brain will over-reach
max_depth             = 2          # brain -> worker -> refused (also via SWAMP_DEPTH)
worker_timeout        = "25m"
brain_turn_timeout    = "15m"
grace_period          = "5s"       # SIGTERM -> SIGKILL window
run_budget_usd        = 25.0
node_budget_usd       = 3.0        # -> claude --max-budget-usd; codex has no equivalent
max_prompt_bytes      = 200000
max_result_bytes      = 8000       # cap on worker output fed back to the brain
unsafe_ack            = false      # required before any --dangerously-* flag is accepted

# ---------------------------------------------------------------- brain
[brain]
transport = "cli"                  # "cli" (default, subscription-safe) | "api" (feature api-brain)
provider  = "anthropic"
account   = "main"
tier      = "high"
reserve_brain_slot = true          # keep this account out of the worker pool
# Swamp always launches with `--permission-prompts none`: nobody is at the terminal to answer
# a prompt. Measured against the real CLI, the two halves are gated separately: "auto" denies
# file writes ("the session currently doesn't have approval enabled for file writes"), so the
# node produces no diff at all, while "acceptEdits" (and "plan", "manual", "dontAsk") writes
# files but denies every Bash call that is not allowed by name. "acceptEdits" plus "Bash" in
# allow_tools is the recommendation here and for the workers below. The trade-off is real: an
# allowed Bash runs commands without asking, the same trust you extend to a CLI agent in your
# own shell, and a worktree is a directory, not a sandbox. deny_tools below still keeps the
# brain from editing files.
permission_mode    = "acceptEdits"
include_partial_messages = true    # smooth chat streaming; workers keep this off
# The brain plans and reads; workers write. Keeps the brain from corrupting parallel worktrees.
# Bash is unqualified so the brain can run git and verify a worker's claim with the test suite.
allow_tools = [
  "mcp__swamp__swamp_dispatch", "mcp__swamp__swamp_await", "mcp__swamp__swamp_status",
  "mcp__swamp__swamp_result", "mcp__swamp__swamp_worker_diff", "mcp__swamp__swamp_note",
  "Read", "Grep", "Glob", "Bash",
]
deny_tools = ["Edit", "Write", "MultiEdit", "NotebookEdit"]
# Swamp reads this file and passes its TEXT via --append-system-prompt.
system_prompt_file = ".swamp/brain.md"

# ---------------------------------------------------------------- dispatch
[dispatch]
policy                  = "least-loaded"   # round-robin | least-loaded | quota-aware
max_attempts            = 3
cross_provider_failover = false
default_provider        = "anthropic"
default_tier            = "mid"

[cooldown]
min = "60s"
max = "6h"
default = "15m"
breaker_threshold = 3
quota_warn_at = 0.90
quota_stop_at = 0.98

# ---------------------------------------------------------------- workspace
[workspace]
isolation     = "worktree"        # worktree | shared | readonly
root          = "~/.swamp/worktrees"   # outside the repo on purpose
base          = "HEAD"
branch_prefix = "swamp"           # -> swamp/<run_short>/<node_short>
include_dirty = false             # true -> base from `git stash create`
require_clean = true
commit_on_success = true
commit_template = "swamp({tier}): {title}\n\nnode: {node}\nrun: {run}"
keep_on_failure = true            # never silently delete a failed worker's work
# A fresh worktree has no node_modules, no target, no .env. A worker that cannot build
# produces a useless diff, expensively. This is the most common cause of a bad first run.
link = ["node_modules", "target", ".venv", ".direnv"]
copy = [".env", ".env.local"]
post_create = ""                  # optional one-shot, e.g. "just setup"
post_create_timeout = "5m"

# ---------------------------------------------------------------- journal
[journal]
fsync         = "barrier"         # always | barrier | interval:250ms | never
max_line_bytes = 8388608          # a base64 blob on one line must truncate, not OOM
keep_runs     = 200
keep_runs_for = "30d"
redact = [
  '(?i)(api[_-]?key|authorization|bearer|secret|password)\s*[:=]\s*\S+',
  'sk-[A-Za-z0-9_\-]{20,}',
]

# ---------------------------------------------------------------- providers
# Model ids live ONLY here. Nothing is hardcoded in the binary.
[providers.anthropic]
adapter = "claude-cli"
models  = { high = "opus", mid = "sonnet", low = "haiku" }
tier_extra = { high = { effort = "high" }, mid = { effort = "medium" }, low = { effort = "low" } }

  [providers.anthropic.worker]
  # "acceptEdits" so the edits land, plus Bash by name so the worker can run the tests it was
  # sent to run. See the note under [brain]: "auto" denies the writes.
  permission_mode = "acceptEdits"
  # Tool NAMES, merged into the single --allowed-tools / --disallowed-tools flag, the way the
  # brain's lists are. A raw flag in `args` would overwrite the other half instead.
  allow_tools = ["Bash"]
  # Behaviour, not safety: a worker that can reach the delegation tools and inherits a global
  # CLAUDE.md telling it to orchestrate will spawn a team and report on its behalf.
  deny_tools = ["Task", "Agent", "Workflow", "Team"]
  # Optional, appended after Swamp's built-in worker role prompt.
  # system_prompt_file = ".swamp/worker.md"
  args = []
  readonly_args = ["--permission-mode", "plan",
                   "--disallowed-tools", "Edit", "Write", "MultiEdit", "NotebookEdit"]

[providers.openai]
adapter = "codex-cli"
models  = { high = "gpt-6-astra", mid = "gpt-5.6-sol", low = "gpt-5.6-terra" }
tier_extra = { high = { model_reasoning_effort = "high" },
               mid  = { model_reasoning_effort = "medium" },
               low  = { model_reasoning_effort = "low" } }

  [providers.openai.worker]
  sandbox = "workspace-write"
  # `codex exec` has NO -a/--ask-for-approval; that is top-level only. Use the config override.
  args = ["-c", "approval_policy=\"never\""]
  readonly_args = ["-s", "read-only"]

# ---------------------------------------------------------------- accounts
# One entry per subscription. `exec` is a wrapper on PATH that sets CLAUDE_CONFIG_DIR / CODEX_HOME.
# Swamp never touches credentials.
[[accounts]]
id = "main"
provider = "anthropic"
exec = "claude-main"
max_concurrency = 3
weight = 2

[[accounts]]
id = "alt"
provider = "anthropic"
exec = "claude-alt"
max_concurrency = 2
# Per-account tier override: this plan has no Opus.
models = { high = "sonnet" }

[[accounts]]
id = "work"
provider = "anthropic"
exec = "claude"                        # no wrapper: point at the config dir directly
env = { CLAUDE_CONFIG_DIR = "~/.claude-work" }
max_concurrency = 1

[[accounts]]
id = "codex-main"
provider = "openai"
exec = "codex-main"
max_concurrency = 2

# ---------------------------------------------------------------- tiers
# Cross-provider order, consulted ONLY when every account of the current provider is cooling
# and dispatch.cross_provider_failover = true.
[tiers.high]
provider_order = ["anthropic", "openai"]
node_budget_usd = 8.0
timeout = "45m"

[tiers.mid]
provider_order = ["anthropic", "openai"]

[tiers.low]
provider_order = ["anthropic"]
node_budget_usd = 0.75
timeout = "10m"

# ---------------------------------------------------------------- failure patterns
# Layer 3 of the classifier. Editable in the field when a CLI changes its wording, with no new
# Swamp release. Detector::Pattern usage is journaled, so `doctor --schema` shows when Swamp has
# silently degraded to these.
[failure.anthropic]
rate_limit = ['(?i)usage limit reached', '(?i)rate.?limit', '(?i)\brate_limit_error\b',
              '(?i)\b5-hour limit\b', '(?i)\b429\b']
auth       = ['(?i)invalid_api_key', '(?i)oauth token has expired',
              '(?i)credit balance is too low', '(?i)please run .?claude.* login']
overloaded = ['(?i)overloaded_error', '(?i)\b529\b']

[failure.openai]
rate_limit = ['usage_limit_reached', "(?i)you've hit your usage limit",
              '(?i)\b429\b', '(?i)too many requests']
auth       = ['(?i)not logged in', '(?i)unauthorized', '(?i)run `codex login`']
overloaded = ['server_overloaded', '(?i)\b503\b']

# ---------------------------------------------------------------- pricing
# Only used to ESTIMATE cost where the CLI reports none (codex). USD per 1M tokens.
# Anything computed here is stamped basis="estimated" and is never presented as a measurement.
# Omit the table and cost stays blank.
[pricing."gpt-5.6-sol"]
input        = 0.25
cached_input = 0.025
output       = 2.00

# ---------------------------------------------------------------- ui
[ui]
refresh_hz    = 20
tree_width    = 46
show_thinking = false
tail_lines    = 200

# ---------------------------------------------------------------- profiles
# `swamp --profile cheap run "..."` layers this over everything above.
[profiles.cheap]
"dispatch.default_tier" = "low"
"limits.max_parallel"   = 2
"brain.tier"            = "mid"
```

Validation runs before anything spawns, and reports **all** errors together with the offending key:
duplicate account ids; `brain.account` not belonging to `brain.provider`; a tier with no model for
any provider that could serve it; unparseable regex; `max_concurrency = 0`;
`quota_warn_at >= quota_stop_at`; `workspace.root` inside `.swamp`; `isolation = "shared"` forces
`max_parallel = 1` with a printed warning; any `--dangerously-*` in `worker.args` without
`limits.unsafe_ack = true`.

---

## 11. Dependencies

Rust 1.98 stable, edition 2024, `resolver = "3"`.

```toml
[package]
name = "swamp"
version = "0.1.0"
edition = "2024"
rust-version = "1.98"

[dependencies]
# async runtime and process supervision
tokio        = { version = "1.47", features = ["rt-multi-thread", "macros", "process", "fs",
                                               "io-util", "io-std", "net", "sync", "time", "signal"] }
tokio-util   = { version = "0.7", features = ["rt"] }        # CancellationToken
futures      = "0.3"
async-trait  = "0.1"                                          # trait Brain only; adapters are sync

# CLI
clap          = { version = "4.5", features = ["derive", "env", "wrap_help", "unicode"] }
clap_complete = "4.5"

# serialization
serde        = { version = "1.0", features = ["derive", "rc"] }
serde_json   = { version = "1.0", features = ["raw_value", "preserve_order"] }
toml         = "0.9"
toml_edit    = "0.23"                                         # span-accurate config errors
humantime-serde = "1.1"                                       # "25m" / "6h" in TOML

# errors and logging
anyhow    = "1.0"
thiserror = "2.0"
tracing   = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json", "fmt"] }
tracing-appender   = "0.2"                                    # .swamp/swamp.log, never the journal

# domain primitives
ulid    = { version = "1.2", features = ["serde"] }           # sortable ids
uuid    = { version = "1.11", features = ["v4", "serde"] }    # claude --session-id needs a UUID
time    = { version = "0.3", features = ["serde-well-known", "macros", "formatting", "parsing"] }
camino  = { version = "1.1", features = ["serde1"] }          # Utf8Path everywhere; no OsString in the journal
sha2    = "0.10"

# fs, locking, process introspection
fs4         = { version = "0.13", features = ["sync"] }       # advisory lock on accounts.json
tempfile    = "3.14"                                          # atomic temp-and-rename
directories = "6.0"
which       = "7.0"                                           # PATH resolution + precise doctor diagnosis
shellexpand = "3.1"                                           # "~/.claude-work" in config env maps
shell-words = "1.1"

# matching and text
regex         = "1.11"                                        # RegexSet for failure patterns + redaction
smallvec      = { version = "1.13", features = ["union", "serde"] }
indexmap      = { version = "2.7", features = ["serde"] }
parking_lot   = "0.12"
rand          = "0.9"                                         # retry jitter
unicode-width = "0.2"
textwrap      = "0.16"
owo-colors    = "4.1"

# TUI and REPL
ratatui   = { version = "0.29", features = ["crossterm"] }
crossterm = { version = "0.28", features = ["event-stream"] }
rustyline = { version = "15.0", default-features = false, features = ["with-file-history"] }

[target.'cfg(unix)'.dependencies]
nix     = { version = "0.29", features = ["process", "signal"] }   # setsid, killpg, kill(pid,0)
sysinfo = "0.33"                                                   # process start time -> PID-reuse-safe liveness

[dev-dependencies]
insta             = { version = "1.41", features = ["json", "redactions"] }
assert_cmd        = "2.0"
predicates        = "3.1"
proptest          = "1.6"
tokio-test        = "0.4"
pretty_assertions = "1.4"
tempfile          = "3.14"

[features]
default   = ["tui"]
tui       = []
api-brain = []    # stub; would add reqwest + eventsource-stream

[profile.release]
lto = "thin"
codegen-units = 1
strip = "debuginfo"
```

### Deliberate non-dependencies

**No `rmcp`.** The surface Swamp needs is `initialize`, `notifications/initialized`, `tools/list`,
`tools/call` over newline-delimited JSON-RPC on stdio: roughly 300 lines against a stable spec. We
need three to six tools and no resources, prompts, sampling or notifications. The socket-plus-bridge
topology is custom regardless, so a framework's transport layer is not reused. `rmcp` is pre-1.0 and
its API moves between releases; taking a churning dependency on the path between the brain and the
dispatcher is a poor trade. `mcp/tools.rs` holds the behaviour and is transport-agnostic, so swapping
the protocol layer later touches two files.

**No `git2` / `gix`.** `workspace/git.rs` shells out to the user's `git` via `tokio::process`. That
inherits their git config, credential helpers, hooks, LFS filters and signing setup, all of which a
worker's commits need. `git worktree` semantics then match exactly what the user gets at their own
prompt, which matters when they debug a worktree by hand. It also removes libgit2's C build. The
cost is parsing porcelain, which is bounded and testable (`--porcelain -z` everywhere). We are
already an expert at spawning subprocesses and parsing their output; that is the whole product.

**No `notify`.** The journal tailer polls a byte offset every 150ms. A filesystem watcher is more
code and more platform surface for a file whose path and writer we already know.

**No database.** Append-only JSONL with a single writer gives crash safety, trivial tailing and zero
migration cost, and keeps traces greppable and diffable. Recovery is truncate-the-torn-tail, not WAL
replay. A run that outgrows an in-memory fold is a signal to prune, not to add sqlite. If that ever
changes, `Projection` is the seam.

**No HTTP client, no provider SDK.** Swamp makes zero network calls. That is the structural guarantee
behind "never handles credentials" and "API key optional", and `doctor` can assert it.

**No `chrono`.** `time` has first-class RFC 3339 serde, no `localtime_r` soundness warning, and a
smaller footprint. The journal stores RFC 3339 UTC exclusively; local time is a rendering concern.

---

## 12. Flag verification notes

Everything below was checked against `docs/ref/claude-help.txt`, `docs/ref/codex-help.txt` and
`docs/ref/codex-exec-help.txt`. Three traps worth recording, because all three are easy to get wrong
and two of them break the product silently.

1. **`codex exec` has no `-a` / `--ask-for-approval`.** That option appears only in the top-level
   `codex --help`. `codex exec -a never` dies at argv parsing, so every OpenAI worker would fail to
   launch, the classifier would land on `Crashed`, and `retries_same_account()` would burn the whole
   backoff schedule on a deterministic bug. Use `-c approval_policy="never"`.

2. **`--append-system-prompt-file` is not a documented option.** `claude --help` lists
   `--append-system-prompt <prompt>` and `--system-prompt <prompt>`, both taking prompt *text*. The
   `-file` spellings appear only inside the `--bare` description's prose. Swamp reads the file
   itself and passes the text. `doctor --probe` would catch it if this ever changes.

3. **`--verbose` is mandatory alongside `--output-format stream-json` in print mode**, and
   `--max-budget-usd`, `--permission-prompts`, `--input-format` and `--no-session-persistence` all
   document "only works with --print".

Flags relied on, all present in the help texts:

- claude: `-p/--print`, `--output-format stream-json`, `--input-format stream-json`, `--verbose`,
  `--model`, `--session-id <uuid>`, `-r/--resume`, `--permission-mode`, `--permission-prompts none`,
  `--max-budget-usd`, `--mcp-config`, `--strict-mcp-config`, `--append-system-prompt`,
  `--allowedTools/--allowed-tools`, `--disallowedTools/--disallowed-tools`,
  `--include-partial-messages`, `--effort`, `--add-dir`, `--setting-sources`,
  `--dangerously-skip-permissions` (gated).
- codex exec: `--json`, `-m/--model`, `-C/--cd`, `-s/--sandbox`, `-o/--output-last-message`,
  `-c key=value`, `--skip-git-repo-check`, `--output-schema`, `--ephemeral`, `--add-dir`,
  `--strict-config`, `-p/--profile`, `exec resume <id>`, `-` for stdin prompt,
  `--dangerously-bypass-approvals-and-sandbox` (gated).

`--strict-mcp-config` passed *without* `--mcp-config` is used deliberately for workers: it means
"only servers from --mcp-config", and there are none, so workers get exactly zero MCP servers. Fast
and deterministic.

---

## 13. Risks

1. **Vendor stream schemas are not a versioned API (highest risk).** Neither
   `claude --output-format stream-json` nor `codex exec --json` is a stability contract, and both
   ship frequently. Mitigations in order of value: (a) raw bytes land in `stream.jsonl` before
   anything parses them, so a parser failure degrades to "we have all the evidence, we just cannot
   summarise it", never to data loss; (b) `#[serde(other)]` on every wire enum and `#[serde(default)]`
   on every field, so unknown variants and new fields are ignored, not fatal; (c) golden tests pinned
   to the two sample files; (d) `WorkerEvent::Unknown` is counted and a high ratio emits a drift note;
   (e) `doctor --probe` after every CLI upgrade; (f) `swamp replay --reparse` re-derives history with
   fixed adapters, which is the only real recovery.

2. **Codex writes non-JSON to stdout.** Observed: `Reading additional input from stdin...` precedes
   the JSONL. Handled, but it is proof that the stream is a human-oriented channel that happens to
   carry JSON. Never assume line N is JSON.

3. **Rate-limit misclassification burns a second account.** Detection is telemetry first, then
   structured result, then regex. A false positive doubles cost; a false negative surfaces a quota
   problem as a generic failure. Mitigations: `max_attempts` caps the blast radius; only
   `RateLimited` / `AuthExpired` rotate; the decision, its `Detector` and its evidence string are
   journaled, so a misclassification is visible in `swamp trace` rather than invisible;
   `--account <ID>` disables failover entirely for users who want to observe before trusting it.

4. **Cost is partly unknowable.** Anthropic reports `total_cost_usd` at list price
   (`costBasis: "list"`), which on a subscription is not money billed. Codex reports tokens only.
   Mitigation: `Option<Cost>` with `CostBasis`; absent renders `-`, never `$0.00`; reported and
   estimated both render with `~`; run totals print `~$1.84 (1 node with no cost data)` rather than
   a confidently wrong number. On pure subscriptions the real budget is quota utilization, not
   dollars.

5. **`--permission-prompts none` silently denies tools.** With nobody to answer, anything requiring
   approval is auto-denied and the worker writes a confident summary of work it never did.
   Mitigation: `result.permission_denials` is parsed, journaled, shown in the trace, and a writing
   worker with denials > 0 is downgraded to `Failed(PermissionDenied)`. Never trust the final message
   over the diff.

6. **Two "accounts" that are one account.** A wrapper that forgets `CLAUDE_CONFIG_DIR` makes dispatch
   double-spend one quota while the UI shows two healthy accounts and failover silently accomplishes
   nothing. `doctor` resolves each exec, detects non-wrappers, and errors on collisions.

7. **The brain competes with its own workers for quota, and can deadlock.** Mitigation:
   `reserve_brain_slot = true` by default. If the brain hits a limit anyway its `session_uuid` is
   journaled and `swamp chat --resume last` restarts it. Failing the *brain* over mid-session is not
   attempted in v1; it would silently change the model in the middle of a conversation.

8. **Worktree isolation is leaky.** A fresh worktree has no `node_modules`, no `.env`, no `target`,
   and a worker that cannot build produces a useless diff expensively. Mitigation:
   `workspace.link` / `copy` / `post_create`, plus a doctor warning when a heavy build dir exists and
   `link` is empty. `isolation = "shared"` is the escape hatch, hard-capped at one worker.

9. **Prompt injection from worker output into a brain that holds dispatch tools.** Worker output is
   attacker-influenced data (repo content, test output, fetched text) fed straight into the brain's
   context. Mitigation: results truncated to `max_result_bytes` and wrapped in an explicit
   `<worker-output node="..." trust="untrusted">` envelope; the brain's system prompt states that
   worker output is data and never instruction; the brain's default tool policy is deny-write, so the
   worst case is a wasted dispatch rather than a corrupted repo; adopting stays a user action. This
   is mitigated, not solved, and is the reason merging is not a brain tool in v1.

10. **Detached workers can outlive an uncontrolled shutdown.** The property that makes recovery work
    also means `kill -9 swamp` leaves workers spending quota. Mitigation: a pidfile with a start-time
    stamp per node; normal SIGINT/SIGTERM shutdown journals intent then `killpg`s every live node;
    startup reports adoptable orphans before doing anything else; `swamp cancel --all` and
    `doctor --reap`.

11. **Unbounded recursion.** A worker whose PATH contains `swamp` can invoke `swamp run`. Guarded
    three ways: `SWAMP_DEPTH` exported into every worker env and refused above `max_depth`; the
    dispatcher rejects `swamp_dispatch` from a node at the depth limit; the global semaphore caps
    total live processes regardless.

12. **Nested subagents are invisible.** A claude worker can spawn its own subagents; Swamp records
    the worker as one node. `--forward-subagent-text` exists and would surface them, but it changes
    the stream shape and inflates the journal. v1 records workers as opaque and says so in `trace`.

13. **Multi-account terms of service.** Pooling several subscriptions may conflict with a provider's
    terms. Swamp routes work within published limits and never evades them: no proxies, no credential
    handling, no header manipulation, no retry-until-it-works, and it honours provider-supplied
    `resetsAt` rather than probing. Defaults are conservative. `doctor` states the concern once and
    the README tells users to check their own agreements. Each account must be the user's own
    legitimate subscription.

14. **Secrets in the journal.** Worker output can echo `.env` contents or a token from a shell
    command. `journal.redact` regexes are applied at the single writer, covering both `journal.jsonl`
    and the raw sink, so there is one place to audit. Not airtight against a novel secret format;
    `.swamp/` should be treated as sensitive and is git-excluded on first run.

15. **Unix only in v1.** Process groups, `killpg`, `setsid`, UDS and `kill(pid, 0)` are POSIX. The
    platform-specific code is confined to three files, so a Windows port is bounded, but it is out of
    scope.

---

## 14. Open questions

1. **`mcp_servers.*` as a codex `-c` key.** Inferred from `codex mcp` ("Manage external MCP servers
   for Codex") and the documented dotted-TOML `-c` override. Not directly attested in the help texts.
   `doctor --probe` must verify the round trip end to end before the codex brain is declared working.
   If the key differs, only `worker/codex.rs::build_argv` changes.

2. **Codex quota telemetry.** `codex exec --json` shows none in the sample. If a future version
   emits per-window utilization, `QuotaAware` becomes a defensible default for the whole pool. Until
   then it stays selectable but not default.

3. **Brain-side `--permission-prompts none` for chat.** A brain that silently loses a tool call is
   worse than one that asks. v1 sets `none` for determinism; revisit once the chat REPL can surface
   a prompt and answer it via `--permission-prompt-tool`.

4. **Raw stream retention.** `gc` deletes rather than compresses in v1. zstd on node finish is a
   two-line change and probably worth it once real runs show the disk cost.

5. **`swamp_await` semantics for the codex brain.** With `ResumePerTurn`, a blocking MCP tool call
   holds the whole `codex exec` process open for the duration of a fan-out. Acceptable for v1;
   `codex app-server` is the upgrade path.

6. **Worker follow-up turns.** The plumbing (`SessionHandle`, `--resume`, `exec resume`) is built and
   tested by the same-account retry path, but no tool exposes "send this worker another turn". The
   question is whether that belongs to the brain or only to the user.

7. **Adoption strategy default.** `apply` is the least surprising, but `merge` preserves authorship
   and the branch. Needs real use before picking a different default.
