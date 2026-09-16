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

`refresh_hz` already exists and drives the chat redraw tick (default 20).

Everything else is additive. `src/ui/chat.rs` becomes the directory `src/ui/chat/`;
`ui::chat::repl(brain, disp, ctx) -> Result<i32>` keeps its signature so `src/cmd/chat.rs` is
untouched. Three helpers in `trace.rs` become `pub(crate)`: `failure_summary`, `failure_detail`,
`account_cell`.

---

## 1. Architecture

### 1.1 An inline viewport, never the alternate screen

Finished blocks scroll into real scrollback so the user can select and copy them with the mouse.
Only the live tail is redrawn. `src/ui/chat/live.rs` owns it, on top of the backend in
`src/ui/chat/live/relative.rs`:

```rust
// live::enter, called from chat::interactive
enable_raw_mode()?;                                  // no EnterAlternateScreen
execute!(stdout(), PushKeyboardEnhancementFlags(
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
  | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS))?;
let mut back = RelativeBackend::new(stdout());
back.open(height)?;            // column 0, Clear(FromCursorDown), then grow by newlines
let mut term = Terminal::with_options(
    back,
    TerminalOptions { viewport: Viewport::Fixed(Rect::new(0, 0, width, height)) },
)?;
```

**No absolute screen row is named anywhere in the chat, and the cursor is never read back.**
The viewport rect sits at `y = 0`, so every row ratatui hands the backend is already an offset
inside the live area. `RelativeBackend` wraps `CrosstermBackend` and keeps two numbers: the row
the hardware cursor is on *inside the area*, and its column. `set_cursor_position` becomes
`MoveToColumn(x)` and then `MoveUp`/`MoveDown` by the difference from the tracked row - the column
move first, because it also settles a pending wrap a bare vertical move would carry into the wrong
row. `get_cursor_position` answers from the tracked pair. Nothing emits a DSR (`cursor::position`),
at startup or after a resize.

Two reasons, both measured:

- a DSR reply arrives on the stdin the key `EventStream` is draining, so it is answered late or
  not at all, and the read times out after two seconds;
- a host answers a *height grow* while it is still pulling rows back out of its history, so the
  row it reports is a row that is about to move. Probing there lost a whole committed turn in
  roughly six of ten tmux resize bursts.

The offset inside the area survives all of that: a resize moves the rows on screen, but it moves
the cursor with the row the cursor is on. The absolute row is what a resize destroys.

`Viewport::Fixed`, not `Viewport::Inline`: `Terminal::resize` on an inline viewport calls
`compute_inline_size`, which asks the backend where the cursor is. `live.rs` places and sizes the
area itself.

**Where the area opens.** On the row the shell's cursor is already on, directly under the host's
own output: `MoveToColumn(0)`, one `Clear(FromCursorDown)`, and then `height - 1` newlines printed
on the area's last row. Nothing above that row is ever written to, so a `swamp chat` started
halfway down a screen leaves no blank band above itself and no scrollback of the shell is lost.

The guard is not `live.rs`'s: `chat::interactive` builds it right after `live::enter` returns
(`let _guard = TerminalGuard::with(live::restore_inline)`, raw off, pop flags, show cursor), so
`live.rs` only ever exposes the restore fn.

`MIN_LIVE = 4`: rule, input, rule, status. `install_panic_hook` and `TerminalGuard` live in
`watch.rs` and are parameterised by the restore fn they take. `restore_inline` does **not** call
`LeaveAlternateScreen`. `swamp watch` keeps the full-screen view; chat is inline.

**Non-tty.** If `!std::io::stdout().is_terminal()`, `repl` runs the plain printer (`drain_turn`,
in `mod.rs`). CI, pipes and scripted runs are unaffected. `cmd::chat` asks the same question
through `ui::chat::interactive_stdout()` before printing the one-shot `run <id>` header: the
welcome box already carries `run: <short>`, so on a tty the header would be a bare line above the
viewport. `swamp run` and the plain transcript still print it.

### 1.2 Committing blocks to scrollback

`Inline::commit` writes each line on **row 0** of the live area and then gets that row out of it:
the area slides down one row, and only a newline printed on the area's **last** row can move the
host. That newline is the only thing that scrolls the host, and the only way a committed row
reaches real scrollback:

```rust
for line in lines {
    self.write_top(line, width)?;         // row 0 of the area: erase, then write trimmed
    self.term.backend_mut().slide()?;     // one "\n" on the area's LAST row
}
self.term.resize(self.rect())?;           // repaint the live area next frame
```

`slide` is the whole trick. When the area's last row is the last row of the screen, printing a
newline there scrolls the host and the top committed row goes into real scrollback. When it is
not - the screen still has free rows under the area, left by a shrink or by a resize - the cursor
simply steps down into one and the area has moved down a row without the host scrolling at all.
Both cases leave the cursor on the area's last row, which is all the backend has to know. Free
rows below the area are therefore consumed by later commits, in order, with no scroll and no
flicker, and no blank row is ever pushed into scrollback between two committed blocks.

Pre-wrapping is mandatory: a row is one screen line, so any wrapping ratatui did itself would
clip. All wrapping happens in `markdown.rs` (`wrap_spans`, span-aware, widths from
`unicode_width`). Scrollback is frozen text; wrap once, at commit time, at the current width.

Every byte - the ratatui diff, the relative cursor moves, the newlines - goes through the
backend's own writer, so the ordering is the writer's and a test can point that writer at a
terminal emulator (§1.3).

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
| Error / notice | immediately, through `commit` like everything else |

The live area height is therefore bounded by the worker board, never by the conversation.

### 1.3 Resizing the live area

Growing, shrinking and a window resize are three different things, and none of them may destroy a
committed row. All three are expressed as moves relative to the tracked cursor (§1.1); none of
them names a screen row.

**Growing** by `k` rows is `RelativeBackend::grow`: park on the area's last row, print `k`
newlines there, and count the area `k` rows taller. If the screen still has free rows under the
area the cursor just steps into them and nothing scrolls; if it does not, the host scrolls and
committed rows move up into its real scrollback. Nothing above the area is written to, either way.

**Shrinking is immediate.** `set_height` calls `RelativeBackend::shrink`, which parks on the first
row it is giving back, erases with one `Clear(FromCursorDown)`, and parks again on the area's new
last row. The rows simply stop being the area's. There is no `free` counter and no bookkeeping:
`commit` walks the area back down over those rows one `slide` at a time (§1.2), so a block leaving
the live area lands on rows the area itself had a moment ago. The live content therefore stays
tight under the last committed block instead of floating over a hole: opening a slash overlay and
closing it again leaves the input bar exactly where it was, not twenty rows below it.

`chat::interactive` sizes the area from the **post-commit** state, immediately before each
`Effect::Commit`, as well as once per frame before `draw`.

```rust
term.set_height(render::live_height(&app, rows))?;   // per frame, before draw
...
Effect::Commit(lines) => {
    term.set_height(render::live_height(&app, rows))?;  // `reduce` already dropped the block
    term.commit(lines)?;
}
```

`want = live_lines.len()` clamped to `[MIN_LIVE, max(MIN_LIVE, rows * 3 / 5)]`. When the clamp
bites, the worker board is the part that collapses (§4.5).

**`Event::Resize` moves the area, and only the cursor still points at it.** A real host shifts
what is on screen: growing pulls rows back out of scrollback and everything moves *down*,
shrinking pushes the top rows into scrollback and everything moves up, and a narrower window
splits every row too wide to hold. What the host does not do is move the cursor off the row it is
on. Two earlier attempts did absolute-row arithmetic across a resize and both destroyed committed
rows; a third read the row back with `cursor::position()` and lost committed turns to the race in
§1.1. `Inline::reflow` does neither.

Every write leaves the hardware cursor on a known row of the live area: `park` puts it on the
area's first row, and `draw` - the only one that has to put it somewhere the user can see - leaves
it on the caret and remembers the caret's row and column, plus the display width of every row it
drew. Those widths are read back from the frame's own buffer - the last cell in the row with
something in it, the measure `write_top` already takes of a committed row - and replace the ones
before them: they are the frame's, never a running maximum over earlier frames. `Terminal::resize`
blanks the whole area, so what the last draw put there is exactly what the screen holds, and a
maximum was wrong the moment the area grew: a slash popup drawn over the rows two full-width rules
had just been on still measured a full width, and `rows_above` counted three screen rows for three
rows that were one.

`Inline::reflow(width, rows)`, in order:

1. **measure.** `rows_above(new_width)`: the caret's tracked row offset when the window got wider
   or kept its width (nothing splits - the rows were drawn no wider than the area they were in),
   and otherwise what those rows became once the host split them,
   `sum(ceil(width_i / new_width))` over the rows above the caret, plus `caret_col / new_width`.
2. **erase the area where it actually is.** `MoveToColumn(0)`, `MoveUp(rows_above)`, one
   `Clear(FromCursorDown)`. Nothing above that point is ever written to.
3. **reopen at that row.** The area is re-established at the cursor with the new size, growing by
   newlines printed on its last row if the new height needs more rows than the screen has left
   under it. It sits directly under the committed tail: no re-anchor, no gap, never a blank row
   above the input bar, and free rows below it are taken by later commits without scrolling.
4. **rebuild.** The ratatui viewport is resized to the new rect, which resets both buffers, so the
   next frame repaints every live row at the new width: no stale rule or prompt can survive.

The `EventStream` is left alone throughout - it is never dropped, recreated, or waited on, because
nothing needs stdin quiet any more.

tmux fires a burst of resize events; only the last size is worth repairing, so the burst is
drained until 50ms of quiet first, and anything else read in that window is replayed afterwards.

Committed rows are written trimmed, not padded to the full width (`write_top` erases the row and
writes up to its last cell with something in it): a padded row is one the host splits in two when
the window narrows, and the half with nothing on it is a blank row in the middle of scrollback.

Every live block is re-wrapped from `Msg::Resize` in the same turn.

**Tests.** `Inline` is generic over its writer, so `live/tests.rs` hands it a `vt100` emulator
with scrollback and asserts on what the user would see. `Host::resize` models a real host rather
than the emulator: it rebuilds the screen bottom-anchored, so a grow pulls rows back out of
scrollback and a shrink pushes them into it, it splits the rows a narrower window cannot hold, and
it moves the cursor with the row it is on - which is all `reflow` is given, since there is no
probe to hand it any more. The suite covers: committed rows survive every grow in order; a notice
committed under a tall board is still there one frame later; no run of more than one blank row
appears between committed blocks; a shrink with no commit behind it leaves at most one blank row
between the last committed block and the live area; shrinking, growing or re-widening the window
keeps the committed rows the emulator still holds and leaves exactly one live area on screen; a
full-width idle layout followed by a taller popup one whose rows are half as wide, reflowed
narrower, keeps every committed row; and a width change leaves the area under the committed tail.
Three cover the relative backend directly: two width changes and then a commit leave no hole, on
screen or in scrollback; a burst of five alternating height changes with no commit between them
loses nothing and leaves no second copy of the live area; and a startup with the cursor mid-screen
puts the welcome block on the row right under the shell's last line, with no blank band.

The `render::live` and `Block::render` snapshots (§3) cover the drawing; these cover the
scrolling.

### 1.4 The loop

Event sources: crossterm keys, the `BrainEvent` stream, journal-fold polls, and the refresh tick.

```rust
let period = Duration::from_millis(1000 / u64::from(cfg.ui.refresh_hz.unwrap_or(20)).max(1));
let mut ticker = tokio::time::interval(period);
loop {
    term.set_height(render::live_height(&app, rows))?;
    let frame = render::compose(&app);
    term.draw(frame.lines, frame.cursor)?;
    let msg = match queued.pop_front() {       // read past a resize burst (§1.3)
        Some(ev) => Msg::Key(ev),
        None => tokio::select! {
            biased;
            Some(ev) = keys.next()           => Msg::Key(ev?),
            Some(be) = brain.events().recv() => Msg::Brain(be),
            lines    = tailer.poll()         => Msg::Journal(lines?),
            _        = ticker.tick()         => Msg::Tick,
            ()       = cmd::shutdown_signal()=> Msg::Signal,
        },
    };
    if let Msg::Resize(..) = msg {             // debounce the burst, then repair
        term.reflow(width, rows)?;             // relative: the key stream stays alive
    }
    let mut effects: VecDeque<Effect> = app.reduce(msg).into();
    while let Some(effect) = effects.pop_front() {
        match effect {
            Effect::Commit(lines) => { term.set_height(render::live_height(&app, rows))?;
                                       term.commit(lines)?; }
            Effect::Send(text)    => brain.send(&text).await?,
            Effect::Interrupt     => brain.interrupt().await?,
            Effect::CancelAll     => { let n = disp.cancel_all();
                                       effects.extend(app.note_cancelled(n)); }
            Effect::Cancel(id)    => disp.cancel(id).await?,
            Effect::Trace(node)   => { let text = trace::render(&RunView::load(&dir, true)?, ..);
                                       effects.extend(app.trace_output(&text)); }
            Effect::Clear         => term.clear_screen()?,
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

Effects are a queue, not a list: `CancelAll` pushes the `⊘ cancelled n nodes` notice back onto
it, so that notice is committed through `Inline::commit` like any other block and can never be
written straight onto a live row.

`App` is pure: `reduce(Msg) -> Vec<Effect>`, no I/O, fully unit-testable. Rendering is
`render::compose(&App) -> Live`, and `Block::render(&Ctx)`, both snapshot-testable with
`TestBackend`.

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
│   cwd: /Users/me/projects/example/repo-one                                                       │
│   brain: anthropic/main · claude-opus-4-20250514 · tier high                                     │
│   workers: 4 accounts · 3 ready, 1 cooling · 12% of the tightest window used                     │
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
`BrainEvent::Ready` lands, and is never rewritten. The `workers:` line comes from
`disp.pool().snapshot()`: the ready/cooling counts fold through `watch::health_word`, and the
trailing percentage is the worst `RateLimitSnapshot::worst_utilization()` across every account,
including estimated windows. There is no parallelism cap and no budget to show: an account with no
`max_concurrency` is bounded by quota headroom alone (§6.3 in DESIGN.md's terms).

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
  ⎿  ⠹ 2 running · 0 done · ~$0.00 · 41m03s
     · a91002  [mid ]  rewrite the seed script                   -                    -        -

✘ brain failed: rate_limited (five_hour, telemetry) resets 14:20
  ⎿  account `main` is cooling until 14:20 · /accounts for the pool · swamp chat --resume 4x4kj6
```

A node that finds every account of its provider cooling, past its measured `quota_stop_at`, or
hard-gated does **not** fail: `AccountPool::acquire_node` journals one `NodeBlocked { until, why }`
for it and waits, so `NodeState::Blocked` renders exactly like `Queued` in the board above -
dim `·` glyph, account and elapsed both `-` - for as long as the wait lasts, and counts toward
`running` in the headline, never toward `failed`. The reason and the reset time are not spelled out
in the row: they live in the journal line and in a `WARN`-level log, `every anthropic account is at
its limit until 14:20: <why>`, printed once per blocked node rather than once per recheck. Two
`esc` within `ARM` cancel every running and blocked node the same way (`Failure::Cancelled { by:
User }`); `swamp run` responds to a single ctrl-c. `Failure::NoCapacity` is reserved for the two
cases that are not a wait: the node's own `--timeout` expiring first
(`dispatch::pool::NoCapacity::Saturated`), and no candidate account existing for the provider at
all (`NoCapacity::Exhausted`, a config problem, not a quota one).

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
for 3 s, `  /diff needs a node id · try /status`.

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
| `enter` | popup open, the text already names a command | submit it; a spelled-out `/status` never costs a second enter |
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
| `/usage` | `[--json]` | per-account tokens and quota windows, `ui::usage::render` shared byte-for-byte with `swamp usage`; `--json` commits the `ui::usage::json` shape as a code block instead |
| `/trace` | `[node]` | `TraceOpts { node, events: true, ..default }`; no arg means the whole run. Collapsed at 3 lines with `ctrl+o`. |
| `/cost` | | in / out / cache-read / cache-write tokens and `~$` from `view.totals()`, a per-account and per-tier breakdown, plus `(N nodes reported no cost data)` when `!cost_complete` |
| `/tier` | `[low\|mid\|high]` | no arg: the current default dispatch tier, one line, `dispatch tier: mid`. With an arg: sets it for subsequent dispatches, echoes `dispatch tier: mid -> low`, updates the status marker. |
| `/cancel` | `<node\|all>` | `disp.cancel(node)` / `cancel_all()`, echoes `⊘ cancelled N nodes` |
| `/diff` | `<node>` | `--stat` for the node's captured patch, collapsed at 10 lines |
| `/thinking` | `[on\|off]` | toggles `ui.show_thinking`; when on, thinking renders `meta` + `ITALIC` under a `✻ thinking` header and commits like assistant text |
| `/clear` | | clears the screen and drops live blocks; echoes `screen cleared; the brain still remembers the conversation` |
| `/resume` | `<run\|last>` | prints the exact `swamp chat --resume <run>` line, quits when confirmed with a second `/resume` |
| `/quit` | | quit 0; aliases `/exit`, `/q` |

`slash::runnable` decides what enter does while the popup is open: the typed text naming a
command, arguments and all, or leaving one candidate spelled exactly as typed, runs it; anything
else completes the selection. `tab` always completes.

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
| ` ```lang ` | emits a `meta` + `DIM` `lang` hint row, opens the fence; a bare ` ``` ` opens it with no row at all |
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

- `` `code` `` -> `code` style, backticks dropped, one space of padding only where the palette
  paints a background; `plain` (`NO_COLOR`, `--no-color`) has none, so the text keeps its own spacing
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
| `live.rs` | 270 | `Inline`: the live area over `RelativeBackend` - open, draw, commit, set_height, reflow, clear; enter and restore |
| `render.rs` | 70 | `render::live`, `render::compose`, `render::live_height`: composes the live area - worker board, spinner and status line, slash popup - into the `Live { lines, cursor }` `live.rs` draws |
| `live/relative.rs` | 285 | `RelativeBackend`: a `Backend` over `CrosstermBackend` that tracks the cursor's row inside the area and moves by `MoveUp`/`MoveDown`. No DSR |
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
