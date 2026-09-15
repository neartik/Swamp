use crate::cli::DoctorArgs;
use crate::cmd::Ctx;
use crate::doctor::{Level, checks};
use serde_json::json;

/// Health checks and repairs. Exit 1 when anything is broken, so CI can gate on it.
pub async fn run(ctx: &Ctx, args: &DoctorArgs) -> anyhow::Result<i32> {
    if args.fix {
        fix(ctx)?;
    }
    let mut results = checks(&ctx.cfg, &ctx.paths, args.probe, args.schema).await;
    if args.reap {
        let removed = crate::doctor::reap(&ctx.paths).await?;
        results.push(crate::doctor::Check {
            name: "reap".into(),
            level: Level::Ok,
            detail: format!("removed {removed} stale sockets, pidfiles and worktrees"),
        });
    }

    let errors = results.iter().filter(|c| c.level == Level::Error).count();
    let warnings = results.iter().filter(|c| c.level == Level::Warn).count();

    if ctx.json {
        let rows: Vec<_> = results
            .iter()
            .map(|c| json!({ "name": c.name, "level": c.level.label(), "detail": c.detail }))
            .collect();
        ctx.out(&format!(
            "{}\n",
            serde_json::to_string_pretty(&json!({
                "checks": rows, "errors": errors, "warnings": warnings
            }))?
        ));
        return Ok(if errors > 0 { 1 } else { 0 });
    }

    let mut text = format!(
        "swamp {} - {}\n\n",
        crate::VERSION,
        std::env::consts::OS.to_owned() + " " + std::env::consts::ARCH
    );
    let mut section = String::new();
    for c in &results {
        let head = c.name.split('/').next().unwrap_or("").to_owned();
        if head != section {
            text.push_str(&format!("{head}\n"));
            section = head;
        }
        text.push_str(&format!(
            "  {:<5} {:<28} {}\n",
            c.level.label(),
            c.name,
            c.detail
        ));
    }
    text.push_str(&format!("\n{warnings} warnings, {errors} errors.\n"));
    ctx.out(&text);
    Ok(if errors > 0 { 1 } else { 0 })
}

/// Only the repairs that cannot lose anything: the directories and the git exclude.
fn fix(ctx: &Ctx) -> anyhow::Result<()> {
    std::fs::create_dir_all(&ctx.paths.dot_swamp)?;
    std::fs::create_dir_all(&ctx.paths.home_swamp)?;
    ctx.paths.ensure_git_excluded()?;
    Ok(())
}
