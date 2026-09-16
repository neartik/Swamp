# Swamp - Usage accounting and usage-aware dispatch

Change spec. Three user-visible outcomes:

1. No global parallelism cap and no budget cap anywhere in Swamp.
2. A `/usage` chat command and a `swamp usage` CLI showing per-account tokens and quota windows.
3. Dispatch balances load across accounts from measured usage, not from a static ceiling.

Invariants that do not change: English only in every string; no credential is ever read, stored,
logged or journaled; every state change is traceable through `journal.jsonl` and
`~/.swamp/accounts.json`.

Companion docs: `DESIGN.md` §4, §6, §7, §8, §10; `UI.md` §3, §5.

---

## 1. Removals

Everything below implements a global parallelism cap or a budget cap. All of it goes. Line numbers
are at the time of writing; grep the symbol, not the line.

### 1.1 `limits.max_parallel` - the machine-wide worker semaphore

| Site | What it is |
|---|---|
| `src/config/schema.rs:46` | `Limits::max_parallel: Option<usize>` |
| `src/config/load.rs:18` | `DEFAULTS_TOML` `max_parallel = 4` |
| `src/config/validate.rs:15-22` | `isolation = "shared"` forces `max_parallel = 1` plus a warning |
| `src/config/validate.rs:106-111` | `max_parallel == 0` validation problem |
| `src/config/mod.rs:39` | doc comment on `warnings` naming the forced value |
| `src/dispatch/pool.rs:20` | `const DEFAULT_MAX_PARALLEL: usize = 4` |
| `src/dispatch/pool.rs:34-35` | `global: Arc<Semaphore>` field and its doc comment |
| `src/dispatch/pool.rs:140-148` | permit computation and the `reserve_brain_slot` subtraction |
| `src/dispatch/pool.rs:176-195` | `acquire` waiting on `global.acquire_owned()` |
| `src/cmd/run.rs:298-300`, `src/cmd/chat.rs:100-102` | `--workers` override |
| `src/cmd/config.rs:9` | `swamp config init` template |
| `src/ui/chat/mod.rs:290-293` | welcome box `max N parallel` |
| `src/ui/chat/app.rs:77,123` | `App::max_parallel` field and its init |
| `src/ui/chat/app.rs:1115-1120` | status-line `N/M workers` |
| `src/ui/chat/tests_support.rs:275,319` | fixture config and expected welcome string |
| tests | `tests/config_load.rs:114,186-213,222-240,261,385-389`; `tests/account_pool.rs:256,280,287,308,403,486,519-552`; `tests/mcp_protocol.rs:29`; `tests/dispatch_failover.rs:587`; `tests/support/mod.rs:217` |

**Replacement.** `AccountPool` keeps no global semaphore. The only concurrency bound is per account
(§1.5) and usage headroom (§4). `AccountPool::acquire` drops the `global` permit entirely; `Lease`
loses `_permit` for workers. `Capacity::Busy` now means every candidate is at its own
`max_concurrency`, which for an account with no cap can never happen.

`brain.reserve_brain_slot` **stays**: it still holds one healthy account out of the worker pool.
Only the sentence "the held permit is the subtraction from `max_parallel`" dies. The brain's own
one-slot semaphore (`pool.rs:36 brain`) stays as it is.

### 1.2 `limits.max_parallel_dispatch` - a process-wide semaphore, not a per-call one

`Dispatcher::batch` (`src/dispatch/mod.rs:51,87-91,104`) is one `Semaphore` shared by every
`swamp_dispatch` call in the process, acquired at `src/dispatch/mod.rs:251`. It is a global
parallelism cap in everything but name. Remove the key (`src/config/schema.rs:48`,
`src/config/load.rs:20`), the field, the acquire, and its use in the brain system prompt
(`src/brain/prompt.rs:18` and `DEFAULT_PARALLEL`). Test `tests/dispatch_failover.rs:749-751`
(`max_parallel_dispatch_serializes_a_batch`) is deleted.

**Replacement.** A `swamp_dispatch` batch starts every task at once; each one then queues on its
account's own capacity. The brain prompt says so instead of naming a number.

### 1.3 `limits.max_high_tier_concurrent` - yes, it is a cap

`Dispatcher::high_tier` (`src/dispatch/mod.rs:52,92-96,105`), acquired for `Tier::High` at
`src/dispatch/mod.rs:253`. Remove the key (`src/config/schema.rs:49`, `src/config/load.rs:21`), the
field, the acquire, `DEFAULT_HIGH`, and its mention in `src/brain/prompt.rs:20`.

**Replacement.** Nothing. A high-tier account is bounded by its own `max_concurrency` and by its
quota headroom, which is the real constraint the cap was approximating.

### 1.4 The budget cap, whole

| Site | What it is | Replacement |
|---|---|---|
| `src/config/schema.rs:57` | `limits.run_budget_usd` | none; cost is measured, never enforced |
| `src/config/schema.rs:58` | `limits.node_budget_usd` | none |
| `src/config/schema.rs:199` | `TierCfg::node_budget_usd` | none |
| `src/config/load.rs:26-27` | both defaults in `DEFAULTS_TOML` | none |
| `src/config/mod.rs:172-177` | `Config::node_budget_usd(tier)` | deleted |
| `src/worker/adapter.rs:32` | `LaunchSpec::budget_usd` | deleted |
| `src/worker/adapter.rs:93` | `Capability::NativeBudget` | deleted |
| `src/worker/claude.rs:85-87` | `--max-budget-usd` argv emission | never emitted |
| `src/model/failure.rs:28,94,122,146` | `Failure::BudgetExceeded` variant, `is_terminal`, `Class::Terminal`, `Display` | deleted |
| `src/worker/classify.rs:41-46,140-142` | `BUDGET_SUBTYPE` const and the subtype arm | the subtype falls through to `WorkerError { subtype: "error_max_budget_usd" }` if a user's own `worker.args` set the flag |
| `src/worker/mod.rs:307-310` | `refine`'s `BudgetExceeded` arm | deleted |
| `src/dispatch/cooldown.rs:162` | `BudgetExceeded` test fixture | deleted |
| `src/ui/trace.rs:259,284-287` | `"budget_exceeded"` word and `BudgetExceeded: spent $X of $Y` | deleted |
| `src/journal/fold.rs:539` | `"budget_exceeded"` digest word | deleted |
| `src/model/core.rs:159` | `CancelSource::Budget` | deleted |
| `src/cmd/run.rs:290` | `Some(Failure::BudgetExceeded { .. }) => 7` | deleted |
| `src/error.rs:38` | `// 7 = budget exceeded` comment | comment becomes `// 5 = merge conflict, 6 = cancelled` |
| `src/cli.rs:95` | `ChatArgs::budget` | deleted |
| `src/cli.rs:128-129` | `RunArgs::budget` | deleted |
| `src/cmd/run.rs:304-307`, `src/cmd/chat.rs:103-105` | `--budget` override | deleted |
| `src/ui/chat/mod.rs:294-296` | welcome `· budget $N` cell | deleted |
| call sites of `node_budget_usd` | `src/cmd/run.rs:385`, `src/cmd/resume.rs:415`, `src/dispatch/mod.rs:374`, `src/brain/mod.rs:135` | the `budget_usd:` field disappears from each `LaunchSpec` literal |
| `budget_usd: None` in fixtures | `src/doctor.rs:378`, `tests/spawn_detached.rs:107`, `tests/dispatch_failover.rs:364`, `tests/parse_claude.rs:49`, `tests/parse_codex.rs:40` | field removed |
| tests | `tests/parse_claude.rs:263-277` (`budget_and_resume_flags_appear_only_when_asked_for`) keeps only the `--resume` half; `tests/parse_claude.rs:562-574` (`the_budget_subtype_is_our_own_guard_not_a_provider_failure`) is replaced by a test asserting the subtype now classifies as `WorkerError` | |

**Exit code 7 is retired.** `swamp run` exit codes become `0` ok, `1` generic, `2` config invalid,
`3` no capacity, `4` node failed, `5` conflict, `6` cancelled. 7 is not reused.

Cost keeps being measured and displayed everywhere it is today: `Cost`, `CostBasis`,
`AccountState::lifetime_cost_usd`, `Config::estimate_cost`, `[pricing]`, `swamp accounts`, `/cost`,
`swamp trace`. Nothing consults it to make a decision.

### 1.5 Flags and chat surface

- `--workers <N>` on `swamp chat` (`src/cli.rs:90-93`) and `swamp run` (`src/cli.rs:110-111`).
- `--budget <USD>` on both.
- `/workers` slash command: the `COMMANDS` row in `src/ui/chat/slash.rs`, the `"workers" =>` arm at
  `src/ui/chat/app.rs:837`, and `App::set_workers` at `src/ui/chat/app.rs:912-927`.
- Status-line centre zone loses `· N/M workers` and prints `· N running` instead
  (`src/ui/chat/app.rs:1115-1120`).
- Welcome box `workers:` line becomes
  `workers: 4 accounts · 3 ready, 1 cooling · 12% of the tightest window used`.
  No `max N parallel`, no `budget $N` (`src/ui/chat/mod.rs:277-299`, `UI.md:433,447`).

`/usage` (§3) replaces `/workers` in the command table.

### 1.6 `accounts[].max_concurrency` stays, becomes optional

`src/config/schema.rs:188` is already `Option<usize>`. Change the downstream:

- `src/dispatch/account.rs:16`: `pub max_concurrency: Option<usize>` (was `usize`).
- `src/dispatch/pool.rs:105-110`: stop defaulting to `DEFAULT_ACCOUNT_CONCURRENCY = 2`; carry the
  `Option` through. Delete the const.
- `src/dispatch/policy.rs:31`: `if let Some(c) = a.max_concurrency && s.inflight >= c { return None }`.
- `src/dispatch/policy.rs:35`: `load` gets a new definition, §4.2.
- `src/cmd/accounts.rs:101`: `format!("{}/{}", s.inflight, cap)` where an absent cap renders `-`.
- `src/config/validate.rs`: `max_concurrency = 0` stays an error. Absent means unlimited.

Documented meaning: **unset = unlimited**. A user who wants a ceiling sets one per account, which is
where subscription concurrency limits actually live.

### 1.7 Doctor

`swamp doctor` has no parallelism or budget check today; the only budget reference is
`budget_usd: None` in the probe `LaunchSpec` at `src/doctor.rs:378`, which goes with the field.
`limits.unsafe_ack` (`src/doctor.rs:422-432`) is unrelated and stays.

New check added under `providers.*`, one line per account:

```
  ok   claude-main   quota telemetry live         5h 13%  7d 5%   observed 12s ago
  note codex-main    quota via app-server         7d 32%          observed 1m ago
  WARN codex-alt     no quota source              tokens only; utilization is estimated
```

Level `WARN` only when an account has neither telemetry nor an out-of-band source, because dispatch
is then balancing that account on token share alone (§4.4).

### 1.8 Documentation mentions to delete

`docs/DESIGN.md:145,147,160-161,378,536,568,740,831,876,921,1120,1236,1386-1396,1868-1869,1883,1938,2023,2025-2026,2031-2032,2183,2191,2231,2239,2385,2392,2434`;
`docs/UI.md:433,447,737`; `docs/PLAN.md:153,184,191,436,470,601,915`;
`README.md:77,190,201,242`; `swamp.example.toml:6,8,9,14,15,167,175,220`.

Byte budgets are not spend budgets and stay: `src/journal/fold.rs:420`, `src/mcp/tools.rs:94,201`,
`src/doctor.rs:760`, `docs/DESIGN.md:155,1727`, `tests/journal_fold.rs:702`.
`src/mcp/tools.rs:46` and `src/brain/prompt.rs:59` are reworded to drop "budget" without losing the
node-count and depth guards, which stay.

---

## 2. Usage model

### 2.1 Per-account state

`src/dispatch/account.rs`, persisted to `~/.swamp/accounts.json`. Every field is
`#[serde(default)]` so an old file loads.

```rust
pub struct AccountState {
    // unchanged
    pub inflight: usize,
    pub health: Health,
    pub cooldown_until: Option<OffsetDateTime>,
    pub consecutive_infra_failures: u32,
    pub last_used: Option<OffsetDateTime>,
    pub lifetime_nodes: u64,
    pub lifetime_cost_usd: f64,
    pub updated_at: Option<OffsetDateTime>,

    // new: token counters
    /// Every token this account ever spent, across runs and repos.
    pub lifetime_tokens: Usage,
    /// Tokens spent inside the window `window_key` names. Zeroed when the window rolls.
    pub window_tokens: Usage,
    /// When the current counting window began.
    pub window_started_at: Option<OffsetDateTime>,
    /// The window `window_tokens` is keyed to. A change means "roll and zero".
    pub window_key: Option<WindowKey>,

    // new: quota provenance
    /// The bucket Swamp routes against. Was `quota`; the name and shape are unchanged.
    pub quota: Option<RateLimitSnapshot>,
    /// Every bucket the provider reported, keyed by limit id. Display only.
    pub quota_buckets: BTreeMap<String, RateLimitSnapshot>,
    pub quota_observed_at: Option<OffsetDateTime>,
    pub quota_source: Option<QuotaSource>,
}

#[derive(PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct WindowKey {
    pub scope: LimitScope,
    pub resets_at: OffsetDateTime,
    /// The provider's own window length, so two readings of one window are recognised as one.
    pub window_minutes: Option<u32>,
}

#[derive(Copy, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaSource {
    /// claude `rate_limit_event`, in the worker's own stream.
    Telemetry,
    /// codex rollout `event_msg` / `token_count` / `rate_limits`.
    Rollout,
    /// codex `app-server` -> `account/rateLimits/read`.
    AppServer,
    /// Swamp's own token counters against a configured window size. Never a measurement.
    Estimated,
}
```

Window roll rule, the only place `window_tokens` resets:

```
on a new snapshot S for account A:
    key = WindowKey { scope: S.tightest_scope(), resets_at: S.tightest_resets_at() }
    if not state.window_key.same_window(key):
        state.window_tokens = Usage::default()
        state.window_started_at = now
        state.window_key = Some(key)
```

`resets_at` moving forward is what a rolled window looks like on the wire, for both providers.
codex reports its reset as a countdown, so the absolute instant Swamp derives from it moves forward
with the age of the reading: `same_window` compares two keys with half a window of tolerance, so
only a move of about a whole window counts as a roll. An
account with no quota source at all keys its window to
`WindowKey { scope: estimated_scope, resets_at: window_started_at + estimated_window }` and rolls on
wall time.

### 2.2 Quota shape changes

`src/model/core.rs`. All additive.

```rust
pub struct LimitWindow {
    pub scope: LimitScope,
    pub utilization: f64,                    // 0.0 ..= 1.0, ALWAYS
    pub resets_at: Option<OffsetDateTime>,
    /// The provider's own window length. 300 -> five_hour, 10080 -> seven_day. Kept so a
    /// window Swamp has no scope for is still labelled honestly in the UI.
    pub window_minutes: Option<u32>,
    /// False when this number came from Swamp's own counters, not the provider.
    pub measured: bool,
}

pub struct RateLimitSnapshot {
    pub status: LimitStatus,
    pub windows: Vec<LimitWindow>,
    pub resets_at: Option<OffsetDateTime>,
    /// codex `limit_id` / claude `rateLimitType`. Names which bucket this is.
    pub limit_id: Option<String>,
    /// codex `ordinaryUsageAllowed`. The authoritative gate: the schema says clients must NOT
    /// infer recovery from percentages or reset times. `Some(false)` makes the account
    /// ineligible whatever the percentages say.
    pub ordinary_usage_allowed: Option<bool>,
    /// Why the limit was reached, when the provider says. Decides cooldown vs park.
    pub reached: Option<LimitReached>,
    /// Plan name, display only.
    pub plan: Option<String>,
}

#[serde(rename_all = "snake_case")]
pub enum LimitReached {
    /// codex `rate_limit_reached`. Cool the account, it comes back.
    RateLimit,
    /// codex `*_credits_depleted` / `*_usage_limit_reached`, claude overage rejected with a
    /// hard reason. A human must act: Health::AuthBroken, no timer.
    CreditsDepleted,
    /// codex `spend_control_reached`. Same handling as CreditsDepleted.
    SpendControl,
}
```

`worst_utilization()` changes in one way: a `LimitScope::Unknown` window is skipped when at least
one known-scope window exists. Today claude's `seven_day_overage_included` (0.07) is maxed in
alongside `seven_day` (0.64); on an overage-enabled account an overage-inclusive window can drive
`Degraded` and `quota_stop_at` off something that is not a plan limit. `soonest_reset()` and
`worst_scope()` filter the same way. New `tightest(&self) -> Option<&LimitWindow>` returns the
argmax window, which §4 scores against.

`RateLimitSnapshot::measured_utilization()` returns `None` when every window has
`measured == false`. §4 uses it for the hard stop; `worst_utilization()` (which includes estimates)
is only ever a penalty input.

### 2.3 Where tokens come from

Usage reaches the pool through one new interface, `src/dispatch/pool.rs`:

```rust
impl AccountPool {
    /// `cumulative` is this node's running total, not a delta. Idempotent: calling it twice
    /// with the same value changes nothing.
    pub fn observe_usage(&self, id: &AccountId, node: NodeId, cumulative: Usage);
    /// The node is terminal: fold its final total into the committed counters and forget it.
    pub fn commit_usage(&self, id: &AccountId, node: NodeId, final_total: Usage);
}
```

Cumulative, not delta, because claude's adapter *replaces* the running total with the `result`
line's (`src/worker/claude.rs:309`, `if usage.billable() > 0 { st.usage = usage }`) and codex's
`turn.completed.usage` is a thread total. A delta interface would double count both. The pool keeps
`inflight_usage: BTreeMap<(AccountId, NodeId), Usage>`; a displayed total is
`committed + sum(inflight_usage for that account)`.

**Live path (new).** `src/worker/mod.rs:235-245` already drains every parsed `WorkerEvent` through a
consumer task into the journal. `ExecReq` gains
`observer: Option<Arc<dyn EventObserver>>`, called from that same loop. `src/dispatch/retry.rs`
passes an observer that forwards `WorkerEvent::Usage` to `observe_usage` and
`WorkerEvent::RateLimit` to `observe_quota`, so both land **while the worker runs**. Today
`retry.rs:297-300` only calls `observe_quota` after the node terminates, from
`RunOutcome::rate_limit`; that post-hoc call stays as the backstop for an adopted or resumed node.

**Anthropic.** `rate_limit_event` arrives at least twice per one-turn run, once before the first
`assistant` line. `src/worker/claude.rs:338-365 snapshot()` gains `window_minutes`,
`measured: true`, `limit_id: rate_limit_info.rateLimitType`, and the overage fields the adapter
currently discards (`overageStatus`, `overageDisabledReason`, `isUsingOverage`) mapped to
`reached: Some(CreditsDepleted)` when `overageStatus == "rejected"` with a hard
`overageDisabledReason`. The window key set varies between two events of the same run
(`five_hour` + `seven_day`, then `seven_day_overage_included` appears); a snapshot merges into the
stored one per scope rather than replacing it, so a key dropping out does not erase a window.

`result.usage` is the **main model only**. The fixture's haiku side-calls (899 in / 12 out) appear
only in `modelUsage` and inside `total_cost_usd`. `FinalSummary` gains
`model_usage: BTreeMap<String, Usage>` parsed from `modelUsage`, and the account counters use
`model_usage.values().sum()` when it is non-empty, falling back to `result.usage`. Node-level
`usage` keeps its current meaning so `swamp trace` does not change. `costBasis: "list"` confirms
`total_cost_usd` is list-price equivalence on a subscription, which is why cost stays a display
number.

**OpenAI.** `codex exec --json` carries no quota. Three sources, in order:

1. **Rollout tail (primary, free, live).** `thread.started` gives `thread_id`; every non-ephemeral
   exec run appends `$CODEX_HOME/sessions/<YYYY>/<MM>/<DD>/rollout-<local-ISO>-<thread_id>.jsonl`.
   New `src/worker/codex_quota.rs` globs `sessions/**/rollout-*-<thread_id>.jsonl` and tails it for
   the last `event_msg` with `payload.type == "token_count"`, reading
   `payload.rate_limits` and `payload.info.total_token_usage`. It appends live, so this works
   mid-run. Source `Rollout`.
   `CODEX_HOME` is owned by the user's wrapper and Swamp cannot read a wrapper. It is resolved once
   per account per process by `codex-<acct> app-server` + `initialize`, whose result echoes
   `codexHome`, and cached. `accounts[].env.CODEX_HOME`, when the user set it, short-circuits the
   probe.
2. **app-server poll (secondary, richer).** `account/rateLimits/read` over JSON-RPC on stdio, free
   of model tokens, ~1.5s handshake, honours `CODEX_HOME`. It returns `rateLimitsByLimitId`, the
   multi-bucket view the rollout does not carry, plus `ordinaryUsageAllowed` and
   `rateLimitReachedType`. Source `AppServer`. It hits the network, so it is **never** on a timer:
   it runs on node terminal and on an explicit `/usage` / `swamp usage --probe` when the cached
   snapshot is older than `dispatch.quota_max_age` (default `60s`), at most one in flight per
   account. `account/read` is **not** called: it returns the account email and Swamp has no use for
   it.
3. **Estimated (fallback).** Neither source available (`--ephemeral`, an app-server that will not
   start, a wrapper Swamp cannot resolve). Swamp synthesises one window from its own counters:

```
utilization = window_tokens.billable() / providers.openai.estimated_window_tokens
resets_at   = window_started_at + providers.openai.estimated_window
measured    = false
source      = Estimated
```

`[providers.openai] estimated_window = "7d"`, `estimated_window_tokens = 0` (meaning: no estimate,
render `-`). An estimated window is rendered with a leading `~`, is reported as `est` in
`swamp usage --json`, and **can only deprioritise an account, never exclude it** (§4.3). Swamp does
not know an OpenAI plan's real ceiling and must not park a working subscription on a guess.

Unit and scope conversion, both mandatory:

- codex `used_percent` / `usedPercent` is **0..100**; `LimitWindow::utilization` is **0..1**. Divide
  by 100 at the boundary. Ingesting raw would put every codex account instantly past
  `quota_stop_at`.
- Scope comes from `window_minutes`, never from the field name. `primary` on the `codex` bucket
  today is a **7-day** window with a null `secondary`; mapping primary -> FiveHour by position is
  wrong. `<= 60 -> Minute`, `240..=420 -> FiveHour`, `9000..=11000 -> SevenDay`, else `Unknown` with
  `window_minutes` preserved.

Bucket selection for `quota` (the one dispatch scores against): `accounts[].limit_id` when set, else
the bucket whose `limit_name` matches the configured model for this account's tier, else `"codex"`,
else the first. Every bucket is kept in `quota_buckets` for display. `worst_utilization()` never
maxes across buckets: `codex_bengalfox` at 0% on a model family this account never runs must not
make `codex` at 32% look worse, and `codex` at 95% must not park a Spark-only task.

Also in this pass: `thread.failed` is unhandled in `src/worker/codex.rs` (it exists in the 0.154.0
event enum and today falls to `CodexLine::Other` -> `WorkerEvent::Unknown`, producing no `Final`, so
a failed thread misclassifies as truncated). It maps to the same terminal path as `turn.failed`.

**Brain.** `src/brain/mod.rs:384-388` already forwards `RateLimit` to `observe_quota`. It gains the
matching `observe_usage` call on `WorkerEvent::Usage` and on `Final`, keyed on the brain's node id,
so the brain's tokens count against its account **per turn**, not once at shutdown. Its cost already
does, through `credit_turn()`. The brain is an account's tenant like any worker.

### 2.4 Journal

Additive only, per `DESIGN.md` §7.2. `src/journal/record.rs`:

```rust
AccountHealth { account, health, cooldown_until, quota,
                // new
                quota_observed_at: Option<OffsetDateTime>,
                quota_source: Option<QuotaSource> },
/// New. Emitted on every commit_usage and on every window roll.
AccountUsage { account: AccountId, window: Usage, lifetime: Usage,
               window_key: Option<WindowKey>, rolled: bool, source: Option<QuotaSource> },
```

`RunView` (`src/journal/fold.rs`) folds `AccountUsage` into `accounts: BTreeMap<AccountId,
AccountState>`, which it already carries, so `swamp trace --json` and `/status` see usage without a
new fold. `swamp replay --reparse` therefore re-derives usage from retained raw streams like
everything else.

No secrets: `accounts.json` and the journal hold an account **id**, an exec name, counters and
percentages. No token, no config-dir path beyond what the user put in `accounts[].env`, no email.

---

## 3. `/usage` and `swamp usage`

### 3.1 Layout

One table per provider, 100 columns, indent 2. Header `meta`, rows coloured by
`watch::health_color`.

```
anthropic
  ACCOUNT       HEALTH       5H   RESETS      7D   RESETS      WINDOW   LIFETIME     COST  FLIGHT
  claude-main   healthy     13%   in 3h02m    5%   in 6d21h    412.0k       8.1M   ~$1.20     1/3
  claude-alt    degraded     3%   in 1h48m   91%   in 3d11h      1.2M      14.7M   ~$4.10     0/2
  claude-work   cooling       -          -    -          -          0          0        -     0/-
                until 22:40 · rate_limit

openai
  ACCOUNT       HEALTH       5H   RESETS      7D   RESETS      WINDOW   LIFETIME     COST  FLIGHT
  codex-main    healthy       -          -   32%   in 5d02h     15.2k     182.4k        -     0/2
  codex-alt     healthy       -          -  ~2%   ~in 6d04h     480.0k      1.9M        -     0/-

observed  claude-main 12s ago telemetry · claude-alt 4m ago telemetry
          codex-main 1m ago app-server · codex-alt estimated
totals    in 2.1M  out 96.4k  cache-read 18.2M  cache-write 441k  ·  ~$5.30
          1 account has no quota source; its utilization is estimated
```

Columns, left to right, with widths:

| Column | Width | Align | Source | Empty |
|---|---|---|---|---|
| ACCOUNT | 13 | left | `AccountId` | never |
| HEALTH | 9 | left | `watch::health_word`, `no-auth` for `AuthBroken` (the word is 11 wide) | never |
| 5H | 5 | right | `LimitScope::FiveHour` window, `NN%` | `-` |
| RESETS | 9 | right | `in 3h02m`, `in 23h`, `in 6d21h` from that window's `resets_at` | `-` |
| 7D | 5 | right | `LimitScope::SevenDay` window | `-` |
| RESETS | 9 | right | same | `-` |
| WINDOW | 10 | right | `fmt::tokens(window_tokens.billable())` | `0` |
| LIFETIME | 10 | right | `fmt::tokens(lifetime_tokens.billable())` | `0` |
| COST | 8 | right | `~$N.NN` from `lifetime_cost_usd`, `~$N.Nk` from $1000 up | `-`, never `$0.00` |
| FLIGHT | 6 | right | `inflight/max_concurrency`, `N/-` when unlimited | never |

Total 96 columns with single-space gaps. A window Swamp has no named column for (`Minute`, or an
`Unknown` with `window_minutes`) is appended as a continuation row
`                 minute 4% · resets in 38s`.

Rules:

- A `~` prefix on a percentage or a reset time marks `measured == false`. Estimated numbers are
  never printed bare.
- A cooling or parked account gets a continuation row: `until HH:MM · <LimitReached word>`, or
  `until HH:MM` when the provider gave no reason, or `auth broken · re-auth <exec>` for
  `Health::AuthBroken`.
- `observed` lists per-account age and source. An account whose `quota_observed_at` is older than
  `dispatch.quota_max_age` renders the age in `err` colour, because a stale percentage is what makes
  dispatch wrong.
- A provider with no configured account prints nothing, not an empty table.
- Accounts in `accounts.json` but not in this repo's config are listed last under
  `not in config (drop with swamp accounts reset <id>)`, matching `swamp accounts`.

Drop order as the terminal narrows: below 96 drop LIFETIME; below 86 drop COST; below 78 collapse
the two RESETS columns into one showing the tightest window only; below 66 drop HEALTH and prefix
ACCOUNT with `fmt::glyph`. ACCOUNT, the tightest window's percentage and WINDOW never drop.

### 3.2 Refresh semantics

`/usage` renders immediately from `disp.pool().snapshot()`, in memory, no I/O, no network. Then, for
each account whose `quota_observed_at` is older than `dispatch.quota_max_age` (default `60s`) **and**
whose provider has an out-of-band source (OpenAI app-server; Anthropic has none, its telemetry only
arrives inside a worker stream), one probe is kicked in the background. When they land the block is
re-rendered once, in place, as a live block, then committed. At most one re-render per invocation,
so `/usage` can never loop.

`swamp usage` reads `~/.swamp/accounts.json` under the `fs4` lock (`persist::load_state`), renders
the same table through the same function, exits 0. It does not need a running supervisor. A
missing file is an empty table; an unreadable one is an error, so a table of zeros never stands in
for spend the file still holds and `--probe` never overwrites what it could not read.
`swamp usage --probe` forces one out-of-band probe per account first and waits for it, with a 10s
per-account timeout; a timeout renders the cached row with its age, never an error.

`/accounts` and `swamp accounts` are unchanged and keep their role: health, cooldown, exec
resolution, admin subcommands. `/usage` is the token and quota view. The split is deliberate; they
share `watch::health_word` and `watch::health_color` so the two cannot disagree on health.

### 3.3 JSON

`swamp usage --json`, and `/usage --json` inside chat (committed as a code block):

```json
{
  "at": "2026-09-15T20:14:03Z",
  "accounts": [
    {
      "provider": "anthropic",
      "account": "claude-main",
      "exec": "claude-main",
      "health": "healthy",
      "in_config": true,
      "inflight": 1,
      "max_concurrency": 3,
      "cooldown_until": null,
      "quota": {
        "source": "telemetry",
        "observed_at": "2026-09-15T20:13:51Z",
        "age_s": 12,
        "limit_id": "five_hour",
        "status": "allowed",
        "ordinary_usage_allowed": null,
        "reached": null,
        "plan": null,
        "windows": [
          {"scope": "five_hour", "utilization": 0.13, "window_minutes": 300,
           "resets_at": "2026-09-15T23:16:00Z", "measured": true},
          {"scope": "seven_day", "utilization": 0.05, "window_minutes": 10080,
           "resets_at": "2026-09-22T12:00:00Z", "measured": true}
        ]
      },
      "quota_buckets": {},
      "tokens": {
        "window": {"input_tokens": 12000, "cached_input_tokens": 380000,
                   "cache_write_tokens": 40000, "output_tokens": 3500,
                   "reasoning_tokens": 0, "billable": 55500},
        "window_started_at": "2026-09-15T18:16:00Z",
        "lifetime": {"input_tokens": 210000, "cached_input_tokens": 7600000,
                     "cache_write_tokens": 290000, "output_tokens": 91000,
                     "reasoning_tokens": 4200, "billable": 591000}
      },
      "nodes": 4,
      "cost_usd": 1.2046795,
      "cost_basis": "reported"
    }
  ],
  "totals": {
    "usage": {"input_tokens": 2100000, "output_tokens": 96400,
              "cached_input_tokens": 18200000, "cache_write_tokens": 441000},
    "cost_usd": 5.30,
    "cost_complete": false,
    "accounts_without_quota_source": 1
  }
}
```

`billable` is `input + cache_write + output`, the same definition `Usage::billable()` already uses.
Field names are snake_case and stable; new fields are additive.

---

## 4. Usage-aware dispatch

### 4.1 `QuotaAware` becomes the default

`src/dispatch/policy.rs`: move `#[default]` from `LeastLoaded` to `QuotaAware`.
`src/config/load.rs` `DEFAULTS_TOML` `[dispatch] policy = "quota-aware"`. The v1 reason for the old
default was that codex emitted nothing comparable; §2.3 gives codex three sources, and §4.4 gives a
term that balances correctly even with none of them.

### 4.2 Terms

All in `[0, 1]` unless stated. `now` is the selection instant.

```
util(s)        = s.quota.worst_utilization()            // includes estimated windows, 0.0 if none
measured(s)    = s.quota.measured_utilization()         // None when every window is estimated
headroom(s)    = 1 - util(s)

saturation(a,s) = match a.max_concurrency {
                      Some(c) => s.inflight as f64 / c as f64,
                      None    => 0.0,
                  }
crowding(s)     = s.inflight as f64 / (s.inflight as f64 + 1.0)   // 0, 0.5, 0.67, 0.75, ...
load(a,s)       = saturation(a,s).max(crowding(s))
```

`crowding` is what keeps an account with no `max_concurrency` from soaking up a whole batch: it
rises with every live node and never reaches 1, so an idle capped account and an idle uncapped
account still start level.

```
pool_window(P)  = sum over eligible accounts of P of window_tokens.billable()
share(s, P)     = s.window_tokens.billable() / max(1, pool_window(P))
```

`share` is the load-balancing term the request asks for. It is measured from Swamp's own counters,
needs no provider telemetry, and is what makes a pool of OpenAI accounts rotate correctly.

```
warn = cooldown.quota_warn_at   (0.90)
stop = cooldown.quota_stop_at   (0.98)
penalty = dispatch.near_exhaustion_penalty   (2.0)

penalised(s) = if util(s) < warn { util(s) }
               else { util(s) + penalty * (util(s) - warn) / (stop - warn) }
```

At `util == stop` the penalty adds a full `penalty` to a term already weighted 0.50, which no other
term can close: `load` contributes at most 0.30 and `share` at most 0.15. That is the intent. The
knee is at `warn`, so an account below 90% is scored on its raw utilization and nothing else.

### 4.3 Algorithm

```rust
struct Weights { util: f64, load: f64, share: f64, weight: f64, idle: f64 }
// [dispatch.weights], defaults:
const W: Weights = Weights { util: 0.50, load: 0.30, share: 0.15, weight: 0.05, idle: 0.02 };

/// Lower is better. None means ineligible right now.
fn score(policy: SelectionPolicy, a: &Account, s: &AccountState,
         pool_window: u64, cfg: &CooldownCfg, now: OffsetDateTime) -> Option<f64> {
    // ---- eligibility, in order. Every `return None` is a hard gate.
    match s.health {
        Health::Disabled | Health::AuthBroken => return None,
        Health::Cooling if s.cooldown_until.is_some_and(|t| t > now) => return None,
        _ => {}
    }
    // The provider's own authoritative gate. codex's schema is explicit that a client must
    // NOT infer recovery from percentages or reset times.
    if s.quota.as_ref().and_then(|q| q.ordinary_usage_allowed) == Some(false) { return None; }
    // A depleted-credits or spend-control stop is not a timer; a human must act.
    if matches!(s.quota.as_ref().and_then(|q| q.reached),
                Some(LimitReached::CreditsDepleted | LimitReached::SpendControl)) { return None; }
    if let Some(c) = a.max_concurrency && s.inflight >= c { return None; }
    // Proactive stop, BEFORE any provider error. MEASURED windows only: an estimated
    // utilization is a guess and must never park a working subscription.
    if s.quota.as_ref().and_then(|q| q.measured_utilization())
         .is_some_and(|u| u >= cfg.quota_stop_at) { return None; }

    // ---- scoring
    let load  = saturation(a, s).max(crowding(s));
    let share = s.window_tokens.billable() as f64 / pool_window.max(1) as f64;
    let idle  = s.last_used.map_or(1.0, |t| ((now - t).as_seconds_f64() / 3600.0).min(1.0));

    Some(match policy {
        SelectionPolicy::RoundRobin  => -idle,
        SelectionPolicy::LeastLoaded => load - 0.01 * (a.weight as f64),
        SelectionPolicy::QuotaAware  =>
              W.util  * penalised(s, cfg)
            + W.load  * load
            + W.share * share
            - W.weight * (a.weight as f64 - 1.0)
            - W.idle  * idle,
    })
}

/// Ties: score, then the account that has spent fewer tokens over its life, then rotation.
/// `lifetime_cost_usd` leaves the tie chain: it is blind for OpenAI, where the CLI reports
/// no cost and `[pricing]` may be unset, so it silently ranked every codex account equal.
struct Rank { score: f64, lifetime_billable: u64, rotation: usize }

impl Rank {
    fn compare(&self, o: &Rank) -> Ordering {
        if (self.score - o.score).abs() > 1e-9 { return self.score.total_cmp(&o.score); }
        self.lifetime_billable.cmp(&o.lifetime_billable)
            .then_with(|| self.rotation.cmp(&o.rotation))
    }
}
```

`pool_window` is computed once per `take_from` call over the candidate set, under the same state
lock, so every candidate in one selection is scored against the same denominator.

### 4.4 Accounts with no quota source

`util == 0.0`, `measured == None`. Such an account is never excluded by `quota_stop_at` and its
`penalised` term is 0. It is ranked entirely by `load` and `share`, which is correct: `share` still
spreads a batch evenly across a pool of untelemetered accounts, and a run that hammers one of them
raises its `share` immediately. This is the concrete fix for the OpenAI half of the pool, where
today `util` reads 0.0, `lifetime_cost_usd` reads 0.0, and `QuotaAware` degrades all the way to
"whichever account the map yields first".

### 4.5 The brain's own consumption

The brain holds one lease for the whole run on its own account. Two consequences, both intended:

- Its `inflight` is 1 for the run's lifetime, so `crowding` = 0.5 on that account and workers
  prefer another one. That is the reservation behaviour `reserve_brain_slot` used to buy with a
  global permit, now expressed in the score.
- Its tokens are fed per turn through `observe_usage` (§2.3), so its `window_tokens` and therefore
  its `share` are live. A brain that spent a run's planning on `claude-main` no longer leaves that
  account looking idle to the next worker selection. Its node is still counted once, at shutdown,
  so `lifetime_nodes` keeps meaning "nodes", not "turns".

### 4.6 Failover: unchanged

`src/dispatch/retry.rs` and `Failure::rotates_account()` are untouched. Only `RateLimited` and
`AuthExpired` rotate accounts. `Overloaded` / `Crashed` / `Truncated` retry the same account with
backoff, resuming the same session. `WorkerError` / `PermissionDenied` / `Timeout` / `NoCapacity` /
`Cancelled` are terminal and never rotate. Removing `BudgetExceeded` removes one terminal variant
and changes nothing about the other arms. Cross-provider failover stays opt-in.

### 4.7 When every account is exhausted

`NoCapacity` gains a variant and `acquire` changes behaviour:

```rust
pub enum NoCapacity {
    /// Every candidate is cooling, past its measured stop threshold, or hard-gated.
    /// `retry_at` is the earliest moment any of them could come back.
    AllExhausted { retry_at: OffsetDateTime, why: String },
    Saturated,
    Exhausted { reason: String },
}
```

`retry_at = min` over candidates of `cooldown_until`, `quota.soonest_reset()`, and for a hard-gated
account `None` (it contributes nothing; a credits-depleted account has no reset).

`acquire` does **not** return an error. It:

1. emits `JournalEvent::NodeBlocked { until: retry_at, why }`;
2. surfaces a visible notice, once per blocked node, not per loop iteration:
   - chat: a `Notice` block, `⏸ every anthropic account is at its limit · earliest reset 22:40
     (in 41m) · esc esc to cancel`;
   - `swamp run`: one line on stderr with the same text and `ctrl-c to cancel`;
   - the span carries a day unit and the clock gains `on <date>` once the reset is not today,
     so a seven-day window reads the same here as in the RESETS column;
3. sleeps until `min(retry_at, deadline)` or until `returned.notify_waiters()` fires, then rechecks.

It fails only when the caller's `deadline` passes (`NoCapacity::Saturated`), when the
`CancellationToken` fires (`Failure::Cancelled { by: User }`), or when the pool has no candidate at
all for the provider (`NoCapacity::Exhausted`, a config problem, not a quota one). Waiting an hour
for a 5-hour window to roll is the correct behaviour for an overnight run; failing immediately is
not. When every candidate is hard-gated with no reset, `retry_at` is absent and `acquire` returns
`Exhausted` with the reason, because sleeping forever helps nobody.

`--timeout` (default 25m per node) remains the bound on how long a node is willing to wait. A user
who wants "fail rather than wait" sets a short timeout.

### 4.8 New config keys

```toml
[dispatch]
policy        = "quota-aware"   # was "least-loaded"
quota_max_age = "60s"           # out-of-band probe threshold for /usage and dispatch
near_exhaustion_penalty = 2.0

  [dispatch.weights]
  util   = 0.50
  load   = 0.30
  share  = 0.15
  weight = 0.05
  idle   = 0.02

[providers.openai]
quota_source           = "auto"   # auto | rollout | app-server | none
estimated_window       = "7d"
estimated_window_tokens = 0       # 0 = no estimate, render "-"

[[accounts]]
id = "codex-main"
limit_id = "codex"                # optional: which quota bucket this account routes against
# max_concurrency omitted = unlimited
```

Validation (`src/config/validate.rs`): weights must be finite and non-negative; `quota_max_age >=
10s`; `near_exhaustion_penalty >= 0`; `estimated_window_tokens` and `estimated_window` must be set
together; `quota_warn_at < quota_stop_at` (already enforced) still holds and now also gates
`penalised`'s denominator, which must be > 0.

### 4.9 Worked examples

Defaults throughout: `warn 0.90`, `stop 0.98`, `penalty 2.0`, weights as §4.3, all weights 1.

**1. Utilization decides between two idle accounts.**
`claude-main`: 5h 0.13, 7d 0.05 -> util 0.13. inflight 0/3. window 412k.
`claude-alt`: 5h 0.03, 7d 0.65 -> util 0.65. inflight 0/2. window 1.2M.
pool_window 1.612M -> share 0.256 / 0.744. Both idle > 1h -> idle 1.0.

```
main = 0.50*0.13 + 0.30*0 + 0.15*0.256 - 0 - 0.02*1.0 = 0.065 + 0.038 - 0.020 = 0.083
alt  = 0.50*0.65 + 0.30*0 + 0.15*0.744 - 0 - 0.02*1.0 = 0.325 + 0.112 - 0.020 = 0.417
```

-> `claude-main`. `LeastLoaded` would have tied these at `-0.01` and fallen through to lifetime
nodes, ignoring that `alt` is 65% into a 7-day window.

**2. Load matters, but utilization outranks it; the cap still gates.**
Same accounts, `claude-main` now at inflight 2/3 -> saturation 0.667, crowding 0.667, load 0.667.

```
main = 0.065 + 0.30*0.667 + 0.038 - 0.020 = 0.283
alt  = 0.417
```

-> still `claude-main`: 2 of 3 slots busy does not outweigh a 52-point utilization gap. At inflight
3/3 `main` is ineligible by its own `max_concurrency` and the node goes to `alt`. Had `main` no
`max_concurrency`, load at inflight 3 would be `crowding = 0.75` -> score 0.308, still under `alt`,
and a fourth worker would start there. That is the intended behaviour: an uncapped account is
bounded by headroom, not by a number.

**3. The near-exhaustion penalty is decisive.**
`claude-alt` 7d rises to 0.93.

```
penalised(alt) = 0.93 + 2.0 * (0.93 - 0.90) / (0.98 - 0.90) = 0.93 + 0.750 = 1.680
alt  = 0.50*1.680 + 0 + 0.15*0.744 - 0.020 = 0.840 + 0.112 - 0.020 = 0.932
main = 0.083
```

-> `claude-main`, by 0.85. No load or share term can close that: their combined ceiling is 0.45.
At 0.985 `alt` is ineligible outright (measured, past `stop`) and `capacity()` reports it as
exhausted with `retry_at = its seven_day resets_at`. `alt` is also `Health::Degraded` from
`health_from_quota`, so `/usage` and `swamp accounts` show it amber before dispatch stops using it.

**4. Two OpenAI accounts with no quota telemetry at all.**
`codex-main` window 15.2k, `codex-alt` window 480k, both inflight 0, both `util 0.0`,
`measured None`. pool_window 495.2k -> share 0.031 / 0.969.

```
main = 0 + 0 + 0.15*0.031 - 0.020 = -0.015
alt  = 0 + 0 + 0.15*0.969 - 0.020 =  0.125
```

-> `codex-main`. `share` alone balanced the pool with zero provider telemetry, which is exactly what
`LeastLoaded` could not do: both accounts read `util 0.0`, `inflight 0`, `lifetime_cost_usd 0.0` and
tied all the way to the map order. If both are also at 0 tokens (a fresh machine) the scores tie at
`-0.020`, the tie falls to `lifetime_billable`, and if that is 0 for both, to `rotation`, so
sequential single-worker runs alternate.

**5. Everything exhausted; Swamp waits, it does not fail.**
`claude-main` measured 7d 0.985 >= stop -> ineligible, `soonest_reset` 22:00.
`claude-alt` cooling until 22:40 after a `RateLimited` with `reached: RateLimit`.
No third anthropic account. `cross_provider_failover = false`.

`capacity()` returns `AllExhausted { retry_at: 22:00, why: "claude-main at 98% of its seven_day
window until 22:00; claude-alt cooling until 22:40" }`. `acquire` journals
`NodeBlocked { until: 22:00, why }`, chat commits
`⏸ every anthropic account is at its limit · earliest reset 22:00 (in 41m) · esc esc to cancel`,
and sleeps. At 22:00 the window rolls: the next `rate_limit_event` from any run carries a later
`resets_at`, `observe_quota` rolls `window_tokens` to zero (§2.1), `health_from_quota` returns
`Healthy`, `notify_waiters` fires and the node leases `claude-main`. Nothing failed, nothing
retried, one journal line records the whole wait. Two `esc` presses at any point cancel the node
with `Failure::Cancelled { by: User }` and exit 6.

---

## 5. Work packages

Five packages. **WP-A and WP-D are file-disjoint**, as required: `src/dispatch/pool.rs` and
`src/dispatch/policy.rs` belong to WP-D alone, including the deletion of WP-A's global semaphore
from `pool.rs`.

**Merge order: A, then B / C / D in parallel, then E.** WP-A is a mechanical deletion that removes
`Config::node_budget_usd`, `LaunchSpec::budget_usd` and `Failure::BudgetExceeded`, whose call sites
reach into files B and C also edit; B and C therefore branch from post-A. B, C and D are mutually
file-disjoint. E owns every documentation file alone and lands last.

### WP-A - Remove the caps

- **Complexity: simple.** Mechanical deletion across many files, no design judgement. Sonnet.
- **Files owned:**
  `src/config/schema.rs`, `src/config/load.rs`, `src/config/validate.rs`, `src/config/mod.rs`,
  `src/cli.rs`, `src/cmd/run.rs`, `src/cmd/chat.rs`, `src/cmd/config.rs`, `src/cmd/resume.rs`,
  `src/error.rs`, `src/model/core.rs`, `src/model/failure.rs`, `src/worker/adapter.rs`,
  `src/worker/claude.rs`, `src/worker/classify.rs`, `src/worker/mod.rs`, `src/dispatch/mod.rs`,
  `src/dispatch/cooldown.rs`, `src/brain/mod.rs`, `src/brain/prompt.rs`, `src/mcp/tools.rs`,
  `src/ui/trace.rs`, `src/journal/fold.rs`, `src/doctor.rs`, `src/ui/chat/mod.rs`,
  `src/ui/chat/app.rs`, `src/ui/chat/slash.rs`, `src/ui/chat/tests_support.rs`,
  `tests/config_load.rs`, `tests/account_pool.rs`, `tests/mcp_protocol.rs`,
  `tests/dispatch_failover.rs`, `tests/parse_claude.rs`, `tests/parse_codex.rs`,
  `tests/spawn_detached.rs`, `tests/support/mod.rs`.
- **Does not touch:** `src/dispatch/{pool,policy,account,persist,retry}.rs`,
  `src/worker/{codex,follow}.rs`, `src/model/event.rs`, any `docs/` file, `README.md`,
  `swamp.example.toml`.
- **Work:** §1.1 through §1.7, plus adding the new config keys of §4.8 to `schema.rs`,
  `load.rs`'s `DEFAULTS_TOML` and `validate.rs`, and the new per-account doctor line of §1.7.
- **Public interface changes:** `Limits` loses 4 fields and gains `dispatch.quota_max_age`,
  `dispatch.near_exhaustion_penalty`, `dispatch.weights`, `providers.openai.quota_source`,
  `providers.openai.estimated_window{,_tokens}`, `accounts[].limit_id`.
  `Failure::BudgetExceeded`, `CancelSource::Budget`, `Capability::NativeBudget`,
  `LaunchSpec::budget_usd`, `Config::node_budget_usd` all deleted. `TierCfg::node_budget_usd`
  deleted. `Dispatcher::{batch,high_tier}` deleted. `ChatArgs::{workers,budget}` and
  `RunArgs::{workers,budget}` deleted.
- **Acceptance tests:**
  1. `swamp --help`, `swamp run --help`, `swamp chat --help` contain no `--workers` and no
     `--budget`; a clap parse of either flag errors.
  2. `[limits] max_parallel = 4` in a config file is a `deny_unknown_fields` error naming the key,
     with a message pointing at `accounts[].max_concurrency`. Same for `max_parallel_dispatch`,
     `max_high_tier_concurrent`, `run_budget_usd`, `node_budget_usd`, `tiers.*.node_budget_usd`.
  3. `argv_for` on a claude `LaunchSpec` never contains `--max-budget-usd`, under any config.
  4. A `result` line with `subtype: "error_max_budget_usd"` classifies as
     `Failure::WorkerError { subtype: "error_max_budget_usd", .. }`, is terminal, and does not
     rotate.
  5. `exit_code` never returns 7; a config with 30 accounts and no `max_concurrency` anywhere
     validates clean.
  6. The welcome-box snapshot at 100 and 62 columns contains neither `parallel` nor `budget`.
  7. `/workers` is an unknown command and the `did_you_mean` suggestion is `/usage`.

### WP-B - Usage state and telemetry ingestion

- **Complexity: complex.** New wire formats, a new subprocess protocol, idempotent accounting.
  Opus.
- **Files owned:**
  `src/model/core.rs`*, `src/model/event.rs`, `src/journal/record.rs`, `src/journal/fold.rs`*,
  `src/dispatch/account.rs`, `src/dispatch/persist.rs`, `src/dispatch/retry.rs`,
  `src/worker/claude.rs`*, `src/worker/codex.rs`, `src/worker/codex_quota.rs` (new),
  `src/worker/mod.rs`*, `src/brain/mod.rs`*,
  `tests/parse_claude.rs`*, `tests/parse_codex.rs`*, `tests/usage_state.rs` (new),
  `docs/ref/codex-rollout-sample.jsonl` (new fixture),
  `docs/ref/codex-ratelimits-sample.json` (new fixture).
  (* also edited by WP-A; branch from post-A.)
- **Work:** §2 in full. `AccountState` fields and the window-roll rule; `LimitWindow` /
  `RateLimitSnapshot` additions and the `Unknown`-scope filter in `worst_utilization`;
  `modelUsage` parsing and the sum-across-models account total; codex `used_percent` /100 and
  scope-from-`window_minutes`; the rollout tailer; the app-server client
  (`initialize` -> `initialized` -> `account/rateLimits/read`, `CODEX_HOME` echo cached per
  account); bucket selection; the `Estimated` fallback; `thread.failed`; the `EventObserver` seam
  in `src/worker/mod.rs`'s consumer loop and its wiring in `retry.rs` and `brain/mod.rs`; the two
  journal events.
- **Public interface changes:** `AccountPool::{observe_usage, commit_usage}` are **called** here
  and **implemented** in WP-D's `pool.rs`; both packages build to the signatures in §2.3, which is
  the single point of coordination between them.
  `ExecReq::observer: Option<Arc<dyn EventObserver>>`. `FinalSummary::model_usage`.
  `JournalEvent::AccountUsage`, `AccountHealth::{quota_observed_at, quota_source}`.
  `codex_quota::{resolve_codex_home, tail_rollout, read_rate_limits}`.
- **Acceptance tests:**
  1. `docs/ref/claude-stream-sample.jsonl` replays to a `RateLimitSnapshot` with
     `five_hour 0.06` and `seven_day 0.64`, `measured: true`, `window_minutes` set, and
     `worst_utilization() == 0.64` even when `seven_day_overage_included` (0.07, `Unknown`) is
     present. A second event carrying only `five_hour` does not erase the stored `seven_day`.
  2. The same fixture's account total is `sum(modelUsage)`, strictly greater than `result.usage`
     by the haiku side-call's 899 in / 12 out.
  3. A codex `rate_limits` payload with `used_percent: 32.0, window_minutes: 10080` yields exactly
     one window, `scope: SevenDay`, `utilization: 0.32`. A `primary` with `window_minutes: 10080`
     and a null `secondary` never produces a `FiveHour` window.
  4. `observe_usage` called three times with cumulative `(100, 200, 200)` for one node leaves
     `window_tokens.billable() == 200`, not 500. `commit_usage` then `observe_usage` again with a
     stale value does not decrease the committed total.
  5. A snapshot whose `resets_at` moves forward zeroes `window_tokens`, sets `window_started_at`,
     and emits `AccountUsage { rolled: true }`. A snapshot with the same `resets_at` does not.
  6. `thread.failed` produces a `Final { ok: false }` and classifies as `WorkerError`, not
     `Truncated`.
  7. `merge_state` across two processes keeps the larger `lifetime_tokens` per field, the way it
     already keeps the larger `lifetime_nodes`, and never resurrects a rolled `window_tokens`.
  8. No test, log or journal line contains an email address, a token, or the contents of
     `auth.json`. `account/read` is never called.

### WP-C - `/usage` and `swamp usage`

- **Complexity: simple.** One renderer, one subcommand, one slash handler. Sonnet.
- **Files owned:** `src/ui/usage.rs` (new), `src/cmd/usage.rs` (new), `src/ui/mod.rs`,
  `src/cmd/mod.rs`, `src/cli.rs`*, `src/ui/chat/slash.rs`*, `src/ui/chat/app.rs`*,
  `src/cmd/accounts.rs`, `tests/usage_render.rs` (new).
  (* also edited by WP-A; branch from post-A.)
- **Work:** §3 in full. `ui::usage::render(&[AccountRow], width, theme) -> Vec<Line>` and
  `ui::usage::json(&[AccountRow]) -> serde_json::Value`, shared byte-for-byte by the chat block and
  the CLI. The `/usage` `COMMANDS` row replacing `/workers`. `Command::Usage(UsageArgs { probe,
  json })`. `src/cmd/accounts.rs` only to render an absent `max_concurrency` as `-` (§1.6).
- **Public interface changes:** `ui::usage::{render, json, AccountRow}`; `cmd::usage::run`;
  `Command::Usage`; `COMMANDS` gains `/usage`.
- **Acceptance tests:**
  1. `TestBackend` snapshots of the §3.1 table at 100, 86, 78 and 62 columns, `Theme::Plain`,
     asserting the documented drop order and that ACCOUNT, the tightest percentage and WINDOW
     survive every width.
  2. An account with no quota renders `-` in every window cell and never `0%`; an estimated one
     renders `~2%` and `~in 6d04h`.
  3. A cooling account renders the `until HH:MM · rate_limit` continuation row; an `AuthBroken` one
     renders `auth broken · re-auth <exec>`.
  4. `swamp usage --json` validates against the §3.3 shape, `billable` equals
     `input + cache_write + output`, and round-trips through `serde_json` unchanged.
  5. `swamp usage` with no supervisor running and a hand-written `~/.swamp/accounts.json` renders
     and exits 0; with a missing file it renders an empty table and exits 0, never an error.
  6. An account whose `quota_observed_at` is older than `quota_max_age` renders its age in the
     `err` role.
  7. A control-character test: an account id carrying `\u{1b}[2J` does not repaint the viewport.
  8. `/usage` output committed in chat is byte-identical to `swamp usage` at the same width.

### WP-D - Quota-aware policy and pool

- **Complexity: complex.** Scoring, eligibility gates, the blocked-wait loop. Opus.
- **Files owned:** `src/dispatch/policy.rs`, `src/dispatch/pool.rs`. **Nothing else.**
- **Work:** §4 in full, plus the §1.1 and §1.6 removals that live in `pool.rs` (the `global`
  semaphore, `DEFAULT_MAX_PARALLEL`, `DEFAULT_ACCOUNT_CONCURRENCY`, the `reserve_brain_slot` permit
  subtraction, the `Option<usize>` capacity), plus implementing
  `AccountPool::{observe_usage, commit_usage}` to §2.3's signatures and the window-roll call in
  `observe_quota`.
- **Public interface changes:** `SelectionPolicy` default moves to `QuotaAware`. `score` takes
  `pool_window: u64` and `&CooldownCfg` instead of a bare `quota_stop_at: f64`. `Rank` becomes
  `{ score, lifetime_billable, rotation }`. `NoCapacity::AllExhausted { retry_at, why }` replaces
  `AllCooling { retry_at }`. `Lease` loses its worker `_permit`.
  `AccountPool::{observe_usage, commit_usage}` added.
- **Acceptance tests:** the five worked examples of §4.9 as five table-driven tests asserting the
  exact scores to 3 decimals and the chosen account, plus:
  6. An estimated window at `utilization 0.99` is still eligible; a measured one at 0.99 is not.
  7. `ordinary_usage_allowed: Some(false)` makes an account ineligible at `utilization 0.01`.
  8. `reached: CreditsDepleted` makes an account ineligible and `AllExhausted` omits it from
     `retry_at`.
  9. Ten concurrent `acquire` calls against two accounts with no `max_concurrency` all succeed and
     spread by `share`; the same against one account with `max_concurrency = 2` leaves eight
     waiting and none failing.
  10. `AllExhausted` journals exactly one `NodeBlocked` per blocked node, not one per loop
      iteration, and `pool.wakeups()` stays bounded while blocked.
  11. A cancelled token while blocked returns promptly with `Cancelled`, not `NoCapacity`.

### WP-E - Documentation and example config

- **Complexity: simple.** Prose, one config file. Sonnet.
- **Files owned:** `docs/DESIGN.md`, `docs/UI.md`, `docs/PLAN.md`, `docs/USAGE.md`, `README.md`,
  `swamp.example.toml`. **No source file.**
- **Work:** apply §1.8's deletions; rewrite `DESIGN.md` §6.3 (Concurrency) around per-account
  capacity and usage headroom, §6.4 (Selection) around §4's scoring, §6.7 (`swamp accounts`) to
  point at `swamp usage`, §8 (CLI surface) for the new subcommand and the dropped flags and exit
  code, §10 (Config format) for §4.8's keys; rewrite `UI.md` §3.1 (welcome box), §3.6 (the blocked
  notice) and §5 (`/usage` replaces `/workers`); update `README.md`'s command table, config
  snippet, exit codes and the "bounds concurrency, quota, budget and depth" sentence;
  rewrite `swamp.example.toml` to match `DEFAULTS_TOML` exactly.
- **Public interface changes:** none.
- **Acceptance tests:**
  1. `swamp config validate swamp.example.toml` exits 0, and every key in the file round-trips
     through `Config` with `deny_unknown_fields` on.
  2. `rg -n 'max_parallel|run_budget_usd|node_budget_usd|max-budget-usd|BudgetExceeded|--workers|--budget|max_high_tier_concurrent|max_parallel_dispatch'` over `docs/`, `README.md` and
     `swamp.example.toml` returns only §1's historical-note lines in `USAGE.md`.
  3. Every exit code listed in `README.md` and `DESIGN.md` §8 matches `src/error.rs` and
     `src/cmd/run.rs::exit_code`; 7 appears nowhere.
  4. Every `docs/` mention of a slash command exists in `slash::COMMANDS`, and vice versa.
