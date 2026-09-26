//! WP2: journal writer/reader round trips, torn-tail repair, fold determinism, paths.

mod common;

use common::journal::*;

use camino::Utf8PathBuf;
use proptest::prelude::*;
use std::str::FromStr;
use std::sync::Arc;
use swamp::ids::{CallSeq, DispatchId, NodeId, RunId};
use swamp::journal::fold::{LlmDigest, Projection, RunView, Scope};
use swamp::journal::paths::{Paths, RunPaths};
use swamp::journal::raw::{RawSink, Redactor};
use swamp::journal::reader::{Tailer, replay};
use swamp::journal::record::{JournalEvent, JournalLine, NoteAuthor};
use swamp::journal::writer::FsyncPolicy;
use swamp::journal::{Journal, JournalHandle};
use swamp::model::core::{
    AccountId, ChangeKind, Cost, CostBasis, EvidenceSource, FileChange, NodeState, Usage,
};
use swamp::model::dispatch::{DispatchCounts, DispatchState, NodeTransition, Phase};
use swamp::model::failure::{Detector, Failure};

fn note(text: &str) -> JournalEvent {
    JournalEvent::Note {
        author: NoteAuthor::Swamp,
        text: text.to_owned(),
    }
}

fn worker_error() -> NodeState {
    NodeState::Failed {
        failure: Failure::WorkerError {
            subtype: "error_during_execution".into(),
            detail: "2 tests still failing".into(),
        },
    }
}

struct Sandbox {
    _tmp: tempfile::TempDir,
    paths: RunPaths,
}

fn sandbox() -> Sandbox {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("utf8 tempdir");
    Sandbox {
        _tmp: tmp,
        paths: RunPaths {
            run: rid(0),
            dir: dir.join("runs").join(rid(0).to_string()),
            sock_dir: dir.join("sock"),
        },
    }
}

/// Collects every folded line so a replay can be compared against what was emitted.
#[derive(Default)]
struct Collect(Vec<JournalLine>);

impl Projection for Collect {
    type Out = Vec<JournalLine>;
    fn apply(&mut self, l: &JournalLine) {
        self.0.push(l.clone());
    }
    fn finish(self) -> Vec<JournalLine> {
        self.0
    }
}

async fn open(
    paths: &RunPaths,
    policy: FsyncPolicy,
) -> (JournalHandle, tokio::task::JoinHandle<()>) {
    Journal::open(paths.clone(), policy, &[])
        .await
        .expect("journal open")
}

// ---------------------------------------------------------------- writer and reader

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_emits_round_trip_with_a_dense_sequence() {
    let sb = sandbox();
    let (handle, task) = open(&sb.paths, FsyncPolicy::Barrier).await;

    let mut set = tokio::task::JoinSet::new();
    for t in 0..8u32 {
        let h = handle.clone();
        set.spawn(async move {
            for i in 0..125u32 {
                let tag = format!("{t}-{i}");
                match i % 3 {
                    0 => h.emit(None, note(&tag)),
                    1 => h.emit(
                        Some(nid(1)),
                        JournalEvent::NodeUsage {
                            usage: Usage::default(),
                            cost: None,
                        },
                    ),
                    _ => h.emit(
                        Some(nid(2)),
                        spawned(nid(2), nid(2), Some(nid(1)), t.max(1)),
                    ),
                }
            }
        });
    }
    while set.join_next().await.is_some() {}
    drop(handle);
    task.await.expect("writer task");

    let lines = replay(&sb.paths.journal(), Collect::default()).expect("replay");
    assert_eq!(lines.len(), 1000);
    for (i, l) in lines.iter().enumerate() {
        assert_eq!(l.seq, i as u64, "sequence is dense and strictly increasing");
    }

    let notes: std::collections::BTreeSet<String> = lines
        .iter()
        .filter_map(|l| match &l.event {
            JournalEvent::Note { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    let expected: std::collections::BTreeSet<String> = (0..8u32)
        .flat_map(|t| {
            (0..125u32)
                .filter(|i| i % 3 == 0)
                .map(move |i| format!("{t}-{i}"))
        })
        .collect();
    assert_eq!(notes, expected, "every emitted event survives exactly once");
}

/// `swamp cancel` appends to a journal whose writer lives in another process. The owner has
/// to continue the sequence past that line, or the fold drops its next one as a replay.
#[tokio::test]
async fn a_line_appended_by_another_process_keeps_the_sequence_dense() {
    let sb = sandbox();
    let (handle, task) = open(&sb.paths, FsyncPolicy::Barrier).await;
    handle
        .emit_durable(None, note("owner 0"))
        .await
        .expect("owner line");
    let theirs = swamp::journal::writer::append_shared(
        &sb.paths.journal(),
        sb.paths.run,
        None,
        note("from another process"),
    )
    .await
    .expect("shared append");
    assert_eq!(theirs, 1);
    handle
        .emit_durable(None, note("owner 1"))
        .await
        .expect("owner line");
    drop(handle);
    task.await.expect("writer task");

    let lines = replay(&sb.paths.journal(), Collect::default()).expect("replay");
    let texts: Vec<(u64, String)> = lines
        .iter()
        .filter_map(|l| match &l.event {
            JournalEvent::Note { text, .. } => Some((l.seq, text.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        vec![
            (0, "owner 0".to_owned()),
            (1, "from another process".to_owned()),
            (2, "owner 1".to_owned()),
        ]
    );
}

#[tokio::test]
async fn emit_after_the_writer_task_is_gone_only_logs() {
    let sb = sandbox();
    let (handle, task) = open(&sb.paths, FsyncPolicy::Always).await;
    handle.emit(None, note("before"));
    task.abort();
    let _ = task.await;

    handle.emit(None, note("after"));
    handle.emit(None, note("after too"));

    let text = std::fs::read_to_string(sb.paths.journal()).expect("journal");
    assert!(!text.contains("after too"));
}

#[tokio::test]
async fn emit_durable_is_on_disk_when_it_returns() {
    let sb = sandbox();
    let (handle, task) = open(&sb.paths, FsyncPolicy::Never).await;
    let seq = handle
        .emit_durable(None, note("durable"))
        .await
        .expect("durable emit");
    assert_eq!(seq, 0);

    let text = std::fs::read_to_string(sb.paths.journal()).expect("journal");
    assert!(text.contains("durable"), "line must be on disk already");
    drop(handle);
    task.await.expect("writer task");
}

/// A durable line never overtakes the lines emitted before it, or a settled dispatch can reach
/// disk ahead of the task transitions it summarises.
#[tokio::test]
async fn a_durable_line_lands_behind_every_earlier_emit() {
    let sb = sandbox();
    let (handle, task) = open(&sb.paths, FsyncPolicy::Never).await;
    for i in 0..50 {
        handle.emit(None, note(&format!("queued-{i}")));
    }
    let seq = handle
        .emit_durable(None, note("durable"))
        .await
        .expect("durable emit");
    assert_eq!(seq, 50);
    let text = std::fs::read_to_string(sb.paths.journal()).expect("journal");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 51, "every earlier emit is on disk too");
    assert!(lines[49].contains("queued-49") && lines[50].contains("durable"));
    drop(handle);
    task.await.expect("writer task");
}

/// The child half of `emit_durable_survives_sigkill`: writes durably, then dies hard.
#[tokio::test]
#[ignore]
async fn durable_emit_child() {
    let Ok(dir) = std::env::var("SWAMP_TEST_DURABLE_DIR") else {
        return;
    };
    let paths = RunPaths {
        run: rid(0),
        sock_dir: Utf8PathBuf::from(&dir),
        dir: Utf8PathBuf::from(dir),
    };
    let (handle, _task) = Journal::open(paths, FsyncPolicy::Never, &[])
        .await
        .expect("journal open");
    handle
        .emit_durable(None, note("survives-sigkill"))
        .await
        .expect("durable emit");
    nix::sys::signal::kill(nix::unistd::Pid::this(), nix::sys::signal::Signal::SIGKILL)
        .expect("sigkill self");
    unreachable!("the process is gone");
}

#[test]
fn emit_durable_survives_sigkill() {
    use std::os::unix::process::ExitStatusExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = Utf8PathBuf::from_path_buf(tmp.path().join("run")).expect("utf8 tempdir");
    let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "durable_emit_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("SWAMP_TEST_DURABLE_DIR", dir.as_str())
        .output()
        .expect("spawn child");

    assert_eq!(
        out.status.signal(),
        Some(9),
        "child must die by SIGKILL, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = std::fs::read_to_string(dir.join("journal.jsonl")).expect("journal");
    assert!(text.contains("survives-sigkill"));
    assert!(text.ends_with('\n'));
}

#[tokio::test]
async fn a_torn_tail_is_repaired_and_the_sequence_continues() {
    let sb = sandbox();
    let (handle, task) = open(&sb.paths, FsyncPolicy::Always).await;
    for i in 0..3 {
        handle
            .emit_durable(None, note(&format!("line-{i}")))
            .await
            .expect("durable emit");
    }
    drop(handle);
    task.await.expect("writer task");

    let journal = sb.paths.journal();
    let good = std::fs::read_to_string(&journal).expect("journal");
    let torn = format!("{good}{{\"seq\":3,\"at\":\"2024");
    std::fs::write(&journal, &torn).expect("write torn journal");

    // replay tolerates the partial line instead of failing.
    let lines = replay(&journal, Collect::default()).expect("replay a damaged journal");
    assert_eq!(lines.len(), 3);

    let (handle, task) = open(&sb.paths, FsyncPolicy::Always).await;
    let seq = handle
        .emit_durable(None, note("after-repair"))
        .await
        .expect("durable emit");
    assert_eq!(seq, 3, "sequence continues rather than restarting");
    drop(handle);
    task.await.expect("writer task");

    let repaired = std::fs::read_to_string(&journal).expect("journal");
    assert!(!repaired.contains("\"at\":\"2024"), "torn line was dropped");
    let lines = replay(&journal, Collect::default()).expect("replay");
    assert_eq!(lines.len(), 4);
    assert_eq!(
        lines.iter().map(|l| l.seq).collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
}

#[tokio::test]
async fn tailer_yields_history_then_new_lines_and_never_a_partial_one() {
    use std::io::Write;

    let sb = sandbox();
    let (handle, task) = open(&sb.paths, FsyncPolicy::Always).await;
    handle.emit_durable(None, note("one")).await.unwrap();
    handle.emit_durable(None, note("two")).await.unwrap();

    let journal = sb.paths.journal();
    let mut tailer = Tailer::open(&journal).expect("tailer");
    let first = tailer.poll().await.expect("poll");
    assert_eq!(first.len(), 2, "historical lines come first");

    handle.emit_durable(None, note("three")).await.unwrap();
    let second = tailer.poll().await.expect("poll");
    assert_eq!(second.len(), 1);
    drop(handle);
    task.await.expect("writer task");

    // A line written in two syscalls is returned once, complete, after the newline lands.
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&journal)
        .expect("append");
    let half = serde_json::to_string(&line(9, None, note("split"))).expect("encode");
    let (a, b) = half.split_at(half.len() / 2);
    f.write_all(a.as_bytes()).unwrap();
    f.flush().unwrap();
    assert!(tailer.poll().await.expect("poll").is_empty());

    f.write_all(b.as_bytes()).unwrap();
    f.write_all(b"\n").unwrap();
    f.flush().unwrap();
    let third = tailer.poll().await.expect("poll");
    assert_eq!(third.len(), 1);
    assert!(matches!(&third[0].event, JournalEvent::Note { text, .. } if text == "split"));
}

// ---------------------------------------------------------------- redaction

#[tokio::test]
async fn secrets_are_masked_in_the_journal_and_the_raw_sink() {
    let key = "sk-ant-api03-ZZZZZZZZZZZZZZZZZZZZZZZZ";
    let token = "tok_5f4dcc3b5aa765d61d8327deb882cf99";
    let secret_line = format!("ANTHROPIC_API_KEY={key} Authorization: Bearer {token}");
    let patterns = vec![
        r"(?i)(api[_-]?key|authorization|secret|password)\s*[:=]\s*\S+".to_owned(),
        r"(?i)bearer\s+[A-Za-z0-9._\-]+".to_owned(),
        r"sk-[A-Za-z0-9_\-]{20,}".to_owned(),
    ];

    let sb = sandbox();
    let (handle, task) = Journal::open(sb.paths.clone(), FsyncPolicy::Always, &patterns)
        .await
        .expect("journal open");
    handle
        .emit_durable(None, note(&secret_line))
        .await
        .expect("durable emit");
    drop(handle);
    task.await.expect("writer task");

    let redactor = Arc::new(Redactor::new(&patterns).expect("redactor"));
    let node = nid(3);
    let mut sink = RawSink::open(&sb.paths, node, redactor)
        .await
        .expect("raw sink");
    sink.noise(&secret_line).await;
    sink.stderr_line(&secret_line).await;
    sink.flush().await.expect("flush");

    for path in [
        sb.paths.journal(),
        sb.paths.noise(node),
        sb.paths.stderr(node),
    ] {
        let text = std::fs::read_to_string(&path).expect("read sink");
        assert!(!text.contains(key), "{path} leaked the api key");
        assert!(!text.contains(token), "{path} leaked the bearer token");
        assert!(text.contains("redacted"), "{path} has no mask");
    }
    // Masking must not corrupt the JSON: a greedy pattern must never eat a closing quote.
    let lines = replay(&sb.paths.journal(), Collect::default()).expect("replay");
    assert_eq!(lines.len(), 1);
}

#[test]
fn a_redactor_without_patterns_borrows_its_input() {
    let r = Redactor::new(&[]).expect("redactor");
    assert!(matches!(
        r.apply("nothing to hide"),
        std::borrow::Cow::Borrowed(_)
    ));
}

// ---------------------------------------------------------------- fold

fn fold(lines: &[JournalLine]) -> RunView {
    let mut v = RunView::default();
    for l in lines {
        v.apply(l);
    }
    v
}

fn digest(v: &RunView) -> String {
    format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{}|{}|{}|{:?}",
        v.nodes,
        v.roots,
        v.children,
        v.by_logical,
        v.dispatches,
        v.tasks,
        v.transitions,
        v.call_seq,
        v.accounts,
        v.totals,
        v.cost_usd,
        v.cost_complete,
        v.last_seq,
        v.tree()
            .iter()
            .map(|r| (r.logical, r.depth, r.attempts.clone()))
            .collect::<Vec<_>>()
    )
}

fn generated(ops: &[(u8, usize)]) -> Vec<JournalLine> {
    let mut out = vec![line(
        0,
        None,
        JournalEvent::RunStarted {
            swamp_version: "0.1.0".into(),
            schema: 1,
            argv: vec!["swamp".into()],
            cwd: Utf8PathBuf::from("/repo"),
            repo: Some(Utf8PathBuf::from("/repo")),
            base: Some("HEAD".into()),
            config_sha256: "abc".into(),
            task: Some("t".into()),
        },
    )];
    for (i, (kind, idx)) in ops.iter().enumerate() {
        let seq = i as u64 + 1;
        let node = nid(idx + 10);
        let event = match kind % 6 {
            0 => spawned(node, node, Some(nid(1)), 1),
            1 => JournalEvent::NodeUsage {
                usage: Usage {
                    input_tokens: 7,
                    cached_input_tokens: 1,
                    cache_write_tokens: 2,
                    output_tokens: 3,
                    reasoning_tokens: 0,
                },
                cost: Some(Cost {
                    usd: 0.5,
                    basis: CostBasis::Estimated,
                }),
            },
            2 => finished(NodeState::Succeeded, Some(0.25)),
            3 => JournalEvent::NodeFiles {
                files: vec![FileChange {
                    path: Utf8PathBuf::from("src/lib.rs"),
                    kind: ChangeKind::Modify,
                    added: 1,
                    removed: 0,
                    source: EvidenceSource::Git,
                }],
            },
            4 => note("n"),
            _ => JournalEvent::AccountHealth {
                account: AccountId("main".into()),
                health: swamp::dispatch::account::Health::Degraded,
                cooldown_until: None,
                quota: None,
                quota_observed_at: None,
                quota_source: None,
            },
        };
        out.push(line(seq, Some(node), event));
    }
    out
}

proptest! {
    #[test]
    fn folding_a_prefix_in_halves_matches_folding_it_whole(
        ops in prop::collection::vec((0u8..6, 0usize..4), 0..30)
    ) {
        let lines = generated(&ops);
        let whole = digest(&fold(&lines));
        for split in 0..=lines.len() {
            let mut v = RunView::default();
            for l in &lines[..split] { v.apply(l); }
            for l in &lines[split..] { v.apply(l); }
            prop_assert_eq!(digest(&v), whole.clone(), "split at {}", split);
        }
    }

    #[test]
    fn folding_the_same_prefix_twice_changes_nothing(
        ops in prop::collection::vec((0u8..6, 0usize..4), 0..30)
    ) {
        let lines = generated(&ops);
        let once = digest(&fold(&lines));
        let mut twice = RunView::default();
        for l in &lines { twice.apply(l); }
        for l in &lines { twice.apply(l); }
        prop_assert_eq!(digest(&twice), once);
    }
}

#[test]
fn a_retry_chain_collapses_into_one_row_with_every_attempt() {
    let root = nid(1);
    let logical = nid(2);
    let mut lines = vec![line(0, Some(root), spawned(root, root, None, 1))];
    for (i, attempt) in [nid(2), nid(3), nid(4)].into_iter().enumerate() {
        let mut rec = record(attempt, logical, Some(root), i as u32 + 1);
        rec.retry_of = (i > 0).then(|| nid(i + 1));
        lines.push(line(
            i as u64 + 1,
            Some(attempt),
            JournalEvent::NodeSpawned {
                node: Box::new(rec),
            },
        ));
    }
    let view = fold(&lines);
    let tree = view.tree();
    assert_eq!(tree.len(), 2, "root plus one collapsed row");
    assert_eq!(tree[0].depth, 0);
    assert_eq!(tree[1].depth, 1);
    assert_eq!(tree[1].logical, logical);
    assert_eq!(tree[1].attempts, vec![nid(2), nid(3), nid(4)]);
}

#[test]
fn mark_orphans_only_touches_running_nodes_whose_process_is_gone() {
    let dead = nid(1);
    let live = nid(2);
    let mut lines = Vec::new();
    for (i, id) in [dead, live].into_iter().enumerate() {
        lines.push(line(i as u64 * 2, Some(id), spawned(id, id, None, 1)));
        lines.push(line(
            i as u64 * 2 + 1,
            Some(id),
            JournalEvent::ProcessStarted {
                pid: 4000 + i as i32,
                pgid: 4000 + i as i32,
                argv: vec!["claude".into()],
                env_overrides: Default::default(),
                cwd: Utf8PathBuf::from("/repo"),
            },
        ));
    }
    let mut view = fold(&lines);
    view.mark_orphans(&|n| n == live);
    assert!(matches!(
        view.nodes[&dead].state,
        NodeState::Orphaned { pid: 4000, .. }
    ));
    assert!(matches!(view.nodes[&live].state, NodeState::Running { .. }));
}

#[test]
fn an_unknown_cost_makes_the_total_incomplete_without_zeroing_it() {
    let reported = nid(1);
    let unknown = nid(2);
    let lines = vec![
        line(0, Some(reported), spawned(reported, reported, None, 1)),
        line(1, Some(unknown), spawned(unknown, unknown, None, 1)),
        line(
            2,
            Some(reported),
            finished(NodeState::Succeeded, Some(1.84)),
        ),
        line(3, Some(unknown), finished(NodeState::Succeeded, None)),
    ];
    let view = fold(&lines);
    let totals = view.totals();
    assert!(!totals.cost_complete);
    assert!((totals.cost_usd - 1.84).abs() < f64::EPSILON);
    assert_eq!(totals.usage.input_tokens, 200);
    assert_eq!(totals.nodes, 2);
}

#[test]
fn a_failed_node_is_counted_and_the_run_is_marked_finished() {
    let a = nid(1);
    let lines = vec![
        line(0, Some(a), spawned(a, a, None, 1)),
        line(1, Some(a), finished(worker_error(), Some(0.5))),
        line(
            2,
            None,
            JournalEvent::RunFinished {
                state: worker_error(),
                nodes: 1,
                usage: Usage::default(),
                cost_usd: Some(0.5),
            },
        ),
    ];
    let view = fold(&lines);
    assert!(view.finished);
    assert_eq!(view.totals().failed, 1);
}

#[tokio::test]
async fn run_view_loads_from_a_run_directory() {
    let sb = sandbox();
    let (handle, task) = open(&sb.paths, FsyncPolicy::Always).await;
    let n = nid(1);
    handle
        .emit_durable(Some(n), spawned(n, n, None, 1))
        .await
        .unwrap();
    handle
        .emit_durable(
            Some(n),
            JournalEvent::NodeEvent {
                offset: 512,
                event: swamp::model::event::WorkerEvent::AssistantText {
                    text: "hello".into(),
                },
            },
        )
        .await
        .unwrap();
    drop(handle);
    task.await.expect("writer task");

    let plain = RunView::load(&sb.paths.dir, false).expect("load");
    assert!(plain.events.is_empty());
    assert_eq!(plain.nodes[&n].stream_offset, 512);

    let full = RunView::load(&sb.paths.dir, true).expect("load");
    assert_eq!(full.events[&n].len(), 1);
}

#[test]
fn the_llm_digest_fits_its_budget_and_still_names_every_failure() {
    let root = nid(1);
    let mut lines = vec![line(0, Some(root), spawned(root, root, None, 1))];
    let mut seq = 1u64;
    let mut failed = Vec::new();
    for i in 0..30usize {
        let id = nid(i + 10);
        lines.push(line(seq, Some(id), spawned(id, id, Some(root), 1)));
        seq += 1;
        let state = if i % 7 == 0 {
            failed.push(id);
            worker_error()
        } else {
            NodeState::Succeeded
        };
        lines.push(line(seq, Some(id), finished(state, Some(0.1))));
        seq += 1;
    }

    let max_bytes = 900;
    let mut d = LlmDigest::new(max_bytes);
    for l in &lines {
        Projection::apply(&mut d, l);
    }
    let out = d.finish();

    assert!(out.len() <= max_bytes, "digest is {} bytes", out.len());
    for id in &failed {
        assert!(
            out.contains(&id.short()),
            "failed node {} missing from the digest:\n{out}",
            id.short()
        );
    }
    assert!(out.contains("FAIL worker_error"));
}

#[test]
fn the_llm_digest_names_rejected_and_queued_tasks() {
    let mut d = LlmDigest::new(4096);
    for l in &schema_2() {
        Projection::apply(&mut d, l);
    }
    let out = d.finish();
    assert!(out.contains("docs [REJECTED worker_error]"), "{out}");
    assert!(out.contains("bench [queued]"), "{out}");
}

// ---------------------------------------------------------------- paths

fn paths_in(repo: &camino::Utf8Path, home: &camino::Utf8Path) -> Paths {
    Paths {
        repo: repo.to_path_buf(),
        dot_swamp: repo.join(".swamp"),
        home_swamp: home.to_path_buf(),
    }
}

#[test]
fn discover_walks_up_to_the_git_root() {
    let (_tmp, root) = common::tmp_repo();
    let nested = root.join("a").join("b");
    std::fs::create_dir_all(&nested).unwrap();
    let paths = Paths::discover(&nested).expect("discover");
    assert_eq!(paths.repo, root);
    assert_eq!(paths.dot_swamp, root.join(".swamp"));
    assert!(paths.accounts_state().ends_with("accounts.json"));
    assert!(
        paths
            .worktree_root()
            .as_str()
            .contains(root.file_name().unwrap())
    );
}

#[test]
fn discover_outside_a_repo_is_an_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("utf8");
    assert!(Paths::discover(&dir).is_err());
}

#[test]
fn ensure_git_excluded_is_idempotent_and_leaves_gitignore_alone() {
    let (_tmp, root) = common::tmp_repo();
    let home = root.join("home");
    std::fs::write(root.join(".gitignore"), "target\n").unwrap();
    let paths = paths_in(&root, &home);

    paths.ensure_git_excluded().expect("first");
    paths.ensure_git_excluded().expect("second");

    let exclude = std::fs::read_to_string(root.join(".git").join("info").join("exclude")).unwrap();
    assert_eq!(
        exclude.lines().filter(|l| l.trim() == "/.swamp/").count(),
        1,
        "exclude entry must be written once"
    );
    assert_eq!(
        std::fs::read_to_string(root.join(".gitignore")).unwrap(),
        "target\n",
        "the tracked .gitignore is never touched"
    );
}

#[test]
fn resolve_run_handles_ids_prefixes_last_and_offsets() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("utf8");
    let paths = paths_in(&root, &root.join("home"));

    let a = RunId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
    let b = RunId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAW").unwrap();
    let c = RunId::from_str("01BRZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
    for r in [a, b, c] {
        std::fs::create_dir_all(paths.run_dir(r)).unwrap();
    }

    assert_eq!(paths.list_runs().unwrap(), vec![c, b, a], "newest first");
    assert_eq!(paths.resolve_run("last").unwrap(), c);
    assert_eq!(paths.resolve_run("-2").unwrap(), b);
    assert_eq!(paths.resolve_run("-3").unwrap(), a);
    assert!(paths.resolve_run("-9").is_err());
    assert_eq!(paths.resolve_run(&a.to_string()).unwrap(), a);
    assert_eq!(paths.resolve_run("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(), a);
    assert_eq!(paths.resolve_run("01B").unwrap(), c);

    let err = paths.resolve_run("01A").unwrap_err().to_string();
    assert!(err.contains("ambiguous"), "{err}");
    assert!(
        err.contains(&a.to_string()) && err.contains(&b.to_string()),
        "{err}"
    );
    assert!(
        paths
            .resolve_run("09ZZZ")
            .unwrap_err()
            .to_string()
            .contains("no run")
    );
}

#[test]
fn run_paths_name_every_node_artifact_and_link_last() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("utf8");
    let paths = paths_in(&root, &root.join("home"));
    let run = rid(0);
    let rp = paths.run_paths(run);
    let n = nid(7);

    assert_eq!(rp.journal(), paths.run_dir(run).join("journal.jsonl"));
    assert_eq!(
        rp.node_dir(n),
        paths.run_dir(run).join("nodes").join(n.short())
    );
    assert!(rp.prompt(n).ends_with("prompt.md"));
    assert!(rp.stream(n).ends_with("stream.jsonl"));
    assert!(rp.stderr(n).ends_with("stderr.log"));
    assert!(rp.noise(n).ends_with("noise.log"));
    assert!(rp.last_message(n).ends_with("last-message.txt"));
    assert!(rp.patch(n).ends_with("patch.diff"));
    assert!(rp.pidfile(n).ends_with("pid"));
    assert!(rp.socket().as_str().ends_with(".sock"));
    // SUN_LEN is 104 bytes on macOS: the socket must not inherit the repo's depth.
    assert!(!rp.socket().starts_with(&rp.dir));

    std::fs::create_dir_all(&rp.dir).unwrap();
    rp.link_last().expect("link");
    rp.link_last().expect("relink is idempotent");
    let link = paths.dot_swamp.join("last");
    assert_eq!(
        std::fs::read_link(&link).unwrap().to_str().unwrap(),
        format!("runs/{run}")
    );
}

#[test]
fn fsync_policies_parse_from_config_text() {
    assert_eq!(
        FsyncPolicy::from_str("always").unwrap(),
        FsyncPolicy::Always
    );
    assert_eq!(
        FsyncPolicy::from_str("barrier").unwrap(),
        FsyncPolicy::Barrier
    );
    assert_eq!(FsyncPolicy::from_str("never").unwrap(), FsyncPolicy::Never);
    assert_eq!(
        FsyncPolicy::from_str("interval:250ms").unwrap(),
        FsyncPolicy::Interval(std::time::Duration::from_millis(250))
    );
    assert!(FsyncPolicy::from_str("sometimes").is_err());
    assert!(FsyncPolicy::from_str("interval:soon").is_err());
}

/// The line-level `node` id and the spawned record must not collide on the wire.
#[test]
fn a_node_spawned_line_reads_back() {
    let n = nid(1);
    let l = line(0, Some(n), spawned(n, n, None, 1));
    let json = serde_json::to_string(&l).expect("encode");
    let back: JournalLine = serde_json::from_str(&json).expect("decode");
    assert_eq!(back.node, Some(n));
    match back.event {
        JournalEvent::NodeSpawned { node } => assert_eq!(node.id, n),
        other => panic!("wrong event: {other:?}"),
    }
}

#[test]
fn a_detector_tagged_failure_still_renders_in_the_digest() {
    let a = nid(1);
    let lines = vec![
        line(0, Some(a), spawned(a, a, None, 1)),
        line(
            1,
            Some(a),
            finished(
                NodeState::Failed {
                    failure: Failure::RateLimited {
                        resets_at: None,
                        scope: swamp::model::core::LimitScope::SevenDay,
                        detected_by: Detector::Telemetry,
                        evidence: "quota".into(),
                    },
                },
                None,
            ),
        ),
    ];
    let mut d = LlmDigest::new(4096);
    for l in &lines {
        Projection::apply(&mut d, l);
    }
    let out = d.finish();
    assert!(out.contains("FAIL rate_limited"), "{out}");
}

/// `swamp resume` calls `RunSession::start` with the existing run id, so a second RunStarted
/// lands in the same journal. The run's origin is the first one: its task, base and clock.
#[test]
fn a_second_run_started_never_overwrites_the_run_header() {
    let mut view = RunView::default();
    view.apply(&line(
        0,
        None,
        JournalEvent::RunStarted {
            swamp_version: "0.1.0".into(),
            schema: 1,
            argv: vec!["swamp".into(), "run".into(), "port the parser".into()],
            cwd: Utf8PathBuf::from("/repo"),
            repo: Some(Utf8PathBuf::from("/repo")),
            base: Some("aaaa111".into()),
            config_sha256: "abc".into(),
            task: Some("port the parser".into()),
        },
    ));
    let first = view.header.as_ref().expect("a header").started_at;

    view.apply(&line(
        1,
        None,
        JournalEvent::RunStarted {
            swamp_version: "0.1.0".into(),
            schema: 1,
            argv: vec!["swamp".into(), "resume".into(), "last".into()],
            cwd: Utf8PathBuf::from("/repo"),
            repo: Some(Utf8PathBuf::from("/repo")),
            base: Some("bbbb222".into()),
            config_sha256: "abc".into(),
            task: None,
        },
    ));
    let h = view.header.as_ref().expect("a header");
    assert_eq!(h.task.as_deref(), Some("port the parser"));
    assert_eq!(h.base.as_deref(), Some("aaaa111"));
    assert_eq!(h.argv[1], "run");
    assert_eq!(h.started_at, first, "elapsed would measure from the resume");
}

// ---------------------------------------------------------------- schema 2: dispatches

fn fixture_path(name: &str) -> Utf8PathBuf {
    Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// The tree as text, exactly as it was recorded before schema 2 existed.
fn tree_text(view: &RunView) -> String {
    let t = view.totals();
    let mut s = String::new();
    for r in view.tree() {
        s.push_str(&format!(
            "{}{} {} {} {:?}\n",
            "  ".repeat(r.depth as usize),
            r.logical.short(),
            r.title,
            swamp::ui::fmt::state_word(&r.state),
            r.attempts.iter().map(|a| a.short()).collect::<Vec<_>>()
        ));
    }
    s.push_str(&format!(
        "nodes {} failed {} in {} out {} cost {:.2} complete {}\n",
        t.nodes, t.failed, t.usage.input_tokens, t.usage.output_tokens, t.cost_usd, t.cost_complete
    ));
    s
}

/// Recorded with the schema-1 fold: the new fold must draw the identical tree from it.
#[test]
fn a_schema_1_journal_folds_to_its_golden_tree() {
    let view = RunView::load(&fixture_path("journal-schema1.jsonl"), false).expect("load");
    assert_eq!(view.header.as_ref().map(|h| h.schema), Some(1));
    let golden = std::fs::read_to_string(fixture_path("journal-schema1.tree")).expect("golden");
    assert_eq!(tree_text(&view), golden);

    // Every worker of a schema-1 run sits in the legacy bucket, which closes with the run.
    assert_eq!(view.dispatches.len(), 1);
    let legacy = &view.dispatches[&DispatchId::LEGACY];
    assert!(legacy.record.is_none());
    assert_eq!(legacy.state, DispatchState::Settled);
    assert_eq!(legacy.tasks.len(), 3);
    for t in view.tasks.values() {
        assert_eq!(t.dispatch, DispatchId::LEGACY);
        assert_eq!((t.depth, &t.state), (None, &None), "{t:?}");
    }
    let parse = legacy.tasks[1];
    assert_eq!(view.attempts(parse).len(), 2);
    let r = view.rollup(Scope::Dispatch(DispatchId::LEGACY));
    assert_eq!((r.nodes, r.failed, r.rejected), (3, 1, 0));
    assert!(!r.cost_complete, "the docs task reported no cost");
    assert!((r.cost_usd - 1.51).abs() < 1e-9, "{}", r.cost_usd);
}

fn seq_of(lines: &[JournalLine], pred: impl Fn(&JournalEvent) -> bool) -> usize {
    lines
        .iter()
        .position(|l| pred(&l.event))
        .expect("the fixture has the line")
}

/// Chained (each `from` is the previous `to`) and never leaving a terminal state.
fn well_formed(chain: &[NodeTransition]) -> bool {
    chain.iter().all(|t| !t.from.is_terminal())
        && chain.windows(2).all(|w| Phase::from(&w[0].to) == w[1].from)
}

#[test]
fn a_schema_2_journal_groups_tasks_by_dispatch() {
    let view = fold(&schema_2());
    let (brain, a, b, c, d, e) = (nid(0), nid(1), nid(2), nid(3), nid(4), nid(5));
    let (d1, d2, d3) = (did(40), did(41), did(42));

    assert!(!view.dispatches.contains_key(&DispatchId::LEGACY));
    assert_eq!(view.dispatches[&d1].tasks, vec![a, b]);
    assert_eq!(view.dispatches[&d1].state, DispatchState::Settled);
    assert_eq!(view.dispatches[&d1].counts.map(|c| c.succeeded), Some(2));
    assert_eq!(view.dispatches[&d2].tasks, vec![c, d]);
    assert_eq!(view.dispatches[&d2].state, DispatchState::Open);
    let record = view.dispatches[&d2].record.as_ref().expect("a record");
    assert_eq!((record.caller, record.call_seq), (brain, Some(CallSeq(2))));
    assert_eq!(view.call_seq, Some(CallSeq(2)));

    for t in [a, b, c, d] {
        assert_eq!(view.tasks[&t].parent, Some(brain));
    }
    assert_eq!(view.tasks[&a].dispatch, d1);
    assert_eq!(view.tasks[&d].dispatch, d2);
    assert_eq!(view.tasks[&a].depth, Some(1));
    assert_eq!(view.nodes[&nid(12)].depth, 1);
    assert_eq!(view.nodes[&nid(12)].dispatch, Some(d1));
    assert_eq!(view.tasks[&e].parent, Some(a));
    assert_eq!(view.tasks[&e].dispatch, d3);
    assert_eq!(view.tasks[&e].depth, Some(2));
    assert_eq!(view.nodes[&nid(51)].depth, 2);
    assert_eq!(view.dispatches[&d3].state, DispatchState::Settled);

    assert_eq!(view.state_of(a), Some(NodeState::Succeeded));
    assert!(matches!(
        view.state_of(c),
        Some(NodeState::Rejected {
            reason: Failure::WorkerError { .. }
        })
    ));
    assert_eq!(view.state_of(d), Some(NodeState::Queued));

    let rows: Vec<(NodeId, u32)> = view.tree().iter().map(|r| (r.logical, r.depth)).collect();
    assert_eq!(
        rows,
        vec![(brain, 0), (a, 1), (e, 2), (b, 1), (c, 1), (d, 1)],
        "a task with no attempt yet is still a row under its caller"
    );
    let t = view.totals();
    assert_eq!((t.nodes, t.failed, t.rejected), (5, 0, 1));
}

#[test]
fn a_schema_2_journal_chains_attempts_and_transitions() {
    let lines = schema_2();
    let view = fold(&lines);
    let (a, b) = (nid(1), nid(2));

    let chain: Vec<NodeId> = view.attempts(a).iter().map(|n| n.id).collect();
    assert_eq!(chain, vec![nid(11), nid(12)]);
    assert_eq!(
        view.attempts(nid(12)).len(),
        2,
        "an attempt id resolves to its task"
    );
    assert!(view.exited.contains(&nid(11)));

    for (id, chain) in &view.transitions {
        assert!(well_formed(chain), "{id}: {chain:?}");
    }
    let phases: Vec<Phase> = view.transitions[&a]
        .iter()
        .map(|t| Phase::from(&t.to))
        .collect();
    assert_eq!(
        phases,
        vec![
            Phase::Leased,
            Phase::Queued,
            Phase::Leased,
            Phase::Succeeded
        ]
    );

    // Between attempts the task reads as waiting again, not as its failed first attempt.
    let rotated = seq_of(&lines, |e| {
        matches!(
            e,
            JournalEvent::NodeStateChanged {
                to: NodeState::Queued,
                ..
            }
        )
    });
    let mid = fold(&lines[..rotated]);
    assert_eq!(mid.state_of(a), Some(leased("main")));
    let mid = fold(&lines[..=rotated]);
    assert_eq!(mid.state_of(a), Some(NodeState::Queued));
    let blocked = seq_of(&lines, |e| matches!(e, JournalEvent::NodeBlocked { .. }));
    let mid = fold(&lines[..=blocked + 1]);
    assert!(matches!(mid.state_of(b), Some(NodeState::Blocked { .. })));
    // While an attempt runs, the row shows the process.
    let running = seq_of(&lines, |e| matches!(e, JournalEvent::ProcessStarted { .. }));
    let mid = fold(&lines[..=running]);
    assert!(matches!(
        mid.state_of(a),
        Some(NodeState::Running { pid: 101, .. })
    ));
}

#[test]
fn a_schema_2_journal_rolls_up_cost_and_tokens() {
    let view = fold(&schema_2());
    let (brain, a) = (nid(0), nid(1));
    let (d1, d2) = (did(40), did(41));
    let close = |x: f64, y: f64| (x - y).abs() < 1e-9;

    let r = view.rollup(Scope::Dispatch(d1));
    assert_eq!((r.nodes, r.failed, r.rejected), (2, 0, 0));
    assert!(close(r.cost_usd, 0.85) && r.cost_complete, "{r:?}");
    assert_eq!(r.usage.input_tokens, 300, "three attempts");

    let r = view.rollup(Scope::Task(a));
    assert!(close(r.cost_usd, 0.6), "{r:?}");
    assert_eq!(r.usage.input_tokens, 200);
    let same = view.rollup(Scope::Task(nid(11)));
    assert!(close(same.cost_usd, r.cost_usd));

    let r = view.rollup(Scope::Dispatch(d2));
    assert_eq!((r.nodes, r.rejected), (1, 1));
    assert!(close(r.cost_usd, 0.0) && r.cost_complete);

    let r = view.rollup(Scope::Subtree(brain));
    assert_eq!((r.nodes, r.rejected), (5, 1), "brain, A, E, B and D");
    assert!(close(r.cost_usd, 1.9), "{r:?}");
    let r = view.rollup(Scope::Subtree(a));
    assert!(close(r.cost_usd, 0.65), "A and the task it dispatched");
}

#[test]
fn folding_a_schema_2_journal_is_idempotent_and_prefix_stable() {
    let lines = schema_2();
    let once = digest(&fold(&lines));
    let mut twice = RunView::default();
    for l in lines.iter().chain(lines.iter()) {
        twice.apply(l);
    }
    assert_eq!(digest(&twice), once);
    for split in 0..=lines.len() {
        let mut v = RunView::default();
        for l in &lines[..split] {
            v.apply(l);
        }
        for l in &lines[split.saturating_sub(3)..] {
            v.apply(l);
        }
        assert_eq!(digest(&v), once, "split at {split}");
    }
}

#[test]
fn a_node_whose_process_exited_is_never_an_orphan() {
    let n = nid(1);
    let lines = vec![
        line(0, Some(n), spawned(n, n, None, 1)),
        line(1, Some(n), started(4000)),
        line(2, Some(n), exited()),
    ];
    let mut view = fold(&lines);
    view.mark_orphans(&|_| false);
    assert!(
        matches!(view.nodes[&n].state, NodeState::Running { .. }),
        "the process is gone by its own account: it is being finalized"
    );
    assert_eq!(view.nodes[&n].exit.and_then(|e| e.code), Some(0));

    let mut view = fold(&lines[..2]);
    view.mark_orphans(&|_| false);
    assert!(matches!(view.nodes[&n].state, NodeState::Orphaned { .. }));
}

#[test]
fn the_board_draws_tasks_that_never_reached_an_attempt() {
    use swamp::ui::board::model::{RunPane, Section};
    use swamp::ui::board::sources::Tail;

    let (a, c, d) = (nid(1), nid(3), nid(4));
    let dir = Utf8PathBuf::from("/repo/.swamp/runs/x");
    let mut pane = RunPane::new(
        RunPaths {
            run: rid(0),
            dir: dir.clone(),
            sock_dir: dir.clone(),
        },
        Tail::detached(dir.join("journal.jsonl").as_str()),
    );
    pane.apply(&schema_2());
    let rows = pane.rows();
    let row = |id: NodeId| {
        rows.iter()
            .find(|r| r.logical == id)
            .unwrap_or_else(|| panic!("no row for {id}: {rows:?}"))
    };

    assert!(matches!(row(c).state, NodeState::Rejected { .. }));
    assert_eq!(row(c).section(), Section::Recent);
    assert_eq!((row(c).title.as_str(), row(c).attempt), ("docs", 0));
    assert_eq!(row(d).state, NodeState::Queued);
    assert_eq!(row(d).section(), Section::Waiting);
    assert_eq!(
        row(d).created_at,
        at(0),
        "a waiting row counts from its dispatch"
    );
    assert_eq!(row(a).state, NodeState::Succeeded);
}

#[test]
fn resume_ends_stranded_tasks_and_then_their_dispatch() {
    use swamp::cmd::resume::{open_dispatches, open_tasks};

    let mut lines = schema_2();
    let (c, d) = (nid(3), nid(4));
    let d2 = did(41);
    let view = fold(&lines);
    let ends = open_tasks(&view);
    assert_eq!(ends.len(), 1, "{ends:?}");
    assert_eq!((ends[0].logical, ends[0].from), (d, Phase::Queued));
    assert!(matches!(ends[0].to, NodeState::Cancelled { .. }));
    assert!(
        open_dispatches(&view).is_empty(),
        "d2 still has a task open"
    );

    let seq = lines.len() as u64;
    lines.push(line(
        seq,
        Some(d),
        changed(ends[0].from, ends[0].to.clone(), "interrupted"),
    ));
    let view = fold(&lines);
    assert!(open_tasks(&view).is_empty());
    let settle = open_dispatches(&view);
    assert_eq!(settle.len(), 1);
    let (caller, id, counts, cost) = settle[0];
    assert_eq!((caller, id), (nid(0), d2));
    assert_eq!(
        counts,
        DispatchCounts {
            cancelled: 1,
            rejected: 1,
            ..DispatchCounts::default()
        }
    );
    assert!(cost.is_none(), "neither task ran");
    assert!(view.tasks.contains_key(&c));
}

/// A task whose last attempt ended while the task itself still reads leased.
#[test]
fn resume_ends_a_task_whose_terminal_transition_was_lost() {
    use swamp::cmd::resume::open_tasks;

    let lines = schema_2();
    let a = nid(1);
    let last = lines
        .iter()
        .rposition(|l| {
            l.node == Some(a)
                && matches!(
                    l.event,
                    JournalEvent::NodeStateChanged {
                        to: NodeState::Succeeded,
                        ..
                    }
                )
        })
        .expect("A's terminal transition");
    let lost: Vec<JournalLine> = lines
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != last)
        .map(|(_, l)| l.clone())
        .collect();
    let view = fold(&lost);
    assert!(matches!(view.state_of(a), Some(NodeState::Leased { .. })));
    let ends = open_tasks(&view);
    let end = ends.iter().find(|e| e.logical == a).expect("A is stranded");
    assert_eq!(
        (end.from, end.to.clone()),
        (Phase::Leased, NodeState::Succeeded)
    );
}

/// `swamp_inspect` names why a task waits with the pool's own structured verdict, and why a
/// rejected one never ran.
#[test]
fn inspect_carries_the_blocked_verdict_and_the_rejection() {
    use swamp::journal::inspect;
    let lines = schema_2();
    let blocked_at = lines
        .iter()
        .position(|l| matches!(l.event, JournalEvent::NodeBlocked { .. }))
        .expect("the fixture blocks a task");
    let mut view = RunView::default();
    for l in &lines[..=blocked_at + 1] {
        view.apply(l);
    }
    let b = inspect::task(&view, nid(2), at(10)).expect("task b");
    assert_eq!(b.state, Phase::Blocked);
    assert_eq!(b.depth, Some(1));
    let blocked = b.blocked.expect("a blocked verdict");
    assert_eq!(blocked.why, "main cooling");
    assert_eq!(blocked.ineligible.len(), 1);
    assert_eq!(blocked.ineligible[0].account, AccountId("main".into()));
    let json = serde_json::to_value(&blocked).expect("json");
    assert_eq!(
        json["ineligible"][0]["reason"],
        serde_json::json!("cooling")
    );

    let mut view = RunView::default();
    for l in &lines {
        view.apply(l);
    }
    let c = inspect::task(&view, nid(3), at(10)).expect("task c");
    assert_eq!(c.state, Phase::Rejected);
    assert!(c.attempts.is_empty());
    assert!(matches!(c.rejected, Some(Failure::WorkerError { .. })));
    assert!(
        inspect::task(&view, nid(2), at(10))
            .expect("b")
            .blocked
            .is_none()
    );

    // Any attempt id names its task.
    let a = inspect::task(&view, nid(11), at(10)).expect("a by its first attempt");
    assert_eq!(a.node, nid(1));
    assert_eq!(a.attempts.len(), 2);
    assert_eq!(a.dispatches, vec![did(42).to_string()]);
}

/// A transition journaled after a task ended, such as a racing retry, never revives it.
#[test]
fn the_first_terminal_task_state_wins() {
    let (caller, task, d) = (nid(0), nid(2), did(1));
    let v = fold(&[
        line(0, Some(caller), issued(caller, d, Some(1), &[(task, "t")])),
        line(1, Some(task), queued(task, d, "t", 1)),
        line(
            2,
            Some(task),
            changed(
                Phase::Queued,
                NodeState::Cancelled {
                    by: swamp::model::core::CancelSource::User,
                },
                "cancelled by user",
            ),
        ),
        line(
            3,
            Some(task),
            changed(Phase::Queued, leased("main"), "retry"),
        ),
        line(
            4,
            Some(task),
            changed(Phase::Leased, NodeState::Succeeded, "succeeded"),
        ),
    ]);
    assert!(
        matches!(v.state_of(task), Some(NodeState::Cancelled { .. })),
        "{:?}",
        v.state_of(task)
    );
    assert_eq!(v.transitions[&task].len(), 3);
}
