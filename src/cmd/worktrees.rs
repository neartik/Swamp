use crate::cli::{WorktreesArgs, WorktreesCmd};
use crate::cmd::{Ctx, worktree_root};
use crate::workspace::{Git, worktree};

/// Inspect, prune and open worker worktrees. They live outside the repo on purpose.
pub async fn run(ctx: &Ctx, args: &WorktreesArgs) -> anyhow::Result<i32> {
    let git = Git::discover(&ctx.paths.repo).await?;
    let root = worktree_root(ctx);
    match args.command.as_ref().unwrap_or(&WorktreesCmd::Ls) {
        WorktreesCmd::Ls => {
            let mut text = String::new();
            for path in worktree::list(&git).await? {
                if !path.starts_with(&root) {
                    continue;
                }
                let clean = git.is_clean_at(&path).await.unwrap_or(false);
                text.push_str(&format!(
                    "{:<8} {path}\n",
                    if clean { "clean" } else { "dirty" }
                ));
            }
            if text.is_empty() {
                text.push_str(&format!("no worktrees under {root}\n"));
            }
            ctx.out(&text);
            Ok(0)
        }
        WorktreesCmd::Prune => {
            let mut removed = 0u32;
            for path in worktree::list(&git).await? {
                if !path.starts_with(&root) || !git.is_clean_at(&path).await.unwrap_or(false) {
                    continue;
                }
                if worktree::remove(&git, &path, false).await.is_ok() {
                    removed += 1;
                }
            }
            removed += worktree::prune(&git).await?;
            println!("pruned {removed} worktrees");
            Ok(0)
        }
        WorktreesCmd::Open { node } => {
            let (_, record) = ctx.find_node(node)?;
            let path = record.workspace.path();
            anyhow::ensure!(path.is_dir(), "the worktree {path} is gone");
            println!("{path}");
            Ok(0)
        }
    }
}
