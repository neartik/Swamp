use crate::cli::AdoptArgs;
use crate::cmd::Ctx;
use crate::journal::record::JournalEvent;
use crate::journal::writer::{FsyncPolicy, Writer};
use crate::workspace::adopt::AdoptResult;
use crate::workspace::{Git, adopt};

/// Land a worker's work in the user's checkout. Always a user action: v1 never auto-merges.
pub async fn run(ctx: &Ctx, args: &AdoptArgs) -> anyhow::Result<i32> {
    let git = Git::discover(&ctx.paths.repo).await?;
    let mut code = 0;
    for spec in &args.nodes {
        let (paths, node) = ctx.find_node(spec)?;
        let Some(work) = node.work.clone() else {
            println!("node {} produced no branch to adopt", node.id.short());
            code = code.max(1);
            continue;
        };
        let result = adopt(
            &git,
            &work,
            args.strategy,
            args.into.as_deref(),
            args.force,
            args.dry_run,
        )
        .await?;
        match &result {
            AdoptResult::Clean { commit } => {
                println!(
                    "adopted {} from {}{}",
                    node.id.short(),
                    work.branch,
                    commit
                        .as_deref()
                        .map(|c| format!(" as {}", crate::ui::fmt::short_sha(c)))
                        .unwrap_or_else(|| " into the working tree".to_owned())
                );
                if !args.dry_run {
                    journal_adopted(&paths, node.id, &work.branch, commit.as_deref(), &[]).await;
                }
            }
            AdoptResult::Conflicted { paths: conflicts } => {
                println!("conflicts adopting {}:", node.id.short());
                for p in conflicts {
                    println!("  {p}");
                }
                if !args.dry_run {
                    journal_adopted(&paths, node.id, &work.branch, None, conflicts).await;
                }
                code = code.max(5);
            }
            AdoptResult::Rejected { reason } => {
                println!("refused to adopt {}: {reason}", node.id.short());
                code = code.max(1);
            }
        }
    }
    Ok(code)
}

/// The adoption is part of the run's history, so it is appended to that run's journal.
async fn journal_adopted(
    paths: &crate::journal::paths::RunPaths,
    node: crate::ids::NodeId,
    branch: &str,
    commit: Option<&str>,
    conflicts: &[camino::Utf8PathBuf],
) {
    let Ok(mut writer) = Writer::open(&paths.journal(), FsyncPolicy::Always).await else {
        return;
    };
    let line = crate::journal::JournalLine {
        seq: writer.seq,
        at: time::OffsetDateTime::now_utc(),
        run: paths.run,
        node: Some(node),
        event: JournalEvent::Adopted {
            into: branch.to_owned(),
            commit: commit.unwrap_or_default().to_owned(),
            conflicts: conflicts.to_vec(),
        },
    };
    if let Err(e) = writer.append(&line).await {
        tracing::warn!("could not journal the adoption: {e}");
    }
    let _ = writer.sync().await;
}
