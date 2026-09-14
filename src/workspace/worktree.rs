use crate::ids::{NodeId, RunId};
use crate::workspace::git::Git;
use camino::{Utf8Path, Utf8PathBuf};

pub fn branch_name(prefix: &str, run: RunId, node: NodeId, attempt: u32) -> String {
    let prefix = prefix.trim_matches('/');
    let prefix = if prefix.is_empty() { "swamp" } else { prefix };
    format!("{prefix}/{}/{}-{attempt}", run.short(), node.short())
}

pub async fn add(git: &Git, path: &Utf8Path, branch: &str, base: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let root = git.root.clone();
    git.run(&root, &["worktree", "add", "--detach", path.as_str(), base])
        .await?;
    git.run(path, &["switch", "-c", branch]).await?;
    Ok(())
}

pub async fn remove(git: &Git, path: &Utf8Path, force: bool) -> anyhow::Result<()> {
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(path.as_str());
    let root = git.root.clone();
    let out = git.output(&root, &args).await?;
    if !out.ok {
        anyhow::bail!(
            "cannot remove worktree {path}: {}",
            out.stderr.trim().replace('\n', "; ")
        );
    }
    Ok(())
}

/// Returns how many stale administrative entries git dropped.
pub async fn prune(git: &Git) -> anyhow::Result<u32> {
    let root = git.root.clone();
    let listing = git.run(&root, &["worktree", "list", "--porcelain"]).await?;
    let stale = listing
        .lines()
        .filter(|l| l.starts_with("prunable"))
        .count();
    git.run(&root, &["worktree", "prune"]).await?;
    Ok(stale as u32)
}

/// link/copy seeds: a worktree that cannot build produces a useless diff, expensively.
pub async fn seed(
    main: &Utf8Path,
    wt: &Utf8Path,
    link: &[String],
    copy: &[String],
) -> anyhow::Result<Vec<String>> {
    let mut warnings = Vec::new();
    for entry in link {
        let src = main.join(entry);
        let dst = wt.join(entry);
        if !src.exists() {
            warnings.push(format!("link: {entry} does not exist in {main}, skipped"));
            continue;
        }
        if dst.exists() || dst.is_symlink() {
            warnings.push(format!(
                "link: {entry} already present in the worktree, kept"
            ));
            continue;
        }
        if let Some(parent) = dst.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        if let Err(e) = tokio::fs::symlink(&src, &dst).await {
            warnings.push(format!("link: {entry} could not be symlinked: {e}"));
        }
    }
    for entry in copy {
        let src = main.join(entry);
        let dst = wt.join(entry);
        if !src.is_file() {
            warnings.push(format!("copy: {entry} does not exist in {main}, skipped"));
            continue;
        }
        if let Some(parent) = dst.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        if let Err(e) = tokio::fs::copy(&src, &dst).await {
            warnings.push(format!("copy: {entry} could not be copied: {e}"));
        }
    }
    Ok(warnings)
}

pub async fn list(git: &Git) -> anyhow::Result<Vec<Utf8PathBuf>> {
    let root = git.root.clone();
    let out = git.run(&root, &["worktree", "list", "--porcelain"]).await?;
    let mut paths = Vec::new();
    let mut current: Option<Utf8PathBuf> = None;
    let mut prunable = false;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            if let Some(path) = current.take()
                && !prunable
            {
                paths.push(path);
            }
            current = Some(Utf8PathBuf::from(rest));
            prunable = false;
        } else if line.starts_with("prunable") {
            prunable = true;
        }
    }
    if let Some(path) = current.take()
        && !prunable
    {
        paths.push(path);
    }
    Ok(paths)
}
