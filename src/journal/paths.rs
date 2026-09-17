use crate::error::SwampError;
use crate::ids::{NodeId, RunId};
use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::str::FromStr;
use time::OffsetDateTime;

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
            sock_dir: self.sock_dir(),
        }
    }

    /// ~/.swamp/sock, shared by every repo on the machine.
    pub fn sock_dir(&self) -> Utf8PathBuf {
        self.home_swamp.join("sock")
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

    /// ~/.swamp/runs.json, the cross-repo run index `swamp board --all` reads.
    pub fn runs_index(&self) -> Utf8PathBuf {
        self.home_swamp.join("runs.json")
    }

    /// ~/.swamp/board.pid
    pub fn board_pid(&self) -> Utf8PathBuf {
        self.home_swamp.join("board.pid")
    }

    /// Appends (or refreshes) this repo's entry, keyed by the run's short id, and prunes
    /// every entry whose journal has disappeared. Called at `swamp run` / `swamp chat`
    /// startup; last-writer-wins per key, exactly like `accounts.json`.
    pub fn register_run(&self, run: RunId, dir: &Utf8Path) -> anyhow::Result<()> {
        let entry = RunIndexEntry {
            run,
            repo: self.repo.clone(),
            dir: dir.to_path_buf(),
            pid: std::process::id(),
            started_at: OffsetDateTime::now_utc(),
        };
        update_runs_index(&self.runs_index(), |index| {
            index.insert(run.short(), entry);
        })
    }

    /// Removes this run's entry, called when the owning process exits normally. A process
    /// that dies without deregistering leaves the entry for `load_runs_index` to prune once
    /// its run directory is gone (e.g. `swamp gc`).
    pub fn deregister_run(&self, run: RunId) -> anyhow::Result<()> {
        update_runs_index(&self.runs_index(), |index| {
            index.remove(&run.short());
        })
    }

    /// Every run any repo on this machine has registered, minus entries whose journal file
    /// no longer exists on disk.
    pub fn load_runs_index(&self) -> anyhow::Result<RunsIndex> {
        let path = self.runs_index();
        let _guard = lock_index(&path)?;
        let mut index = read_runs_index_unlocked(&path)?;
        index.retain(|_, e| e.dir.join("journal.jsonl").is_file());
        Ok(index)
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
        // The short id the UI prints is the ULID's LAST 6 chars, so a prefix match alone
        // rejects the one spelling a user can actually see.
        let short = needle.to_ascii_lowercase();
        let matches: Vec<RunId> = runs
            .iter()
            .copied()
            .filter(|r| r.0.to_string().starts_with(&needle) || r.short() == short)
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

/// One repo's live-or-recent run, as recorded in `~/.swamp/runs.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunIndexEntry {
    pub run: RunId,
    pub repo: Utf8PathBuf,
    pub dir: Utf8PathBuf,
    pub pid: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
}

/// Keyed by the run's short id, the same spelling `swamp trace` and `swamp board` print.
pub type RunsIndex = BTreeMap<String, RunIndexEntry>;

/// Read-modify-write of `runs.json` under the fs4 lock, mirroring
/// `dispatch::persist`'s pattern for `accounts.json`.
fn update_runs_index(path: &Utf8Path, f: impl FnOnce(&mut RunsIndex)) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {dir}"))?;
    }
    let _guard = lock_index(path)?;
    let mut index = read_runs_index_unlocked(path)?;
    f(&mut index);
    write_runs_index_locked(path, &index)
}

fn read_runs_index_unlocked(path: &Utf8Path) -> anyhow::Result<RunsIndex> {
    if !path.is_file() {
        return Ok(RunsIndex::new());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    if text.trim().is_empty() {
        return Ok(RunsIndex::new());
    }
    serde_json::from_str(&text).with_context(|| format!("parsing {path}"))
}

fn write_runs_index_locked(path: &Utf8Path, index: &RunsIndex) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{path} has no parent directory"))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir.as_std_path())
        .with_context(|| format!("creating a temp file in {dir}"))?;
    let body = serde_json::to_vec_pretty(index)?;
    tmp.write_all(&body)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path.as_std_path())
        .map_err(|e| anyhow::anyhow!("renaming into {path}: {}", e.error))?;
    Ok(())
}

/// A sibling lock file, not the index itself: the lock must outlive the rename.
fn lock_index(path: &Utf8Path) -> anyhow::Result<File> {
    let lock_path = path.with_extension("lock");
    if let Some(dir) = lock_path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {dir}"))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening {lock_path}"))?;
    FileExt::lock_exclusive(&file).with_context(|| format!("locking {lock_path}"))?;
    Ok(file)
}

/// Records this process's pid so a second `swamp board` can tell one is already running.
/// PID-reuse safe: reuses `worker::liveness`'s start-time comparison rather than a bare pid.
pub fn write_board_pid(path: &Utf8Path) -> anyhow::Result<()> {
    crate::worker::liveness::write_pidfile(path, std::process::id() as i32)
}

/// The pid on file, if any, regardless of whether that process is still alive.
pub fn read_board_pid(path: &Utf8Path) -> Option<i32> {
    let text = std::fs::read_to_string(path).ok()?;
    text.split_whitespace().next()?.parse().ok()
}

/// True when the recorded board process is still the one running.
pub fn board_is_alive(path: &Utf8Path) -> bool {
    crate::worker::liveness::is_ours(path)
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
    /// Where the control socket lives, kept away from the run directory on purpose.
    pub sock_dir: Utf8PathBuf,
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
    /// Short by construction. macOS caps a unix socket path at 104 bytes (SUN_LEN) and a repo
    /// can sit arbitrarily deep, so the control socket never lives under the run directory.
    pub fn socket(&self) -> Utf8PathBuf {
        self.sock_dir.join(format!("{}.sock", self.run.short()))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_with(runs: &[&str]) -> (tempfile::TempDir, Paths) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("utf8 tempdir");
        for r in runs {
            std::fs::create_dir_all(root.join(".swamp").join("runs").join(r)).expect("run dir");
        }
        let paths = Paths {
            repo: root.clone(),
            dot_swamp: root.join(".swamp"),
            home_swamp: root.join("home"),
        };
        (tmp, paths)
    }

    /// `swamp trace` prints `run bxfrkv`, so `swamp trace bxfrkv` has to work: the short id is
    /// the ULID's last 6 chars, which no prefix match can reach.
    #[test]
    fn a_run_resolves_by_the_short_id_the_ui_prints() {
        let id = "run_01M2HJPC5EMCD62141ZXBXFRKV";
        let (_tmp, paths) = paths_with(&[id, "run_01ARZ3NDEKTSV4RRFFQ69G5FAV"]);
        let want = RunId::from_str(id).expect("run id");
        assert_eq!(want.short(), "bxfrkv");

        for spec in [
            "bxfrkv",
            "BXFRKV",
            id,
            "01M2HJPC5EMCD62141ZXBXFRKV",
            "01M2HJ",
        ] {
            assert_eq!(paths.resolve_run(spec).expect(spec), want, "{spec}");
        }
        assert!(paths.resolve_run("zzzzzz").is_err());
    }

    fn fake_run_dir(paths: &Paths, run: RunId) -> Utf8PathBuf {
        let dir = paths.run_dir(run);
        std::fs::create_dir_all(&dir).expect("run dir");
        std::fs::write(dir.join("journal.jsonl"), "").expect("journal file");
        dir
    }

    #[test]
    fn runs_index_round_trips_through_register_and_load() {
        let (_tmp, paths) = paths_with(&[]);
        let run = RunId::new();
        let dir = fake_run_dir(&paths, run);

        paths.register_run(run, &dir).expect("register");

        let index = paths.load_runs_index().expect("load");
        assert_eq!(index.len(), 1);
        let entry = &index[&run.short()];
        assert_eq!(entry.run, run);
        assert_eq!(entry.repo, paths.repo);
        assert_eq!(entry.dir, dir);
        assert_eq!(entry.pid, std::process::id());
    }

    #[test]
    fn deregister_removes_the_entry() {
        let (_tmp, paths) = paths_with(&[]);
        let run = RunId::new();
        let dir = fake_run_dir(&paths, run);
        paths.register_run(run, &dir).expect("register");

        paths.deregister_run(run).expect("deregister");

        assert!(paths.load_runs_index().expect("load").is_empty());
    }

    /// A run whose directory was `gc`'d away must not linger forever in the cross-repo index.
    #[test]
    fn load_prunes_entries_whose_journal_is_gone() {
        let (_tmp, paths) = paths_with(&[]);
        let alive = RunId::new();
        let alive_dir = fake_run_dir(&paths, alive);
        paths
            .register_run(alive, &alive_dir)
            .expect("register alive");

        let gone = RunId::new();
        let gone_dir = paths.run_dir(gone); // never created: no journal.jsonl
        paths.register_run(gone, &gone_dir).expect("register gone");

        let index = paths.load_runs_index().expect("load");
        assert_eq!(index.len(), 1, "{index:?}");
        assert!(index.contains_key(&alive.short()));
        assert!(!index.contains_key(&gone.short()));
    }

    /// Two repos sharing one `~/.swamp` must merge, not overwrite: this is the same
    /// last-writer-wins-per-key contract `dispatch::persist` gives `accounts.json`.
    #[test]
    fn two_repos_registering_concurrently_both_survive() {
        let (_tmp_a, paths_a) = paths_with(&[]);
        let other_repo = paths_a.repo.parent().expect("parent").join("other-repo");
        let paths_b = Paths {
            repo: other_repo.clone(),
            dot_swamp: other_repo.join(".swamp"),
            home_swamp: paths_a.home_swamp.clone(),
        };

        let run_a = RunId::new();
        let dir_a = fake_run_dir(&paths_a, run_a);
        paths_a.register_run(run_a, &dir_a).expect("register a");

        let run_b = RunId::new();
        let dir_b = fake_run_dir(&paths_b, run_b);
        paths_b.register_run(run_b, &dir_b).expect("register b");

        let index = paths_a.load_runs_index().expect("load");
        assert_eq!(index.len(), 2);
        assert_eq!(index[&run_a.short()].repo, paths_a.repo);
        assert_eq!(index[&run_b.short()].repo, paths_b.repo);
    }

    #[test]
    fn board_pid_round_trips_and_reports_liveness() {
        let (_tmp, paths) = paths_with(&[]);
        let pid_path = paths.board_pid();
        assert_eq!(read_board_pid(&pid_path), None, "nothing written yet");
        assert!(!board_is_alive(&pid_path));

        write_board_pid(&pid_path).expect("write");
        assert_eq!(read_board_pid(&pid_path), Some(std::process::id() as i32));
        assert!(board_is_alive(&pid_path), "this process is still running");
    }
}
