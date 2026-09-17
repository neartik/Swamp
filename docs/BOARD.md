# `swamp board` - the dispatch board

> "The dispatching of tasks must be visible; always show on the side the graph of which account
> works on what."

The board is a **separate full-screen program** you keep open in a pane next to `swamp chat`. It
answers one question at a glance: *which account is working on what, right now, and what is
waiting on whom.* It is read-only: it tails files and never talks to a supervisor.

---

## 0. Why a pane and not a column inside chat

Two layouts were considered and rejected.

- **A right-hand column inside `swamp chat`** is impossible without rewriting the chat UI. Chat is
  inline and scrollback-preserving (`RelativeBackend`, relative cursor moves only, no alternate
  screen, `docs/UI.md` §1.1-1.3): committed lines belong to the terminal, not to us, so nothing can
  be painted beside them. Getting a column means an alternate-screen TUI, which costs native
  scrollback and whole-transcript copy-paste.
- **A live block pinned between the transcript and the input** fits the current model, but it is
  not "on the side": it steals vertical rows from the conversation exactly when a batch is large,
  and `live_height` already caps the live area at three fifths of the screen.

**The chat UI does not change at all.** The board is its own process, in its own pane, sized by the
user, and it outlives any one chat session.

### 0.1 Where the command lives

New top-level command `swamp board`, `src/cmd/board.rs`, `src/ui/board/`. Not a flag on `watch`:

- `swamp watch` is single-run by construction (`WatchArgs { run }` -> `Ctx::run_paths` -> one
  `RunPaths`), keeps `with_events: true`, and its panes are tree/node-detail/footer. The board is
  multi-run, account-first, and deliberately keeps no event stream.
- Different default argument semantics: `watch` with no argument means *the last run*; `board` with
  no argument means *everything currently alive*.

`swamp watch --board` is accepted as a hidden alias that forwards to `cmd::board::run`, because it
is the name people will guess. Nothing in `src/ui/watch.rs` changes.

---

## 1. Discovery: what it shows with no arguments

Runs do **not** live under `~/.swamp`. They live per repo, in `<repo>/.swamp/runs/<ULID>/`, while
`~/.swamp` holds `accounts.json` (+ `accounts.lock`), `sock/<run-short>.sock` and `worktrees/`.
Discovery therefore has two halves.

**Accounts (machine-wide, always).** `Paths::accounts_state()` -> `~/.swamp/accounts.json`, read
through `dispatch::persist::load_state` so the fs4 advisory lock is honoured. That is every account
the machine knows, including ones no run is currently using, and it is the only source that is
correct across repos. `ui::usage::rows_from(&cfg.accounts, &pool_like, &stale)` turns it into the
same `AccountRow` values `/usage` and `swamp usage` render, so the three surfaces cannot drift.

**Runs (this repo by default).** `Paths::list_runs()` gives every run newest first; a run is *live*
when `RunView::finished` is false **and** it has at least one non-terminal node whose pidfile passes
`worker::liveness::is_ours`, or its control socket `RunPaths::socket()` still exists. Live runs are
tailed; the newest finished run is kept in `recent` only.

**Cross-repo.** `~/.swamp/sock/*.sock` names every live run on the machine but not its repo, so it
cannot be mapped back to a journal. WP7 adds `~/.swamp/runs.json`, an fs4-locked index written at
`swamp run` / `swamp chat` startup and on exit: `{ "<run-short>": { run, repo, dir, pid,
started_at } }`, merged last-writer-wins exactly like `accounts.json`. With it, `swamp board --all`
shows every repo. Until WP7 lands, `--all` prints what it cannot see and falls back to this repo.

**Flags.** `--run <id|last|-N>` pins one run (`Paths::resolve_run`, same grammar as `watch`).
`--all` is cross-repo. `--interval <ms>` overrides the poll period. `--once` prints one frame and
exits, for scripts. `--json` dumps the same model as JSON and exits.

---

## 2. Data model and refresh

### 2.1 Sources

| Source | Read by | Gives |
|---|---|---|
| `<repo>/.swamp/runs/<id>/journal.jsonl` | `journal::reader::Tailer::open` + `poll` | every node transition |
| `journal::fold::RunView` (`with_events: false`) | `apply` per tailed line | `nodes`, `children`, `by_logical`, `accounts`, `totals()`, `tree()` |
| `~/.swamp/accounts.json` | `dispatch::persist::load_state` | `AccountState` per account: `inflight`, `health`, `cooldown_until`, `quota`, `quota_buckets`, `window_tokens`, `lifetime_*` |
| `<repo>/.swamp/runs/<id>/nodes/<short>/pid` | `worker::liveness::is_ours` | is this node's process actually alive |
| repo config | `Config::accounts`, `dispatch::policy::Scoring::from_config` | `max_concurrency`, weights, `warn_at`/`stop_at` |

Nothing new is invented. `RunView` is folded from the journal exactly as `swamp watch` and the chat
board do, so the three agree by construction.

```rust
// src/ui/board/model.rs
pub struct Board {
    pub runs: Vec<RunPane>,                 // one per tailed run, newest first
    pub accounts: Vec<AccountRow>,          // ui::usage::AccountRow, machine-wide
    pub scoring: Scoring,                   // dispatch::policy::Scoring
    pub policy: SelectionPolicy,
    pub selected: Selection,
    pub now: OffsetDateTime,
}
pub struct RunPane {
    pub run: RunId,
    pub paths: RunPaths,
    pub view: RunView,
    pub tailer: Tailer,
    pub brain: NodeId,                      // NodeId(run.0), per brain::build
    pub selection: BTreeMap<NodeId, SelectionNote>,   // from JournalEvent::AccountSelected
    pub stale: Option<OffsetDateTime>,      // when the brain stopped being alive
}
```

`SelectionNote { account, policy, reason, excluded }` is folded by the board's own projection from
`JournalEvent::AccountSelected`, which already carries `{ account, exec, policy, reason, excluded }`
(`src/journal/record.rs`). `RunView` drops `policy` and `reason` today and is left alone.

### 2.2 Rows

Node rows are `view.tree()` collapsed by `logical`, taking `attempts.last()` as the live record, the
same `latest()` rule `trace.rs` uses, so a retry changes the short id in place and the id shown is
the one `swamp diff` and `swamp adopt` take. Grouping is by `NodeRecord.account` (provider from
`NodeRecord.provider`), not by run, which is what makes the tree read "account -> what it works on".
Elapsed is recomputed every frame against `now`, never accumulated: from `started_at`, or from
`created_at` for a row that has not started, which is the wait a queued node is judged on.

Sections: **in flight** (`Running`, `Leased`), **waiting** (`Queued`, `Blocked { until, why }`),
**recent** (last `N = 8` terminal nodes by `ended_at`, fading through `meta` then dropped after
`RECENT_TTL = 5m`). `waiting` and the unknown-account list draw at most `SECTION_MAX = 32` rows,
longest wait first; their headings read `waiting 32 of 480` when a fan-out batch queues more, so
a frame costs the same whatever the backlog. An account's `inflight/max_concurrency` counts every
tailed run even while `tab` draws one: what is drawn is a view, what an account carries is a fact.

### 2.3 Refresh

Polling, not `notify`: the crate is not a dependency, `Tailer::poll` already sleeps internally when
there is nothing new, and kqueue on macOS needs one fd per watched file and misses the
truncate-and-rewrite case. Cadence:

| What | Period |
|---|---|
| journal tails | `Tailer::poll`, driven by the same `select!` as the ticker |
| `accounts.json` | 1 s, and only when `mtime` changed; the fs4 lock is held for the read only |
| run discovery (`list_runs`) | 5 s |
| spinner / elapsed redraw | `ui.refresh_hz` (default 20), paused when nothing is animating |

The frame is drawn once per loop turn; deltas coalesce. An idle board wakes on the 1 s account poll
and nothing else.

**Staleness.** A run whose brain node is non-terminal but whose pidfile fails `is_ours` is marked
stale: its header gets `· stale` in `err`, its spinners freeze to `?` (`Glyph::Orphaned`), and
`RunView::mark_orphans` runs with the same liveness closure `watch::App` uses. An `accounts.json`
older than `cfg.quota_max_age()` renders its percentages in `meta` + `DIM` with `observed 4m ago`
in the header, the same rule `ui::usage` uses.

---

## 3. What it draws

Tree, top to bottom: **provider -> account -> the nodes that account is running**, then `waiting`,
then `recent`, then the dispatch reason for the selected row, then the key hints.

Per account: health glyph (`shown_health` -> `health_word`/`health_color`), `inflight/max_concurrency`
(`-` when unset, which means unlimited), a 5h and a 7d bar with percentage and reset countdown, and
tokens for the current window. Bars are `watch::gauge_bar`-shaped; the board uses `▇`/`░` at width 10
(width 5 under 52 columns), and `-` when there is no measured window at all.

Per node: state glyph (`spinner` frames while `Running`, `Glyph::Queued`/`Leased`, else
`fmt::glyph(&state)`), short id with `·N` when `attempt > 1`, tier, model, title truncated, elapsed,
live tokens, and cost where it fits. The brain gets its own row on its account, marked `◆`, titled
with the working verb.

Three layouts, chosen by width. The 40-column one is the contract: a side pane is usually narrow.

### 3.1 40 columns

```
swamp board       2 runs · 4 in flight
────────────────────────────────────────
anthropic
● main       3/4  5h ▇▇▇▇░  71% ↻38m
                  7d ▇▇░░░  32% ↻4d2h
  ◆ brain    -     4m12s          ↓ 214k
    orchestrating
  ⠋ 9g5f01   mid   3m10s          ↓ 118k
    add pagination to /users
  ⠙ 9g5f02   low   1m02s           ↓ 61k
    rename the fixtures
◐ alt        1/-  5h ▇▇▇▇▇  93% ↻12m
                  7d ▇▇▇░░  54% ↻2d7h
    degraded
  ⠹ 9g5f03·2 high  2m48s          ↓ 223k
    backfill the users index
openai
✖ codex-main 0/2  5h ▇▇▇▇▇  99% ↻1h04m
                  7d ▇▇▇▇░  81% ↻3d1h
    cooling until 15:11

waiting 1
  · 9g5f04   mid   4m02s                -
    rebuild the index
    anthropic at limit · 14:45
recent 3
  ✔ 9g5efe   low   1m51s           ↓ 96k
  ✘ 9g5eff   mid   0m44s           ↓ 12k
────────────────────────────────────────
9g5f03 → alt   score .41
  util .93×.50   load .33×.30
  share .12×.15  main lost on util
────────────────────────────────────────
↑↓ · ↵ trace · tab run · q quit
```

Titles move to their own indented line, the model and cost cells drop, bars shrink to 5 cells, and
the reason block wraps to three lines. Nothing that identifies a node is ever dropped.

### 3.2 60 columns

```
swamp board              2 runs · 4 in flight · 14:07
────────────────────────────────────────────────────────────
anthropic                                         2 accounts
● main          3/4  5h ▇▇▇▇▇▇▇░░░  71% ↻ 38m      ↓ 812k
                     7d ▇▇▇░░░░░░░  32% ↻ 4d2h
  ◆ brain    -     orchestrating            4m12s   ↓ 214k
  ⠋ 9g5f01   mid   add pagination to /users 3m10s   ↓ 118k
  ⠙ 9g5f02   low   rename the fixtures      1m02s    ↓ 61k
◐ alt           1/-  5h ▇▇▇▇▇▇▇▇▇░  93% ↻ 12m      ↓ 402k
                     7d ▇▇▇▇▇░░░░░  54% ↻ 2d7h   degraded
  ⠹ 9g5f03·2 high  backfill the users index 2m48s   ↓ 223k
openai                                             1 account
✖ codex-main    0/2  5h ▇▇▇▇▇▇▇▇▇▇  99% ↻ 1h04m       ↓ 0
                     7d ▇▇▇▇▇▇▇▇░░  81% ↻ 3d1h    cooling

waiting 1
  · 9g5f04   mid   rebuild the index        4m02s  blocked
    every anthropic account is at its limit
    earliest reset 14:45 (in 38m)
recent 3
  ✔ 9g5efe   low   add the /users route     1m51s    ↓ 96k
  ✘ 9g5eff   mid   seed script              0m44s    ↓ 12k
  ⊘ 9g5f00   low   probe the migration      0m08s     ↓ 2k
────────────────────────────────────────────────────────────
9g5f03 → alt   score .41 = util .93×.50 + load .33×.30
               + share .12×.15 − weight .00 − idle .02
               main scored .58 and lost on util
────────────────────────────────────────────────────────────
↑↓ select · ↵ trace · tab run · a accounts · q quit
```

### 3.3 100 columns

```
swamp board                                    2 runs · 4 in flight · 3 accounts · ~$0.47 · 14:07:22
────────────────────────────────────────────────────────────────────────────────────────────────────
anthropic                                                                   2 accounts · 4 in flight
● main          healthy     3/4  5h ▇▇▇▇▇▇▇░░░  71%  ↻ 38m    7d ▇▇▇░░░░░░░  32%  ↻ 4d2h     ↓ 812k
  ◆ brain   9g5fav  -      orchestrating                    sonnet-4.5   4m12s  ↓ 214k  ~$0.09
  ⠋ 9g5f01  9g5fav  [mid ] add pagination to /users         sonnet-4     3m10s  ↓ 118k  ~$0.08
  ⠙ 9g5f02  9g5fav  [low ] rename the fixtures              haiku-4.5    1m02s   ↓ 61k  ~$0.01
◐ alt           degraded    1/-  5h ▇▇▇▇▇▇▇▇▇░  93%  ↻ 12m    7d ▇▇▇▇▇░░░░░  54%  ↻ 2d7h     ↓ 402k
  ⠹ 9g5f03  9g5fbz  [high] backfill the users index  ·2     opus-4.1     2m48s  ↓ 223k  ~$0.21
openai                                                                       1 account · 0 in flight
✖ codex-main    cooling     0/2  5h ▇▇▇▇▇▇▇▇▇▇  99%  ↻ 1h04m  7d ▇▇▇▇▇▇▇▇░░  81%  ↻ 3d1h        ↓ 0

waiting 1
  · 9g5f04  9g5fbz  [mid ] rebuild the index                -            4m02s       -       -
    every anthropic account is at its limit · earliest reset 14:45 (in 38m)
recent 3
  ✔ 9g5efe  9g5fav  [low ] add the /users route             sonnet-4     1m51s   ↓ 96k  ~$0.04
  ✘ 9g5eff  9g5fav  [mid ] seed script                      sonnet-4     0m44s   ↓ 12k  ~$0.01
  ⊘ 9g5f00  9g5fbz  [low ] probe the migration              haiku-4.5    0m08s    ↓ 2k  ~$0.00
────────────────────────────────────────────────────────────────────────────────────────────────────
9g5f03 → alt   score .41 = util .93×.50 + load .33×.30 + share .12×.15 − weight .00 − idle .02
               main scored .58 and lost on util; codex-main ineligible (cooldown until 15:11)
────────────────────────────────────────────────────────────────────────────────────────────────────
↑↓ select · enter trace · tab run · a accounts · r raw · q quit           9g5fav 9g5fbz · fresh 0.4s
```

With two or more runs the run column appears (100 columns) or `tab` cycles which run's nodes are
shown (40 and 60). Column drop order, widest first: cost, model, run, tier, tokens. Glyph, id and
title never drop; the title moves to its own line rather than disappearing.

### 3.4 The dispatch reason

The selected node's footer explains **why that account won**. `JournalEvent::AccountSelected` already
records `{ account, policy, reason, excluded }`, but `reason` is only `format!("score {sc:.4}")`
today. WP5 adds `policy::explain(policy, a, s, pool_window, cfg, now) -> String`, the term-by-term
string above, and `pool.rs:951` emits it instead. The board then prints the recorded string, so it
shows the terms **as they were at dispatch**, not a re-scored guess. Runner-up and exclusion lines
come from `excluded` plus a live re-score of the other accounts, and are labelled `now` when they
disagree with the recorded reason. Theme: `accent` for the winning account, `meta` for the terms,
`err` for a hard gate.

---

## 4. Keys

| Key | Behaviour |
|---|---|
| `↑` `↓` | move the selection through accounts, nodes, waiting and recent, in draw order |
| `←` `→` | collapse / expand an account's node list |
| `enter` | open the selected node's trace, `trace::render(&view, &TraceOpts { node, events: true, .. })`, in a full-pane scroll view; `esc` returns |
| `tab` / `shift+tab` | next / previous run when more than one is tailed; `0` shows all runs merged |
| `a` | accounts-only view: hide nodes, show every account with full quota buckets, the `/usage` table |
| `r` | raw: the last 200 journal lines of the selected run |
| `f` | follow on/off (follow pins the selection to the newest in-flight node) |
| `g` / `G` | top / bottom |
| `q`, `ctrl+c`, `ctrl+d` | quit 0 |

No key mutates anything: no cancel, no enable/disable. Read-only is what makes it safe to leave open
in a pane forever. `swamp cancel` stays the way to act.

Theme is `ui::chat::theme::Theme::detect(color, cfg.ui.chat_theme)`, reusing `Role::{Accent, Ok, Err,
Meta, Name, Run, TierHi}` and the `Glyph` table with its ASCII fallback, so the board matches chat
byte for byte on glyphs and colours. Full-screen means `EnterAlternateScreen` plus
`watch::TerminalGuard` and `watch::install_panic_hook`, which already take a restore fn.

---

## 5. Launch ergonomics

`swamp chat` prints one `meta` hint line in its welcome block, only when `TMUX` is set and no board
is attached:

```
  board: `swamp board` in a split, or restart with `swamp chat --board`
```

`swamp chat --board`, inside tmux, runs
`tmux split-window -h -l 46 -d swamp board --run <id>` once at startup and carries on; outside tmux
it prints one line naming the command to run in another terminal and does not fail. A `--board-width`
config key (`ui.board_width`, default 46) sizes the split.

**How they find each other: they do not need to.** Both read the same files; there is no IPC, no
socket, no shared memory. The only coordination is the hint: the board writes `~/.swamp/board.pid`
(pid plus the tty it owns) at startup and removes it on exit, and chat suppresses the hint when that
file names a live process. A stale pid file is harmless - chat checks liveness the same way
`is_ours` does, and the worst case is one extra hint line.

---

## 6. Work packages

| WP | Files | Contents |
|---|---|---|
| 1 | `src/cli.rs`, `src/cmd/board.rs`, `src/cmd/mod.rs` | `BoardArgs { run, all, interval, once, json }`, the `swamp board` subcommand, the hidden `watch --board` alias |
| 2 | `src/ui/board/model.rs` | `Board`, `RunPane`, `SelectionNote`, discovery, the board's own `AccountSelected` projection. Pure, no I/O beyond the loaders it is handed |
| 3 | `src/ui/board/sources.rs` | multi-run `Tailer` set, `accounts.json` mtime-gated reload under the fs4 lock, `list_runs` rediscovery, staleness via `is_ours` |
| 4 | `src/ui/board/render.rs` | the three layouts, `layout_for(width)`, column drop order, bars, spinners, `Theme` wiring |
| 5 | `src/dispatch/policy.rs`, `src/dispatch/pool.rs` | `policy::explain`, emitted into `AccountSelected.reason` (one-line change at `pool.rs:951`) |
| 6 | `src/ui/board/app.rs` | keys, selection, the trace overlay, the `select!` loop and the alternate-screen guard |
| 7 | `src/journal/paths.rs`, `src/cmd/{run,chat}.rs` | `~/.swamp/runs.json` index for `--all`; `~/.swamp/board.pid` |
| 8 | `src/ui/chat/mod.rs`, `src/cli.rs` | the tmux hint line and `swamp chat --board` |

WP1-4 alone give a usable board. WP5 is independently shippable and improves `swamp trace` too.

---

## 7. Tests

`src/ui/board/screens.rs`, mirroring `src/ui/chat/screens.rs`: `TestBackend` plus `insta`, all
offline, `Theme::Plain` for stable bytes.

1. **Width snapshots.** One fixture journal pair, rendered at 40, 60 and 100 columns. Asserts the
   drop order and that id, glyph and title survive at 40.
2. **Two runs, one exhausted account.** Journal A has two running nodes on `main`; journal B has one
   node `Blocked { until, why }` after `main` hit `stop_at`. Asserts: one `waiting` entry with the
   reason and the earliest reset, `codex-main` shown `cooling`, and the totals line summing both runs.
3. **Staleness.** A run whose brain node is `Running` with no pidfile renders `· stale` and frozen
   glyphs; `mark_orphans` is exercised with the same closure `watch.rs` tests use.
4. **Selection reason.** A journal carrying `AccountSelected { reason: "<terms>", excluded: [main] }`
   renders the footer verbatim; a journal with the old `score 0.41` form degrades to one line.
5. **`vt100`** round trip through `src/ui/board/tests.rs`, the pattern `src/ui/chat/live/tests.rs`
   already uses: enter alternate screen, three frames, a resize from 100 to 40 and back, assert the
   emulator holds exactly one board and no torn row.
6. **Control characters.** A node title carrying `\u{1b}[2J` must not repaint the pane;
   `fmt::sanitize` and `fmt::truncate` on every string that came from a model or the filesystem.
7. **tmux smoke script** (`scripts/board-tmux.sh`, manual, not CI): open a 120x40 session, split
   `-h -l 46`, run chat left and board right, drive a two-node dispatch, resize the split to 40 and
   back, and diff `capture-pane` output against the snapshots. This is the only place the real
   terminal is exercised; everything above is deterministic.

---

## 8. Risks

- **Journal tailing across truncation.** `Tailer` follows an append-only file by offset. `swamp gc`
  or a replay that rewrites a journal makes the offset meaningless. Mitigation: record `(dev, ino,
  len)` with the offset and re-open from zero when the inode changes or the length shrinks; the
  fold is idempotent (`RunView::apply` replays a prefix to the same state), so a re-read is safe.
- **kqueue on macOS** is why WP3 polls. If polling ever shows up in a profile, `notify` can be added
  behind a feature flag, with polling kept as the fallback; it must not become the only path.
- **fs4 lock contention.** A dispatcher writing `accounts.json` holds an exclusive lock on the
  sibling `.lock` file across a rename. A board polling every second with a blocking lock can stall
  it. Mitigation: `try_lock_shared`, skip the frame on failure, and never hold the lock across a
  render.
- **Many runs.** `--all` folds every live run's journal. Cap at 8 tailed runs, newest first, and say
  so in the header.
- **Drift with `/usage` and `swamp watch`.** Mitigated structurally: the board reuses `AccountRow`,
  `rows_from`, `health_word`, `shown_health`, `gauge_bar`, `fmt::*`, `trace::*` and folds the same
  `RunView`. Any new formatting helper goes in the shared module, not in `src/ui/board/`.
- **Two windows disagreeing.** Chat's pool snapshot is in-process and instant; the board's is the
  persisted file, up to one second behind. The board's header carries `fresh 0.4s` so the lag is
  visible rather than confusing.
