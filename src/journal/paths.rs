use crate::error::SwampError;
use crate::ids::{NodeId, RunId};
use camino::{Utf8Path, Utf8PathBuf};
use sha2::{Digest, Sha256};
use std::str::FromStr;

/// The entry Swamp appends to `.git/info/exclude`.
const EXCLUDE_ENTRY: &str = "/.swamp/";

/// Repo root, its `.swamp`, and the machine-wide `~/.swamp`.
#[derive(Debug, Clone)]
pub struct Paths {
    pub repo: Utf8PathBuf,
    pub dot_swamp: Utf8PathBuf,
    pub home_swamp: Utf8PathBuf,
}

impl Paths {
    /// Walks up to the git root.
    pub fn discover(cwd: &Utf8Path) -> Result<Paths, SwampError> {
        let start = std::fs::canonicalize(cwd)
            .ok()
            .and_then(|p| Utf8PathBuf::from_path_buf(p).ok())
            .unwrap_or_else(|| cwd.to_path_buf());

        let mut dir = start.as_path();
        loop {
            if dir.join(".git").exists() {
                let repo = dir.to_path_buf();
                let dot_swamp = repo.join(".swamp");
                return Ok(Paths {
                    repo,
                    dot_swamp,
                    home_swamp: home_swamp()?,
                });
            }
            match dir.parent() {
                Some(p) => dir = p,
                None => return Err(SwampError::NotAGitRepo(start)),
            }
        }
    }

    pub fn run_dir(&self, run: RunId) -> Utf8PathBuf {
        self.dot_swamp.join("runs").join(run.to_string())
    }

    pub fn run_paths(&self, run: RunId) -> RunPaths {
        RunPaths {
            run,
            dir: self.run_dir(run),
        }
    }

    /// ~/.swamp/worktrees/<repo>-<hash8>
    pub fn worktree_root(&self) -> Utf8PathBuf {
        let name = self.repo.file_name().unwrap_or("repo");
        let mut h = Sha256::new();
        h.update(self.repo.as_str().as_bytes());
        let hash = h.finalize();
        let hash8: String = hash.iter().take(4).map(|b| format!("{b:02x}")).collect();
        self.home_swamp
            .join("worktrees")
            .join(format!("{name}-{hash8}"))
    }

    /// ~/.swamp/accounts.json
    pub fn accounts_state(&self) -> Utf8PathBuf {
        self.home_swamp.join("accounts.json")
    }

    /// .git/info/exclude, not .gitignore.
    pub fn ensure_git_excluded(&self) -> anyhow::Result<()> {
        let info = self.git_dir()?.join("info");
        std::fs::create_dir_all(&info)?;
        let exclude = info.join("exclude");
        let current = std::fs::read_to_string(&exclude).unwrap_or_default();
        let already = current.lines().any(|l| {
            let t = l.trim();
            matches!(t, "/.swamp/" | "/.swamp" | ".swamp/" | ".swamp")
        });
        if already {
            return Ok(());
        }
        let mut next = current;
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str(EXCLUDE_ENTRY);
        next.push('\n');
        std::fs::write(&exclude, next)?;
        Ok(())
    }

    /// Newest first.
    pub fn list_runs(&self) -> anyhow::Result<Vec<RunId>> {
        let dir = self.dot_swamp.join("runs");
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(anyhow::Error::new(e).context(format!("reading {dir}"))),
        };
        let mut runs: Vec<RunId> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter_map(|e| RunId::from_str(e.file_name().to_str()?).ok())
            .collect();
        runs.sort_unstable();
        runs.reverse();
        Ok(runs)
    }

    /// id | prefix | "last" | "-2"
    pub fn resolve_run(&self, spec: &str) -> anyhow::Result<RunId> {
        let spec = spec.trim();
        anyhow::ensure!(!spec.is_empty(), "empty run specifier");
        let runs = self.list_runs()?;
        anyhow::ensure!(!runs.is_empty(), "no runs recorded in {}", self.dot_swamp);

        if spec.eq_ignore_ascii_case("last") {
            return Ok(runs[0]);
        }
        if let Some(back) = spec.strip_prefix('-')
            && let Ok(n) = back.parse::<usize>()
        {
            anyhow::ensure!(n >= 1, "run offset must be 1 or more");
            return runs.get(n - 1).copied().ok_or_else(|| {
                anyhow::anyhow!("only {} runs recorded, cannot go back {n}", runs.len())
            });
        }

        let needle = spec
            .strip_prefix("run_")
            .unwrap_or(spec)
            .to_ascii_uppercase();
        let matches: Vec<RunId> = runs
            .iter()
            .copied()
            .filter(|r| r.0.to_string().starts_with(&needle))
            .collect();
        match matches.len() {
            1 => Ok(matches[0]),
            0 => anyhow::bail!("no run matches `{spec}`"),
            _ => {
                let names: Vec<String> = matches.iter().take(8).map(|r| r.to_string()).collect();
                anyhow::bail!("run `{spec}` is ambiguous: {}", names.join(", "))
            }
        }
    }

    /// Resolves the real git directory, following a `.git` file in a linked worktree.
    fn git_dir(&self) -> anyhow::Result<Utf8PathBuf> {
        let dot_git = self.repo.join(".git");
        let meta =
            std::fs::metadata(&dot_git).map_err(|_| SwampError::NotAGitRepo(self.repo.clone()))?;
        if meta.is_dir() {
            return Ok(dot_git);
        }
        let text = std::fs::read_to_string(&dot_git)?;
        let target = text
            .lines()
            .find_map(|l| l.trim().strip_prefix("gitdir:"))
            .map(str::trim)
            .ok_or_else(|| anyhow::anyhow!("{dot_git} has no gitdir: pointer"))?;
        let target = Utf8PathBuf::from(target);
        let joined = if target.is_absolute() {
            target
        } else {
            self.repo.join(target)
        };
        let real = std::fs::canonicalize(&joined)?;
        Utf8PathBuf::from_path_buf(real)
            .map_err(|p| anyhow::anyhow!("non-utf8 git dir {}", p.display()))
    }
}

fn home_swamp() -> Result<Utf8PathBuf, SwampError> {
    let home = directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from))
        .ok_or_else(|| {
            SwampError::Io(std::io::Error::other("cannot determine the home directory"))
        })?;
    let home = Utf8PathBuf::from_path_buf(home)
        .map_err(|p| SwampError::Io(std::io::Error::other(format!("non-utf8 home {p:?}"))))?;
    Ok(home.join(".swamp"))
}

#[derive(Debug, Clone)]
pub struct RunPaths {
    pub run: RunId,
    pub dir: Utf8PathBuf,
}

impl RunPaths {
    pub fn journal(&self) -> Utf8PathBuf {
        self.dir.join("journal.jsonl")
    }
    pub fn node_dir(&self, n: NodeId) -> Utf8PathBuf {
        self.dir.join("nodes").join(n.short())
    }
    pub fn prompt(&self, n: NodeId) -> Utf8PathBuf {
        self.node_dir(n).join("prompt.md")
    }
    pub fn stream(&self, n: NodeId) -> Utf8PathBuf {
        self.node_dir(n).join("stream.jsonl")
    }
    pub fn stderr(&self, n: NodeId) -> Utf8PathBuf {
        self.node_dir(n).join("stderr.log")
    }
    pub fn noise(&self, n: NodeId) -> Utf8PathBuf {
        self.node_dir(n).join("noise.log")
    }
    pub fn last_message(&self, n: NodeId) -> Utf8PathBuf {
        self.node_dir(n).join("last-message.txt")
    }
    pub fn patch(&self, n: NodeId) -> Utf8PathBuf {
        self.node_dir(n).join("patch.diff")
    }
    pub fn pidfile(&self, n: NodeId) -> Utf8PathBuf {
        self.node_dir(n).join("pid")
    }
    pub fn result(&self, n: NodeId) -> Utf8PathBuf {
        self.node_dir(n).join("result.json")
    }
    pub fn socket(&self) -> Utf8PathBuf {
        self.dir.join("ctl.sock")
    }
    pub fn link_last(&self) -> anyhow::Result<()> {
        let runs = self
            .dir
            .parent()
            .ok_or_else(|| anyhow::anyhow!("{} has no parent", self.dir))?;
        let root = runs
            .parent()
            .ok_or_else(|| anyhow::anyhow!("{runs} has no parent"))?;
        let link = root.join("last");
        match std::fs::symlink_metadata(&link) {
            Ok(m) if m.is_dir() && !m.is_symlink() => {
                anyhow::bail!("{link} is a directory, refusing to replace it")
            }
            Ok(_) => std::fs::remove_file(&link)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let target = Utf8PathBuf::from("runs").join(self.run.to_string());
        std::os::unix::fs::symlink(target, &link)?;
        Ok(())
    }
}
