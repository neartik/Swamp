use crate::cli::DiffArgs;
use crate::cmd::Ctx;
use crate::ui::fmt;
use std::io::Write;

/// Show one worker node's diff. Git is authoritative: this is the patch that was captured.
pub async fn run(ctx: &Ctx, args: &DiffArgs) -> anyhow::Result<i32> {
    let (paths, node) = ctx.find_node(&args.node)?;
    let patch = node
        .work
        .as_ref()
        .map(|w| w.patch.clone())
        .unwrap_or_else(|| paths.patch(node.id));

    if args.name_only {
        let mut text = String::new();
        for f in &node.files {
            text.push_str(&format!("{}\n", f.path));
        }
        ctx.out(&text);
        return Ok(0);
    }
    if args.stat {
        let mut text = String::new();
        for f in &node.files {
            text.push_str(&format!(
                "{:<50} +{:<6} -{:<6} {:?}\n",
                fmt::truncate(f.path.as_str(), 50),
                f.added,
                f.removed,
                f.kind
            ));
        }
        let w = node.work.as_ref();
        text.push_str(&format!(
            "{} files, +{} -{}\n",
            node.files.len(),
            w.map_or(0, |w| w.insertions),
            w.map_or(0, |w| w.deletions),
        ));
        ctx.out(&text);
        return Ok(0);
    }

    let bytes = std::fs::read(&patch)
        .map_err(|e| anyhow::anyhow!("no patch for node {}: {patch}: {e}", node.id.short()))?;
    let mut out = std::io::stdout().lock();
    out.write_all(&bytes)?;
    out.flush()?;
    Ok(0)
}
