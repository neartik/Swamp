use crate::brain::{Brain, BrainEvent};
use crate::cmd::Ctx;
use crate::dispatch::Dispatcher;
use crate::journal::fold::RunView;
use crate::ui::fmt;
use crate::ui::trace::{TraceOpts, render};
use std::io::Write;
use std::sync::Arc;

const PROMPT: &str = "swamp> ";

/// rustyline REPL rendering BrainEvent plus inline worker progress.
pub async fn repl(
    mut brain: Box<dyn Brain>,
    disp: Arc<Dispatcher>,
    ctx: &Ctx,
) -> anyhow::Result<i32> {
    brain.start().await?;
    println!(
        "swamp {} - /help for commands, ctrl-d to leave",
        crate::VERSION
    );

    let mut editor = rustyline::DefaultEditor::new()?;
    let mut code = 0;
    loop {
        let read = tokio::task::spawn_blocking(move || {
            let line = editor.readline(PROMPT);
            (line, editor)
        })
        .await?;
        editor = read.1;
        let text = match read.0 {
            Ok(text) => text,
            Err(rustyline::error::ReadlineError::Interrupted) => {
                brain.interrupt().await?;
                continue;
            }
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(e) => return Err(e.into()),
        };
        let text = text.trim().to_owned();
        if text.is_empty() {
            continue;
        }
        let _ = editor.add_history_entry(text.as_str());
        if let Some(command) = text.strip_prefix('/') {
            match builtin(command, &disp, ctx).await? {
                Control::Continue => continue,
                Control::Quit => break,
            }
        }
        brain.send(&text).await?;
        code = drain_turn(&mut brain, ctx).await;
    }
    brain.shutdown().await?;
    Ok(code)
}

enum Control {
    Continue,
    Quit,
}

async fn builtin(command: &str, disp: &Arc<Dispatcher>, ctx: &Ctx) -> anyhow::Result<Control> {
    let mut parts = command.split_whitespace();
    match parts.next().unwrap_or_default() {
        "quit" | "exit" | "q" => return Ok(Control::Quit),
        "help" | "?" => println!("/status  /accounts  /cancel <node>  /quit"),
        "status" => {
            let view = RunView::load(&disp.journal.paths().dir, false)?;
            print!("{}", render(&view, &TraceOpts::default()));
        }
        "accounts" => {
            for (provider, id, state) in disp.pool().snapshot() {
                println!(
                    "{provider:<10} {:<10} {:<10} {}/{} ~${:.2}",
                    id.0,
                    crate::ui::watch::health_word(state.health),
                    state.inflight,
                    state.lifetime_nodes,
                    state.lifetime_cost_usd,
                );
            }
        }
        "cancel" => match parts.next().map(str::parse::<crate::ids::NodeId>) {
            Some(Ok(node)) => disp.cancel(node).await?,
            _ => println!("usage: /cancel <node id>"),
        },
        other => println!("unknown command `{other}`; try /help"),
    }
    let _ = ctx;
    Ok(Control::Continue)
}

/// One turn: stream the brain's events until it settles, then hand the prompt back.
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
            BrainEvent::ToolCall { name, preview } => {
                println!("\n  -> {name} {}", fmt::truncate(&preview, 80));
            }
            BrainEvent::ToolDone { name, ok } => {
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
