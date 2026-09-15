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
use crate::dispatch::Dispatcher;
use crate::journal::fold::RunView;
use crate::journal::reader::Tailer;
use crate::ui::chat::app::{App, Effect, MIN_LIVE, Msg};
use crate::ui::chat::blocks::WelcomeInfo;
use crate::ui::chat::input::History;
use crate::ui::chat::theme::Theme;
use crate::ui::fmt;
use crate::ui::trace::{TraceOpts, render};
use crossterm::event::{Event, EventStream};
use futures::StreamExt;
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
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut code = 0;
    while let Some(line) = lines.next_line().await? {
        let text = line.trim().to_owned();
        if text.is_empty() {
            continue;
        }
        if let Some(command) = text.strip_prefix('/') {
            match command.split_whitespace().next().unwrap_or_default() {
                "quit" | "exit" | "q" => break,
                "status" => {
                    let view = RunView::load(&disp.journal.paths().dir, false)?;
                    print!("{}", render(&view, &TraceOpts::default()));
                }
                "help" | "?" => {
                    for c in slash::COMMANDS {
                        println!("{:<12}{}", c.name, c.help);
                    }
                }
                other => println!("unknown command /{other}; try /help"),
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
    app.set_pool(disp.pool().snapshot());
    app.set_stale_accounts(disp.pool().unconfigured());

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
    let mut keys = EventStream::new();
    // Events read past a resize while waiting for the burst to end, replayed once it is handled.
    let mut queued: std::collections::VecDeque<Event> = std::collections::VecDeque::new();

    let code = loop {
        app.now = OffsetDateTime::now_utc();
        app.width = width;
        app.rows = rows;
        app.set_pool(disp.pool().snapshot());
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
        let (exec, limit_id) = (account.exec.clone(), account.limit_id.clone());
        let env = crate::config::resolve::expand_env(&account.env);
        let pool = Arc::clone(disp.pool());
        tokio::spawn(async move {
            let read = tokio::time::timeout(
                crate::worker::codex_quota::PROBE_TIMEOUT,
                crate::worker::codex_quota::read_rate_limits(&exec, &env),
            )
            .await;
            if let Ok(Ok(read)) = read
                && let Some(snap) = read.select(limit_id.as_deref(), None)
            {
                pool.observe_quota_from(
                    &id,
                    snap,
                    crate::dispatch::account::QuotaSource::AppServer,
                );
            }
        });
    }
}

fn size() -> (u16, u16) {
    crossterm::terminal::size()
        .map(|(w, h)| (w.max(20), h.max(MIN_LIVE)))
        .unwrap_or((100, 24))
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

    let snapshot = disp.pool().snapshot();
    let accounts = snapshot.len();
    let ready = snapshot
        .iter()
        .filter(|(_, _, s)| crate::ui::watch::health_word(s.health) == "healthy")
        .count();
    let mut workers = format!(
        "{accounts} account{} · {ready} ready",
        if accounts == 1 { "" } else { "s" }
    );
    let cooling = accounts - ready;
    if cooling > 0 {
        workers.push_str(&format!(", {cooling} cooling"));
    }
    let tightest = snapshot
        .iter()
        .filter_map(|(_, _, s)| s.quota.as_ref())
        .map(|q| q.worst_utilization())
        .fold(0.0_f64, f64::max);
    workers.push_str(&format!(
        " · {:.0}% of the tightest window used",
        tightest * 100.0
    ));
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
