pub mod app;
pub mod blocks;
pub mod input;
pub mod live;
pub mod markdown;
pub mod render;
pub mod slash;
pub mod spinner;
pub mod theme;
pub mod workers;

#[cfg(test)]
mod screens;
#[cfg(test)]
pub mod tests_support;

use crate::brain::{Brain, BrainEvent};
use crate::cmd::Ctx;
use crate::dispatch::AccountPool;
use crate::dispatch::Dispatcher;
use crate::dispatch::account::{AccountState, Health};
use crate::journal::fold::RunView;
use crate::journal::reader::Tailer;
use crate::ui::chat::app::{App, Effect, MIN_LIVE, Msg};
use crate::ui::chat::blocks::WelcomeInfo;
use crate::ui::chat::input::History;
use crate::ui::chat::theme::{Role, Theme};
use crate::ui::fmt;
use crate::ui::trace::{TraceOpts, render};
use crossterm::event::{Event, EventStream};
use futures::StreamExt;
use ratatui::text::Line;
use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;

const DEFAULT_REFRESH_HZ: u16 = 20;
const DEFAULT_HISTORY: usize = 500;
/// How long a resize burst has to stay quiet before the live area is repaired.
const RESIZE_QUIET: Duration = Duration::from_millis(50);

/// Whether the chat gets the inline viewport. `swamp chat` asks before printing its run
/// header, so the header and the welcome box never both claim the run id.
pub fn interactive_stdout() -> bool {
    std::io::stdout().is_terminal()
}

/// docs/BOARD.md §5: shown once, before the first prompt, only when tmux is active and no
/// board owns `~/.swamp/board.pid`.
fn board_hint(in_tmux: bool, board_alive: bool) -> Option<&'static str> {
    (in_tmux && !board_alive)
        .then_some("board: `swamp board` in a split, or restart with `swamp chat --board`")
}

fn board_hint_for(ctx: &Ctx) -> Option<&'static str> {
    board_hint(
        std::env::var_os("TMUX").is_some(),
        crate::journal::paths::board_is_alive(&ctx.paths.board_pid()),
    )
}

/// The chat UI. A tty gets the inline viewport; a pipe gets the plain transcript, so CI and
/// scripted runs are unaffected.
pub async fn repl(brain: Box<dyn Brain>, disp: Arc<Dispatcher>, ctx: &Ctx) -> anyhow::Result<i32> {
    let mut brain = brain;
    brain.start().await?;
    if interactive_stdout() {
        interactive(brain, disp, ctx).await
    } else {
        plain(brain, disp, ctx).await
    }
}

/// No raw mode, no escape sequences, one block of text per turn.
async fn plain(mut brain: Box<dyn Brain>, disp: Arc<Dispatcher>, ctx: &Ctx) -> anyhow::Result<i32> {
    use tokio::io::AsyncBufReadExt;
    println!("swamp {} - /help for commands", crate::VERSION);
    if let Some(hint) = board_hint_for(ctx) {
        println!("  {hint}");
    }
    // No live view here: the blocked notice has to reach stderr or nothing explains the wait.
    disp.pool().notices_to_stderr();
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut code = 0;
    while let Some(line) = lines.next_line().await? {
        let text = line.trim().to_owned();
        if text.is_empty() {
            continue;
        }
        if let Some(command) = text.strip_prefix('/') {
            if plain_slash(command, &disp, ctx).await? {
                break;
            }
            continue;
        }
        brain.send(&text).await?;
        code = {
            let turn = drain_turn(&mut brain, ctx);
            tokio::pin!(turn);
            tokio::select! {
                code = &mut turn => code,
                () = crate::cmd::shutdown_signal() => {
                    // Workers run in their own process groups: a terminal signal never
                    // reaches them, so the shutdown path has to stop them itself.
                    eprintln!("\ninterrupted: cancelling {} nodes", disp.cancel_all());
                    6
                }
            }
        };
        if code == 6 {
            break;
        }
    }
    brain.shutdown().await?;
    Ok(code)
}

/// Both halves of the pool view move together: the pool adopts accounts other repos wrote
/// while the chat runs, and a `/usage` redrawn from live rows next to a startup-time stale
/// list would disagree with `swamp usage`.
pub fn refresh_pool(app: &mut App, pool: &AccountPool) {
    app.set_pool(pool.snapshot());
    app.set_stale_accounts(pool.unconfigured());
}

/// One slash table for both surfaces: `/help` here lists what the viewport lists, so it
/// cannot advertise a command the pipe then refuses. True means the session is over.
async fn plain_slash(command: &str, disp: &Arc<Dispatcher>, ctx: &Ctx) -> anyhow::Result<bool> {
    let paths = disp.journal.paths().clone();
    let mut app = App::new(
        paths.run,
        Theme::detect(ctx.color, None),
        WelcomeInfo::default(),
        History::load(None, 0),
        &disp.cfg,
    );
    app.disable_quota_probe();
    app.view = RunView::load(&paths.dir, true)?;
    app.pool = disp.pool().snapshot();
    app.set_stale_accounts(disp.pool().unconfigured());
    app.now = OffsetDateTime::now_utc();

    let mut quit = false;
    let mut effects: std::collections::VecDeque<Effect> = app.command(command).into();
    while let Some(effect) = effects.pop_front() {
        match effect {
            Effect::Commit(body) => {
                println!("{}", blocks::text_of(&body).join("\n"));
            }
            Effect::Trace(node) => {
                let text = render(
                    &app.view,
                    &TraceOpts {
                        node,
                        events: true,
                        ..TraceOpts::default()
                    },
                );
                effects.extend(app.trace_output(&text));
            }
            Effect::CancelAll => {
                let n = disp.cancel_all();
                effects.extend(app.note_cancelled(n));
            }
            Effect::Cancel(id) => disp.cancel(id).await?,
            Effect::Quit(_) => quit = true,
            _ => {}
        }
    }
    Ok(quit)
}

async fn interactive(
    mut brain: Box<dyn Brain>,
    disp: Arc<Dispatcher>,
    ctx: &Ctx,
) -> anyhow::Result<i32> {
    let cfg = disp.cfg.clone();
    let paths = disp.journal.paths().clone();
    let theme = Theme::detect(ctx.color, cfg.ui.chat_theme.as_deref());
    let history = History::load(
        Some(&ctx.paths.dot_swamp.join("chat_history")),
        cfg.ui.chat_history.unwrap_or(DEFAULT_HISTORY),
    );
    let mut app = App::new(paths.run, theme, welcome(ctx, &disp), history, &cfg);
    refresh_pool(&mut app, disp.pool());

    let mut tailer = Tailer::open(&paths.journal())?;
    let period = Duration::from_millis(
        1000 / u64::from(cfg.ui.refresh_hz.unwrap_or(DEFAULT_REFRESH_HZ).max(1)),
    );
    let mut ticker = tokio::time::interval(period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let (mut width, mut rows) = size();
    app.width = width;
    app.rows = rows;
    let mut term = live::enter(width, rows, MIN_LIVE)?;
    let _guard = crate::ui::watch::TerminalGuard::with(live::restore_inline);
    term.commit(app.take_welcome())?;
    if let Some(hint) = board_hint_for(ctx) {
        term.commit(vec![Line::from(
            theme.span(format!("  {hint}"), Role::Meta),
        )])?;
    }
    let mut keys = EventStream::new();
    // Events read past a resize while waiting for the burst to end, replayed once it is handled.
    let mut queued: std::collections::VecDeque<Event> = std::collections::VecDeque::new();

    let code = loop {
        app.now = OffsetDateTime::now_utc();
        app.width = width;
        app.rows = rows;
        refresh_pool(&mut app, disp.pool());
        term.set_height(render::live_height(&app, rows))?;
        let frame = render::compose(&app);
        term.draw(frame.lines, frame.cursor)?;

        let msg = if let Some(event) = queued.pop_front() {
            match event {
                Event::Key(k) => Some(Msg::Key(k)),
                _ => None,
            }
        } else {
            tokio::select! {
                biased;
                key = keys.next() => match key {
                    Some(Ok(Event::Key(k))) => Some(Msg::Key(k)),
                    Some(Ok(Event::Resize(w, h))) => {
                        width = w.max(20);
                        rows = h.max(MIN_LIVE);
                        Some(Msg::Resize(width, rows))
                    }
                    Some(Ok(_)) => None,
                    Some(Err(e)) => return Err(e.into()),
                    None => Some(Msg::Quit),
                },
                event = brain.events().recv() => match event {
                    Some(event) => Some(Msg::Brain(event)),
                    // The brain is gone: nothing more will ever arrive on this channel.
                    None => Some(Msg::Quit),
                },
                lines = tailer.poll() => Some(Msg::Journal(lines?)),
                _ = ticker.tick(), if app.animating() => Some(Msg::Tick),
                () = crate::cmd::shutdown_signal() => Some(Msg::Signal),
            }
        };
        let Some(msg) = msg else {
            continue;
        };
        if matches!(msg, Msg::Quit) {
            break app.exit_code;
        }
        let msg = match msg {
            Msg::Resize(..) => {
                // tmux fires a burst; only the last size is worth a repair.
                loop {
                    match tokio::time::timeout(RESIZE_QUIET, keys.next()).await {
                        Ok(Some(Ok(Event::Resize(w, h)))) => {
                            width = w.max(20);
                            rows = h.max(MIN_LIVE);
                        }
                        Ok(Some(Ok(event))) => queued.push_back(event),
                        Ok(Some(Err(e))) => return Err(e.into()),
                        Ok(None) | Err(_) => break,
                    }
                }
                term.reflow(width, rows)?;
                Msg::Resize(width, rows)
            }
            msg => msg,
        };
        app.now = OffsetDateTime::now_utc();
        let mut effects: std::collections::VecDeque<Effect> = app.reduce(msg).into();
        app.mark_orphans(&|id| crate::worker::liveness::is_ours(&paths.pidfile(id)));
        let mut quit = None;
        while let Some(effect) = effects.pop_front() {
            match effect {
                Effect::Commit(lines) => {
                    // `reduce` has already dropped the block, so this is the height the live
                    // area needs *after* the commit: the rows it frees are the rows these
                    // lines are written into.
                    term.set_height(render::live_height(&app, rows))?;
                    term.commit(lines)?;
                }
                Effect::Send(text) => brain.send(&text).await?,
                Effect::Interrupt => brain.interrupt().await?,
                Effect::CancelAll => {
                    let n = disp.cancel_all();
                    effects.extend(app.note_cancelled(n));
                }
                Effect::Cancel(id) => disp.cancel(id).await?,
                Effect::Trace(node) => {
                    let view = RunView::load(&paths.dir, true)?;
                    let text = render(
                        &view,
                        &TraceOpts {
                            node,
                            events: true,
                            ..TraceOpts::default()
                        },
                    );
                    effects.extend(app.trace_output(&text));
                }
                Effect::ProbeQuota(ids) => probe_quota(&cfg, &disp, ids),
                Effect::Clear => term.clear_screen()?,
                Effect::Quit(c) => quit = Some(c),
            }
        }
        if let Some(code) = quit {
            break code;
        }
    };

    drop(_guard);
    println!();
    brain.shutdown().await?;
    Ok(code)
}

/// One `account/rateLimits/read` per stale account, off the chat loop: the table is already
/// on screen and re-renders itself when a reading lands.
fn probe_quota(
    cfg: &Arc<crate::config::Config>,
    disp: &Arc<Dispatcher>,
    ids: Vec<crate::model::core::AccountId>,
) {
    for id in ids {
        let Some(account) = cfg.accounts.iter().find(|a| a.id == id) else {
            continue;
        };
        let exec = account.exec.clone();
        let recorded = disp
            .pool()
            .snapshot()
            .into_iter()
            .find(|(_, a, _)| a == &id)
            .and_then(|(_, _, s)| s.quota.and_then(|q| q.limit_id));
        let limit_id =
            crate::worker::codex_quota::probe_limit_id(cfg, &account.id, recorded.as_deref());
        let model = crate::worker::codex_quota::quota_model(cfg, &account.id);
        let env = crate::config::resolve::expand_env(&account.env);
        let pool = Arc::clone(disp.pool());
        let max_age = cfg.quota_max_age();
        tokio::spawn(async move {
            // The same single flight a terminating node takes: one subprocess per account,
            // and a reading that landed while we waited answers for us too.
            let Some(claim) = crate::dispatch::retry::claim_probe(&id, max_age).await else {
                return;
            };
            if !stale(&pool, &id, max_age) {
                return;
            }
            let read = tokio::time::timeout(
                crate::worker::codex_quota::PROBE_TIMEOUT,
                crate::worker::codex_quota::read_rate_limits(&exec, &env),
            )
            .await;
            claim.stamp();
            if let Ok(Ok(read)) = read
                && let Some(snap) = read.select(limit_id.as_deref(), model.as_deref())
            {
                pool.observe_quota_read(
                    &id,
                    &read.buckets,
                    snap,
                    crate::dispatch::account::QuotaSource::AppServer,
                );
            }
        });
    }
}

/// Whether the cached snapshot for `id` is older than `max_age`, read after the probe gate
/// was taken: another caller's reading may have landed while we waited for it.
fn stale(
    pool: &Arc<crate::dispatch::AccountPool>,
    id: &crate::model::core::AccountId,
    max_age: std::time::Duration,
) -> bool {
    let now = time::OffsetDateTime::now_utc();
    pool.snapshot()
        .into_iter()
        .find(|(_, a, _)| a == id)
        .and_then(|(_, _, s)| s.quota_observed_at)
        .is_none_or(|at| (now - at) > max_age)
}

fn size() -> (u16, u16) {
    crossterm::terminal::size()
        .map(|(w, h)| (w.max(20), h.max(MIN_LIVE)))
        .unwrap_or((100, 24))
}

/// The §1.5 workers line. Every health state is named for what it is: only `Cooling` comes
/// back on a timer, so an expired login or a disabled account must not borrow that word.
fn workers_line(
    snapshot: &[(
        crate::model::core::Provider,
        crate::model::core::AccountId,
        AccountState,
    )],
) -> String {
    let accounts = snapshot.len();
    let now = OffsetDateTime::now_utc();
    // The same word every other surface shows: a live timer is a cooldown, not readiness.
    let count = |h: Health| {
        snapshot
            .iter()
            .filter(|(_, _, s)| {
                crate::ui::watch::shown_health(s.health, s.cooldown_until, now) == h
            })
            .count()
    };
    let mut out = format!(
        "{accounts} account{} \u{b7} {} ready",
        if accounts == 1 { "" } else { "s" },
        count(Health::Healthy)
    );
    for (n, word) in [
        (count(Health::Cooling), "cooling"),
        (count(Health::Degraded), "degraded"),
        (count(Health::AuthBroken), "auth-broken"),
        (count(Health::Disabled), "disabled"),
    ] {
        if n > 0 {
            out.push_str(&format!(", {n} {word}"));
        }
    }
    let tightest = snapshot
        .iter()
        .filter_map(|(_, _, s)| s.quota.as_ref())
        .map(|q| q.worst_utilization_at(now))
        .fold(0.0_f64, f64::max);
    out.push_str(&format!(
        " \u{b7} {:.0}% of the tightest window used",
        tightest * 100.0
    ));
    out
}

fn welcome(ctx: &Ctx, disp: &Arc<Dispatcher>) -> WelcomeInfo {
    let cfg = &disp.cfg;
    let tier = cfg.brain.tier.unwrap_or(crate::model::core::Tier::High);
    let provider = cfg
        .brain
        .provider
        .unwrap_or(crate::model::core::Provider::Anthropic);
    let account = cfg
        .brain
        .account
        .clone()
        .or_else(|| cfg.accounts_for(provider).first().map(|a| a.id.clone()));
    let model = account
        .as_ref()
        .and_then(|a| cfg.model_for(provider, tier, Some(a)).ok())
        .or_else(|| cfg.model_for(provider, tier, None).ok())
        .unwrap_or_else(|| "-".to_owned());
    let brain = format!(
        "{provider}/{} · {model} · tier {tier}",
        account.map(|a| a.0).unwrap_or_else(|| "-".to_owned())
    );

    let workers = workers_line(&disp.pool().snapshot());
    WelcomeInfo {
        cwd: ctx.paths.repo.to_string(),
        brain,
        workers,
        run: disp.journal.run().short(),
    }
}

/// One turn, printed plainly. `swamp run` shares this with the non-tty chat.
pub async fn drain_turn(brain: &mut Box<dyn Brain>, ctx: &Ctx) -> i32 {
    let show_thinking = ctx.cfg.ui.show_thinking.unwrap_or(false);
    let mut out = std::io::stdout();
    let mut code = 0;
    while let Some(event) = brain.events().recv().await {
        match event {
            BrainEvent::Ready { session, model } => {
                println!("[brain {model} session {}]", fmt::truncate(&session, 12));
            }
            BrainEvent::Text { delta } => {
                let _ = write!(out, "{delta}");
                let _ = out.flush();
            }
            BrainEvent::Thinking { delta } if show_thinking => {
                let _ = write!(out, "{delta}");
                let _ = out.flush();
            }
            BrainEvent::Thinking { .. } => {}
            BrainEvent::ToolCall { name, preview, .. } => {
                println!("\n  -> {name} {}", fmt::truncate(&preview, 80));
            }
            BrainEvent::ToolDone { name, ok, .. } => {
                println!("  <- {name} {}", if ok { "ok" } else { "failed" });
            }
            BrainEvent::TurnDone { usage, cost } => {
                println!(
                    "\n[{} in / {} out  {}]",
                    fmt::tokens(usage.input_tokens),
                    fmt::tokens(usage.output_tokens),
                    fmt::cost(cost)
                );
                break;
            }
            BrainEvent::Fatal { message } => {
                eprintln!("\nbrain failed: {message}");
                code = 1;
                break;
            }
        }
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::core::{AccountId, Provider};

    fn entry(id: &str, health: Health) -> (Provider, AccountId, AccountState) {
        (
            Provider::Anthropic,
            AccountId(id.to_owned()),
            AccountState {
                health,
                // Every surface derives the word from the timer, so a cooling account has one.
                cooldown_until: (health == Health::Cooling)
                    .then(|| OffsetDateTime::now_utc() + time::Duration::minutes(5)),
                ..Default::default()
            },
        )
    }

    #[test]
    fn the_welcome_line_names_every_health_state() {
        let line = workers_line(&[
            entry("a", Health::Healthy),
            entry("b", Health::Healthy),
            entry("c", Health::Cooling),
            entry("d", Health::AuthBroken),
        ]);
        assert!(
            line.starts_with("4 accounts \u{b7} 2 ready, 1 cooling, 1 auth-broken"),
            "{line}"
        );
        assert_eq!(line.matches("cooling").count(), 1, "{line}");
    }

    /// `swamp accounts enable` rewrites health without touching the timer, and `score` gates
    /// on the timer alone: an account no node can lease must not be counted as ready.
    #[test]
    fn a_live_cooldown_is_never_counted_as_ready() {
        let mut cooling = entry("b", Health::Healthy);
        cooling.2.cooldown_until = Some(OffsetDateTime::now_utc() + time::Duration::minutes(30));
        let line = workers_line(&[entry("a", Health::Healthy), cooling]);
        assert!(
            line.starts_with("2 accounts \u{b7} 1 ready, 1 cooling"),
            "{line}"
        );
    }

    #[test]
    fn a_disabled_account_is_never_called_cooling() {
        let line = workers_line(&[entry("a", Health::Healthy), entry("b", Health::Disabled)]);
        assert!(line.contains("1 disabled"), "{line}");
        assert!(!line.contains("cooling"), "{line}");
    }

    #[test]
    fn the_board_hint_shows_only_in_tmux_with_no_live_board() {
        assert!(board_hint(true, false).is_some());
        assert!(board_hint(true, true).is_none());
        assert!(board_hint(false, false).is_none());
        assert!(board_hint(false, true).is_none());
    }
}
