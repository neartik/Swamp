//! WP3: the only half of the board that touches the filesystem. Polling, not `notify`: the
//! crate is not a dependency, kqueue needs one fd per watched file on macOS, and it misses
//! the truncate-and-rewrite case entirely.

use crate::config::Config;
use crate::dispatch::policy::Scoring;
use crate::ids::{NodeId, RunId};
use crate::journal::fold::RunView;
use crate::journal::paths::{Paths, RunPaths};
use crate::journal::reader::Tailer;
use crate::journal::record::JournalLine;
use crate::ui::board::model::{Board, MAX_RUNS, Replay, RunPane, is_live};
use crate::worker::liveness;
use camino::{Utf8Path, Utf8PathBuf};
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use time::OffsetDateTime;

/// `list_runs` is a directory scan plus a liveness probe per candidate; 5 s is often enough
/// for a board that only has to notice a new `swamp chat`.
pub const DISCOVERY_PERIOD: Duration = Duration::from_secs(5);

/// The shortest useful period for a file another process rewrites on every usage commit.
pub const ACCOUNTS_PERIOD: Duration = Duration::from_secs(1);

/// How deep into a repo's run history discovery looks. Runs are newest first and a live run
/// is recent by definition, so anything past this is finished by construction.
const SCAN_LIMIT: usize = 64;

// ---------------------------------------------------------------- tailing

/// `(dev, ino, len)` at the last read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mark {
    dev: u64,
    ino: u64,
    len: u64,
}

impl Mark {
    fn of(path: &Utf8Path) -> Option<Mark> {
        let m = std::fs::metadata(path).ok()?;
        Some(Mark {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
        })
    }
}

#[derive(Debug, Default)]
pub struct TailUpdate {
    pub lines: Vec<JournalLine>,
    /// The journal was replaced or truncated: the fold has to start over.
    pub restarted: bool,
}

/// A `Tailer` that survives `swamp gc` and `swamp replay`. Both rewrite a journal, which
/// makes a byte offset meaningless; re-reading from zero is safe because `RunView::apply`
/// folds any prefix to the same state.
#[derive(Debug)]
pub struct Tail {
    tailer: Tailer,
    mark: Option<Mark>,
}

impl Tail {
    pub fn open(journal: &Utf8Path) -> anyhow::Result<Tail> {
        Ok(Tail {
            tailer: Tailer::open(journal)?,
            mark: Mark::of(journal),
        })
    }

    /// A tail over a path that is never read, for tests of the pure model.
    pub fn detached(journal: &str) -> Tail {
        Tail {
            tailer: Tailer {
                path: Utf8PathBuf::from(journal),
                offset: 0,
            },
            mark: None,
        }
    }

    pub fn path(&self) -> &Utf8Path {
        &self.tailer.path
    }

    pub fn offset(&self) -> u64 {
        self.tailer.offset
    }

    /// Positions the tail at the end of what a replay has already folded.
    pub fn seek_to_end(&mut self) {
        self.tailer.offset = self.mark.map_or(0, |m| m.len);
    }

    /// May sleep up to `reader::POLL_INTERVAL` when the journal has nothing new.
    pub async fn poll(&mut self) -> anyhow::Result<TailUpdate> {
        let restarted = self.rotated();
        if restarted {
            self.tailer.offset = 0;
        }
        let lines = self.tailer.poll().await?;
        if let Some(mark) = Mark::of(&self.tailer.path) {
            self.mark = Some(mark);
        }
        Ok(TailUpdate { lines, restarted })
    }

    /// A new inode is a rename over the path; a shorter file is a truncation in place.
    fn rotated(&mut self) -> bool {
        let (Some(prev), Some(next)) = (self.mark, Mark::of(&self.tailer.path)) else {
            return false;
        };
        prev.dev != next.dev || prev.ino != next.ino || next.len < prev.len
    }
}

// ---------------------------------------------------------------- accounts

/// Never blocks on the fs4 lock and never holds it across a render: a contended frame keeps
/// the numbers it already had. The loader and the row builder both live in the shared
/// modules so `/usage`, `swamp usage` and the board cannot drift.
pub use crate::dispatch::persist::try_load_state;
pub use crate::ui::usage::rows_from_state;

// ---------------------------------------------------------------- sources

/// Which runs the board is asked to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scope {
    /// This repo's live runs.
    #[default]
    Repo,
    /// Every repo `~/.swamp/runs.json` knows about, plus this one.
    All,
    /// `--run <id>`: exactly one, live or not.
    Pinned(RunId),
}

pub struct Sources {
    pub paths: Arc<Paths>,
    pub cfg: Arc<Config>,
    pub scope: Scope,
    accounts_mtime: Option<SystemTime>,
    accounts_read: bool,
    accounts_due: Option<Instant>,
    discovery_due: Option<Instant>,
}

impl Sources {
    pub fn new(paths: Arc<Paths>, cfg: Arc<Config>, scope: Scope) -> Sources {
        Sources {
            paths,
            cfg,
            scope,
            accounts_mtime: None,
            accounts_read: false,
            accounts_due: None,
            discovery_due: None,
        }
    }

    /// The first frame: discover, fold what is already on disk, and read the accounts once.
    pub fn board(&mut self, at: Instant) -> anyhow::Result<Board> {
        let mut board = Board::new(
            Scoring::from_config(&self.cfg),
            self.cfg.dispatch.policy.unwrap_or_default(),
            OffsetDateTime::now_utc(),
        );
        self.sync_runs(&mut board, at)?;
        self.sync_accounts(&mut board, at)?;
        self.refresh_liveness(&mut board);
        Ok(board)
    }

    /// Rediscovery, gated at `DISCOVERY_PERIOD`. True when the tailed set changed.
    pub fn sync_runs(&mut self, board: &mut Board, at: Instant) -> anyhow::Result<bool> {
        if !due(&mut self.discovery_due, at, DISCOVERY_PERIOD) {
            return Ok(false);
        }
        let (wanted, hidden) = self.live_runs(board)?;
        board.hidden_runs = hidden;
        let me = &*self;
        Ok(board.sync(&wanted, |run| me.open_run(run)))
    }

    /// Reads `accounts.json`, gated at `ACCOUNTS_PERIOD` and again on its mtime. True when
    /// the rows changed; a lock the dispatcher holds means "keep this frame's numbers".
    pub fn sync_accounts(&mut self, board: &mut Board, at: Instant) -> anyhow::Result<bool> {
        if !due(&mut self.accounts_due, at, ACCOUNTS_PERIOD) {
            return Ok(false);
        }
        let path = self.paths.accounts_state();
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if self.accounts_read && mtime == self.accounts_mtime {
            return Ok(false);
        }
        let Some(state) = try_load_state(&path)? else {
            return Ok(false);
        };
        self.accounts_mtime = mtime;
        self.accounts_read = true;
        board.accounts = rows_from_state(&self.cfg, &state);
        board.accounts_at = Some(OffsetDateTime::now_utc());
        Ok(true)
    }

    /// The same liveness closure `watch::App` uses, per tailed run.
    pub fn refresh_liveness(&self, board: &mut Board) {
        let now = board.now;
        for pane in &mut board.runs {
            let paths = pane.paths.clone();
            pane.refresh_liveness(&|id| liveness::is_ours(&paths.pidfile(id)), now);
        }
    }

    /// Waits for whichever tailed journal speaks first and folds what it said. True when
    /// anything was applied.
    pub async fn poll(&mut self, board: &mut Board) -> anyhow::Result<bool> {
        if board.runs.is_empty() {
            tokio::time::sleep(crate::journal::reader::POLL_INTERVAL).await;
            return Ok(false);
        }
        let (idx, update) = {
            let polls = board
                .runs
                .iter_mut()
                .enumerate()
                .map(|(i, p)| Box::pin(async move { (i, p.tailer.poll().await) }));
            let ((idx, update), _, _rest) = futures::future::select_all(polls).await;
            (idx, update?)
        };
        if update.restarted {
            board.runs[idx].rewind();
        }
        if update.lines.is_empty() && !update.restarted {
            return Ok(false);
        }
        board.runs[idx].apply(&update.lines);
        Ok(true)
    }

    /// Folds the journal that is already on disk, then tails from where the fold stopped.
    /// The tail is positioned before the replay reads, so a line appended in between is
    /// folded twice rather than lost; `RunView::apply` is idempotent and drops it.
    pub fn open_run(&self, run: RunId) -> anyhow::Result<RunPane> {
        let paths = self.run_paths(run);
        let journal = paths.journal();
        let mut tail = Tail::open(&journal)?;
        tail.seek_to_end();
        let mut pane = RunPane::new(paths, tail);
        if journal.is_file() {
            pane.seed(crate::journal::reader::replay(&journal, Replay::default())?);
        }
        Ok(pane)
    }

    /// Live runs newest first, capped at `MAX_RUNS`, plus the count that did not fit. The
    /// newest run that is NOT live is tailed too when there is room: its nodes can only
    /// land in `recent`, which is what keeps that section from being empty on a fresh board.
    fn live_runs(&self, board: &Board) -> anyhow::Result<(Vec<RunId>, usize)> {
        if let Scope::Pinned(run) = self.scope {
            return Ok((vec![run], 0));
        }
        let mut live = Vec::new();
        let mut hidden = 0usize;
        let mut newest_done = None;
        for run in self.candidates()?.into_iter().take(SCAN_LIMIT) {
            if self.alive(run, board) {
                match live.len() < MAX_RUNS {
                    true => live.push(run),
                    false => hidden += 1,
                }
            } else if newest_done.is_none() {
                newest_done = Some(run);
            }
        }
        if let Some(run) = newest_done
            && live.len() < MAX_RUNS
        {
            live.push(run);
        }
        Ok((live, hidden))
    }

    /// Newest first, deduplicated. ULIDs sort by their timestamp prefix, so one sort covers
    /// this repo and every repo the cross-repo index names.
    fn candidates(&self) -> anyhow::Result<Vec<RunId>> {
        let mut runs = self.paths.list_runs()?;
        if self.scope == Scope::All {
            match self.paths.load_runs_index() {
                Ok(index) => runs.extend(index.values().map(|e| e.run)),
                Err(e) => tracing::warn!("board: no cross-repo run index: {e:#}"),
            }
        }
        runs.sort_unstable();
        runs.dedup();
        runs.reverse();
        Ok(runs)
    }

    /// A pane the board already folded answers for itself; anything else pays for a probe
    /// only when a socket or a pidfile suggests there is something to find.
    fn alive(&self, run: RunId, board: &Board) -> bool {
        let paths = self.run_paths(run);
        let socket = paths.socket().exists();
        let alive = |id: NodeId| liveness::is_ours(&paths.pidfile(id));
        if let Some(pane) = board.pane(run) {
            return is_live(&pane.view, &alive, socket);
        }
        if !socket && !any_pid_ours(&paths) {
            return false;
        }
        match RunView::load(&paths.dir, false) {
            Ok(view) => is_live(&view, &alive, socket),
            Err(_) => false,
        }
    }

    fn run_paths(&self, run: RunId) -> RunPaths {
        let local = self.paths.run_paths(run);
        if self.scope != Scope::All || local.journal().is_file() {
            return local;
        }
        // Another repo's run: the index is the only thing that maps a run back to its dir.
        match self.paths.load_runs_index() {
            Ok(index) => match index.values().find(|e| e.run == run) {
                Some(e) => RunPaths {
                    run,
                    dir: e.dir.clone(),
                    sock_dir: self.paths.sock_dir(),
                },
                None => local,
            },
            Err(_) => local,
        }
    }
}

/// A journal with a live worker is one whose pidfile is still ours; nothing else needs the
/// fold, so this is the gate that keeps rediscovery off every finished run in the repo.
fn any_pid_ours(paths: &RunPaths) -> bool {
    let Ok(entries) = std::fs::read_dir(paths.dir.join("nodes")) else {
        return false;
    };
    entries.filter_map(|e| e.ok()).any(|e| {
        Utf8PathBuf::from_path_buf(e.path().join("pid"))
            .map(|p| liveness::is_ours(&p))
            .unwrap_or(false)
    })
}

/// True once per `period`, and the first time it is asked.
fn due(slot: &mut Option<Instant>, at: Instant, period: Duration) -> bool {
    match *slot {
        Some(last) if at.duration_since(last) < period => false,
        _ => {
            *slot = Some(at);
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::chat::tests_support as fx;
    use fs4::fs_std::FileExt;
    use std::fs::OpenOptions;

    fn tmp() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 tempdir");
        (dir, path)
    }

    fn write_lines(path: &Utf8Path, lines: &[JournalLine]) {
        let body: String = lines
            .iter()
            .map(|l| format!("{}\n", serde_json::to_string(l).expect("journal line")))
            .collect();
        std::fs::write(path, body).expect("writing the journal");
    }

    /// `swamp gc` and a replay both rewrite a journal, which makes a byte offset a lie.
    #[tokio::test]
    async fn a_truncated_journal_is_re_read_from_zero() {
        let (_dir, root) = tmp();
        let journal = root.join("journal.jsonl");
        let lines = fx::running();
        write_lines(&journal, &lines);

        let mut tail = Tail::open(&journal).expect("open");
        let first = tail.poll().await.expect("first poll");
        assert_eq!(first.lines.len(), lines.len());
        assert!(!first.restarted);
        assert!(tail.offset() > 0);

        // Truncate in place and write a shorter journal: same inode, smaller file.
        write_lines(&journal, &lines[..2]);
        let second = tail.poll().await.expect("second poll");
        assert!(second.restarted, "a shorter file is a rewrite");
        assert_eq!(
            second.lines.len(),
            2,
            "re-read from zero, not from the offset"
        );

        let mut pane = RunPane::new(
            crate::journal::paths::RunPaths {
                run: fx::run_id(),
                dir: root.clone(),
                sock_dir: root.clone(),
            },
            Tail::detached(journal.as_str()),
        );
        pane.apply(&first.lines);
        pane.rewind();
        pane.apply(&second.lines);
        assert_eq!(pane.view.nodes.len(), 1, "only what the new bytes hold");
    }

    /// `swamp replay` writes a temp file and renames it over the journal: the length can be
    /// the same or larger, and only the inode gives it away.
    #[tokio::test]
    async fn a_replaced_journal_is_re_read_from_zero() {
        let (_dir, root) = tmp();
        let journal = root.join("journal.jsonl");
        let lines = fx::running();
        write_lines(&journal, &lines[..2]);

        let mut tail = Tail::open(&journal).expect("open");
        assert_eq!(tail.poll().await.expect("first poll").lines.len(), 2);

        let replacement = root.join("journal.new");
        write_lines(&replacement, &lines);
        std::fs::rename(replacement.as_std_path(), journal.as_std_path()).expect("rename");

        let update = tail.poll().await.expect("second poll");
        assert!(update.restarted, "a new inode is a rewrite");
        assert_eq!(update.lines.len(), lines.len());
    }

    /// An append is not a rewrite: the offset has to survive it.
    #[tokio::test]
    async fn an_append_keeps_the_offset() {
        let (_dir, root) = tmp();
        let journal = root.join("journal.jsonl");
        let lines = fx::running();
        write_lines(&journal, &lines[..2]);

        let mut tail = Tail::open(&journal).expect("open");
        assert_eq!(tail.poll().await.expect("first poll").lines.len(), 2);

        write_lines(&journal, &lines);
        let update = tail.poll().await.expect("second poll");
        assert!(!update.restarted);
        assert_eq!(update.lines.len(), lines.len() - 2, "only the new lines");
    }

    #[tokio::test]
    async fn a_journal_that_does_not_exist_yet_is_not_an_error() {
        let (_dir, root) = tmp();
        let mut tail = Tail::open(&root.join("journal.jsonl")).expect("open");
        let update = tail.poll().await.expect("poll");
        assert!(update.lines.is_empty());
        assert!(!update.restarted);
    }

    /// The dispatcher holds the lock across a rename; the board must skip, never block.
    #[test]
    fn a_locked_state_file_skips_the_frame() {
        let (_dir, root) = tmp();
        let path = root.join("accounts.json");
        std::fs::write(&path, "{}").expect("state file");
        assert!(try_load_state(&path).expect("read").is_some());

        let held = OpenOptions::new()
            .create(true)
            .read(true)
            .truncate(false)
            .write(true)
            .open(path.with_extension("lock").as_std_path())
            .expect("lock file");
        FileExt::lock_exclusive(&held).expect("exclusive lock");
        assert!(
            try_load_state(&path).expect("read").is_none(),
            "contention must not block the board"
        );

        FileExt::unlock(&held).expect("unlock");
        assert!(try_load_state(&path).expect("read").is_some());
    }

    #[test]
    fn a_missing_state_file_is_an_empty_pool() {
        let (_dir, root) = tmp();
        let state = try_load_state(&root.join("accounts.json"))
            .expect("read")
            .expect("no contention");
        assert!(state.is_empty());
        let rows = rows_from_state(&fx::config(), &state);
        assert_eq!(rows.len(), 1, "the configured account still has a row");
        assert_eq!(rows[0].account.0, "main");
    }

    #[test]
    fn the_cadence_fires_once_per_period() {
        let mut slot = None;
        let start = Instant::now();
        assert!(due(&mut slot, start, DISCOVERY_PERIOD));
        assert!(!due(
            &mut slot,
            start + Duration::from_secs(1),
            DISCOVERY_PERIOD
        ));
        assert!(due(&mut slot, start + DISCOVERY_PERIOD, DISCOVERY_PERIOD));
    }
}
