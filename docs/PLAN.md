# Swamp - Implementation Plan

Nine work packages. WP0 is the scaffold: it defines every data type concretely and every function
signature as `todo!()`, so WP1 through WP7 compile against one agreed interface and can run in
parallel. WP8 is integration.

Read `docs/DESIGN.md` first. Where this file and DESIGN.md disagree, DESIGN.md wins; raise the
conflict rather than diverging.

## Rules for every package

1. **File ownership is exclusive.** Touch only the files listed under your package. If you need a
   change in a file you do not own, stop and report it; do not edit it.
2. **Never change a public signature that WP0 fixed.** If a signature is wrong, report it. A silent
   change breaks every other agent in flight.
3. `cargo build`, `cargo clippy --all-targets -- -D warnings` and `cargo test` must pass when you
   finish. Leaving another package's `todo!()` in place is fine; leaving your own is not.
4. Comments are rare and short. One line, only for non-obvious logic.
5. No model ids, no account names, no provider-specific magic strings outside
   `src/worker/claude.rs`, `src/worker/codex.rs` and config.
6. No em dashes anywhere.
7. Tests use `docs/ref/*.jsonl` fixtures (copied in by WP0) and `tempfile`. No network, ever.
8. Every wire-format enum gets `#[serde(other)] Other`; every wire field gets `#[serde(default)]`.

## Dependency graph

```
WP0 scaffold
  |
WP1 core (config, error, model impls)
  |
  +-- WP2 journal ---+
  +-- WP3 worker ----+
  +-- WP5 workspace -+---> WP4 dispatch ---> WP6 mcp+brain ---> WP7 ui+cmd ---> WP8 integration
```

WP2, WP3 and WP5 are fully parallel after WP1. WP4 compiles against WP2/WP3/WP5 stubs and can start
at the same time, integrating as they land. WP6 and WP7 can start against stubs too; they only need
real behaviour from their dependencies at WP8.

---

## WP0 - Scaffold

**Goal:** a compiling skeleton in which every type is real and every function body is `todo!()`.
This is the contract for all other packages. Nothing here is allowed to be clever.

### Files owned

```
Cargo.toml
rust-toolchain.toml
.gitignore
swamp.example.toml
docs/ref/claude-help.txt          (copied)
docs/ref/codex-help.txt           (copied)
docs/ref/codex-exec-help.txt      (copied)
docs/ref/claude-stream-sample.jsonl   (copied)
docs/ref/codex-stream-sample.jsonl    (copied)
src/main.rs
src/lib.rs
src/cli.rs
tests/common/mod.rs
```

Plus the **initial creation** of every other `src/**.rs` file listed in DESIGN.md section 3. After
WP0 merges, those files belong to their packages; WP0 retains only the list above.

### What WP0 must do

1. `Cargo.toml` exactly as DESIGN.md section 11. `rust-toolchain.toml` pins `channel = "1.98.0"`.
2. Copy the five reference files from the scratchpad `ref/` directory into `docs/ref/`.
3. `src/lib.rs`: declare every module, `pub use` the cross-package surface.
4. **Write every type in DESIGN.md sections 4, 5.2, 6.1 and 7.2 verbatim, with real fields and real
   derives.** Types are not stubbed. This is what lets the other packages compile.
5. Stub every function, method and trait method with `todo!("WPn")` naming the owning package.
   Exception: trivial pure accessors that DESIGN.md already shows in full (`Usage::absorb`,
   `Failure::rotates_account`, `NodeState::is_terminal`, `RunId::short`, `WorkspaceRef::path`,
   `RateLimitSnapshot::worst_utilization`) may be written out; they are covered by WP1's tests.
6. `src/cli.rs`: full clap derive tree for DESIGN.md section 8, including `mcp-bridge` with
   `#[command(hide = true)]`. Every flag, correct types, correct defaults. No logic.
7. `src/main.rs`: `#[tokio::main]`, tracing init to `.swamp/swamp.log` via `tracing-appender`,
   config load, match on `Command` and call `cmd::*::run(...)`, map errors through
   `error::exit_code` and `std::process::exit`.
8. `swamp.example.toml`: DESIGN.md section 10 verbatim.
9. `tests/common/mod.rs`, fully implemented, no stubs:

```rust
pub fn fixture(name: &str) -> std::path::PathBuf;      // docs/ref/<name>
pub fn fixture_lines(name: &str) -> Vec<String>;       // non-empty lines, trailing \n stripped
pub fn tmp_repo() -> (tempfile::TempDir, camino::Utf8PathBuf);  // `git init` + one commit
pub fn tmp_repo_dirty() -> (tempfile::TempDir, camino::Utf8PathBuf);
```

### Acceptance

- `cargo build` succeeds. `cargo clippy --all-targets -- -D warnings` succeeds
  (`#[allow(dead_code)]` at module level is acceptable in this package only).
- `cargo test` runs and passes: the only tests are `tests/common` self-checks
  (`fixture_lines("codex-stream-sample.jsonl").len() == 5`, `tmp_repo()` yields a dir where
  `git rev-parse HEAD` succeeds).
- `swamp --help`, `swamp run --help`, `swamp trace --help` print without panicking. `swamp --help`
  must not list `mcp-bridge`.
- `grep -rn 'todo!' src/ | wc -l` is reported in the handoff, per module.
- Every type in DESIGN.md sections 4, 5.2, 6.1 and 7.2 exists with the documented field names.

---

## WP1 - Core: config, errors, model impls

**Goal:** the domain layer works and config loads. Zero IO except reading config files. No tokio.

### Files owned

```
src/ids.rs
src/error.rs
src/model/mod.rs
src/model/core.rs
src/model/failure.rs
src/model/node.rs
src/model/event.rs
src/model/result.rs
src/config/mod.rs
src/config/schema.rs
src/config/load.rs
src/config/resolve.rs
src/config/validate.rs
tests/config_load.rs
```

### Public interface

```rust
// config/mod.rs
pub struct Config { /* fields mirror schema.rs */ }

impl Config {
    /// defaults <- ~/.config/swamp/config.toml <- <repo>/.swamp/config.toml <- SWAMP_* env
    /// <- --config <- flag overrides. Reports ALL validation errors at once.
    pub fn load(repo: &Utf8Path, explicit: Option<&Utf8Path>, profile: Option<&str>)
        -> Result<Config, SwampError>;
    pub fn effective_toml(&self) -> String;                 // for `swamp config show --effective`
    pub fn sha256(&self) -> String;                         // journaled in RunStarted

    pub fn model_for(&self, p: Provider, t: Tier, account: Option<&AccountId>)
        -> Result<String, SwampError>;
    pub fn tier_extra(&self, p: Provider, t: Tier) -> BTreeMap<String, String>;
    pub fn account(&self, id: &AccountId) -> Option<&AccountCfg>;
    pub fn accounts_for(&self, p: Provider) -> Vec<&AccountCfg>;
    pub fn provider_order(&self, t: Tier) -> Vec<Provider>;
    pub fn failure_patterns(&self, p: Provider) -> Result<FailurePatterns, SwampError>;
    pub fn node_timeout(&self, t: Tier) -> Duration;
    pub fn estimate_cost(&self, model: &str, u: &Usage) -> Option<Cost>;  // basis = Estimated
}

/// Compiled once at load, shared by every classifier call.
pub struct FailurePatterns { /* three RegexSet + the source strings for evidence */ }
impl FailurePatterns {
    pub fn rate_limit_match<'a>(&self, s: &'a str) -> Option<&'a str>;
    pub fn auth_match<'a>(&self, s: &'a str) -> Option<&'a str>;
    pub fn overloaded_match<'a>(&self, s: &'a str) -> Option<&'a str>;
}

// error.rs
pub fn exit_code(e: &anyhow::Error) -> i32;
```

Plus all `impl` blocks on the model types: `Usage::absorb/billable`, `Failure::{rotates_account,
retries_same_account, is_terminal}`, `RateLimitSnapshot::{worst_utilization, soonest_reset,
worst_scope}`, `NodeState::is_terminal`, `WorkspaceRef::path`, `NodeRecord::duration`,
`Tier`/`Provider` `FromStr` + `Display`, ULID id newtypes.

### Depends on

WP0 only.

### Acceptance

- `Config::load` on `swamp.example.toml` succeeds and `model_for(Anthropic, High, None) == "opus"`.
- Per-account override: `model_for(Anthropic, High, Some("alt")) == "sonnet"`.
- `model_for` on an unmapped tier returns `SwampError::TierUnmapped`, not a panic or a default.
- Layering: a repo-level file overriding `limits.max_nodes_per_run` wins over the user-level file,
  and `SWAMP_LIMITS__MAX_NODES_PER_RUN=9` wins over both.
- `--profile cheap` applies dotted-key overrides.
- Validation reports **all** errors in one message. Test at least: duplicate account id;
  `brain.account` belonging to the wrong provider; `quota_warn_at >= quota_stop_at`;
  `max_concurrency = 0`; an unparseable regex; a `--dangerously-skip-permissions` in `worker.args`
  without `unsafe_ack`. Assert the message names each offending key.
- `Usage::absorb` is commutative and associative over three random usages (proptest).
- Failure predicates: exactly `RateLimited` and `AuthExpired` return true from `rotates_account`;
  exactly `Overloaded`, `Crashed`, `Truncated` from `retries_same_account`; the three sets are
  pairwise disjoint and together cover every variant (exhaustive match test that fails to compile
  if a variant is added without classifying it).
- `RateLimitSnapshot::worst_utilization` on `{five_hour: 0.06, seven_day: 0.64}` returns `0.64`.
- `estimate_cost` returns `None` when no `[pricing]` row exists, never `Some(0.0)`.
- Round-trip: every model type serializes and deserializes identically (insta snapshots for
  `NodeRecord`, `Failure`, `WorkerEvent`).

---

## WP2 - Journal

**Goal:** append-only JSONL with one writer, a tolerant reader, a deterministic fold, and raw sinks.

### Files owned

```
src/journal/mod.rs
src/journal/record.rs
src/journal/writer.rs
src/journal/reader.rs
src/journal/fold.rs
src/journal/raw.rs
src/journal/paths.rs
tests/journal_fold.rs
```

### Public interface

```rust
// paths.rs
pub struct Paths { /* repo root, .swamp, ~/.swamp */ }
impl Paths {
    pub fn discover(cwd: &Utf8Path) -> Result<Paths, SwampError>;   // walks up to the git root
    pub fn run_dir(&self, run: RunId) -> Utf8PathBuf;
    pub fn worktree_root(&self) -> Utf8PathBuf;         // ~/.swamp/worktrees/<repo>-<hash8>
    pub fn accounts_state(&self) -> Utf8PathBuf;        // ~/.swamp/accounts.json
    pub fn ensure_git_excluded(&self) -> anyhow::Result<()>;  // .git/info/exclude, not .gitignore
    pub fn list_runs(&self) -> anyhow::Result<Vec<RunId>>;    // newest first
    pub fn resolve_run(&self, spec: &str) -> anyhow::Result<RunId>;  // id | prefix | "last" | "-2"
}

pub struct RunPaths { pub run: RunId, /* ... */ }
impl RunPaths {
    pub fn journal(&self) -> Utf8PathBuf;
    pub fn node_dir(&self, n: NodeId) -> Utf8PathBuf;
    pub fn prompt(&self, n: NodeId) -> Utf8PathBuf;
    pub fn stream(&self, n: NodeId) -> Utf8PathBuf;
    pub fn stderr(&self, n: NodeId) -> Utf8PathBuf;
    pub fn noise(&self, n: NodeId) -> Utf8PathBuf;
    pub fn last_message(&self, n: NodeId) -> Utf8PathBuf;
    pub fn patch(&self, n: NodeId) -> Utf8PathBuf;
    pub fn pidfile(&self, n: NodeId) -> Utf8PathBuf;
    pub fn socket(&self) -> Utf8PathBuf;
    pub fn link_last(&self) -> anyhow::Result<()>;
}

// mod.rs
#[derive(Clone)]
pub struct JournalHandle { /* ... */ }
impl JournalHandle {
    pub fn emit(&self, node: Option<NodeId>, event: JournalEvent);
    pub async fn emit_durable(&self, node: Option<NodeId>, event: JournalEvent) -> anyhow::Result<u64>;
    pub fn run(&self) -> RunId;
    pub fn paths(&self) -> &RunPaths;
}

pub struct Journal;
impl Journal {
    pub async fn open(paths: RunPaths, policy: FsyncPolicy, redact: &[String])
        -> anyhow::Result<(JournalHandle, tokio::task::JoinHandle<()>)>;
}

// raw.rs
pub struct RawSink { /* ... */ }
impl RawSink {
    pub async fn open(paths: &RunPaths, node: NodeId, redact: Arc<Redactor>) -> anyhow::Result<Self>;
    pub async fn noise(&mut self, line: &str);
    pub async fn stderr_line(&mut self, line: &str);
    pub async fn flush(&mut self) -> anyhow::Result<()>;
}
pub struct Redactor;
impl Redactor {
    pub fn new(patterns: &[String]) -> anyhow::Result<Self>;
    pub fn apply<'a>(&self, s: &'a str) -> std::borrow::Cow<'a, str>;
}

// reader.rs
pub fn replay<P: Projection>(journal: &Utf8Path, p: P) -> anyhow::Result<P::Out>;
pub struct Tailer { /* path, offset */ }
impl Tailer {
    pub fn open(journal: &Utf8Path) -> anyhow::Result<Self>;
    /// Complete lines only; a partial trailing line is held back until its newline arrives.
    pub async fn poll(&mut self) -> anyhow::Result<Vec<JournalLine>>;
}

// fold.rs
pub trait Projection { type Out; fn apply(&mut self, l: &JournalLine); fn finish(self) -> Self::Out; }
pub struct RunView { /* DESIGN.md 7.4 */ }
impl RunView {
    pub fn apply(&mut self, l: &JournalLine);
    pub fn load(dir: &Utf8Path, with_events: bool) -> anyhow::Result<Self>;
    pub fn mark_orphans(&mut self, alive: &dyn Fn(NodeId) -> bool);
    pub fn tree(&self) -> Vec<TreeRow>;
    pub fn totals(&self) -> Totals;
}
/// Byte-budgeted compact rendering for the brain's swamp_status tool. Same fold, different output.
pub struct LlmDigest { pub max_bytes: usize }
impl Projection for LlmDigest { type Out = String; /* ... */ }
```

### Depends on

WP1 (types, `SwampError`).

### Acceptance

- Writer/reader round trip: emit 1000 mixed events from 8 concurrent tasks, close, replay. `seq` is
  dense `0..N` and strictly increasing; every event is present exactly once.
- `emit` never blocks and never returns an error. Dropping the writer task only logs.
- `emit_durable` returns only after the line is on disk: kill the process with `SIGKILL` right after
  the await resolves (subprocess test) and the line is still there.
- **Torn tail:** write a valid journal, truncate the file mid-line, reopen. `Journal::open` repairs
  by truncating back to the last newline and continues `seq` from there rather than restarting at 0.
  `replay` on the same damaged file skips the partial line, reports it, and does not error.
- **Fold determinism (proptest):** for any prefix of a generated line list, `RunView` built by
  folding the whole prefix equals `RunView` built by folding it in two halves. Folding the same
  prefix twice yields identical output.
- Retry chains: three `NodeSpawned` sharing one `logical` with `retry_of` links produce one
  `TreeRow` with `attempts.len() == 3` in creation order.
- `mark_orphans` turns a `Running` node with no `NodeFinished` and a dead pid into `Orphaned`, and
  leaves a live one alone.
- Cost: a view with one `Reported` node and one node with `None` has `cost_complete == false` and
  `cost_usd` equal to the single reported value. Never `0.0` for unknown.
- `Tailer` returns historical lines first, then new ones. A line written in two `write` calls is
  returned once, complete, after the newline lands.
- `Redactor` masks `ANTHROPIC_API_KEY=sk-ant-...` and a `Bearer <token>` in both the journal and
  the raw sink. Assert the secret's bytes are absent from both files.
- `Paths::ensure_git_excluded` appends to `.git/info/exclude`, is idempotent, and never touches a
  tracked `.gitignore`.
- `resolve_run` handles a full id, a unique prefix, an ambiguous prefix (error naming candidates),
  `last`, and `-2`.
- `LlmDigest` output on a 30-node tree stays under `max_bytes` and still names every failed node.

---

## WP3 - Worker protocol

**Goal:** both adapters build correct argv, parse the real sample streams, and classify outcomes.
Detached spawn and offset-resume follow.

### Files owned

```
src/worker/mod.rs
src/worker/adapter.rs
src/worker/spawn.rs
src/worker/follow.rs
src/worker/liveness.rs
src/worker/claude.rs
src/worker/codex.rs
src/worker/classify.rs
tests/parse_claude.rs
tests/parse_codex.rs
tests/spawn_detached.rs
```

### Public interface

```rust
// adapter.rs: LaunchSpec, SessionPlan, McpAttach, ParseState, ParseOutput, ExitContext,
// Capability, BrainTransport, ProviderAdapter - all exactly as DESIGN.md 5.2.

pub fn adapter_for(p: Provider) -> Arc<dyn ProviderAdapter>;

// spawn.rs
pub struct Detached { pub pid: i32, pub pgid: i32, pub started_at: OffsetDateTime }
pub struct NodeIo { pub node: NodeId, pub prompt: Utf8PathBuf, pub stdout: Utf8PathBuf,
                    pub stderr: Utf8PathBuf, pub pidfile: Utf8PathBuf, pub depth: u32 }
pub fn spawn_detached(argv: &[OsString], env: &[(OsString, OsString)],
                      cwd: &Utf8Path, io: &NodeIo) -> anyhow::Result<Detached>;
/// SIGTERM to -pgid, wait `grace`, then SIGKILL. Reaps the worker's own grandchildren.
pub async fn terminate(pgid: i32, grace: Duration) -> anyhow::Result<()>;

// liveness.rs
pub fn write_pidfile(path: &Utf8Path, pid: i32) -> anyhow::Result<()>;   // pid + start ticks
/// PID-reuse safe: compares the recorded process start time, not just the pid.
pub fn is_ours(path: &Utf8Path) -> bool;
pub fn wait_exit(pid: i32, poll: Duration) -> impl Future<Output = Option<ExitInfo>>;

// follow.rs
pub async fn follow(node: NodeId, path: &Utf8Path, offset: u64,
                    adapter: Arc<dyn ProviderAdapter>, st: &mut ParseState,
                    sink: &mut RawSink, out: mpsc::Sender<(NodeId, WorkerEvent, u64)>,
                    alive: Arc<dyn Fn() -> bool + Send + Sync>) -> anyhow::Result<u64>;

// mod.rs - the seam WP4 calls
pub struct RunOutcome {
    pub failure: Option<Failure>, pub exit: Option<ExitInfo>,
    pub session: Option<SessionHandle>, pub usage: Usage, pub cost: Option<Cost>,
    pub summary: Option<String>, pub files: Vec<FileChange>,
    pub rate_limit: Option<RateLimitSnapshot>,
    pub stream_offset: u64, pub unparsed_lines: u32, pub permission_denials: u32,
}
pub struct Executor { /* adapters, journal, paths, patterns */ }
impl Executor {
    pub fn new(journal: JournalHandle, cfg: Arc<Config>) -> Self;
    pub fn adapter(&self, p: Provider) -> Arc<dyn ProviderAdapter>;
    /// Writes prompt.md, spawns detached, journals ProcessStarted, follows to the terminal
    /// event, classifies, returns. Resumable via `resume_from`.
    pub async fn run(&self, spec: &LaunchSpec, timeout: Duration,
                     cancel: CancellationToken) -> anyhow::Result<RunOutcome>;
    pub async fn resume_from(&self, spec: &LaunchSpec, pid: i32, offset: u64)
        -> anyhow::Result<RunOutcome>;
}

// classify.rs
pub fn classify(cx: &ExitContext<'_>) -> Option<Failure>;
pub const MAX_LINE: usize = 8 * 1024 * 1024;
```

### Depends on

WP1 (types, `FailurePatterns`), WP2 (`RawSink`, `JournalHandle`, `RunPaths`).

### Acceptance

**Argv (assert exact vectors, no network):**

- Claude worker argv contains `-p`, `--output-format stream-json`, `--verbose`, `--model <cfg>`,
  `--session-id <uuid>`, `--permission-mode`, `--permission-prompts none`, `--strict-mcp-config`.
- Claude worker argv contains **no** `--mcp-config` and **no** prompt positional argument. The prompt
  is never on argv for either provider: assert with a 2 MB prompt that argv length stays under 4 KB.
- Codex worker argv contains `exec`, `--json`, `-m`, `-C`, `-s`, `-o`, `-c approval_policy="never"`
  and ends with `-`.
- **Codex argv must never contain `-a` or `--ask-for-approval`.** That flag is top-level only;
  `codex exec -a never` fails argv parsing. Assert its absence explicitly, with the reason in the
  test name.
- Argv contains **no** `--append-system-prompt-file`. When a system prompt file is configured, the
  argv carries `--append-system-prompt <text>` with the file's contents.
- Brain argv adds `--input-format stream-json`, `--mcp-config`, `--strict-mcp-config`,
  `--include-partial-messages`; worker argv does not.
- `--resume` appears only for `SessionPlan::Resume`.
- Codex brain argv carries `-c mcp_servers.swamp.command=...` and `-c mcp_servers.swamp.args=[...]`.
- `--dangerously-skip-permissions` in `worker.args` is dropped with an error when
  `unsafe_ack = false`, and passed through when true.

**Parsing (golden, against `docs/ref/*.jsonl`):**

- `claude-stream-sample.jsonl` yields, in order: `SessionStarted { auth_hint: Some("none") }`,
  `RateLimit`, `AssistantText`, `Usage`, `RateLimit`, `Final`.
- The `rate_limit_event` produces **two** `LimitWindow`s: `FiveHour 0.06` and `SevenDay 0.64`.
  `worst_utilization() == 0.64`. Collapsing to one window is a bug.
- `Final` carries `cost == Some(Cost { usd: 0.153239, basis: Reported })`, `ok == true`,
  `subtype == "success"`, `permission_denials == 0`, and usage
  `input 2 / cache_read 10480 / cache_write 7472 / output 4`.
- **A synthetic assistant message with a text block AND a tool_use block yields two events.**
  Returning only the first is the exact bug this arity exists to prevent.
- A synthetic `result` with `permission_denials: [{...}]` classifies as
  `Failure::PermissionDenied { denials: 1 }` even when `subtype == "success"`.
- `codex-stream-sample.jsonl`: line 1 (`Reading additional input from stdin...`) yields
  `noise: true`, zero events, and does not error. Lines 2..5 yield `SessionStarted` (thread id
  `01a0a1ec-...`), `AssistantText { text: "pong" }`, `Final` with `cost == None` and usage
  `input 15300 / cached 12160 / output 5`.
- An unknown `"type"` on either side yields `WorkerEvent::Unknown`, increments `unparsed`, and never
  errors. Adding an unknown field to a known type is ignored.
- A 20 MB single line is truncated at `MAX_LINE`, counted as unparsed, and does not allocate the
  whole line twice (assert peak by construction, not by measurement: the reader caps before parse).
- Codex `item.completed` with `file_change` yields `FileChanged` per change with
  `EvidenceSource::EventStream`.

**Classification:**

- Telemetry first: `rate_limit_info.status = "rejected"` yields `RateLimited` with
  `Detector::Telemetry`, even when the exit code is 0.
- `api_error_status = 429` yields `RateLimited { detected_by: StructuredResult }`; `529` yields
  `Overloaded`; `subtype = "error_max_budget_usd"` yields `WorkerError { subtype: "error_max_budget_usd", .. }`.
- A configured regex matching the final text yields `Detector::Pattern` with the matching substring
  as `evidence`, truncated to 400 bytes.
- No terminal event + signal yields `Crashed`; no terminal event + exit 0 yields `Truncated`;
  exit 127 yields `AuthExpired { detected_by: ExitCode }`.
- `subtype = "error_during_execution"` yields `WorkerError`, whose `rotates_account()` is false.
  This is the guard against a bad prompt draining every subscription; assert it directly.

**Spawn and follow:**

- `spawn_detached` with a script that prints to stdout, sleeps, then prints again: stdout lands in
  the file, `follow` from offset 0 sees both lines, and the returned offset equals the file length.
- Kill the *parent* test task mid-run: the child keeps running and keeps writing (assert the file
  grows after the follower is dropped). This is the whole point of the detached model.
- Restart: run `follow` to a mid-file offset, drop it, start a new `follow` from that offset. No
  event is duplicated and none is lost.
- `terminate` on a script that spawns its own child kills both (assert the grandchild's pid is gone).
- `is_ours` returns false for a recycled pid: write a pidfile with a live pid and a bogus start time.
- `run` honours the timeout: a script that never exits is terminated and yields
  `Failure::Timeout`, and the process group is gone afterwards.

---

## WP4 - Dispatch and accounts

**Goal:** the account pool, selection, cooldowns and the attempt loop. This package owns the policy
that protects the user's quota.

### Files owned

```
src/dispatch/mod.rs
src/dispatch/account.rs
src/dispatch/pool.rs
src/dispatch/policy.rs
src/dispatch/cooldown.rs
src/dispatch/persist.rs
src/dispatch/retry.rs
tests/dispatch_failover.rs
tests/account_pool.rs
```

### Public interface

```rust
// pool.rs
pub struct AccountPool { /* ... */ }
pub struct Lease { pub account: AccountId, pub exec: String, pub env: BTreeMap<String, String> }
pub enum NoCapacity { AllExhausted { retry_at: OffsetDateTime, why: String }, Saturated, Exhausted { reason: String }, Cancelled }

impl AccountPool {
    pub fn new(cfg: Arc<Config>, state_path: Utf8PathBuf, journal: JournalHandle) -> anyhow::Result<Arc<Self>>;
    pub async fn acquire(self: &Arc<Self>, provider: Provider, exclude: &HashSet<AccountId>,
                         deadline: Instant) -> Result<Lease, NoCapacity>;
    pub fn report(&self, id: &AccountId, failure: Option<&Failure>, cost: Option<Cost>);
    pub fn observe_quota(&self, id: &AccountId, snap: RateLimitSnapshot);
    pub fn snapshot(&self) -> Vec<(Provider, AccountId, AccountState)>;
    pub fn set_enabled(&self, id: &AccountId, on: bool);
    pub fn clear(&self, id: &AccountId);
    pub fn cooldown(&self, id: &AccountId, d: Duration, why: &str);
    /// Reserve one healthy account of `p` for the brain, excluded from worker selection.
    pub fn reserve_for_brain(&self, p: Provider) -> Option<AccountId>;
}

// persist.rs - cross-run, cross-repo, fs4-locked, temp-write-and-rename
pub fn load_state(path: &Utf8Path) -> anyhow::Result<BTreeMap<AccountId, AccountState>>;
pub fn save_state(path: &Utf8Path, s: &BTreeMap<AccountId, AccountState>) -> anyhow::Result<()>;

// cooldown.rs
pub fn cooldown_for(f: &Failure, consecutive: u32, cfg: &CooldownCfg, now: OffsetDateTime) -> Option<Duration>;

// mod.rs
pub struct Dispatcher { /* pool, executor, workspace, journal, semaphores, cfg */ }
impl Dispatcher {
    pub fn new(cfg: Arc<Config>, pool: Arc<AccountPool>, exec: Arc<Executor>,
               ws: Arc<WorkspaceManager>, journal: JournalHandle) -> Arc<Self>;
    pub async fn dispatch_batch(self: &Arc<Self>, parent: NodeId, tasks: Vec<TaskRequest>,
                                max_wait: Duration) -> Vec<NodeResult>;
    pub async fn dispatch_one(self: &Arc<Self>, parent: NodeId, task: TaskRequest) -> NodeResult;
    pub async fn await_nodes(&self, ids: &[NodeId], timeout: Option<Duration>) -> Vec<NodeResult>;
    pub async fn cancel(&self, id: NodeId) -> anyhow::Result<()>;
    pub fn result(&self, id: NodeId) -> Option<NodeResult>;
    pub fn pool(&self) -> &Arc<AccountPool>;
}

// retry.rs
pub struct NodeCtx { /* ... */ }
pub async fn run_node(cx: &NodeCtx, spec: LaunchSpec, task: &TaskRequest) -> NodeOutcome;
```

### Depends on

WP1, WP2, WP3 (`Executor`, `LaunchSpec`), WP5 (`WorkspaceManager`).

Tests use an in-process fake `ProviderAdapter` and a fake `Executor` trait object, not real CLIs.
To make that possible, `Executor::run` is reached through a small `trait NodeRunner` that `NodeCtx`
holds; WP3 provides the real impl, WP4's tests provide a scripted one.

### Acceptance

- **Failover:** a fake runner returning `RateLimited` on `main` then success on `alt` produces two
  nodes sharing one `logical`, the second with `retry_of == Some(first)`, and `main` cooling.
- **No failover on task failure:** a fake returning `WorkerError` produces exactly **one** node and
  leaves every account's health `Healthy`. Assert the account was used once. This is the most
  important test in the package.
- **Same-account retry:** `Overloaded` retries on the same account with a backoff gap, resuming the
  same `SessionHandle`. Assert the second attempt's `LaunchSpec.session` is `Resume` with the same
  account, and that a *different* account would have discarded it.
- **Session handle is account-scoped:** force the retry onto a different account and assert the
  handle is dropped and replaced with a fresh `New { preassigned }`. A resumed session under the
  wrong account would silently start a new conversation.
- **Fresh worktree per attempt:** assert `WorkspaceManager::create` is called once per attempt with
  distinct paths, and that attempt 2 never runs in attempt 1's directory.
- **Exhaustion:** every account rate-limited yields `SwampError::NoAccountAvailable`, exit code 3,
  and at most `max_attempts` spawns.
- **Cooldown from provider time:** `RateLimited { resets_at: now + 20m }` sets `cooldown_until` to
  that instant. `resets_at` in the past, or 10 hours out, is clamped into `[min, max]`.
- **Circuit breaker:** `breaker_threshold` consecutive `Crashed` results park the account.
- **Proactive quota stop:** `observe_quota` at `0.99` with `quota_stop_at = 0.98` makes the account
  ineligible for new leases while a running lease is untouched.
- **Selection:** with identical state, `RoundRobin` alternates; `LeastLoaded` picks the account with
  fewer inflight; `QuotaAware` prefers `util 0.1` over `util 0.9` at equal load, and degrades to
  `LeastLoaded` when no quota is known. Default policy is `QuotaAware`.
- **Concurrency:** `max_concurrency = 2` never yields a third simultaneous lease; an account with
  no `max_concurrency` is bounded by its own headroom, not by a machine-wide count. `acquire` does
  not busy-spin (assert bounded wakeups).
- **Lease drop:** a panicking task releases its permit and decrements `inflight`.
- **Persistence:** cooldowns written by one `AccountPool` are honoured by a fresh one constructed
  from the same path. Two pools writing concurrently do not corrupt the file (`fs4` lock + rename);
  assert a valid parse after 100 interleaved saves.
- **Brain reservation:** with `reserve_brain_slot = true` and one healthy Anthropic account, a worker
  batch cannot take it.
- **Caps:** `max_nodes_per_run` and `max_depth` are enforced in the dispatcher and return a clear
  `NodeResult` failure rather than spawning. A `TaskRequest` with non-empty `deps` is rejected with
  a message naming v1's limitation.
- `dispatch_batch` returns within `max_wait` with still-running nodes marked `"running"`, never
  blocking forever.

---

## WP5 - Workspace and git

**Goal:** worktree isolation, authoritative diffs, and adoption into the user's checkout.

### Files owned

```
src/workspace/mod.rs
src/workspace/git.rs
src/workspace/worktree.rs
src/workspace/diff.rs
src/workspace/adopt.rs
tests/workspace_git.rs
```

### Public interface

```rust
// git.rs - thin async wrapper over the `git` CLI. Porcelain only, -z everywhere.
pub struct Git { pub root: Utf8PathBuf }
impl Git {
    pub async fn discover(cwd: &Utf8Path) -> Result<Git, SwampError>;
    pub async fn run(&self, cwd: &Utf8Path, args: &[&str]) -> anyhow::Result<String>;
    pub async fn head(&self) -> anyhow::Result<String>;
    pub async fn is_clean(&self) -> anyhow::Result<bool>;
    pub async fn stash_create(&self) -> anyhow::Result<Option<String>>;   // --include-dirty base
    pub async fn version(&self) -> anyhow::Result<(u32, u32)>;
}

// mod.rs
pub struct WorkspaceManager { /* git, paths, cfg, global mutex over .git/worktrees */ }
pub struct NodeWorktree { pub node: NodeId, pub path: Utf8PathBuf, pub branch: String, pub base: String }

impl WorkspaceManager {
    pub async fn new(git: Git, paths: Arc<Paths>, cfg: Arc<Config>, journal: JournalHandle)
        -> anyhow::Result<Arc<Self>>;
    pub async fn base_commit(&self, requested: Option<&str>, include_dirty: bool) -> anyhow::Result<String>;
    /// `git worktree add --detach <base>` then `git switch -c swamp/<run>/<node>-<attempt>`.
    /// Applies link/copy seeds and runs post_create. Serialized behind a mutex + flock.
    pub async fn create(&self, logical: NodeId, attempt: u32) -> anyhow::Result<NodeWorktree>;
    /// Commits leftover dirt, then `git diff base..HEAD`. Git is authoritative for files touched.
    pub async fn finalize(&self, wt: &NodeWorktree, title: &str, tier: Tier)
        -> anyhow::Result<Option<WorkResultRef>>;
    pub async fn remove(&self, wt: &NodeWorktree, force: bool) -> anyhow::Result<()>;
    pub async fn prune(&self) -> anyhow::Result<u32>;
    pub async fn list(&self) -> anyhow::Result<Vec<NodeWorktree>>;
    /// Shared isolation: at most one process mutating the user's real tree.
    pub async fn shared_lock(&self) -> tokio::sync::OwnedMutexGuard<()>;
}

// diff.rs
pub struct DiffSummary { pub files: Vec<FileChange>, pub insertions: u32, pub deletions: u32,
                         pub head: String, pub patch: Utf8PathBuf, pub empty: bool }
pub async fn collect(git: &Git, wt: &Utf8Path, base: &str, out: &Utf8Path) -> anyhow::Result<DiffSummary>;

// adopt.rs
pub enum MergeStrategy { Apply, Merge, CherryPick }
pub enum AdoptResult { Clean { commit: Option<String> }, Conflicted { paths: Vec<Utf8PathBuf> },
                       Rejected { reason: String } }
pub async fn adopt(git: &Git, work: &WorkResultRef, strategy: MergeStrategy,
                   into: Option<&str>, force: bool, dry_run: bool) -> anyhow::Result<AdoptResult>;
```

### Depends on

WP1, WP2 (`Paths`, `JournalHandle`).

### Acceptance

All tests use `tests/common::tmp_repo()`. No network.

- `create` produces a worktree at `~/.swamp/worktrees/...` (outside the repo), on branch
  `swamp/<run>/<node>-<attempt>`, checked out at the pinned base. Assert the path is **not** under
  the repo root.
- Two concurrent `create` calls both succeed. Serialization matters: concurrent
  `git worktree add` contends on `.git/worktrees` and the index lock and fails intermittently
  without it. Run 8 concurrent creates and assert 8 successes.
- `link` symlinks the named directories from the main tree; `copy` copies the named files.
  A `link` entry that does not exist in the main tree is skipped with a warning, not an error.
- `post_create` runs once in the fresh worktree and its non-zero exit fails `create` with a clear
  message.
- `finalize` on a worktree where a file was edited returns `FileChange` entries with
  `EvidenceSource::Git` and correct `+added/-removed` from `--numstat`, and writes a patch that
  `git apply --check` accepts.
- `finalize` detects a file created by a shell heredoc (never touched by an Edit tool), proving git
  is authoritative rather than the event stream.
- `finalize` with no changes returns `empty: true` and does not create a commit.
- Rename and delete are classified as `ChangeKind::Rename` / `Delete` from `--name-status`.
- Paths with spaces and UTF-8 names survive (`-z` parsing).
- `base_commit(None, include_dirty = true)` on a dirty tree returns a `stash create` sha that
  contains the uncommitted change; with `include_dirty = false` and `require_clean = true`, `create`
  returns `SwampError::DirtyTree`.
- `adopt` with `Apply` on a clean tree applies the patch; on a dirty tree it refuses unless `force`.
- `adopt` with `Merge` on two worktrees that touched the same line returns
  `Conflicted { paths }` naming the file, and leaves the repo in a state the user can abort.
  `dry_run` reports the same conflict and changes nothing (assert `git status` is unchanged).
- `NotAGitRepo` is returned loudly for a non-git directory rather than silently sharing a tree.
- `prune` removes worktrees for finished runs, and refuses one with uncommitted changes unless
  forced.

---

## WP6 - MCP server, bridge, and brain

**Goal:** the brain runs as a CLI subprocess and can dispatch work through Swamp's tools.

### Files owned

```
src/mcp/mod.rs
src/mcp/jsonrpc.rs
src/mcp/server.rs
src/mcp/tools.rs
src/mcp/bridge.rs
src/brain/mod.rs
src/brain/claude.rs
src/brain/codex.rs
src/brain/prompt.rs
tests/mcp_protocol.rs
tests/brain_session.rs
```

### Public interface

```rust
// jsonrpc.rs
pub struct Request { pub id: Option<Value>, pub method: String, pub params: Value }
pub struct Response { pub id: Option<Value>, pub result: Result<Value, RpcError> }
pub struct RpcError { pub code: i32, pub message: String }
pub fn parse_line(s: &str) -> Result<Request, RpcError>;
pub fn encode(r: &Response) -> String;

// mod.rs
pub struct McpServer { /* listener, registry */ }
impl McpServer {
    /// Binds a UDS at paths.socket() with 0600 in a 0700 dir; removes it on drop.
    pub async fn bind(paths: &RunPaths, disp: Arc<Dispatcher>, view: Arc<JournalHandle>)
        -> anyhow::Result<(Self, Utf8PathBuf)>;
    pub fn serve(self) -> tokio::task::JoinHandle<()>;
    /// The JSON handed to `claude --mcp-config`, using std::env::current_exe(), never "swamp".
    pub fn mcp_config_json(socket: &Utf8Path) -> String;
    pub fn codex_config_args(socket: &Utf8Path) -> Vec<String>;   // -c mcp_servers.swamp.*
}

// tools.rs - plain async fns plus hand-written JSON Schemas. Depends on dispatch + journal,
// and on NOTHING in brain/. That is what keeps dispatch from deadlocking against the brain.
pub fn schemas() -> Vec<ToolSchema>;
pub async fn call(disp: &Arc<Dispatcher>, name: &str, args: Value) -> Result<Value, RpcError>;
/// Worker output is attacker-influenced data. Truncate and wrap before it reaches the brain.
pub fn wrap_untrusted(node: NodeId, text: &str, max_bytes: usize) -> String;

// bridge.rs
pub async fn run_bridge(socket: &Utf8Path) -> anyhow::Result<()>;   // stdio <-> UDS, zero logic

// brain/mod.rs
pub enum BrainEvent { Ready { session: String, model: String }, Text { delta: String },
                      Thinking { delta: String }, ToolCall { name: String, preview: String },
                      ToolDone { name: String, ok: bool },
                      TurnDone { usage: Usage, cost: Option<Cost> }, Fatal { message: String } }

#[async_trait]
pub trait Brain: Send {
    async fn start(&mut self) -> anyhow::Result<()>;
    async fn send(&mut self, text: &str) -> anyhow::Result<()>;
    fn events(&mut self) -> &mut mpsc::Receiver<BrainEvent>;
    async fn interrupt(&mut self) -> anyhow::Result<()>;
    async fn shutdown(self: Box<Self>) -> anyhow::Result<()>;
    fn session(&self) -> Option<&SessionHandle>;
}
pub fn build(cfg: &Config, lease: Lease, paths: &RunPaths, socket: &Utf8Path,
             journal: JournalHandle, resume: Option<SessionHandle>) -> anyhow::Result<Box<dyn Brain>>;

// prompt.rs
pub fn system_prompt(cfg: &Config) -> String;
```

### Depends on

WP1, WP2, WP3 (adapters build the brain argv), WP4 (`Dispatcher`).

### Acceptance

- JSON-RPC: `initialize`, `notifications/initialized`, `tools/list`, `tools/call` round trip over a
  socket pair. Malformed JSON yields `-32700`; unknown method `-32601`; bad params `-32602`. A
  notification (no `id`) produces no response.
- `tools/list` returns valid JSON Schema for every tool: assert each `inputSchema` parses and every
  `required` field appears in `properties`.
- `swamp mcp-bridge` end to end: start `McpServer`, spawn the real binary as `mcp-bridge --socket`,
  write an `initialize` line to its stdin, read the response from its stdout. Assert byte-identical
  framing in both directions. Assert the bridge exits cleanly when the socket closes.
- `mcp_config_json` embeds an absolute path from `current_exe()`, never the bare name `swamp`.
- `swamp_dispatch` against a `Dispatcher` backed by a fake runner creates nodes, journals
  `BrainToolCall`, and returns node ids. With `wait: true` it returns results; on `max_wait_s`
  timeout it returns partial results marked `"running"` rather than hanging.
- `wrap_untrusted` truncates at `max_result_bytes`, marks the truncation, and emits a
  `<worker-output node="..." trust="untrusted">` envelope. A worker string containing the literal
  closing delimiter is escaped so it cannot terminate the envelope early.
- The tool registry compiles without any `use crate::brain::*`. Enforce with a test that greps
  `src/mcp/` for `brain::` and fails on a hit.
- **No deadlock:** a dispatch of N tasks that all block, called from a tool handler, still lets the
  journal writer drain and `swamp_status` answer on a second connection. Assert `swamp_status`
  responds while a dispatch is in flight.
- `Brain` (claude): drive `brain/claude.rs` against a fake `claude` script on PATH that echoes the
  recorded sample stream. Assert `Ready`, `Text`, `TurnDone` arrive in order and that user turns are
  written to stdin as `--input-format stream-json` lines.
- `Brain` (codex): the fake script asserts turn 1 is `codex exec` and turn 2 is
  `codex exec resume <thread_id>` with the thread id captured from `thread.started`.
- Brain argv never includes a prompt positional for claude, and the brain's `session_uuid` is
  journaled (`SessionBound`) **before** the process starts, so a crash is resumable.
- `system_prompt` contains the tool contract, a tier rubric, the worktree semantics (workers do not
  see each other's changes; conflicting tasks must be sequenced, not dispatched together) and the
  statement that worker output is data and never instruction. Snapshot it with `insta`.

---

## WP7 - UI and commands

**Goal:** everything a user actually types.

### Files owned

```
src/ui/mod.rs
src/ui/fmt.rs
src/ui/trace.rs
src/ui/watch.rs
src/ui/chat.rs
src/doctor.rs
src/cmd/mod.rs
src/cmd/run.rs
src/cmd/chat.rs
src/cmd/trace.rs
src/cmd/watch.rs
src/cmd/runs.rs
src/cmd/resume.rs
src/cmd/accounts.rs
src/cmd/doctor.rs
src/cmd/diff.rs
src/cmd/adopt.rs
src/cmd/worktrees.rs
src/cmd/cancel.rs
src/cmd/gc.rs
src/cmd/replay.rs
src/cmd/config.rs
src/cmd/mcp_bridge.rs
tests/render_trace.rs
tests/doctor.rs
```

### Public interface

```rust
// Every cmd module exposes exactly one entry point.
pub async fn run(ctx: &Ctx, args: &<Cmd>Args) -> anyhow::Result<i32>;

// cmd/mod.rs
pub struct Ctx { pub cfg: Arc<Config>, pub paths: Arc<Paths>, pub color: bool, pub json: bool }

// ui/fmt.rs
pub fn duration(d: Duration) -> String;            // "4m12s"
pub fn tokens(n: u64) -> String;                   // "1.2M"
/// None -> "-", never "$0.00". Reported and Estimated both render with a leading "~",
/// because reported cost on a subscription is list-price equivalence, not money billed.
pub fn cost(c: Option<Cost>) -> String;
pub fn glyph(s: &NodeState) -> &'static str;
pub fn truncate(s: &str, n: usize) -> String;      // unicode-width aware

// ui/trace.rs
pub struct TraceOpts { pub node: Option<NodeId>, pub events: bool, pub raw: bool, pub stderr: bool,
                       pub depth: Option<u32>, pub failed: bool, pub json: bool }
pub fn render(view: &RunView, o: &TraceOpts) -> String;
pub async fn follow(paths: &RunPaths, o: &TraceOpts) -> anyhow::Result<()>;

// ui/watch.rs
pub async fn run_tui(paths: RunPaths, cfg: Arc<Config>) -> anyhow::Result<()>;

// ui/chat.rs
pub async fn repl(brain: Box<dyn Brain>, disp: Arc<Dispatcher>, ctx: &Ctx) -> anyhow::Result<i32>;

// doctor.rs
pub struct Check { pub name: String, pub level: Level, pub detail: String }
pub enum Level { Ok, Note, Warn, Error }
pub async fn checks(cfg: &Config, paths: &Paths, probe: bool, schema: bool) -> Vec<Check>;
pub async fn reap(paths: &Paths) -> anyhow::Result<u32>;
```

### Depends on

WP1 through WP6.

### Acceptance

- `ui::fmt::cost(None) == "-"`, never `"$0.00"`. `cost(Some(Reported 1.84)) == "~$1.84"`.
  `duration` and `tokens` snapshot-tested at boundary values.
- `ui::trace::render` snapshots (insta) over a synthetic `RunView` covering: a brain node, a success,
  a failure with its evidence line, a two-attempt retry chain rendered as one row with both attempts
  and the rate-limit reason, and a node with unknown cost. Assert the footer prints
  `(1 node with no cost data)` when `cost_complete` is false.
- `--json` emits the folded `RunView`, and `trace --node <id> --json` is byte-identical to the
  corresponding entry from the full `--json` output. Same fold, one source of truth.
- `trace --raw` streams `stream.jsonl` verbatim: byte-compare against the file.
- `trace --follow` on a journal being appended to prints new rows without re-printing old ones.
- `cmd::run` with `--no-brain` on a fake CLI produces exactly one node, one worktree, one journal
  with `RunStarted` and `RunFinished`, one patch, and exit code 0. This is the smoke path from
  DESIGN.md section 1 and it must pass before anything else in this package is considered done.
- `cmd::run` maps outcomes to exit codes: node failure 4, no capacity 3, config invalid 2, Ctrl-C 6.
- Ctrl-C during a run journals `RunFinished` and `killpg`s every live node. Assert no orphan
  process group survives.
- `cmd::resume --plan` prints the `Recovery` plan and spawns nothing. Assert zero processes started.
- `cmd::accounts` renders the table with blank 5H/7D columns when the account has no quota source at
  all, rather than zeros. `--json` is stable.
- `doctor::checks` on a good fixture returns zero `Error`s. Injected faults each produce exactly one
  failing check with a message naming the fix:
  - a missing exec on PATH,
  - a tier with no model,
  - `.swamp` not writable,
  - **two accounts whose execs resolve to the same binary with the same effective config dir**
    (this is `Level::Error`, and the message must say the two accounts are one subscription),
  - a `--dangerously-*` flag without `unsafe_ack`,
  - a heavy build directory with an empty `workspace.link`.
- `doctor --schema` replays the two fixture streams through both adapters and fails if the
  unparsed-line ratio or the `Detector::Pattern` rate exceeds its threshold.
- `watch`: headless test constructs the `App` state from a journal and asserts the tree pane rows
  and the account gauges. Rendering is tested via `ratatui::backend::TestBackend`, not a real tty.
  Assert the `TerminalGuard` `Drop` and the panic hook both call `disable_raw_mode`.
- `gc --dry-run` lists what it would delete and deletes nothing. `gc` refuses a worktree with
  uncommitted changes unless `--force`.
- `replay --reparse` on a run whose journal was deleted reconstructs it from the retained raw
  streams, and the resulting `RunView` equals the original for every node's usage, cost, state and
  files. This is the recovery path for vendor schema drift and it must be exercised.

---

## WP8 - Integration and end-to-end smoke

**Goal:** prove the whole thing works against fake CLIs, with no network and no real subscription.

### Files owned

```
tests/support/mod.rs
tests/support/fake_claude.rs
tests/support/fake_codex.rs
tests/e2e_smoke.rs
tests/e2e_failover.rs
tests/e2e_brain.rs
tests/e2e_recovery.rs
README.md
```

### What WP8 builds

`tests/support` compiles two fake CLIs as extra `[[bin]]` test binaries, placed on a temp `PATH`
under wrapper names (`claude-main`, `claude-alt`, `codex-main`). Each is driven by a scenario file
so a test can script exactly what a "subscription" does:

```rust
pub struct Scenario {
    pub emit: Vec<String>,        // lines written to stdout, verbatim
    pub exit_code: i32,
    pub delay_ms: u64,
    pub edit: Vec<(String, String)>,   // files the fake worker writes into its cwd
    pub expect_argv: Vec<String>,      // assertions on the argv it was given
}
pub fn install_fakes(dir: &Utf8Path, scenarios: &BTreeMap<String, Scenario>) -> Utf8PathBuf; // new PATH
pub struct Harness { /* tempdir repo, config, PATH, home */ }
impl Harness {
    pub fn new() -> Self;
    pub fn with_accounts(self, n_anthropic: usize, n_openai: usize) -> Self;
    pub fn scenario(self, account: &str, s: Scenario) -> Self;
    pub fn swamp(&self, args: &[&str]) -> assert_cmd::Command;
    pub fn view(&self, run: RunId) -> RunView;
}
```

The fakes replay the real recorded sample streams by default, so the e2e path exercises the same
bytes as the golden parser tests.

### Acceptance

Each is a full `swamp` invocation through `assert_cmd`.

1. **Smoke.** `swamp run "task" --no-brain --tier mid` with one fake account. Exit 0. Assert:
   `.swamp/runs/<id>/journal.jsonl` exists with `RunStarted` and `RunFinished`; exactly one worker
   node; the node's `model` equals the configured mid model; `prompt.md` matches the task;
   `stream.jsonl` is byte-identical to the fixture; a worktree existed under `~/.swamp/worktrees`
   and a patch was written; `swamp trace last` renders the node; `.git/info/exclude` contains
   `.swamp/`.
2. **Codex smoke.** The same with `--provider openai`, using the codex fixture including its
   non-JSON first line. Assert `noise.log` has exactly one line and the run still succeeds.
3. **Failover.** `claude-main` emits a rate-limit stream and exits non-zero; `claude-alt` succeeds.
   Assert: two nodes, one `logical`, `retry_of` set, `main` cooling with a `resets_at`-derived
   duration, the second node's argv used `claude-alt`, and `swamp accounts` shows `main` cooling.
   Then assert the cooldown survives: run `swamp accounts` from a **second repo** and `main` is
   still cooling.
4. **No failover on task failure.** The fake emits `result` with
   `subtype: "error_during_execution"`. Assert exactly one node, exit code 4, and that `alt` was
   never invoked. Assert by counting the fake's invocation log.
5. **Parallel isolation.** Three tasks dispatched at once, each fake writing a different file.
   Assert three distinct worktrees, three branches, three patches, and that the user's checkout is
   untouched (`git status --porcelain` empty).
6. **Brain dispatch.** A fake `claude` brain that emits a `tool_use` for `swamp_dispatch` with two
   tasks, then a `result`. Assert the two worker nodes are children of the brain node, the tree has
   the right shape, the brain's stdin received a `stream-json` user turn, and the MCP bridge was
   spawned as an absolute path.
7. **Recovery.** Start a detached worker, `SIGKILL` the `swamp` process mid-stream, then run
   `swamp resume last --plan`. Assert the plan says `Adopt` (the worker is still alive) and names the
   journaled `stream_offset`. Then let the worker finish, run `swamp resume last`, and assert the
   node reaches `Succeeded` with **no duplicated events** in the folded view.
8. **Interrupt.** Send SIGINT during a run. Assert `RunFinished` is journaled, exit code 6, and no
   process group from the run survives.
9. **Permission denials.** The fake emits a `result` with `subtype: "success"` and a non-empty
   `permission_denials`. Assert the node is `Failed(PermissionDenied)`, not `Succeeded`, and that
   `swamp trace` shows the denial count.
10. **Adopt.** After a successful run, `swamp adopt <node>` applies the patch; on a dirty tree it
    refuses; with `--force` it applies. Assert the file content in the user's checkout.
11. **Doctor.** `swamp doctor` on the harness exits 0. With two fake accounts pointing at the same
    config dir it exits non-zero and names both accounts.
12. **Secrets.** A fake worker echoes `ANTHROPIC_API_KEY=sk-ant-fake123` in its output. Assert the
    literal token appears in neither `journal.jsonl` nor `stream.jsonl`.
13. **No network.** Run the full suite with outbound sockets blocked (or assert no `connect` by
    construction: the dependency tree contains no HTTP client, checked by a `cargo tree` test).

`README.md`: install, the wrapper-script pattern for multi-account, the `swamp doctor` first run,
the smoke command, and a short, plain statement that the worktree is a directory and not a sandbox.

---

## Sequencing

| Wave | Packages | Notes |
|---|---|---|
| 1 | WP0 | Blocks everything. Mechanical; do not embellish. |
| 2 | WP1 | Blocks everything else. Types are already real from WP0, so this is impls plus config. |
| 3 | WP2, WP3, WP5 | Fully parallel. No shared files. |
| 4 | WP4 | Starts in wave 3 against stubs; finishes when WP3 and WP5 land. |
| 5 | WP6, WP7 | WP7 starts on `fmt`/`trace`/`doctor` in wave 3. |
| 6 | WP8 | Needs real behaviour everywhere. |

**Definition of done for the milestone:** WP8 tests 1 through 5 pass. Tests 6 through 13 are the
second milestone. The order matters: `swamp run --no-brain` working end to end is worth more than a
half-finished brain, and everything else is built on that path.
