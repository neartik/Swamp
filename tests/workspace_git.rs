//! WP5: worktree isolation, authoritative diffs, adoption. Every test is offline and uses
//! a throwaway repo from `tests/common`.

mod common;

use camino::{Utf8Path, Utf8PathBuf};
use std::process::Command;
use std::sync::Arc;
use swamp::config::{Config, WorkspaceCfg};
use swamp::error::SwampError;
use swamp::ids::{NodeId, RunId};
use swamp::journal::JournalHandle;
use swamp::journal::paths::{Paths, RunPaths};
use swamp::journal::record::JournalEvent;
use swamp::journal::writer::{FsyncPolicy, Writer};
use swamp::model::core::{ChangeKind, EvidenceSource, Tier};
use swamp::workspace::adopt::{AdoptResult, MergeStrategy, adopt};
use swamp::workspace::{Git, NodeWorktree, WorkspaceManager};
use tokio::sync::mpsc::UnboundedReceiver;

type Events = UnboundedReceiver<(Option<NodeId>, JournalEvent)>;

struct Harness {
    _repo_tmp: tempfile::TempDir,
    _home_tmp: tempfile::TempDir,
    repo: Utf8PathBuf,
    home: Utf8PathBuf,
    mgr: Arc<WorkspaceManager>,
    events: Events,
}

fn git_in(dir: &Utf8Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .current_dir(dir)
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"))
}

fn git_ok(dir: &Utf8Path, args: &[&str]) -> String {
    let out = git_in(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn utf8_tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = std::fs::canonicalize(tmp.path()).expect("canonicalize");
    (tmp, Utf8PathBuf::from_path_buf(path).expect("utf8 tempdir"))
}

fn config(home: &Utf8Path, tweak: impl FnOnce(&mut WorkspaceCfg)) -> Config {
    let mut workspace = WorkspaceCfg {
        root: Some(home.join("worktrees")),
        base: Some("HEAD".into()),
        branch_prefix: Some("swamp".into()),
        include_dirty: Some(false),
        require_clean: Some(true),
        commit_on_success: Some(true),
        commit_template: Some("swamp({tier}): {title}\n\nnode: {node}\nrun: {run}".into()),
        keep_on_failure: Some(true),
        ..Default::default()
    };
    tweak(&mut workspace);
    Config {
        version: 1,
        limits: Default::default(),
        brain: Default::default(),
        dispatch: Default::default(),
        cooldown: Default::default(),
        workspace,
        journal: Default::default(),
        providers: Default::default(),
        accounts: Vec::new(),
        tiers: Default::default(),
        failure: Default::default(),
        pricing: Default::default(),
        ui: Default::default(),
        profiles: Default::default(),
        sources: Vec::new(),
        warnings: Vec::new(),
    }
}

async fn manager(repo: &Utf8Path, home: &Utf8Path, cfg: Config) -> (Arc<WorkspaceManager>, Events) {
    let (tx, events) = tokio::sync::mpsc::unbounded_channel();
    let run = RunId::new();
    let dir = home.join("runs").join(run.to_string());
    std::fs::create_dir_all(&dir).expect("run dir");
    let paths_run = RunPaths {
        run,
        sock_dir: dir.clone(),
        dir,
    };
    let writer = Writer::open(&paths_run.journal(), FsyncPolicy::Never)
        .await
        .expect("journal writer");
    let journal = JournalHandle {
        run,
        tx,
        paths: Arc::new(paths_run),
        writer: Arc::new(tokio::sync::Mutex::new(writer)),
    };
    let paths = Paths {
        repo: repo.to_owned(),
        dot_swamp: repo.join(".swamp"),
        home_swamp: home.join(".swamp"),
    };
    let git = Git::discover(repo).await.expect("git repo");
    let mgr = WorkspaceManager::new(git, Arc::new(paths), Arc::new(cfg), journal)
        .await
        .expect("workspace manager");
    (mgr, events)
}

async fn harness_with(dirty: bool, tweak: impl FnOnce(&mut WorkspaceCfg)) -> Harness {
    let (repo_tmp, repo) = if dirty {
        common::tmp_repo_dirty()
    } else {
        common::tmp_repo()
    };
    git_ok(&repo, &["config", "user.name", "swamp tests"]);
    git_ok(&repo, &["config", "user.email", "tests@swamp.invalid"]);
    let (home_tmp, home) = utf8_tempdir();
    let cfg = config(&home, tweak);
    let (mgr, events) = manager(&repo, &home, cfg).await;
    Harness {
        _repo_tmp: repo_tmp,
        _home_tmp: home_tmp,
        repo,
        home,
        mgr,
        events,
    }
}

async fn harness() -> Harness {
    harness_with(false, |_| {}).await
}

impl Harness {
    fn commit(&self, name: &str, body: &str) {
        std::fs::write(self.repo.join(name), body).expect("write");
        git_ok(&self.repo, &["add", name]);
        git_ok(&self.repo, &["commit", "-q", "-m", &format!("add {name}")]);
    }

    /// A second manager over the same repo and worktree root, on a different run.
    async fn other_run(&self) -> Arc<WorkspaceManager> {
        let cfg = config(&self.home, |_| {});
        manager(&self.repo, &self.home, cfg).await.0
    }

    fn drain(&mut self) -> Vec<JournalEvent> {
        let mut out = Vec::new();
        while let Ok((_, ev)) = self.events.try_recv() {
            out.push(ev);
        }
        out
    }
}

fn write(wt: &NodeWorktree, name: &str, body: &str) {
    let path = wt.path.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent");
    }
    std::fs::write(path, body).expect("write in worktree");
}

// ---------------------------------------------------------------- git.rs

#[tokio::test]
async fn discover_refuses_a_directory_that_is_not_a_repo() {
    let (_tmp, dir) = utf8_tempdir();
    let err = Git::discover(&dir).await.expect_err("not a repo");
    assert!(matches!(err, SwampError::NotAGitRepo(p) if p == dir));
}

#[tokio::test]
async fn git_reports_its_version_and_cleanliness() {
    let (_tmp, repo) = common::tmp_repo();
    let git = Git::discover(&repo).await.unwrap();
    assert_eq!(git.root, repo);
    let (major, minor) = git.version().await.unwrap();
    assert!(
        major > 2 || (major == 2 && minor >= 5),
        "git {major}.{minor}"
    );
    assert!(git.is_clean().await.unwrap());
    std::fs::write(repo.join("README.md"), "changed\n").unwrap();
    assert!(!git.is_clean().await.unwrap());
}

// ---------------------------------------------------------------- create

#[tokio::test]
async fn create_checks_out_the_pinned_base_outside_the_repo() {
    let mut h = harness().await;
    let node = NodeId::new();
    let wt = h.mgr.create(node, 1).await.unwrap();

    assert!(
        !wt.path.starts_with(&h.repo),
        "{} must live outside {}",
        wt.path,
        h.repo
    );
    assert!(wt.path.starts_with(h.home.join("worktrees")));
    assert!(wt.path.is_dir());

    let head = git_ok(&h.repo, &["rev-parse", "HEAD"]).trim().to_owned();
    assert_eq!(wt.base, head);
    assert_eq!(git_ok(&wt.path, &["rev-parse", "HEAD"]).trim(), head);

    let expected = format!("swamp/{}/{}-1", h.mgr.journal.run.short(), node.short());
    assert_eq!(wt.branch, expected);
    assert_eq!(
        git_ok(&wt.path, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        expected
    );

    let events = h.drain();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, JournalEvent::WorktreeCreated { path, .. } if path == &wt.path)),
        "WorktreeCreated was not journaled"
    );
}

#[tokio::test]
async fn eight_concurrent_creates_all_succeed() {
    let h = harness().await;
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let mgr = h.mgr.clone();
        tasks.push(tokio::spawn(
            async move { mgr.create(NodeId::new(), 1).await },
        ));
    }
    let mut paths = Vec::new();
    for task in tasks {
        let wt = task.await.unwrap().expect("concurrent create");
        paths.push(wt.path);
    }
    assert_eq!(paths.len(), 8);
    paths.sort();
    paths.dedup();
    assert_eq!(paths.len(), 8, "worktree paths must be unique");
    assert_eq!(h.mgr.list().await.unwrap().len(), 8);
}

#[tokio::test]
async fn every_attempt_gets_its_own_worktree_and_branch() {
    let h = harness().await;
    let node = NodeId::new();
    let first = h.mgr.create(node, 1).await.unwrap();
    let second = h.mgr.create(node, 2).await.unwrap();
    assert_ne!(first.path, second.path);
    assert!(first.branch.ends_with("-1") && second.branch.ends_with("-2"));
    assert_eq!(first.base, second.base, "the base is pinned for the run");
}

#[tokio::test]
async fn seeds_link_directories_and_copy_files() {
    let (repo_tmp, repo) = common::tmp_repo();
    git_ok(&repo, &["config", "user.email", "tests@swamp.invalid"]);
    git_ok(&repo, &["config", "user.name", "swamp tests"]);
    std::fs::create_dir_all(repo.join("node_modules/pkg")).unwrap();
    std::fs::write(repo.join("node_modules/pkg/index.js"), "ok\n").unwrap();
    std::fs::write(repo.join(".env"), "TOKEN=1\n").unwrap();

    let (home_tmp, home) = utf8_tempdir();
    let cfg = config(&home, |w| {
        w.link = vec!["node_modules".into(), "missing_dir".into()];
        w.copy = vec![".env".into(), ".env.local".into()];
        w.require_clean = Some(false); // the seeds themselves are untracked
    });
    let (mgr, mut events) = manager(&repo, &home, cfg).await;
    let wt = mgr.create(NodeId::new(), 1).await.expect("create");

    let linked = wt.path.join("node_modules");
    assert!(linked.is_symlink(), "node_modules must be a symlink");
    assert_eq!(
        std::fs::read_to_string(linked.join("pkg/index.js")).unwrap(),
        "ok\n"
    );
    let copied = wt.path.join(".env");
    assert!(copied.is_file() && !copied.is_symlink());
    assert_eq!(std::fs::read_to_string(&copied).unwrap(), "TOKEN=1\n");

    let mut notes = Vec::new();
    while let Ok((_, ev)) = events.try_recv() {
        if let JournalEvent::Note { text, .. } = ev {
            notes.push(text);
        }
    }
    assert!(
        notes.iter().any(|n| n.contains("missing_dir")),
        "a missing link must warn, not fail: {notes:?}"
    );
    assert!(notes.iter().any(|n| n.contains(".env.local")));
    drop((repo_tmp, home_tmp));
}

#[tokio::test]
async fn post_create_runs_once_in_the_fresh_worktree() {
    let h = harness_with(false, |w| {
        w.post_create = Some("printf 'ran\\n' >> marker.txt".into());
    })
    .await;
    let wt = h.mgr.create(NodeId::new(), 1).await.expect("create");
    let marker = std::fs::read_to_string(wt.path.join("marker.txt")).expect("marker");
    assert_eq!(marker, "ran\n");
}

#[tokio::test]
async fn a_failing_post_create_fails_create_with_its_output() {
    let h = harness_with(false, |w| {
        w.post_create = Some("echo 'setup is broken' >&2; exit 3".into());
    })
    .await;
    let err = h
        .mgr
        .create(NodeId::new(), 1)
        .await
        .expect_err("post_create must fail create");
    let msg = format!("{err:#}");
    assert!(msg.contains("post_create"), "{msg}");
    assert!(msg.contains("exit 3"), "{msg}");
    assert!(msg.contains("setup is broken"), "{msg}");
}

// ---------------------------------------------------------------- finalize

#[tokio::test]
async fn finalize_reports_git_sourced_changes_and_a_patch_that_applies() {
    let mut h = harness().await;
    let wt = h.mgr.create(NodeId::new(), 1).await.unwrap();
    write(&wt, "README.md", "swamp test repo\nplus a line\n");
    write(&wt, "src/new.rs", "fn main() {}\n");

    let work = h
        .mgr
        .finalize(&wt, "add a line", Tier::Mid)
        .await
        .unwrap()
        .expect("a work result");
    assert!(!work.empty);
    assert_eq!(work.branch, wt.branch);
    assert_eq!(work.insertions, 2);
    assert_eq!(work.deletions, 0);
    assert!(work.patch.is_file());

    let events = h.drain();
    let files = events
        .iter()
        .find_map(|e| match e {
            JournalEvent::NodeFiles { files } => Some(files.clone()),
            _ => None,
        })
        .expect("NodeFiles");
    assert_eq!(files.len(), 2);
    assert!(files.iter().all(|f| f.source == EvidenceSource::Git));
    let readme = files
        .iter()
        .find(|f| f.path == "README.md")
        .expect("README change");
    assert_eq!(readme.kind, ChangeKind::Modify);
    assert_eq!((readme.added, readme.removed), (1, 0));
    let added = files.iter().find(|f| f.path == "src/new.rs").unwrap();
    assert_eq!(added.kind, ChangeKind::Add);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, JournalEvent::DiffCaptured { files, .. } if *files == 2))
    );

    let check = git_in(&h.repo, &["apply", "--check", work.patch.as_str()]);
    assert!(
        check.status.success(),
        "git apply --check rejected the patch: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    assert_eq!(
        git_ok(&wt.path, &["log", "-1", "--pretty=%s"]).trim(),
        "swamp(mid): add a line"
    );
}

#[tokio::test]
async fn finalize_sees_a_file_no_edit_tool_ever_touched() {
    let h = harness().await;
    let wt = h.mgr.create(NodeId::new(), 1).await.unwrap();
    // A shell heredoc: git is authoritative, the event stream would never report this.
    let script = format!(
        "cat > '{}/heredoc.txt' <<'EOF'\nwritten by a shell\nEOF\n",
        wt.path
    );
    let out = Command::new("sh").arg("-c").arg(&script).output().unwrap();
    assert!(out.status.success());

    let work = h
        .mgr
        .finalize(&wt, "heredoc", Tier::Low)
        .await
        .unwrap()
        .unwrap();
    let patch = std::fs::read_to_string(&work.patch).unwrap();
    assert!(patch.contains("heredoc.txt"), "{patch}");
    assert!(!work.empty);
}

#[tokio::test]
async fn finalize_without_changes_is_empty_and_commits_nothing() {
    let h = harness().await;
    let wt = h.mgr.create(NodeId::new(), 1).await.unwrap();
    let before = git_ok(&wt.path, &["rev-parse", "HEAD"]).trim().to_owned();

    let work = h
        .mgr
        .finalize(&wt, "nothing", Tier::Low)
        .await
        .unwrap()
        .expect("a work result");
    assert!(work.empty);
    assert_eq!(work.insertions, 0);
    assert_eq!(work.deletions, 0);
    assert_eq!(git_ok(&wt.path, &["rev-parse", "HEAD"]).trim(), before);
    assert_eq!(std::fs::read_to_string(&work.patch).unwrap(), "");
}

#[tokio::test]
async fn renames_and_deletes_are_classified() {
    let mut h = harness().await;
    h.commit("keep.rs", "fn keep() {}\n");
    h.commit("gone.rs", "fn gone() {}\n");
    let wt = h.mgr.create(NodeId::new(), 1).await.unwrap();
    git_ok(&wt.path, &["mv", "keep.rs", "moved.rs"]);
    git_ok(&wt.path, &["rm", "-q", "gone.rs"]);

    h.mgr
        .finalize(&wt, "shuffle", Tier::Mid)
        .await
        .unwrap()
        .unwrap();
    let files = h
        .drain()
        .into_iter()
        .find_map(|e| match e {
            JournalEvent::NodeFiles { files } => Some(files),
            _ => None,
        })
        .unwrap();
    let moved = files.iter().find(|f| f.path == "moved.rs").expect("rename");
    assert_eq!(moved.kind, ChangeKind::Rename);
    let gone = files.iter().find(|f| f.path == "gone.rs").expect("delete");
    assert_eq!(gone.kind, ChangeKind::Delete);
}

#[tokio::test]
async fn paths_with_spaces_and_unicode_survive() {
    let mut h = harness().await;
    let wt = h.mgr.create(NodeId::new(), 1).await.unwrap();
    write(&wt, "a file with spaces.txt", "one\n");
    write(&wt, "héllo ünicode/naïve.txt", "two\n");

    h.mgr
        .finalize(&wt, "unicode", Tier::Low)
        .await
        .unwrap()
        .unwrap();
    let files = h
        .drain()
        .into_iter()
        .find_map(|e| match e {
            JournalEvent::NodeFiles { files } => Some(files),
            _ => None,
        })
        .unwrap();
    let paths: Vec<_> = files.iter().map(|f| f.path.as_str()).collect();
    assert!(paths.contains(&"a file with spaces.txt"), "{paths:?}");
    assert!(paths.contains(&"héllo ünicode/naïve.txt"), "{paths:?}");
}

// ---------------------------------------------------------------- base

#[tokio::test]
async fn include_dirty_bases_on_a_stash_that_carries_the_change() {
    let h = harness_with(true, |_| {}).await;
    let head = git_ok(&h.repo, &["rev-parse", "HEAD"]).trim().to_owned();
    let base = h.mgr.base_commit(None, true).await.unwrap();
    assert_ne!(base, head, "a dirty tree must base on a stash commit");
    let blob = git_ok(&h.repo, &["show", &format!("{base}:README.md")]);
    assert!(blob.contains("dirty"), "{blob}");
}

#[tokio::test]
async fn a_dirty_tree_is_refused_when_clean_is_required() {
    let h = harness_with(true, |_| {}).await;
    let err = h
        .mgr
        .create(NodeId::new(), 1)
        .await
        .expect_err("dirty tree must be refused");
    assert!(
        matches!(
            err.downcast_ref::<SwampError>(),
            Some(SwampError::DirtyTree)
        ),
        "{err:#}"
    );
}

#[tokio::test]
async fn an_explicit_base_is_resolved_to_a_commit() {
    let h = harness().await;
    let first = git_ok(&h.repo, &["rev-parse", "HEAD"]).trim().to_owned();
    h.commit("later.rs", "fn later() {}\n");
    let base = h.mgr.base_commit(Some(&first[..8]), false).await.unwrap();
    assert_eq!(base, first);
}

// ---------------------------------------------------------------- remove and prune

#[tokio::test]
async fn prune_drops_finished_worktrees_and_keeps_dirty_ones() {
    let h = harness().await;
    let clean = h.mgr.create(NodeId::new(), 1).await.unwrap();
    let dirty = h.mgr.create(NodeId::new(), 1).await.unwrap();
    h.mgr.finalize(&clean, "done", Tier::Low).await.unwrap();
    write(&dirty, "unfinished.txt", "work in progress\n");

    let next = h.other_run().await;
    let removed = next.prune().await.unwrap();
    assert_eq!(removed, 1, "only the clean worktree is prunable");
    assert!(!clean.path.exists());
    assert!(dirty.path.exists(), "uncommitted work must survive prune");

    next.remove(&dirty, false)
        .await
        .expect_err("a dirty worktree needs force");
    next.remove(&dirty, true).await.expect("forced removal");
    assert!(!dirty.path.exists());
}

#[tokio::test]
async fn list_rebuilds_worktrees_from_their_metadata() {
    let h = harness().await;
    let node = NodeId::new();
    let wt = h.mgr.create(node, 3).await.unwrap();
    let listed = h.mgr.list().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].node, node);
    assert_eq!(listed[0].branch, wt.branch);
    assert_eq!(listed[0].base, wt.base);
    assert_eq!(listed[0].path, wt.path);
}

#[tokio::test]
async fn the_shared_lock_admits_one_holder_at_a_time() {
    let h = harness().await;
    let guard = h.mgr.shared_lock().await;
    let mgr = h.mgr.clone();
    let second = tokio::spawn(async move { mgr.shared_lock().await });
    tokio::task::yield_now().await;
    assert!(!second.is_finished(), "the second holder must wait");
    drop(guard);
    second.await.unwrap();
}

// ---------------------------------------------------------------- adopt

async fn work_from(
    h: &Harness,
    file: &str,
    body: &str,
    title: &str,
) -> swamp::model::node::WorkResultRef {
    let wt = h.mgr.create(NodeId::new(), 1).await.unwrap();
    write(&wt, file, body);
    h.mgr
        .finalize(&wt, title, Tier::Mid)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn apply_lands_the_patch_in_a_clean_tree_and_refuses_a_dirty_one() {
    let h = harness().await;
    let work = work_from(&h, "README.md", "swamp test repo\nadopted\n", "adopt me").await;
    let git = Git::discover(&h.repo).await.unwrap();

    let dry = adopt(&git, &work, MergeStrategy::Apply, None, false, true)
        .await
        .unwrap();
    assert_eq!(dry, AdoptResult::Clean { commit: None });
    assert!(git.is_clean().await.unwrap(), "a dry run changes nothing");

    let result = adopt(&git, &work, MergeStrategy::Apply, None, false, false)
        .await
        .unwrap();
    assert_eq!(result, AdoptResult::Clean { commit: None });
    assert!(
        std::fs::read_to_string(h.repo.join("README.md"))
            .unwrap()
            .contains("adopted")
    );

    let refused = adopt(&git, &work, MergeStrategy::Apply, None, false, false)
        .await
        .unwrap();
    assert!(
        matches!(&refused, AdoptResult::Rejected { reason } if reason.contains("dirty")),
        "{refused:?}"
    );
}

#[tokio::test]
async fn merge_reports_conflicting_paths_and_leaves_an_abortable_state() {
    let h = harness().await;
    let mine = work_from(&h, "README.md", "swamp test repo\nmine\n", "mine").await;
    let theirs = work_from(&h, "README.md", "swamp test repo\ntheirs\n", "theirs").await;
    let git = Git::discover(&h.repo).await.unwrap();

    let first = adopt(&git, &mine, MergeStrategy::Merge, None, false, false)
        .await
        .unwrap();
    assert!(
        matches!(first, AdoptResult::Clean { commit: Some(_) }),
        "{first:?}"
    );

    let status_before = git_ok(&h.repo, &["status", "--porcelain"]);
    let dry = adopt(&git, &theirs, MergeStrategy::Merge, None, false, true)
        .await
        .unwrap();
    let AdoptResult::Conflicted { paths } = &dry else {
        panic!("expected a conflict, got {dry:?}");
    };
    assert_eq!(paths, &[Utf8PathBuf::from("README.md")]);
    assert_eq!(
        git_ok(&h.repo, &["status", "--porcelain"]),
        status_before,
        "a dry run must not touch the repo"
    );

    let real = adopt(&git, &theirs, MergeStrategy::Merge, None, false, false)
        .await
        .unwrap();
    assert_eq!(real, dry, "the dry run predicted the real conflict");
    assert!(
        h.repo.join(".git/MERGE_HEAD").exists(),
        "the merge must be abortable"
    );
    git_ok(&h.repo, &["merge", "--abort"]);
    assert!(git.is_clean().await.unwrap());
}

#[tokio::test]
async fn cherry_pick_lands_a_single_commit() {
    let h = harness().await;
    let work = work_from(&h, "picked.txt", "one commit\n", "pick me").await;
    let git = Git::discover(&h.repo).await.unwrap();

    let dry = adopt(&git, &work, MergeStrategy::CherryPick, None, false, true)
        .await
        .unwrap();
    assert_eq!(dry, AdoptResult::Clean { commit: None });

    let result = adopt(&git, &work, MergeStrategy::CherryPick, None, false, false)
        .await
        .unwrap();
    let AdoptResult::Clean { commit: Some(sha) } = result else {
        panic!("expected a commit, got {result:?}");
    };
    assert_eq!(sha, git_ok(&h.repo, &["rev-parse", "HEAD"]).trim());
    assert!(h.repo.join("picked.txt").is_file());
}

#[tokio::test]
async fn an_empty_result_is_never_adopted() {
    let h = harness().await;
    let wt = h.mgr.create(NodeId::new(), 1).await.unwrap();
    let work = h
        .mgr
        .finalize(&wt, "nothing", Tier::Low)
        .await
        .unwrap()
        .unwrap();
    let git = Git::discover(&h.repo).await.unwrap();
    let result = adopt(&git, &work, MergeStrategy::Apply, None, false, false)
        .await
        .unwrap();
    assert!(
        matches!(&result, AdoptResult::Rejected { reason } if reason.contains("no changes")),
        "{result:?}"
    );
}

#[tokio::test]
async fn adopting_into_a_missing_branch_is_rejected() {
    let h = harness().await;
    let work = work_from(&h, "README.md", "swamp test repo\nbranch\n", "branch").await;
    let git = Git::discover(&h.repo).await.unwrap();
    let result = adopt(
        &git,
        &work,
        MergeStrategy::Apply,
        Some("no-such-branch"),
        false,
        false,
    )
    .await
    .unwrap();
    assert!(
        matches!(&result, AdoptResult::Rejected { reason } if reason.contains("no-such-branch")),
        "{result:?}"
    );
}
