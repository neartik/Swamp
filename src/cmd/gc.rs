use crate::cli::GcArgs;
use crate::cmd::{Ctx, parse_duration};
use crate::journal::fold::RunView;
use crate::workspace::{Git, worktree};
use camino::Utf8PathBuf;
use time::OffsetDateTime;

/// Delete old runs and worktrees. `gc` deletes rather than compresses, and it never
/// silently throws away uncommitted work.
pub async fn run(ctx: &Ctx, args: &GcArgs) -> anyhow::Result<i32> {
    let keep = args.keep.or(ctx.cfg.journal.keep_runs).unwrap_or(200) as usize;
    let older_than = match &args.older_than {
        Some(d) => Some(parse_duration(d)?),
        None => ctx.cfg.journal.keep_runs_for,
    };
    let cutoff = older_than.map(|d| OffsetDateTime::now_utc() - d);

    let runs = ctx.paths.list_runs()?;
    let mut doomed: Vec<Utf8PathBuf> = Vec::new();
    let mut kept_live = 0;
    for (i, id) in runs.iter().enumerate() {
        let paths = ctx.paths.run_paths(*id);
        let view = RunView::load(&paths.dir, false).unwrap_or_default();
        let started = view.header.as_ref().map(|h| h.started_at);
        let too_old = match (cutoff, started) {
            (Some(cut), Some(at)) => at < cut,
            _ => false,
        };
        if i < keep && !too_old {
            continue;
        }
        if !view.finished && !args.force {
            kept_live += 1;
            continue;
        }
        doomed.push(paths.dir.clone());
    }

    let git = Git::discover(&ctx.paths.repo).await?;
    let mut worktrees: Vec<(Utf8PathBuf, bool)> = Vec::new();
    let root = crate::cmd::worktree_root(ctx);
    for path in worktree::list(&git).await? {
        if !path.starts_with(&root) {
            continue;
        }
        let clean = git.is_clean_at(&path).await.unwrap_or(false);
        if clean || args.force {
            worktrees.push((path, clean));
        }
    }

    if args.dry_run {
        let mut text = String::new();
        for d in &doomed {
            text.push_str(&format!("would delete run   {d}\n"));
        }
        for (w, clean) in &worktrees {
            text.push_str(&format!(
                "would delete tree  {w}{}\n",
                if *clean { "" } else { " (uncommitted work)" }
            ));
        }
        text.push_str(&format!(
            "{} runs, {} worktrees; nothing deleted\n",
            doomed.len(),
            worktrees.len()
        ));
        ctx.out(&text);
        return Ok(0);
    }

    let mut removed = 0;
    for (path, clean) in &worktrees {
        if !clean && !args.force {
            continue;
        }
        if worktree::remove(&git, path, args.force).await.is_ok() {
            removed += 1;
        }
    }
    worktree::prune(&git).await.ok();
    for dir in &doomed {
        std::fs::remove_dir_all(dir).ok();
    }
    println!(
        "deleted {} runs and {removed} worktrees{}",
        doomed.len(),
        if kept_live > 0 {
            format!("; kept {kept_live} unfinished runs (use --force)")
        } else {
            String::new()
        }
    );
    Ok(0)
}
