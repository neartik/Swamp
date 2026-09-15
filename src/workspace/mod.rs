pub mod adopt;
pub mod diff;
pub mod git;
pub mod worktree;

pub use adopt::{AdoptResult, MergeStrategy, adopt};
pub use diff::DiffSummary;
pub use git::Git;

use crate::config::Config;
use crate::error::SwampError;
use crate::ids::NodeId;
use crate::journal::JournalHandle;
use crate::journal::paths::Paths;
use crate::journal::record::{JournalEvent, NoteAuthor};
use crate::model::core::Tier;
use crate::model::node::WorkResultRef;
use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

const DEFAULT_BRANCH_PREFIX: &str = "swamp";
const DEFAULT_POST_CREATE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const LOCK_TIMEOUT: Duration = Duration::from_secs(120);
const META_FILE: &str = "swamp-worktree.json";

#[derive(Debug, Clone)]
pub struct NodeWorktree {
    pub node: NodeId,
    pub path: Utf8PathBuf,
    pub branch: String,
    pub base: String,
}

/// Written into the worktree's administrative directory so `list` can rebuild a
/// `NodeWorktree` after a restart: the path only carries shortened ids.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorktreeMeta {
    node: String,
    run: String,
    branch: String,
    base: String,
    attempt: u32,
}

/// Owns the global git gate: concurrent `git worktree add` contends on .git/worktrees.
pub struct WorkspaceManager {
    pub git: Git,
    pub paths: Arc<Paths>,
    pub cfg: Arc<Config>,
    pub journal: JournalHandle,
    root: Utf8PathBuf,
    gate: tokio::sync::Mutex<()>,
    shared: Arc<tokio::sync::Mutex<()>>,
    base: tokio::sync::OnceCell<String>,
}

impl WorkspaceManager {
    pub async fn new(
        git: Git,
        paths: Arc<Paths>,
        cfg: Arc<Config>,
        journal: JournalHandle,
    ) -> anyhow::Result<Arc<Self>> {
        let root = worktree_root(&paths, &cfg);
        tokio::fs::create_dir_all(&root)
            .await
            .with_context(|| format!("creating the worktree root {root}"))?;
        Ok(Arc::new(Self {
            git,
            paths,
            cfg,
            journal,
            root,
            gate: tokio::sync::Mutex::new(()),
            shared: Arc::new(tokio::sync::Mutex::new(())),
            base: tokio::sync::OnceCell::new(),
        }))
    }

    /// Where this repo's worktrees live: outside the repo unless `[workspace] root` says otherwise.
    pub fn root(&self) -> &Utf8Path {
        &self.root
    }

    /// Pinned on first call: every node of a run must diff against the same commit.
    pub async fn base_commit(
        &self,
        requested: Option<&str>,
        include_dirty: bool,
    ) -> anyhow::Result<String> {
        self.base
            .get_or_try_init(|| self.resolve_base(requested, include_dirty))
            .await
            .cloned()
    }

    async fn resolve_base(
        &self,
        requested: Option<&str>,
        include_dirty: bool,
    ) -> anyhow::Result<String> {
        let require_clean = self.cfg.workspace.require_clean.unwrap_or(true);
        if !include_dirty && require_clean && !self.git.is_clean().await? {
            return Err(SwampError::DirtyTree.into());
        }
        let requested = requested.map(str::trim).filter(|r| !r.is_empty());
        if include_dirty
            && requested.is_none_or(|r| r == "HEAD")
            && let Some(sha) = self.git.stash_create().await?
        {
            return Ok(sha);
        }
        match requested {
            Some(r) => {
                let root = self.git.root.clone();
                let rev = format!("{r}^{{commit}}");
                let out = self
                    .git
                    .run(&root, &["rev-parse", &rev])
                    .await
                    .with_context(|| format!("resolving the requested base `{r}`"))?;
                Ok(out.trim().to_owned())
            }
            None => self.git.head().await,
        }
    }

    /// Fresh worktree per attempt, seeded and serialized behind a mutex plus a flock.
    pub async fn create(&self, logical: NodeId, attempt: u32) -> anyhow::Result<NodeWorktree> {
        let ws = &self.cfg.workspace;
        let base = self
            .base_commit(ws.base.as_deref(), ws.include_dirty.unwrap_or(false))
            .await?;
        let run = self.journal.run;
        let prefix = ws.branch_prefix.as_deref().unwrap_or(DEFAULT_BRANCH_PREFIX);
        let branch = worktree::branch_name(prefix, run, logical, attempt);
        let path = self
            .root
            .join(run.short())
            .join(format!("{}-{attempt}", logical.short()));

        let _gate = self.gate.lock().await;
        let _flock = self.flock().await?;
        worktree::add(&self.git, &path, &branch, &base).await?;
        self.write_meta(
            &path,
            &WorktreeMeta {
                node: logical.to_string(),
                run: run.to_string(),
                branch: branch.clone(),
                base: base.clone(),
                attempt,
            },
        )
        .await?;

        let warnings = worktree::seed(&self.paths.repo, &path, &ws.link, &ws.copy).await?;
        for w in warnings {
            tracing::warn!(worktree = %path, "{w}");
            self.note(logical, w);
        }
        self.post_create(&path).await?;

        self.emit(
            logical,
            JournalEvent::WorktreeCreated {
                path: path.clone(),
                branch: branch.clone(),
                base: base.clone(),
            },
        );
        Ok(NodeWorktree {
            node: logical,
            path,
            branch,
            base,
        })
    }

    /// Commits whatever the worker left uncommitted, then lets git report what changed.
    pub async fn finalize(
        &self,
        wt: &NodeWorktree,
        title: &str,
        tier: Tier,
    ) -> anyhow::Result<Option<WorkResultRef>> {
        if !wt.path.is_dir() {
            return Ok(None);
        }
        if !self.git.is_clean_at(&wt.path).await? {
            let message = self.commit_message(title, tier, wt);
            self.git.run(&wt.path, &["add", "-A"]).await?;
            self.git
                .run(
                    &wt.path,
                    &[
                        "-c",
                        "commit.gpgsign=false",
                        "commit",
                        "--no-verify",
                        "-m",
                        &message,
                    ],
                )
                .await?;
        }

        let patch = self.patch_path(wt.node);
        let summary = diff::collect(&self.git, &wt.path, &wt.base, &patch).await?;
        self.emit(
            wt.node,
            JournalEvent::DiffCaptured {
                patch: summary.patch.clone(),
                head: summary.head.clone(),
                files: summary.files.len() as u32,
                insertions: summary.insertions,
                deletions: summary.deletions,
            },
        );
        self.emit(
            wt.node,
            JournalEvent::NodeFiles {
                files: summary.files.clone(),
            },
        );
        Ok(Some(WorkResultRef {
            head: summary.head,
            branch: wt.branch.clone(),
            patch: summary.patch,
            insertions: summary.insertions,
            deletions: summary.deletions,
            empty: summary.empty,
        }))
    }

    pub async fn remove(&self, wt: &NodeWorktree, force: bool) -> anyhow::Result<()> {
        let _gate = self.gate.lock().await;
        let _flock = self.flock().await?;
        worktree::remove(&self.git, &wt.path, force).await
    }

    /// Drops finished runs' worktrees. Anything still carrying uncommitted work is kept.
    pub async fn prune(&self) -> anyhow::Result<u32> {
        let current = self.root.join(self.journal.run.short());
        let keep = self.cfg.workspace.keep_on_failure != Some(false);
        let mut removed = 0;
        for wt in self.list().await? {
            if wt.path.starts_with(&current) {
                continue;
            }
            if keep && !self.git.is_clean_at(&wt.path).await.unwrap_or(false) {
                continue;
            }
            let _gate = self.gate.lock().await;
            let _flock = self.flock().await?;
            if worktree::remove(&self.git, &wt.path, !keep).await.is_ok() {
                removed += 1;
            }
        }
        removed += worktree::prune(&self.git).await?;
        Ok(removed)
    }

    pub async fn list(&self) -> anyhow::Result<Vec<NodeWorktree>> {
        let mut out = Vec::new();
        for path in worktree::list(&self.git).await? {
            if !path.starts_with(&self.root) {
                continue;
            }
            let Some(meta) = self.read_meta(&path).await else {
                continue;
            };
            let Ok(node) = NodeId::from_str(&meta.node) else {
                continue;
            };
            out.push(NodeWorktree {
                node,
                path,
                branch: meta.branch,
                base: meta.base,
            });
        }
        Ok(out)
    }

    /// Shared isolation: at most one process mutating the user's real tree.
    pub async fn shared_lock(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.shared.clone().lock_owned().await
    }

    fn commit_message(&self, title: &str, tier: Tier, wt: &NodeWorktree) -> String {
        let template = self
            .cfg
            .workspace
            .commit_template
            .clone()
            .unwrap_or_else(|| "swamp({tier}): {title}".to_owned());
        template
            .replace("{tier}", &tier.to_string())
            .replace("{title}", title)
            .replace("{node}", &wt.node.short())
            .replace("{run}", &self.journal.run.short())
    }

    fn patch_path(&self, node: NodeId) -> Utf8PathBuf {
        self.journal.paths.patch(node)
    }

    async fn post_create(&self, path: &Utf8Path) -> anyhow::Result<()> {
        let Some(cmd) = self
            .cfg
            .workspace
            .post_create
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
        else {
            return Ok(());
        };
        let timeout = self
            .cfg
            .workspace
            .post_create_timeout
            .unwrap_or(DEFAULT_POST_CREATE_TIMEOUT);
        let run = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(path)
            .stdin(std::process::Stdio::null())
            .output();
        let out = match tokio::time::timeout(timeout, run).await {
            Err(_) => anyhow::bail!("post_create `{cmd}` timed out after {timeout:?} in {path}"),
            Ok(r) => r.with_context(|| format!("running post_create `{cmd}` in {path}"))?,
        };
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            anyhow::bail!(
                "post_create `{cmd}` failed with exit {} in {path}: {}",
                out.status.code().unwrap_or(-1),
                tail(&[stderr.trim(), stdout.trim()].join(" ")),
            );
        }
        Ok(())
    }

    async fn meta_path(&self, wt: &Utf8Path) -> Option<Utf8PathBuf> {
        let out = self
            .git
            .output(wt, &["rev-parse", "--absolute-git-dir"])
            .await
            .ok()?;
        out.ok
            .then(|| Utf8PathBuf::from(out.stdout.trim()).join(META_FILE))
    }

    async fn write_meta(&self, wt: &Utf8Path, meta: &WorktreeMeta) -> anyhow::Result<()> {
        let path = self
            .meta_path(wt)
            .await
            .with_context(|| format!("locating the git directory of {wt}"))?;
        tokio::fs::write(&path, serde_json::to_vec_pretty(meta)?).await?;
        Ok(())
    }

    async fn read_meta(&self, wt: &Utf8Path) -> Option<WorktreeMeta> {
        let path = self.meta_path(wt).await?;
        let raw = tokio::fs::read(&path).await.ok()?;
        serde_json::from_slice(&raw).ok()
    }

    /// Cross-process half of the gate: another `swamp` on the same repo contends here.
    async fn flock(&self) -> anyhow::Result<std::fs::File> {
        use fs4::fs_std::FileExt;
        let path = self.root.join(".worktrees.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening the worktree lock {path}"))?;
        let deadline = Instant::now() + LOCK_TIMEOUT;
        loop {
            if file.try_lock_exclusive()? {
                return Ok(file);
            }
            if Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for the worktree lock at {path}");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn emit(&self, node: NodeId, event: JournalEvent) {
        self.journal.emit(Some(node), event);
    }

    fn note(&self, node: NodeId, text: String) {
        self.emit(
            node,
            JournalEvent::Note {
                author: NoteAuthor::Swamp,
                text,
            },
        );
    }
}

/// `<root>/<repo-name>-<hash8>`, with `~` expanded. Outside the repo on purpose.
fn worktree_root(paths: &Paths, cfg: &Config) -> Utf8PathBuf {
    let base = match &cfg.workspace.root {
        Some(root) => Utf8PathBuf::from(shellexpand::tilde(root.as_str()).into_owned()),
        None => paths.home_swamp.join("worktrees"),
    };
    let name = paths.repo.file_name().unwrap_or("repo");
    base.join(format!("{name}-{}", hash8(paths.repo.as_str())))
}

fn hash8(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes())
        .iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn tail(s: &str) -> String {
    const MAX: usize = 400;
    let trimmed = s.trim();
    if trimmed.len() <= MAX {
        return trimmed.to_owned();
    }
    let start = trimmed.len() - MAX;
    let start = (start..trimmed.len())
        .find(|i| trimmed.is_char_boundary(*i))
        .unwrap_or(trimmed.len());
    format!("...{}", &trimmed[start..])
}
