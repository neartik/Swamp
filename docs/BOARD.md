# `swamp board` - the dispatch board

> "The dispatching of tasks must be visible; always show on the side the graph of which account
> works on what."

The board is a **separate full-screen program** you keep open in a pane next to `swamp chat`. It
answers one question at a glance: *which dispatch is doing what, on which account, right now, and
what is stuck and why.* It is read-only except a confirmed cancel: it tails files, never talks to
a supervisor, and the one thing it ever changes, after a `y` / `n` prompt, is the cancel
`swamp cancel` makes from any terminal.

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
is the name people will guess. `swamp watch` shares the board's key table (`src/ui/keys.rs`) and
its cancel (`ui::actions::spawn_cancel`), nothing else.

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

Rows are grouped **by dispatch**, from the schema-2 fold (`RunView.dispatches`, `RunView.tasks`),
never by account. Per tailed run, newest run first:

1. **The brain row**: the run's root node, titled `orchestrating` while it runs.
2. **Active root dispatches**: open dispatches whose caller is the run's brain, in stable
   `call_seq` order (those without one follow, by `record.at` then id). The schema-1 `legacy`
   bucket comes last and stays open until `RunFinished`. An open dispatch whose tasks have all
   ended stays active until `DispatchSettled`.
3. **Tasks** under each dispatch, ranked by `ui::order::rank`: failed, rejected and orphaned
   first, then running and leased, then blocked, then queued, then done and cancelled; ties keep
   dispatch order. Legacy rows keep `view.tree()` order instead. A dispatch draws at most
   `DISPATCH_ROWS = 8` tasks and counts the rest.
4. **Nested dispatches** (`inspect::issued_by`) right under the task that issued it, one level
   deeper, recursively down to `dispatches::MAX_NESTING`.
5. **`recent`**: settled root dispatches, newest first by their last task's end, at most
   `RECENT = 8`, dropped after `RECENT_TTL = 5m`, folded by default.

A task row is its live attempt (the id `swamp diff` and `swamp adopt` take, `·N` once it is a
retry), or its logical id before it has one. Elapsed is recomputed every frame against `now`:
from the live attempt's `started_at`, from the dispatch's `at` for a task still waiting, and blank
for one that ended without ever starting. The header counts (`running`, `stuck` = failed +
rejected + orphaned + blocked, `queued`) cover every open dispatch of every tailed run, even when
`tab` draws one; so does an account's `inflight/max_concurrency` in the strip.

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
stale: its header gets `N stale` in `err`, its spinners freeze to `?` (`Glyph::Orphaned`), and
`RunView::mark_orphans` runs with the same liveness closure `watch::App` uses. An `accounts.json`
older than `cfg.quota_max_age()` renders its percentages in `meta` + `DIM` with `observed 4m ago`
in the header, the same rule `ui::usage` uses.

---

## 3. What it draws

Top to bottom: the header (two rows at Narrow, one otherwise), a rule, the body (runs, dispatch
groups and their tasks, `recent`), a rule, the accounts strip (one line per account, at most
`ACCOUNTS_MAX = 6`), a rule, the detail block for the selected row (a fixed `detail_max` lines,
shrinking down to 1 so the body keeps 8), and the key hints. The body scrolls to follow the
selection; everything under it stays put.

Principles: one line per thing, and a second line only for a stuck task (blocked, failed,
rejected, orphaned) naming why; titles are cut with `…` and the full title is in the trace pager;
cells that do not apply are blank rather than `-`; only state gets colour.

### 3.1 Layout tiers

`render::layout_for` picks one of three declared `Layout` values; nothing else turns a width into a
decision. Within a tier only the title width and the header and hint cells that fit change.

| Field | `NARROW` `<60` | `MEDIUM` `60-99` | `WIDE` `>=100` |
|---|---|---|---|
| header rows | 2 | 1 | 1 |
| task right block | elapsed | account 8, elapsed, cost | account 10, model 10, elapsed, tokens, cost |
| `[mid ]` tier cell | no | no | yes |
| `title_min` at the tier's narrowest width (40 for Narrow) | 12 | 16 | 24 |
| indent cap | 1 | 1 | 2 |
| short id after `#N` | no | yes | yes |
| dispatch cost | inline cell | cost column | cost column |
| blocked line | first refusal, `+N` | every refusal, as words | every refusal, with numbers |
| account strip | percentages | 5h bar, 7d percentage | both bars, the gate, tokens |
| account name width | 10 | 11 | 12 |
| `detail_max` | 5 | 6 | 4 |

The header reads the same at every width: `N running · N stuck · N queued · ~$X · observed Xs
ago`, with `N runs` first when more than one run is tailed and `N stale` in `err`. When the cells
do not fit, `queued`, then `runs`, then the cost drop; `running`, `stuck`, `stale` and the
freshness never do. Freshness is `err` once older than `quota_max_age`.

A dispatch header is `▾`/`▸`, the label (`#N`, the short id without a call number, or `legacy`),
`by <caller>` on a nested one, `N tasks`, the count phrase in rank order, and, on a `recent` row,
the first failure's short reason. When it does not fit, `N tasks` drops, then trailing
`queued`/`done`/`cancelled` counts, then the short id, then the Narrow cost cell; failure,
rejection, orphan, running and blocked counts never drop.

### 3.2 Mockups

The fixture every snapshot uses: dispatch #1 with a retry, a blocked task and a queued one; #2
rejected by `max_nodes_per_run`. Until a key moves the cursor it sits on the first failed task,
else the first blocked one, else the first running one, else the brain.

**40 columns (Narrow), default selection:**

```
swamp board              observed 4s ago
2 running · 1 stuck · 1 queued · ~$0.43
────────────────────────────────────────
  ◆ brain      orchestrating       4m12s

  ▾ #1 · 2 running · 1 blocked · ~$0.34
    ⠋ 9g5f01   add pagination to…  3m10s
    ⠙ 9g5f09·2 backfill the user…  2m48s
▌   ⏸ 9g5f04   rebuild the index   3m20s
      until 22:54 · main at capacity +1
    · 9g5f0a   write the changel…  3m20s
    ✔ 9g5f05   add the /users ro…  2m10s

  recent
  ▸ #2 · 1 rejected · max_nodes_per_run
────────────────────────────────────────
  ● main       2/2  5h  71%  7d  32%
  ◐ alt        1/-  5h  93%  7d  54%
  ✘ codex-main 0/2  cooling until 23:20
────────────────────────────────────────
  9g5f04 blocked · until 22:54 (in 38m)
    main  at capacity (2/2)
    alt   quota stop (93%)


↑↓ select · k cancel · ? keys · q quit
```

**60 columns (Medium), the retry selected:**

```
swamp board   2 running · 1 stuck · ~$0.43 · observed 4s ago
────────────────────────────────────────────────────────────
  ◆ brain      orchestrating         main      4m12s  ~$0.09

  ▾ #1 9g5f18 · 2 running · 1 blocked          3m20s  ~$0.34
    ⠋ 9g5f01   add pagination to /u… main      3m10s  ~$0.08
▌   ⠙ 9g5f09·2 backfill the users i… alt       2m48s  ~$0.22
    ⏸ 9g5f04   rebuild the index               3m20s
      until 22:54 · main at capacity · alt quota stop
    · 9g5f0a   write the changelog             3m20s
    ✔ 9g5f05   add the /users route  main      2m10s  ~$0.04

  recent
  ▸ #2 9g5f1c · 1 task · 1 rejected · max_nodes_per_run
────────────────────────────────────────────────────────────
  ● main        2/2   5h ▇▇▇▇▇▇▇░░░  71% ↻ 38m     7d  32%
  ◐ alt         1/-   5h ▇▇▇▇▇▇▇▇▇░  93% ↻ 12m     7d  54%
  ✘ codex-main  0/2   cooling until 23:20
────────────────────────────────────────────────────────────
  9g5f09·2 → alt   score .41 = util .93×.50 + load .33×.30 +
                   share .12×.15 − weight .00 − idle .02
                   passed over (now): main at capacity (2/2)
                   · codex-main cooling until 23:20
                   attempt 1 9g5f08 on main: rate_limited
                   (five_hour) after 41s
↑↓ select · k cancel · enter open · r raw · ? keys · q quit
```

**140 columns (Wide):**

```
swamp board                                                                        2 running · 1 stuck · 1 queued · ~$0.43 · observed 4s ago
────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
  ◆ brain             orchestrating                                                             main       opus-4-1    4m12s  ↓ 214k  ~$0.09

  ▾ #1 9g5f18 · 5 tasks · 2 running · 1 blocked · 1 queued · 1 done                                                    3m20s  ↓ 437k  ~$0.34
    ⠋ 9g5f01   [mid ] add pagination to /users                                                  main       sonnet-4-5  3m10s  ↓ 118k  ~$0.08
▌   ⠙ 9g5f09·2 [high] backfill the users index                                                  alt        opus-4-1    2m48s  ↓ 223k  ~$0.22
    ⏸ 9g5f04   [mid ] rebuild the index                                                                                3m20s
      until 22:54 (in 38m) · main at capacity (2/2) · alt quota stop (93%)
    · 9g5f0a   [low ] write the changelog                                                                              3m20s
    ✔ 9g5f05   [low ] add the /users route                                                      main       sonnet-4-5  2m10s   ↓ 96k  ~$0.04

  recent
  ▸ #2 9g5f1c · 1 task · 1 rejected · max_nodes_per_run
────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
  ● main         2/2   5h ▇▇▇▇▇▇▇░░░  71% ↻ 38m     7d ▇▇▇░░░░░░░  32% ↻ 4d02h   at capacity (2/2)                                    ↓ 812k
  ◐ alt          1/-   5h ▇▇▇▇▇▇▇▇▇░  93% ↻ 12m     7d ▇▇▇▇▇░░░░░  54% ↻ 2d07h   quota stop (93%)                                     ↓ 402k
  ✘ codex-main   0/2   5h ▇▇▇▇▇▇▇▇▇▇  99% ↻ 1h04m   7d ▇▇▇▇▇▇▇▇░░  81% ↻ 3d01h   cooling until 23:20                                     ↓ 0
────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
  9g5f09·2 → alt   score .41 = util .93×.50 + load .33×.30 + share .12×.15 − weight .00 − idle .02
                   passed over (now): main at capacity (2/2) · codex-main cooling until 23:20
                   attempt 1 9g5f08 on main: rate_limited (five_hour) after 41s

↑↓ select · k cancel · enter open · r raw · ←→ fold · ! stuck · a accounts · tab run · f follow · 0 all runs · g G ends · ? keys · q quit
```

A nested dispatch, under the task that issued it (60 columns, the dispatch selected):

```
swamp board   2 running · 1 stuck · ~$0.43 · observed 4s ago
────────────────────────────────────────────────────────────
  ◆ brain      orchestrating         main      4m12s  ~$0.09

  ▾ #1 9g5f18 · 2 running · 1 blocked          3m20s  ~$0.34
    ⠋ 9g5f01   add pagination to /u… main      3m10s  ~$0.08
▌     ▾ #3 9g5f1k · by 9g5f01 · 2 queued       1m02s
        · 9g5f1m   split the handler           1m02s
        · 9g5f1n   write the tests             1m02s
    ⠙ 9g5f09·2 backfill the users i… alt       2m48s  ~$0.22
    ⏸ 9g5f04   rebuild the index               3m20s
      until 22:54 · main at capacity · alt quota stop
    · 9g5f0a   write the changelog             3m20s
    ✔ 9g5f05   add the /users route  main      2m10s  ~$0.04

  recent
  ▸ #2 9g5f1c · 1 task · 1 rejected · max_nodes_per_run
────────────────────────────────────────────────────────────
  ● main        2/2   5h ▇▇▇▇▇▇▇░░░  71% ↻ 38m     7d  32%
  ◐ alt         1/-   5h ▇▇▇▇▇▇▇▇▇░  93% ↻ 12m     7d  54%
  ✘ codex-main  0/2   cooling until 23:20
────────────────────────────────────────────────────────────
  #3 9g5f1k · by 9g5f01 · open · 1m02s ago
  2 tasks · 2 queued
  swamp dispatch 9g5f1k



↑↓ select · k cancel · enter open · r raw · ? keys · q quit
```

A schema-1 run is one bucket: `▾ legacy · 3 tasks · 2 running · 1 done      4m02s  ~$0.19`.

### 3.3 Stuck lines, the row cap and the accounts strip

A stuck task's second line starts under its title: `until 22:54` plus the refusals the pool
recorded in `NodeBlocked.ineligible` (never the free-text `why`), in the tier's form; the short
failure of a failed task; `rejected: <reason>`; `orphaned: pid N is gone`. Past `DISPATCH_ROWS`
the tail of the rank order becomes `… +N more · enter lists all`.

The strip is one line per account: health glyph, name, `inflight/max_concurrency` recounted from
the journals (`-` when unlimited), then the tier's quota cells. An account that is cooling, has
broken auth or is disabled says so instead of its bars below Wide; at Wide the status cell is the
gate `policy::gate` finds (`at capacity (2/2)`, `quota stop (93%)`, `cooling until 23:20`), else
its health word when it is not healthy. A percentage at or past `stop_at` is `err`; a stale one is
`meta`.

### 3.4 The detail block

The selected task's detail explains **why that account won**. `JournalEvent::AccountSelected`
records `{ account, policy, reason, excluded }` and `policy::explain` writes the term-by-term
reason, so the board prints the terms **as they were at dispatch**, not a re-scored guess. At
Narrow the score stays on the head line and the terms wrap under it; wider tiers wrap the whole
string on a hanging indent.

`passed over` names every account in `excluded`. The verdict is the recorded one when the task's
`TaskView.ineligible` (from its latest `NodeBlocked`) lists that account; otherwise it is the live
`policy::gate`, tagged `now` (the prefix becomes `passed over (now):` when every item is live), or
`eligible now` when nothing holds the account back today. Then `failed:` for a failed task and one
line per earlier attempt (`attempt 1 9g5f08 on main: rate_limited (five_hour) after 41s`).

A blocked task's detail is its `until` and one line per recorded refusal with its numbers; a
dispatch's is its label, caller, state and age, its totals, and `swamp dispatch <id>`, the one line
on the board meant to be copied. Theme: `accent` for the winning account, `meta` for the terms,
`err` for every gate except `at capacity`.

---

## 4. Keys: read-only except confirmed cancel

The keys are the README's [Keys](../README.md#keys) table, generated from `src/ui/keys.rs`: the
hint line, the `?` pager and the README cannot disagree, and the same action is the same key on the
board, in the pagers and in `swamp watch`.

| Key | Behaviour |
|---|---|
| `↑` `↓`, `g` `G` | move through the brain, each dispatch and its tasks, `recent`, then the accounts, in draw order |
| `!` | the next stuck row: a failed, rejected, orphaned or blocked task, or a folded dispatch hiding a failure |
| `←` `→` | fold / unfold the selected dispatch; `←` on a task folds its dispatch and moves to the header |
| `enter` | the trace of a task (`trace::render`), `swamp dispatch` of a dispatch, the accounts view of an account |
| `r` | the last 200 journal lines of the selected run, also from inside a trace or dispatch pager |
| `k` | cancel the selected task or dispatch, after `y` / `n` |
| `tab` / `shift+tab`, `0` | next / previous run, all runs merged |
| `a`, `f`, `?` | accounts view, follow the newest running task, the key list |
| `esc` | back from a pager or the accounts view; `q`, `ctrl+c`, `ctrl+d` quit |

`k` is the one key that changes anything. It asks first (`cancel 9g5f04 "rebuild the index"? y /
n`, or `cancel #1 9g5f18 · 4 live tasks? y / n`); `y` runs `dispatch::cancel::cancel_node` for the
task, or for the dispatch's direct tasks (`dispatch_tasks`, exactly what `swamp cancel dsp_…`
covers), on a task of its own through `ui::actions::spawn_cancel`, so a grace period never freezes
a frame. Any other key dismisses the prompt. The brain, a finished task and an account get a
one-line notice instead of a prompt. The whole action sits behind `ui.board_actions` (default
`true`); with it off, `k` only says `read-only: ui.board_actions = false` and leaves the hints and
the key list.

Theme is `ui::chat::theme::Theme::detect(color, cfg.ui.chat_theme)`, reusing `Role::{Accent, Ok,
Err, Meta, Name, Run, TierHi, Code}` and the `Glyph` table with its ASCII fallback, so the board
matches chat byte for byte on glyphs and colours. Full-screen means `EnterAlternateScreen` plus
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
offline, `Theme::plain()` for stable bytes, one fixture (`tests_support::p4_journal`) shared with
chat.

1. **Tier snapshots** at 40, 52, 60, 85, 100 and 140 columns, plus a nested dispatch and the
   confirm prompt. The tiers start at 0, 60 and 100, and each keeps `title_min` title columns at
   its narrowest width.
2. **Model**: tasks group under their dispatch in call order and rank inside it; a settled dispatch
   moves to `recent` and an open one whose tasks ended does not; a nested dispatch hangs under its
   task; a schema-1 run is one legacy bucket in tree order; the default selection is the first
   stuck task; header tallies sum every run whatever the focus.
3. **Staleness**: a run whose brain has no live pidfile renders `stale` and frozen glyphs.
4. **Detail**: the recorded terms verbatim, recorded refusals first and live gates tagged `now`,
   the old `score 0.41` form on one line.
5. **Keys and cancel**: `k` then `y` is exactly one `Action::Cancel`; `n`, `esc` or anything else
   emits nothing and clears the prompt; the brain, a finished task and a read-only board get a
   notice. The hint line equals `keys::hints(Board, …)`.
6. **Pagers**: trace, dispatch and raw each title themselves and carry their own hints.
7. **`vt100`** round trip through `src/ui/board/tests.rs`: enter the alternate screen, three
   frames, a resize from 100 to 40 and back, exactly one board and no torn row.
8. **Control characters.** A title carrying `\u{1b}[2J` must not repaint the pane.
9. **tmux smoke script** (`scripts/board-tmux.sh`, manual, not CI).

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
  persisted file, up to one second behind. The board's header carries `observed 4s ago` so the lag is
  visible rather than confusing.
