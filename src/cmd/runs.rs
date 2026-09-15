use crate::cli::RunsArgs;
use crate::cmd::Ctx;
use crate::journal::fold::RunView;
use crate::ui::fmt;
use serde_json::json;

const DEFAULT_LIMIT: usize = 20;

/// List runs, newest first.
pub async fn run(ctx: &Ctx, args: &RunsArgs) -> anyhow::Result<i32> {
    let limit = match (args.all, args.limit) {
        (true, _) => usize::MAX,
        (_, Some(n)) => n,
        _ => DEFAULT_LIMIT,
    };
    let mut rows = Vec::new();
    for id in ctx.paths.list_runs()? {
        let paths = ctx.paths.run_paths(id);
        let Ok(view) = RunView::load(&paths.dir, false) else {
            continue;
        };
        if args.interrupted && view.finished {
            continue;
        }
        rows.push((id, view));
        if rows.len() >= limit {
            break;
        }
    }

    if ctx.json {
        let out: Vec<_> = rows
            .iter()
            .map(|(id, view)| {
                let t = view.totals();
                json!({
                    "run": id.to_string(),
                    "started_at": view.header.as_ref().map(|h| h.started_at.to_string()),
                    "task": view.header.as_ref().and_then(|h| h.task.clone()),
                    "finished": view.finished,
                    "nodes": t.nodes,
                    "failed": t.failed,
                    "cost_usd": t.cost_usd,
                    "cost_complete": t.cost_complete,
                })
            })
            .collect();
        ctx.out(&format!("{}\n", serde_json::to_string_pretty(&out)?));
        return Ok(0);
    }

    let mut text = format!(
        "{:<10}  {:<8}  {:<5}  {:<6}  {:<8}  {}\n",
        "RUN", "STARTED", "NODES", "FAILED", "COST", "TASK"
    );
    for (id, view) in &rows {
        let t = view.totals();
        text.push_str(&format!(
            "{:<10}  {:<8}  {:<5}  {:<6}  {:<8}  {}\n",
            id.short(),
            view.header
                .as_ref()
                .map(|h| fmt::clock(h.started_at))
                .unwrap_or_else(|| "-".to_owned()),
            t.nodes,
            t.failed,
            if t.cost_complete {
                format!("~${:.2}", t.cost_usd)
            } else {
                format!("~${:.2}+", t.cost_usd)
            },
            summary(view),
        ));
    }
    if rows.is_empty() {
        text.push_str("no runs recorded\n");
    }
    ctx.out(&text);
    Ok(0)
}

fn summary(view: &RunView) -> String {
    let task = view
        .header
        .as_ref()
        .and_then(|h| h.task.clone())
        .unwrap_or_else(|| "-".to_owned());
    let mark = if view.finished { "" } else { " (interrupted)" };
    format!(
        "{}{mark}",
        fmt::truncate(task.lines().next().unwrap_or(""), 60)
    )
}
