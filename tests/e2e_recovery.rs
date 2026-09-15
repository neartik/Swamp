//! WP8: what survives a dead supervisor, a Ctrl-C, and what it takes to land the work.

mod support;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use std::time::Duration;
use support::{Harness, Scenario, wait_for};
use swamp::model::core::NodeState;

const TASK: &str = "port the parser to the new lexer";
const PATIENCE: Duration = Duration::from_secs(30);

/// WP8 acceptance 7: Workers are detached on purpose: killing the supervisor does not kill the work, and
/// `swamp resume` picks it back up without re-reading a byte it already parsed.
#[test]
fn a_killed_supervisor_leaves_an_adoptable_worker_and_resume_finishes_it() {
    let h = Harness::new().scenario(
        "main",
        Scenario::claude().slow(900).edits("fixed.txt", "patched\n"),
    );

    let mut child = h.spawn(&["run", "--no-brain", TASK]);
    wait_for("the run directory", PATIENCE, || !h.runs().is_empty());
    let run = h.last_run();
    let journal = run.journal();
    wait_for("the first parsed stream event", PATIENCE, || {
        std::fs::read_to_string(&journal)
            .map(|t| t.contains(r#""ev":"node_event""#))
            .unwrap_or(false)
    });

    // SIGKILL: no unwinding, no RunFinished, no chance to clean up after itself.
    kill(Pid::from_raw(child.id() as i32), Signal::SIGKILL).expect("SIGKILL the supervisor");
    let _ = child.wait();

    let view = h.last_view();
    assert!(!view.finished, "an interrupted run has no RunFinished");
    let node = view.nodes.values().next().expect("the node").clone();
    assert!(
        matches!(node.state, NodeState::Running { .. }),
        "{:?}",
        node.state
    );
    assert!(node.stream_offset > 0, "nothing was parsed before the kill");

    // The plan spends nothing: it reports that the worker is alive and where to resume it.
    h.swamp(&["resume", "last", "--plan"])
        .assert()
        .success()
        .stdout(predicates::str::contains("adopt"))
        .stdout(predicates::str::contains(format!(
            "stream_offset {}",
            node.stream_offset
        )))
        .stdout(predicates::str::contains("nothing was started"));
    assert_eq!(
        h.invocations("main").len(),
        1,
        "--plan must not launch anything"
    );

    h.swamp(&["resume", "last"]).assert().success();
    assert_eq!(
        h.invocations("main").len(),
        1,
        "the original worker was adopted, not rerun"
    );

    let view = h.last_view();
    let node = view.nodes.values().next().expect("the node");
    assert_eq!(node.state, NodeState::Succeeded);
    assert!(
        node.work.as_ref().is_some_and(|w| !w.empty),
        "the diff was captured"
    );

    // The journal is the fold's only input: a re-read of the stream shows up here twice. One
    // line can still fan out to several events, so the pair is what has to be unique.
    let text = std::fs::read_to_string(&journal).expect("journal");
    let events = node_events(&text);
    let unique: std::collections::BTreeSet<(u64, String)> = events.iter().cloned().collect();
    assert_eq!(events.len(), unique.len(), "an event was parsed twice");
    assert!(
        events.windows(2).all(|w| w[0].0 <= w[1].0),
        "the parser went backwards: {:?}",
        events.iter().map(|e| e.0).collect::<Vec<_>>()
    );
    assert_eq!(
        text.matches(r#""e":"session_started""#).count(),
        1,
        "the stream was re-read from the beginning"
    );
    assert_eq!(
        text.matches(r#""e":"final""#).count(),
        1,
        "one terminal event"
    );
}

/// WP8 acceptance 8: Ctrl-C stops the workers and still closes the journal: an interrupted run must never
/// look like a crashed one.
#[test]
fn an_interrupted_run_journals_its_end_and_leaves_no_process_group_behind() {
    let h = Harness::new().scenario(
        "main",
        Scenario::claude()
            .slow(2000)
            .edits("fixed.txt", "patched\n"),
    );

    let child = h.spawn(&["run", "--no-brain", TASK]);
    wait_for("the run directory", PATIENCE, || !h.runs().is_empty());
    let run = h.last_run();
    let journal = run.journal();
    wait_for("the worker process", PATIENCE, || {
        std::fs::read_to_string(&journal)
            .map(|t| t.contains(r#""ev":"process_started""#))
            .unwrap_or(false)
    });
    let worker = worker_pid(&h);

    kill(Pid::from_raw(child.id() as i32), Signal::SIGINT).expect("SIGINT the supervisor");
    let out = child.wait_with_output().expect("the run exits");
    assert_eq!(out.status.code(), Some(6), "an interrupt is exit 6");

    let view = h.last_view();
    assert!(view.finished, "RunFinished is journaled on the way out");
    let node = view.nodes.values().next().expect("the node");
    assert!(
        !matches!(node.state, NodeState::Succeeded),
        "a cancelled worker did not succeed: {:?}",
        node.state
    );

    wait_for("the worker's process group to go", PATIENCE, || {
        kill(Pid::from_raw(worker), None).is_err()
    });
}

/// WP8 acceptance 10: Landing the work is always the user's call, and it never overwrites uncommitted work
/// unless the user insists.
#[test]
fn adopt_applies_a_patch_and_refuses_a_dirty_tree_without_force() {
    let h = Harness::new().scenario(
        "main",
        Scenario::claude().edits("fixed.txt", "from the worker\n"),
    );
    h.swamp(&["run", "--no-brain", TASK]).assert().success();

    let view = h.last_view();
    let node = view.nodes.values().next().expect("the node");
    let id = node.id.short();
    assert!(!h.repo.join("fixed.txt").exists(), "nothing is landed yet");

    // A dirty checkout is a refusal, not a merge attempt.
    std::fs::write(h.repo.join("README.md"), "local edit\n").expect("dirty the tree");
    h.swamp(&["adopt", &id])
        .assert()
        .code(1)
        .stdout(predicates::str::contains("refused to adopt"))
        .stdout(predicates::str::contains("--force"));
    assert!(!h.repo.join("fixed.txt").exists(), "still nothing landed");

    h.swamp(&["adopt", &id, "--force"])
        .assert()
        .success()
        .stdout(predicates::str::contains("adopted"));
    assert_eq!(
        std::fs::read_to_string(h.repo.join("fixed.txt")).expect("the adopted file"),
        "from the worker\n"
    );
    assert_eq!(
        std::fs::read_to_string(h.repo.join("README.md")).expect("README"),
        "local edit\n",
        "the user's own edit is untouched"
    );
    assert!(
        h.journal_text(h.last_run().run)
            .contains(r#""ev":"adopted""#),
        "the adoption is part of the run's history"
    );
}

/// Every journaled worker event as (stream offset, normalized event).
fn node_events(journal: &str) -> Vec<(u64, String)> {
    journal
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["ev"] == "node_event")
        .map(|v| {
            (
                v["offset"].as_u64().unwrap_or_default(),
                v["event"].to_string(),
            )
        })
        .collect()
}

fn worker_pid(h: &Harness) -> i32 {
    let view = h.last_view();
    let node = view.nodes.values().next().expect("the node");
    let text = std::fs::read_to_string(h.last_run().pidfile(node.id)).expect("the pidfile");
    text.split_whitespace()
        .next()
        .and_then(|p| p.parse().ok())
        .expect("a pid")
}
