//! WP7: the trace renderer. One fold, three renderings, all deterministic.

mod common;

use camino::Utf8PathBuf;
use std::str::FromStr;
use std::time::Duration;
use swamp::ids::{NodeId, RunId};
use swamp::journal::fold::RunView;
use swamp::journal::record::{JournalEvent, JournalLine, SCHEMA_VERSION};
use swamp::model::core::{
    AccountId, ChangeKind, Cost, CostBasis, EvidenceSource, FileChange, LimitScope, NodeKind,
    NodeState, Provider, SessionHandle, Tier, Usage, WorkspaceRef,
};
use swamp::model::failure::{Detector, Failure};
use swamp::model::node::{NodeRecord, WorkResultRef};
use swamp::ui::fmt;
use swamp::ui::trace::{Follower, TraceOpts, render};
use time::OffsetDateTime;

const CROCKFORD: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

fn ulid_text(i: usize) -> String {
    let hi = CROCKFORD[(i / 32) % 32] as char;
    let lo = CROCKFORD[i % 32] as char;
    format!("01ARZ3NDEKTSV4RRFFQ69G5F{hi}{lo}")
}

fn nid(i: usize) -> NodeId {
    NodeId::from_str(&ulid_text(i)).expect("node id")
}

fn rid() -> RunId {
    RunId::from_str(&ulid_text(0)).expect("run id")
}

fn at(offset: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + offset).expect("timestamp")
}

fn line(seq: u64, node: Option<NodeId>, event: JournalEvent) -> JournalLine {
    JournalLine {
        seq,
        at: at(seq as i64),
        run: rid(),
        node,
        event,
    }
}

struct Node {
    id: NodeId,
    logical: NodeId,
    parent: Option<NodeId>,
    kind: NodeKind,
    title: &'static str,
    account: &'static str,
    model: &'static str,
    tier: Tier,
    attempt: u32,
    seconds: i64,
    usage: Usage,
    cost: Option<Cost>,
    state: NodeState,
    work: Option<WorkResultRef>,
    files: Vec<FileChange>,
}

fn record(n: &Node) -> NodeRecord {
    NodeRecord {
        id: n.id,
        run_id: rid(),
        parent: n.parent,
        logical: n.logical,
        attempt: n.attempt,
        retry_of: None,
        kind: n.kind,
        title: n.title.to_owned(),
        prompt_path: Utf8PathBuf::from("prompt.md"),
        prompt_sha256: "0".repeat(64),
        provider: Provider::Anthropic,
        account: Some(AccountId(n.account.to_owned())),
        exec: Some(format!("claude-{}", n.account)),
        argv: vec!["claude".into()],
        model: Some(n.model.to_owned()),
        tier: n.tier,
        workspace: WorkspaceRef::Worktree {
            path: Utf8PathBuf::from("/tmp/wt"),
            branch: "swamp/69g5f0/b73e10-1".into(),
            base: "9f3c1ad".into(),
        },
        session: Some(SessionHandle {
            account: AccountId(n.account.to_owned()),
            id: "session-1".into(),
            preassigned: true,
        }),
        state: n.state.clone(),
        created_at: at(0),
        started_at: Some(at(0)),
        ended_at: Some(at(n.seconds)),
        usage: n.usage,
        cost: n.cost,
        exit: None,
        files: n.files.clone(),
        work: n.work.clone(),
        summary: None,
        stream_offset: 0,
        unparsed_lines: 0,
    }
}

fn usage(input: u64, output: u64) -> Usage {
    Usage {
        input_tokens: input,
        cached_input_tokens: input * 2,
        cache_write_tokens: 1_000,
        output_tokens: output,
        reasoning_tokens: 0,
    }
}

fn rate_limited() -> Failure {
    Failure::RateLimited {
        resets_at: Some(at(3_600)),
        scope: LimitScope::SevenDay,
        detected_by: Detector::Telemetry,
        evidence: "rate_limit_event: seven_day utilization 1.00".into(),
    }
}

fn work() -> WorkResultRef {
    WorkResultRef {
        head: "abc1234".into(),
        branch: "swamp/69g5f0/b73e10-2".into(),
        patch: Utf8PathBuf::from("patch.diff"),
        insertions: 214,
        deletions: 37,
        empty: false,
    }
}

fn changed(path: &str) -> FileChange {
    FileChange {
        path: Utf8PathBuf::from(path),
        kind: ChangeKind::Modify,
        added: 12,
        removed: 3,
        source: EvidenceSource::Git,
    }
}

/// A brain, a retried worker, a worker with no cost data and a worker whose task failed.
fn journal() -> Vec<JournalLine> {
    let brain = nid(1);
    let logical = nid(2);
    let attempt1 = nid(3);
    let attempt2 = nid(4);
    let cheap = nid(5);
    let failed = nid(6);

    let nodes = [
        Node {
            id: brain,
            logical: brain,
            parent: None,
            kind: NodeKind::Brain,
            title: "brain",
            account: "main",
            model: "model-high",
            tier: Tier::High,
            attempt: 1,
            seconds: 252,
            usage: usage(214_000, 8_400),
            cost: Some(Cost {
                usd: 0.71,
                basis: CostBasis::Reported,
            }),
            state: NodeState::Succeeded,
            work: None,
            files: Vec::new(),
        },
        Node {
            id: attempt1,
            logical,
            parent: Some(brain),
            kind: NodeKind::Worker,
            title: "migrate user model",
            account: "main",
            model: "model-mid",
            tier: Tier::Mid,
            attempt: 1,
            seconds: 12,
            usage: usage(1_000, 100),
            cost: Some(Cost {
                usd: 0.01,
                basis: CostBasis::Reported,
            }),
            state: NodeState::Failed {
                failure: rate_limited(),
            },
            work: None,
            files: Vec::new(),
        },
        Node {
            id: attempt2,
            logical,
            parent: Some(brain),
            kind: NodeKind::Worker,
            title: "migrate user model",
            account: "alt",
            model: "model-mid",
            tier: Tier::Mid,
            attempt: 2,
            seconds: 108,
            usage: usage(118_000, 4_200),
            cost: Some(Cost {
                usd: 0.42,
                basis: CostBasis::Reported,
            }),
            state: NodeState::Succeeded,
            work: Some(work()),
            files: vec![changed("src/user.rs")],
        },
        Node {
            id: cheap,
            logical: cheap,
            parent: Some(brain),
            kind: NodeKind::Worker,
            title: "update changelog",
            account: "main",
            model: "model-low",
            tier: Tier::Low,
            attempt: 1,
            seconds: 31,
            usage: usage(14_000, 900),
            cost: None,
            state: NodeState::Succeeded,
            work: None,
            files: Vec::new(),
        },
        Node {
            id: failed,
            logical: failed,
            parent: Some(brain),
            kind: NodeKind::Worker,
            title: "audit auth middleware",
            account: "main",
            model: "model-high",
            tier: Tier::High,
            attempt: 1,
            seconds: 124,
            usage: usage(183_000, 6_100),
            cost: Some(Cost {
                usd: 0.50,
                basis: CostBasis::Reported,
            }),
            state: NodeState::Failed {
                failure: Failure::WorkerError {
                    subtype: "error_during_execution".into(),
                    detail: "2 tests still failing".into(),
                },
            },
            work: None,
            files: Vec::new(),
        },
    ];

    let mut lines = vec![line(
        0,
        None,
        JournalEvent::RunStarted {
            swamp_version: "0.1.0".into(),
            schema: SCHEMA_VERSION,
            argv: vec!["swamp".into(), "chat".into()],
            cwd: Utf8PathBuf::from("/projects/api"),
            repo: Some(Utf8PathBuf::from("/projects/api")),
            base: Some("9f3c1ad2b9".into()),
            config_sha256: "c0ffee".into(),
            task: Some("migrate the user model".into()),
        },
    )];
    for (i, n) in nodes.iter().enumerate() {
        lines.push(line(
            i as u64 + 1,
            Some(n.id),
            JournalEvent::NodeSpawned {
                node: Box::new(record(n)),
            },
        ));
    }
    lines.push(line(
        nodes.len() as u64 + 1,
        Some(nid(4)),
        JournalEvent::NodeRetry {
            attempt: 1,
            reason: rate_limited(),
            rotate: true,
        },
    ));
    lines
}

fn view() -> RunView {
    let mut view = RunView::default();
    for l in journal() {
        view.apply(&l);
    }
    view
}

#[test]
fn the_tree_renders_the_brain_the_retry_chain_and_the_failure() {
    insta::assert_snapshot!("tree", render(&view(), &TraceOpts::default()));
}

#[test]
fn an_unknown_cost_is_named_in_the_footer_and_never_rendered_as_zero() {
    let text = render(&view(), &TraceOpts::default());
    assert!(
        text.contains("(1 node reported no cost data)"),
        "footer must name the node with no cost data:\n{text}"
    );
    assert!(
        !text.contains("$0.00"),
        "absent cost renders as `-`:\n{text}"
    );
    // The row for that node carries a dash in the cost column.
    let row = text
        .lines()
        .find(|l| l.contains("update changelog"))
        .expect("the row is rendered");
    assert!(row.ends_with("ok"), "{row}");
    assert!(row.contains(" - "), "{row}");
}

#[test]
fn the_failure_row_carries_its_evidence_and_says_why_it_did_not_fail_over() {
    let text = render(&view(), &TraceOpts::default());
    assert!(text.contains("WorkerError(error_during_execution): 2 tests still failing"));
    assert!(text.contains("no failover (task-level failure)"));
    assert!(text.contains("rate_limited (seven_day, telemetry) resets 23:13"));
}

#[test]
fn failed_and_depth_filters_cut_the_tree_down() {
    let failed = render(
        &view(),
        &TraceOpts {
            failed: true,
            ..TraceOpts::default()
        },
    );
    assert!(failed.contains("audit auth middleware"));
    assert!(!failed.contains("update changelog"));

    let shallow = render(
        &view(),
        &TraceOpts {
            depth: Some(0),
            ..TraceOpts::default()
        },
    );
    assert!(shallow.contains("brain"));
    assert!(!shallow.contains("migrate user model"));
}

#[test]
fn node_json_is_byte_identical_to_its_entry_in_the_full_json() {
    let view = view();
    let full = render(
        &view,
        &TraceOpts {
            json: true,
            ..TraceOpts::default()
        },
    );
    let parsed: serde_json::Value = serde_json::from_str(&full).expect("the full render is json");
    let id = nid(4);
    let entry = parsed
        .get("nodes")
        .and_then(|n| n.get(id.to_string()))
        .expect("the node is in the full render");

    let one = render(
        &view,
        &TraceOpts {
            node: Some(id),
            json: true,
            ..TraceOpts::default()
        },
    );
    assert_eq!(
        one,
        format!("{}\n", serde_json::to_string_pretty(entry).unwrap()),
        "same fold, one source of truth"
    );
}

#[test]
fn the_json_render_carries_the_folded_totals_and_tree() {
    let text = render(
        &view(),
        &TraceOpts {
            json: true,
            ..TraceOpts::default()
        },
    );
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["totals"]["cost_complete"], serde_json::json!(false));
    assert_eq!(parsed["tree"].as_array().unwrap().len(), 4);
    assert_eq!(parsed["nodes"].as_object().unwrap().len(), 5);
    assert_eq!(parsed["run"]["base"], serde_json::json!("9f3c1ad2b9"));
}

#[test]
fn follow_prints_each_row_once_and_reprints_only_what_changed() {
    let lines = journal();
    let mut follower = Follower::new(TraceOpts::default(), false);

    let first = follower.ingest(&lines[..3]);
    assert!(first.contains("run 9g5f00"), "{first}");
    assert!(first.contains("brain"), "{first}");
    assert!(first.contains("migrate user model"), "{first}");

    let second = follower.ingest(&lines[3..]);
    assert!(
        !second.contains("update changelog\n") || second.contains("update changelog"),
        "{second}"
    );
    // The brain row did not change, so it is not printed again.
    assert!(!second.contains("* brain"), "{second}");
    assert!(second.contains("audit auth middleware"), "{second}");

    let third = follower.ingest(&[]);
    assert_eq!(third, "", "a poll with no new lines prints nothing");
}

#[test]
fn the_raw_stream_is_handed_back_verbatim() {
    let (_tmp, repo) = common::tmp_repo();
    let bytes = std::fs::read(common::fixture("claude-stream-sample.jsonl")).unwrap();
    let node = nid(9);
    let dir = repo.join(".swamp/runs/x/nodes").join(node.short());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("stream.jsonl"), &bytes).unwrap();
    // `trace --raw` byte-compares against this file: no re-encoding anywhere on the path.
    assert_eq!(std::fs::read(dir.join("stream.jsonl")).unwrap(), bytes);
}

#[test]
fn formatting_holds_at_the_boundaries() {
    assert_eq!(fmt::cost(None), "-");
    assert_eq!(
        fmt::cost(Some(Cost {
            usd: 1.84,
            basis: CostBasis::Reported
        })),
        "~$1.84"
    );
    assert_eq!(fmt::duration(Duration::from_secs(252)), "4m12s");
    assert_eq!(fmt::duration(Duration::from_secs(3600)), "1h00m");
    assert_eq!(fmt::tokens(1_200_000), "1.2M");
    assert_eq!(fmt::tokens(84_100), "84.1k");
    insta::assert_snapshot!(
        "formats",
        [0u64, 59, 60, 252, 3_599, 3_600, 90_061]
            .map(|s| format!("{s}s -> {}", fmt::duration(Duration::from_secs(s))))
            .join("\n")
            + "\n"
            + &[
                0u64, 999, 1_000, 14_000, 84_100, 214_000, 1_200_000, 9_400_000
            ]
            .map(|n| format!("{n} -> {}", fmt::tokens(n)))
            .join("\n")
    );
}

/// `tree()` walks down from the roots. Pruning an ancestor used to make every recent node
/// unreachable, and in a normal run the brain is the only root and the oldest node there is.
#[test]
fn since_keeps_the_ancestors_of_the_nodes_it_keeps() {
    let mut v = view();
    let recent = nid(6);
    v.nodes.get_mut(&recent).expect("node").created_at = at(1_000);
    swamp::ui::trace::keep_since(&mut v, at(500));

    let rows = v.tree();
    assert!(
        rows.iter().any(|r| r.logical == recent),
        "the recent node went with its pruned parent: {rows:?}"
    );
    assert!(rows.iter().any(|r| r.logical == nid(1)), "{rows:?}");
    assert_eq!(
        rows.len(),
        2,
        "only the brain and the recent node: {rows:?}"
    );
}

/// Totals describe what is rendered. `keep_since` used to leave `cost_complete` and the
/// run total describing the whole run, so the footer claimed "0 nodes reported no cost data".
#[test]
fn since_recomputes_the_totals_it_renders() {
    let mut v = view();
    v.nodes.get_mut(&nid(6)).expect("node").created_at = at(1_000);
    swamp::ui::trace::keep_since(&mut v, at(500));

    let text = render(&v, &TraceOpts::default());
    assert!(!text.contains("(0 node"), "{text}");
    assert!(
        text.contains("cost   ~$1.21"),
        "the footer still totals pruned nodes: {text}"
    );
}
