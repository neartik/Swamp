# `swamp chat` - terminal UI specification

One implementation pass. Target: Claude Code's terminal grammar, with the live worker board as
the swamp-specific extension. No new crates: `ratatui 0.29`, `crossterm 0.28` (`event-stream`),
`textwrap`, `unicode-width`, `insta` are already in `Cargo.toml`. `rustyline` is dropped.

Two rules settle every conflict below:

1. Where look or keyboard is in question, Claude Code fidelity wins.
2. Where swamp has something Claude Code does not (dispatched workers), the worker tree wins:
   `swamp_dispatch` renders a live board, and the status line reports the run.

---

## 0. Prerequisites

Three small diffs outside `src/ui/`, landed before the blocks work.

**P1 - `src/brain/mod.rs`.** Tool calls and results cannot be paired today: `ToolDone.name` is the
name copied out of `ParseState` and there is no id. Replace the two variants with

```rust
ToolCall { id: String, name: String, preview: String },
ToolDone { id: String, name: String, ok: bool, detail: Option<String> },
```

and forward `id` from `WorkerEvent::ToolCall` / `ToolResult` in `brain_event()`, `name` from the
`st.tool_names` lookup `tool_result` already does.

**P2 - `src/worker/claude.rs`, `src/worker/codex.rs`, `src/model/event.rs`.** `Block::ToolResult`
discards the body. Add `#[serde(default)] content: Option<ClaudeContent>`, flatten it to text, and
add `detail: Option<String>` to `WorkerEvent::ToolResult`, truncated at `RESULT_MAX = 4096` bytes
through `classify::truncate`. Without P2 the `⎿` connector has nothing to print but ok/failed and
the collapse-to-3-lines affordance does not exist.

**P3 - `src/config/schema.rs`, `UiCfg`.** Add

```rust
pub chat_theme: Option<String>,     // auto | truecolor | ansi256 | plain
pub collapse_lines: Option<usize>,  // default 3
pub chat_history: Option<usize>,    // default 500
```

`refresh_hz` already exists and drives the chat redraw tick (default 12).

Everything else is additive. `src/ui/chat.rs` becomes the directory `src/ui/chat/`;
`ui::chat::repl(brain, disp, ctx) -> Result<i32>` keeps its signature so `src/cmd/chat.rs` is
untouched. Three helpers in `trace.rs` become `pub(crate)`: `failure_summary`, `failure_detail`,
`account_cell`.

---

## 1. Architecture

### 1.1 Inline viewport, never the alternate screen

Finished blocks scroll into real scrollback so the user can select and copy them with the mouse.
Only the live tail is redrawn.

```rust
enable_raw_mode()?;                                  // no EnterAlternateScreen
execute!(stdout(), PushKeyboardEnhancementFlags(
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
  | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS))?;
let mut term = Terminal::with_options(
    CrosstermBackend::new(stdout()),
    TerminalOptions { viewport: Viewport::Inline(MIN_LIVE) },
)?;
let _guard = TerminalGuard::with(restore_inline);    // raw off, pop flags, show cursor
```

`MIN_LIVE = 4`: rule, input, rule, status. `install_panic_hook` and `TerminalGuard` move from
`watch.rs` into `ui/mod.rs` unchanged and are parameterised by the restore fn they already take.
`restore_inline` does **not** call `LeaveAlternateScreen`. `swamp watch` keeps the full-screen
view; chat is inline.

**Non-tty.** If `!std::io::stdout().is_terminal()`, `repl` runs today's plain printer
(`drain_turn`, moved verbatim into `mod.rs`). CI, pipes and scripted runs are unaffected.

### 1.2 Committing blocks to scrollback

```rust
fn commit(term: &mut Terminal<B>, lines: Vec<Line<'static>>) -> io::Result<()> {
    for chunk in lines.chunks(CHUNK) {              // CHUNK = 200
        let h = chunk.len() as u16;
        term.insert_before(h, |buf| {
            Paragraph::new(chunk.to_vec()).render(buf.area, buf);  // no Wrap: pre-wrapped
        })?;
    }
    Ok(())
}
```

Pre-wrapping is mandatory: `insert_before` takes the height up front, so any wrapping ratatui did
itself would clip. All wrapping happens in `markdown.rs` (`wrap_spans`, span-aware, widths from
`unicode_width`). Scrollback is frozen text; wrap once, at commit time, at the current width.

What leaves the live area, and when:

| Block | Committed |
|---|---|
| Welcome | immediately at startup |
| User bar | on submit, before the brain is sent the text |
| Assistant text | line by line: every rendered line except the trailing incomplete one |
| Thinking | same, when `ui.show_thinking` |
| Tool call + result | on `ToolDone`, unless the tool is `swamp_dispatch` |
| Dispatch board | when the call is done **and** every owned node is terminal |
| Slash output | immediately |
| Error / notice | immediately |

The live area height is therefore bounded by the worker board, never by the conversation.

### 1.3 Resizing the live area

```rust
fn set_live_height(term: &mut Terminal<B>, want: u16, rows: u16) -> io::Result<()> {
    let area = term.get_frame().area();
    if area.height == want { return Ok(()); }
    if want > area.height {
        // printing newlines scrolls the host terminal and makes room below
        execute!(stdout(), Print("\n".repeat((want - area.height) as usize)))?;
    }
    term.clear()?;                                   // wipe the old rows before they move
    term.resize(Rect::new(0, rows.saturating_sub(want), term.size()?.width, want))?;
    Ok(())
}
```

Called once per frame before `draw`; a no-op in the steady state.
`want = live_lines.len()` clamped to `[MIN_LIVE, max(MIN_LIVE, rows * 3 / 5)]`. When the clamp
bites, the worker board is the part that collapses (§4.5). On `Event::Resize` every live block is
re-wrapped and the same helper runs.

### 1.4 The loop

Event sources: crossterm keys, the `BrainEvent` stream, journal-fold polls, and the refresh tick.

```rust
let period = Duration::from_millis(1000 / u64::from(cfg.ui.refresh_hz.unwrap_or(12)).max(1));
let mut ticker = tokio::time::interval(period);
loop {
    set_live_height(&mut term, app.live_height(w), rows)?;
    term.draw(|f| {
        f.render_widget(Paragraph::new(render::live(&app, f.area().width)), f.area());
        if let Some(p) = app.cursor_xy(f.area()) { f.set_cursor_position(p); }
    })?;
    let msg = tokio::select! {
        biased;
        Some(ev) = keys.next()           => Msg::Key(ev?),
        Some(be) = brain.events().recv() => Msg::Brain(be),
        lines    = tailer.poll()         => Msg::Journal(lines?),
        _        = ticker.tick()         => Msg::Tick,
        ()       = cmd::shutdown_signal()=> Msg::Signal,
    };
    for effect in app.reduce(msg) {
        match effect {
            Effect::Commit(lines) => commit(&mut term, lines)?,
            Effect::Send(text)    => brain.send(&text).await?,
            Effect::Interrupt     => brain.interrupt().await?,
            Effect::CancelAll     => { let n = disp.cancel_all(); app.note_cancelled(n); }
            Effect::Cancel(id)    => disp.cancel(id).await?,
            Effect::Clear         => execute!(stdout(), Clear(ClearType::All), MoveTo(0, 0))?,
            Effect::Quit(code)    => return Ok(code),
        }
    }
}
```

`biased` puts keys first so `esc` and `ctrl+c` stay responsive under a flood of deltas. Deltas are
coalesced: one draw per loop turn. The ticker is paused whenever nothing is animating (no turn in
flight, no live board, no armed timer), so an idle chat does not wake 12 times a second. A real
terminal cursor is used rather than a drawn block, so cursor styles and screen readers behave;
`SetCursorStyle::SteadyBlock` is pushed at startup.

`App` is pure: `reduce(Msg) -> Vec<Effect>`, no I/O, fully unit-testable. Rendering is
`render::live(&App, width) -> Vec<Line<'static>>` and `Block::render(width, &Theme)`, both
snapshot-testable with `TestBackend`.

### 1.5 Worker progress: one fold, two views

Worker state does not travel over `BrainEvent`. The chat process hosts the dispatcher and the
journal writer, and the journal already records every node transition, so chat reads it exactly
the way `swamp watch` does. No new channel, no shared mutable state, no second code path.

```
brain CLI --stdio--> mcp bridge --uds--> McpServer --> Dispatcher --> .swamp/runs/<run>/journal.jsonl
                                                                                   |
                    chat UI  <-- Tailer::poll() --> RunView::apply() --> tree() ---+
```

- `Tailer::open(&paths.journal())` once, polled in the same `select!`. `poll` sleeps internally
  when there is nothing new, so it is a well-behaved branch and needs no extra timer.
- `RunView` is built `with_events: false`: the board needs node records, not transcripts.
- `mark_orphans` runs with the same liveness closure `watch::App` uses, so a dead worker stops
  spinning forever.
- Rows come from `view.tree()` filtered to children of the brain root (`NodeId(paths.run.0)`, per
  `brain::build`), collapsed by `logical`, taking `attempts.last()` for the live record - the same
  `latest()` rule `trace.rs` uses. A retry therefore changes the row's short id in place, which is
  correct: the id shown is always the one `swamp diff` and `swamp adopt` take.
- Elapsed is recomputed per frame from `NodeRecord.started_at` against `now_utc()`, never
  accumulated, so a stall or a resize cannot drift it.
- `view.totals()` drives the status line. `Totals::cost_complete` decides `~$0.32` versus
  `~$0.32+`, matching `watch.rs`. The pool snapshot (`disp.pool().snapshot()`) is used only by the
  welcome box and `/accounts`, which need live inflight counts.

**Batch ownership.** `ToolCall { name: "swamp_dispatch", .. }` opens
`Batch { id, started, owned: BTreeSet<NodeId>, expected: Option<usize> }`; `expected` is parsed
from the preview. Any brain child appearing in the fold while a batch is open joins it. `ToolDone`
closes admission. Nodes appearing with no batch open (a retry, a resumed node) join an implicit
`Batch::loose` rendered the same way under `● workers`. Dispatch calls from one brain are serial,
so this is exact and needs no id plumbing through MCP.

---

## 2. Glyphs and colours

`src/ui/chat/theme.rs`. `Theme::detect(ctx)` order:

1. `ctx.color == false` or `NO_COLOR` set and non-empty -> `Plain`.
2. `ui.chat_theme` if set, honoured verbatim.
3. `COLORTERM` in {`truecolor`, `24bit`} -> `TrueColor`.
4. otherwise `Ansi256`. `Plain` is never guessed.

| Role | Used for | TrueColor | 256 | Plain |
|---|---|---|---|---|
| `accent` | `✻`, assistant `●`, brain spinner, `swamp adopt` hint | `Rgb(215,119,87)` | `Indexed(173)` | default |
| `ok` | finished `●`, `✔` | `Rgb(87,170,120)` | `Indexed(71)` | default |
| `err` | failed `●`, `✘`, error text | `Rgb(214,90,90)` | `Indexed(167)` | `BOLD` |
| `meta` | connectors, `⎿`, results, hints, rules, status line | `Rgb(136,136,136)` | `Indexed(245)` | `DIM` |
| `name` | tool names, headings, worker titles | Reset + `BOLD` | same | `BOLD` |
| `code` | inline code and fences | fg `Rgb(199,182,158)` bg `Rgb(38,38,38)` | `Indexed(180)` / `Indexed(235)` | `DIM` |
| `userbar` | prompt echo background | bg `Rgb(38,38,40)` | bg `Indexed(236)` | none, `> ` only |
| `tier_hi` | `[high]` cell only | `Rgb(96,150,180)` | `Indexed(74)` | default |
| `run` | in-progress `●`, queued worker glyph | as `meta` | as `meta` | `DIM` |

A `Theme::ascii` flag (set when `LANG`/`LC_ALL` lacks `UTF-8`) swaps the right column in. Every
glyph is width 1 under `unicode_width` except the multi-cell strings noted.

| Purpose | Glyph | ASCII |
|---|---|---|
| welcome star, spinner rest frame | `✻` | `*` |
| assistant and tool bullet | `●` | `*` |
| result connector (2sp, glyph, 2sp) | `  ⎿  ` | `  \-  ` |
| worker detail connector | `└ ` | `` |
| brain spinner | `·` `✢` `✳` `✶` `✻` `✽` | `.` `o` `O` `0` `@` `*` |
| worker spinner | `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` | `.oOo` |
| succeeded / failed / cancelled / orphaned | `✔` `✘` `⊘` `?` | `+` `x` `/` `?` |
| queued / leased | `·` `◦` | `.` `o` |
| mode marker | `⏵⏵` | `>>` |
| token arrow | `↓` | `v` |
| ellipsis | `…` | `...` |
| rule | `─` | `-` |
| welcome box | `╭ ╮ ╰ ╯ │ ─` | `+ + + + \| -` |
| popup selection | `▌` | `\|` |
| prompt | `> ` | `> ` |

One `tokio::time::interval` at `ui.refresh_hz` drives both spinners; the brain spinner advances
every 2 ticks, worker rows every tick with a per-row offset `(tick + i) % frames` so a board
shimmers instead of pulsing in lockstep.

Working verbs rotate every 4 s, seeded from the run ULID so a recording is reproducible:
`Thinking, Pondering, Brewing, Orchestrating, Wading, Dredging, Marshalling, Surveying,
Herding, Deliberating`. While a dispatch batch is live the verb is forced to `Orchestrating`.

Every string that came from a model, a worker or the filesystem passes `fmt::sanitize` and
`fmt::truncate` before it reaches a `Span`. That rule is already load-bearing in `fmt.rs`
(`control_characters_never_reach_the_terminal`) and an inline viewport in raw mode is exactly as
forgeable as trace output.

---

## 3. Screen states

Mockups are 100 columns. The ruler is not printed.

```
0        1         2         3         4         5         6         7         8         9         10
1234567890123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890|
```

### 3.1 Welcome and idle

```
╭──────────────────────────────────────────────────────────────────────────────────────────────────╮
│ ✻ Welcome to Swamp                                                                               │
│                                                                                                  │
│   /help for commands, /status for the run tree                                                   │
│   cwd: /Users/quentin/projects/swamp/Swamp                                                       │
│   brain: anthropic/main · claude-opus-4-20250514 · tier high                                     │
│   workers: 4 accounts · 3 ready, 1 cooling · max 4 parallel · budget $10.00                      │
│   run: 4x4kj6                                                                                    │
╰──────────────────────────────────────────────────────────────────────────────────────────────────╯

────────────────────────────────────────────────────────────────────────────────────────────────────
> █ Try "dispatch two workers to split the pagination work"
────────────────────────────────────────────────────────────────────────────────────────────────────
  ? for shortcuts                       ⏵⏵ dispatch mid              run 4x4kj6 · idle · ~$0.00
```

Line 1 is `accent` with `Welcome to Swamp` bold; every following line is `meta`, with the account
id, model and run id in `name`. Box width is `min(width, 100)`, rounded corners, committed once at
startup so it scrolls away like any block. The `brain:` line reads `starting…` until
`BrainEvent::Ready` lands, and is never rewritten. Worker counts come from `disp.pool().snapshot()`
folded through `watch::health_word`; the budget cell appears only when `limits.run_budget_usd` is
set.

The placeholder after the cursor is `meta`, truncated with `fmt::truncate` to `width - 4`, and
disappears on the first keypress.

Status line: three zones on one `meta` row. Left `? for shortcuts`, centre the mode marker, right
the run context. Under 80 columns the centre zone is dropped; the right zone then drops the cost
first, then the worker count.

### 3.2 Brain streaming text

```
> split the pagination work across two workers and land the cheaper one first                       
                                                                                                    
● I'll read the current handler first, then dispatch two workers.

  The endpoint lives in `api/users.rs` and returns the full table today. Two independent pieces of
  work fall out of that:

  1. the handler change, cursor based, no schema change
  2. the index backfill, which must not run in the same worktree

  Reading the handler now█

✳ Pondering… (esc to interrupt · 12s · ↓ 1.2k tokens)
────────────────────────────────────────────────────────────────────────────────────────────────────
> █
────────────────────────────────────────────────────────────────────────────────────────────────────
  ? for shortcuts                       ⏵⏵ dispatch mid              run 4x4kj6 · 1 turn · ~$0.06
```

The echoed prompt is one full-width row with the `userbar` background, `> ` in `meta`, the text in
default fg. It is committed the instant Enter is pressed, before the brain is sent anything.

`●` is `accent`; the first rendered line sits on the bullet row, continuations indent 2 and wrap at
`width - 2`. Everything above `Reading the handler now█` is already in scrollback; that last row is
the live tail and `█` is the streaming caret in `meta`.

Working line: frame and verb in `accent`, parenthetical in `meta`. `12s` is wall time since the
turn started. `↓ 1.2k tokens` is `fmt::tokens` of output tokens for the turn, estimated from delta
byte length / 4 until the real `TurnDone` usage lands, then corrected.

### 3.3 Tool call running, then done

Running:

```
● Read(src/api/users.rs)
  ⎿  ✳ running…
```

Bullet `run` (dim, animated), name `name` (bold), parenthesised argument `meta`.

Done, collapsed at `ui.collapse_lines` (default 3):

```
● Bash(cargo test --test parse_codex)
  ⎿  running 3 tests
     test parse_codex::final_event ... ok
     test parse_codex::rate_limit ... ok
     … +14 lines (ctrl+o to expand)
```

Bullet `ok`. `⎿` and body `meta`; the `… +N lines (ctrl+o to expand)` row is `meta` + `DIM`.
Continuation lines align under the first body character, column 5. Failed:

```
● Bash(cargo build --release)
  ⎿  error[E0308]: mismatched types
       --> src/worker/codex.rs:212:17
     error: could not compile `swamp` (lib) due to 2 previous errors
     … +23 lines (ctrl+o to expand)
```

Bullet `err`, first body line `err`, the rest `meta`.

Argument preview, `blocks::tool_args::preview(name, raw)`: `raw` is `ToolCall.preview`. If it
parses as JSON, per-tool rules apply - `swamp_dispatch` -> `"{n} tasks"`, `swamp_await` ->
`"{n} nodes"`, `swamp_result` / `swamp_worker_diff` -> the short node id, `swamp_status` -> `""`,
`swamp_note` -> first 40 chars. Otherwise whitespace is collapsed and the string is
`fmt::truncate`d to `width - name.len() - 6`. The `mcp__swamp__` prefix is stripped from the name.

### 3.4 swamp_dispatch, two workers live

```
● swamp_dispatch(2 tasks)
  ⎿  ⠹ 2 running · 0 done · ~$0.19 · 1m04s
     ⠹ f3a91c  [low ]  add pagination to /users                  main/sonnet-4     1m04s   ~$0.08
     ⠙ 7c02de  [mid ]  backfill the users index                  alt/sonnet-4        48s   ~$0.11

✻ Orchestrating… (esc to interrupt · 1m12s · 2 workers running)
────────────────────────────────────────────────────────────────────────────────────────────────────
> while those run, check whether the index already exists█
────────────────────────────────────────────────────────────────────────────────────────────────────
  ? for shortcuts              ⏵⏵ dispatch mid · 2/4 workers      run 4x4kj6 · 2 running · ~$0.19
```

The headline row is the batch summary and is what survives collapse. Row layout from the body
column (5):

| Field | Width | Source | Style |
|---|---|---|---|
| state glyph | 1 | worker spinner while `Running`, `·`/`◦` when `Queued`/`Leased`, else `fmt::glyph(&state)` | `run` / `ok` / `err` / `meta` |
| gap | 1 | | |
| attempt short id | 6 | `rec.id.short()` | `meta` |
| gap | 2 | | |
| tier | 6 | `format!("[{}]", fmt::pad(tier, 4))` | `meta`, `tier_hi` for `high` |
| gap | 2 | | |
| title | flex | `fmt::truncate(&row.title, flex)` | `name` |
| gap | 2 | | |
| account/model | 18 | `trace::account_cell` shortened to `main/sonnet-4` | `meta` |
| gap | 2 | | |
| elapsed | 6, right | `fmt::duration` | `meta` |
| gap | 2 | | |
| cost | 7, right | `fmt::cost` | `meta` |

`flex = width.saturating_sub(60).clamp(12, 40)`. Below 78 columns the account/model cell drops,
below 62 the tier cell drops, below 50 the cost drops. Glyph, id and title never drop. The
account/model cell drops its provider prefix before it drops the account id.

The board is a live block: it lives in the viewport and redraws every tick. With more than 8
workers it shows the 8 least-advanced rows plus `     … +N more (ctrl+o to expand)`; the headline
always shows the true totals.

Input stays usable while workers run. Submitting during a turn queues the text
(`pending_send: Option<String>`) and sends it on `TurnDone`; the status line shows `1 queued`.

### 3.5 Workers finished, board committed

```
● swamp_dispatch(2 tasks)
  ⎿  ✔ 1 done · ✘ 1 failed · ~$0.23 · 2m10s
     ✔ f3a91c  [low ]  add pagination to /users                  main/sonnet-4     2m10s   ~$0.14
       └ branch swamp/4x4kj6/f3a91c-1   +21 -1   2 files
     ✘ 7c02de  [mid ]  backfill the users index                  alt/sonnet-4      1m52s   ~$0.09
       └ PermissionDenied: Bash x2
     2 nodes · 1 failed · 2m14s · ~$0.23   (swamp adopt f3a91c)

● The handler change landed on `swamp/4x4kj6/f3a91c-1`. The backfill worker was denied Bash twice,
  so the migration never ran. I'll re-dispatch it with Bash allowed.

      swamp diff f3a91c --stat
      swamp adopt f3a91c
```

- Detail lines indent 7, connector `└ ` in `meta`.
- The branch line reuses `trace::block`'s exact wording (`branch {b}   +{i} -{d}   {n} files`) and
  is omitted when `work` is empty; a zero-diff worker prints no branch line, matching `trace`.
- The failure line uses `trace::failure_summary` verbatim, in `err`, so chat and `swamp trace`
  never disagree.
- The totals row is `meta`; the trailing `(swamp adopt <id>)` appears only when at least one node
  has non-empty `work` and is `accent`, because it is the one thing in the block meant to be copied.
- The brain's closing text follows as an ordinary assistant block. Nothing about it is special.

If the tool returns before the nodes finish (`wait: false`, or `max_wait_s` elapsed), the board
stays live and subsequent assistant text commits above it. That is correct: it is the state the
brain is actually in.

### 3.6 Errors

```
● swamp_dispatch(3 tasks)
  ⎿  ✘ 0 done · 1 failed · ~$0.00 · 0s
     ✘ a91002  [mid ]  rewrite the seed script                   -                    0s        -
       └ NoCapacity: every anthropic account is cooling; next reset 14:20

✘ brain failed: rate_limited (five_hour, telemetry) resets 14:20
  ⎿  account `main` is cooling until 14:20 · /accounts for the pool · swamp chat --resume 4x4kj6
```

`✘` and the headline are `err`, the `⎿` advice line is `meta`. The headline uses
`trace::failure_summary` when the message parses as a `Failure`, otherwise
`fmt::truncate(message, width - 16)`. `BrainEvent::Fatal` commits this block and returns to the
prompt; only a terminal failure (`Failure::is_terminal`) exits, with the existing code 1.

Interrupt, committed as a `meta` notice:

```
⊘ interrupted · 2 workers still running (esc again within 2s to cancel them)
```

Second esc:

```
⊘ cancelled 2 nodes
```

A refused input (a slash command that cannot run) is not committed: it replaces the status line
for 3 s, `  /adopt needs a node id · try /status`.

### 3.7 Slash popup

```
  ⎿  ✔ f3a91c  [low ]  add pagination to /users                  main/sonnet-4     2m10s   ~$0.14

  /tier       show or set the default dispatch tier
▌ /trace      the run tree, same output as swamp trace
  /thinking   show or hide the brain's thinking
  ────────────────────────────────────────────────────────────────────────────────────────────────
> /t█
────────────────────────────────────────────────────────────────────────────────────────────────────
  tab completes · ↑↓ chooses · esc closes                            run 4x4kj6 · idle · ~$0.23
```

Sits directly above the top rule, no border, max 8 rows, pushing `live_height` up. Two-space
indent, name padded to 12 in `name`, description in `meta`; the matched prefix is `accent`. The
selected row is marked `▌` in `accent` and reversed across the width. Filter: case-insensitive
prefix match on the name, then substring match on name and description, stable-sorted prefix
first. Overflow becomes `  … +N more` in `meta`. The status line is replaced by popup hints while
it is open.

### 3.8 Shortcut overlay

`?` on an empty input replaces the popup area with:

```
  enter          send                           ctrl+o    expand the last result
  alt+enter      newline (also shift+enter)      ctrl+l    clear the screen
  esc            interrupt the turn              ctrl+c    clear input, twice to quit
  esc esc        cancel running workers          ctrl+d    quit
  ↑ ↓            history (empty input)           /         commands
```

Any other key dismisses it. It is a live overlay, never committed.

---

## 4. Keyboard

`app.rs`, `fn on_key(&mut self, k: KeyEvent) -> Vec<Effect>`. Only `KeyEventKind::Press` is
handled (Windows consoles repeat otherwise).

| Key | Condition | Behaviour |
|---|---|---|
| `enter` | popup open | complete the selected command into the input, close the popup, do not submit |
| `enter` | buffer ends in `\` | replace the `\` with a newline |
| `enter` | buffer non-empty | commit the `> ` bar, then run the slash command or `Effect::Send` |
| `enter` | turn running | queue the text, status shows `1 queued`, sent on `TurnDone` |
| `enter` | buffer empty | nothing |
| `shift+enter`, `alt+enter` | | insert a newline |
| `esc` | popup or overlay open | close it, highest priority |
| `esc` | turn running | `Effect::Interrupt`, commit the interrupt notice, arm 2 s |
| `esc` | armed | `Effect::CancelAll`, commit `⊘ cancelled N nodes` |
| `esc` | idle, buffer non-empty | clear the buffer |
| `esc` | otherwise | nothing; esc never quits |
| `ctrl+c` | buffer non-empty | clear the buffer, reset the quit arm |
| `ctrl+c` | buffer empty, not armed | status shows `Press ctrl+c again to exit`, armed 2 s |
| `ctrl+c` | armed | interrupt, `cancel_all`, quit (0 when idle, 6 mid-turn) |
| `ctrl+d` | buffer empty | quit 0 |
| `ctrl+l` | | `Clear(All)` + `MoveTo(0,0)`, redraw the live area; scrollback above is untouched |
| `ctrl+o` | | toggle expansion of the last collapsible block (tool result or board). If it is still live, toggle in place; if it is committed, commit a new `meta` block `  ⎿  expanded: <tool>` with the full text, because scrollback is immutable |
| `up` / `down` | buffer empty or cursor on the first/last line | history previous/next; the in-progress buffer is stashed at index `len` |
| `up` / `down` | popup open | move the selection |
| `up` / `down` | otherwise | move the cursor between lines |
| `tab` | popup open | complete the common prefix, then accept the selection |
| `tab` | buffer starts with `/` | open the popup |
| `/` | buffer empty | insert `/` and open the popup |
| `?` | buffer empty | shortcut overlay |
| `left`/`right`/`home`/`end`, `alt+←/→` | | grapheme-aware cursor motion, word motion |
| `ctrl+a`/`ctrl+e`/`ctrl+k`/`ctrl+u`/`ctrl+w`, `alt+backspace` | | readline editing |
| printable | | insert at the cursor; if the buffer empties and started with `/`, close the popup |

`shift+enter` only reaches crossterm under the Kitty keyboard protocol, hence the enhancement
flags in §1.1; `alt+enter` and trailing backslash always work and `/help` says so.

History lives in `.swamp/chat_history`, one entry per line with newlines escaped as `\n`, capped
at `ui.chat_history` (500), loaded at startup, appended on submit. This replaces rustyline's file
history.

---

## 5. Slash commands

`slash.rs`. One const table feeds the popup, `/help` and completion, so they cannot drift.

```rust
pub struct Cmd { pub name: &'static str, pub args: &'static str, pub help: &'static str }
pub const COMMANDS: &[Cmd] = &[ /* … */ ];
```

| Command | Args | Prints |
|---|---|---|
| `/help` | | the table below plus the shortcut block from §3.8 |
| `/status` | | the run tree, `trace::render(&view, &TraceOpts::default())`, committed verbatim in `meta`. Identical bytes to `swamp trace`. |
| `/accounts` | | one row per account from `pool().snapshot()`: `provider/id`, `watch::health_word`, `watch::gauge_bar(util)` + `NN%`, inflight, lifetime nodes, `~$spend`, cooldown `until 14:20`. Coloured by `watch::health_color`. |
| `/trace` | `[node]` | `TraceOpts { node, events: true, ..default }`; no arg means the whole run. Collapsed at 3 lines with `ctrl+o`. |
| `/cost` | | in / out / cache-read / cache-write tokens and `~$` from `view.totals()`, a per-account and per-tier breakdown, plus `(N nodes reported no cost data)` when `!cost_complete` |
| `/tier` | `[low\|mid\|high]` | no arg: the current default dispatch tier and the tier-to-model map from `cfg.model_for`. With an arg: sets it for subsequent dispatches, echoes `dispatch tier: mid -> low`, updates the status marker. |
| `/workers` | `[n]` | show or set `limits.max_parallel` for this session |
| `/cancel` | `<node\|all>` | `disp.cancel(node)` / `cancel_all()`, echoes `⊘ cancelled N nodes` |
| `/diff` | `<node>` | `--stat` for the node's captured patch, collapsed at 10 lines |
| `/thinking` | `[on\|off]` | toggles `ui.show_thinking`; when on, thinking renders `meta` + `ITALIC` under a `✻ thinking` header and commits like assistant text |
| `/clear` | | clears the screen and drops live blocks; echoes `screen cleared; the brain still remembers the conversation` |
| `/resume` | `<run\|last>` | prints the exact `swamp chat --resume <run>` line, quits when confirmed with a second `/resume` |
| `/quit` | | quit 0; aliases `/exit`, `/q` |

Every handler returns a `Block`, so command output commits to scrollback exactly like model output.
Unknown command: an `err` notice, `unknown command /foo; did you mean /force?` using Levenshtein
distance <= 2 over the table, else `try /help`.

---

## 6. Markdown and result rendering

`markdown.rs` is a line-oriented incremental renderer, hand-rolled in ~250 lines. No
pulldown-cmark: the grammar is small and must tolerate a half-arrived line.

```rust
pub struct MdStream { pending: String, fence: Option<String>, list: Vec<ListKind>, width: u16 }
impl MdStream {
    /// Feed a delta. Returns the lines that are now final and ready to commit.
    pub fn push(&mut self, delta: &str) -> Vec<Line<'static>>;
    /// The incomplete trailing line, re-parsed and re-rendered every frame. Never final.
    pub fn tail(&self) -> Vec<Line<'static>>;
    pub fn finish(&mut self) -> Vec<Line<'static>>;
}
```

`push` appends to `pending`, splits off everything up to the last `\n`, and converts each complete
source line. A line is final as soon as its newline arrives, because block context is decided by
the line's opening, never its closing.

| Source | Rendered |
|---|---|
| ` ```lang ` | emits a `meta` `lang` hint row, opens the fence |
| line inside a fence | 2 extra spaces of indent, whole line in `code`, no inline parsing |
| ` ``` ` | closes the fence |
| `# ` .. `###### ` | text in `name`, markers stripped, one blank line before unless the previous line was blank |
| `- ` / `* ` / `+ ` | `  • ` in `accent`, inline spans, hanging indent 4 |
| `1. ` | `  1. ` in `meta`, inline spans, hanging indent 5 |
| `> ` | `  │ ` in `meta`, body `meta` + `ITALIC` |
| `---` / `***` | a `meta` rule of `width - 4` |
| blank | one blank line; runs of 2 or more collapse to one |
| anything else | paragraph: inline spans, indent 2, wrapped at `width - 2` |

Nested lists indent 2 per level, detected from leading whitespace in steps of 2.

Inline pass, single left-to-right scan emitting `Vec<Span>`, never backtracking:

- `` `code` `` -> `code` style, backticks dropped, one space of padding
- `**bold**`, `__bold__` -> `BOLD`
- `*em*`, `_em_` -> `ITALIC`
- `~~strike~~` -> `CROSSED_OUT`
- `[text](url)` -> `text` `UNDERLINED`, then ` (url)` in `meta`, dropping the url when the line
  would exceed the width or the url already appears in the text
- an unmatched marker is emitted literally; `tail()` re-parses the whole pending line each frame,
  so a marker renders literally while it is half-arrived and correctly once it closes

`wrap_spans(spans, width, indent) -> Vec<Line>` is greedy and span-aware: break on the last space
that fits, hard-split a token longer than the line, widths from `unicode_width` so CJK and emoji do
not overflow. `textwrap` is used for plain paragraph text where spans are uniform.

**Tool results and worker titles are truncated, never wrapped.** A wrapped `cargo test` line is
unreadable; a truncated one is not. Results collapse to `ui.collapse_lines` (3) followed by
`… +N lines (ctrl+o to expand)` in `meta` + `DIM`; a failed result shows its first 3 lines in
`err`.

---

## 7. Files under `src/ui/chat/`

`src/ui/chat.rs` is deleted; `src/ui/mod.rs` already resolves `pub mod chat;` to the directory.

| File | ~LOC | Contents |
|---|---|---|
| `mod.rs` | 220 | `repl`: tty probe and the non-tty `drain_turn` fallback, terminal setup and guard, the `select!` loop, `commit`, `set_live_height` |
| `app.rs` | 340 | `App`, `Msg`, `Effect`, `Phase { Idle, Working { since }, Interrupting }`, `reduce`, `on_key`, `on_brain`, `on_journal`, `on_tick`, `live_height`, `cursor_xy`. Pure, no I/O |
| `theme.rs` | 120 | `Theme`, `Role`, `detect`, glyph table, ascii fallback, `state_style(&NodeState)` |
| `live.rs` | 90 | inline terminal helpers: enter, restore, commit chunking, clear |
| `input.rs` | 220 | `Editor`: multi-line buffer, grapheme cursor, readline bindings, `History` load/save |
| `markdown.rs` | 250 | `MdStream`, block grammar, inline pass, `wrap_spans` |
| `blocks.rs` | 280 | `Block`, `Render` impls for welcome, user bar, assistant, tool, notice, slash output; collapse and `ctrl+o`; `tool_args::preview` |
| `workers.rs` | 200 | `Board`, `Batch`, `WorkerRow`, `from_view(&RunView, brain: NodeId)`, admission, flex widths, drop order, committed form with the `└` detail lines |
| `slash.rs` | 230 | `COMMANDS`, filter and complete, handlers, the popup widget |
| `spinner.rs` | 70 | frames, verb rotation, elapsed and token counters |

Core types:

```rust
pub enum Block {
    Welcome(WelcomeInfo),
    User(String),
    Assistant { md: MdStream, done: bool },
    Thinking { md: MdStream },
    Tool { id: String, name: String, preview: String, state: ToolState,
           result: Vec<String>, expanded: bool },
    Dispatch(Batch),
    Slash { title: String, body: Vec<String> },
    Notice { glyph: char, role: Role, head: String, body: Vec<String> },
}

pub trait Render { fn render(&self, width: u16, t: &Theme) -> Vec<Line<'static>>; }
```

Reused untouched: `fmt::{truncate, pad, tokens, cost, duration, glyph, state_word, sanitize,
clock_hm, short_sha}`, `watch::{TerminalGuard, install_panic_hook, gauge_bar, health_word,
health_color}`, `trace::{render, TraceOpts, event_text}`, `journal::reader::Tailer`,
`journal::fold::{RunView, TreeRow, Totals}`. Nothing in `dispatch/`, `mcp/` or `journal/` changes.

---

## 8. Acceptance tests

`ratatui::backend::TestBackend` with `Viewport::Inline` gives a deterministic buffer; `watch.rs`'s
tests already show the pattern (`terminal.backend().buffer().content()` collected to a `String`).
All offline, no tty.

1. `tests/chat_render.rs` - `insta` snapshots of `render::live` and of each committed block for
   every state in §3, at 100 and at 62 columns, built from a hand-made `Vec<JournalLine>` (the
   `journal()` helper in `watch.rs`'s tests) plus a scripted `Vec<BrainEvent>`. `Theme::Plain` for
   stable bytes, plus one `TrueColor` snapshot asserting the RGB of `●` and of `✔`.
2. `app.rs` reducer tests - one `ctrl+c` clears and two quit; `esc` while working emits
   `Interrupt` and `esc esc` emits `CancelAll`; submit while working sets `pending_send` and
   `TurnDone` flushes it; `tab` completes `/t` to the common prefix of `/tier`, `/trace`,
   `/thinking`; history up and down round trip; alt+enter makes a second line.
3. `markdown.rs` - proptest that feeding a document one byte at a time and all at once produces
   identical committed lines; a fence split across three deltas mid-line; an unterminated fence at
   `finish`; an unmatched `**bold`; a CJK line wrapping exactly at the width.
4. `workers.rs` - admission when two dispatches interleave; a retry changing the row's short id in
   place; the board commits only after the last owned node is terminal; the 8-row collapse keeps
   the headline totals honest; column drop order at 78, 62 and 50 columns.
5. `TestBackend::new(100, 20)` round trip for the welcome box and the live board, asserting the
   short run id, a tier cell and a cost cell all appear (mirrors
   `the_tree_pane_and_the_account_gauges_render`).
6. A control-character test on the assistant stream, on tool results and on worker titles: a title
   carrying `\u{1b}[2J` must not repaint the viewport.

---

## 9. Implementation order

1. P1, P2, P3.
2. `theme.rs`, `live.rs`, `mod.rs` with an inline viewport drawing only the input box and the
   status line, plus `commit`. This alone replaces rustyline and is already usable.
3. `input.rs` and the keyboard table.
4. `markdown.rs` and `blocks.rs` driven by `BrainEvent`: assistant text, tool calls, results.
5. `workers.rs` plus the `Tailer` branch: the board lights up with no brain changes.
6. `slash.rs`, wiring `/status`, `/trace`, `/accounts`, `/cost` to the existing renderers.
7. `spinner.rs`, the polish pass, snapshots.

Each step leaves the binary compiling and `swamp chat` working.
