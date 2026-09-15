//! The ids a user can see have to be the ids the CLI accepts: run short ids, logical node
//! short ids and attempt node short ids.

mod support;

use support::{Harness, Scenario, epoch_in};
use swamp::model::core::NodeState;

const TASK: &str = "fix the flaky test in api.rs";

/// Two attempts of one logical node: the first subscription is rate limited, the second works.
fn retried() -> Harness {
    let h = Harness::new()
        .with_accounts(2, 0)
        .prefer("main")
        .scenario("main", Scenario::claude_rate_limited(epoch_in(1800)))
        .scenario("alt", Scenario::claude().edits("fixed.txt", "patched\n"));
    h.swamp(&["run", "--no-brain", TASK]).assert().success();
    h
}

/// The branch carries the logical short id and the node directory the attempt's. Both are on
/// screen, so `diff` and `adopt` accept both; the logical one lands on the finished attempt.
#[test]
fn both_the_logical_and_the_attempt_short_id_resolve_to_a_node() {
    let h = retried();
    let view = h.last_view();
    let winner = view
        .nodes
        .values()
        .find(|n| n.state == NodeState::Succeeded)
        .expect("the second attempt succeeded");
    let logical = winner.logical.short();
    let attempt = winner.id.short();
    assert_ne!(logical, attempt, "the fixture needs two distinct ids");

    let branch = &winner.work.as_ref().expect("a work result").branch;
    assert!(
        branch.contains(&logical),
        "branch {branch} names the logical id"
    );
    assert!(
        h.last_run().node_dir(winner.id).is_dir(),
        "the node dir is named after the attempt"
    );

    for spec in [logical.as_str(), attempt.as_str()] {
        h.swamp(&["diff", spec, "--name-only"])
            .assert()
            .success()
            .stdout(predicates::str::contains("fixed.txt"));
    }

    // The losing attempt still resolves by its own id, and it produced no patch.
    let loser = view
        .nodes
        .values()
        .find(|n| n.attempt == 1)
        .expect("a first attempt");
    h.swamp(&["diff", &loser.id.short(), "--name-only"])
        .assert()
        .success();
}

/// The id the trace prints is the one `diff` is given, so every row leads with it.
#[test]
fn trace_prints_the_attempt_node_id_on_every_node_line() {
    let h = retried();
    let view = h.last_view();
    let out = h.swamp(&["trace", "last"]).assert().success();
    let text = String::from_utf8(out.get_output().stdout.clone()).expect("utf8 trace");
    for n in view.nodes.values() {
        assert!(
            text.contains(&n.id.short()),
            "attempt {} is missing from the trace:\n{text}",
            n.id.short()
        );
    }
}

/// `--node` takes the same two spellings, and picks the directory of a real attempt.
#[test]
fn trace_node_takes_either_short_id() {
    let h = retried();
    let view = h.last_view();
    let winner = view
        .nodes
        .values()
        .find(|n| n.state == NodeState::Succeeded)
        .expect("the second attempt succeeded");

    for spec in [winner.logical.short(), winner.id.short()] {
        h.swamp(&["trace", "--node", &spec])
            .assert()
            .success()
            .stdout(predicates::str::contains(winner.id.short()));
        h.swamp(&["trace", "--node", &spec, "--raw"])
            .assert()
            .success()
            .stdout(predicates::str::contains("\"type\""));
    }
}

/// The trace header prints `run <short>`; that spelling has to resolve.
#[test]
fn a_run_is_named_by_the_short_id_the_header_prints() {
    let h = retried();
    let run = h.last_run().run;
    let short = run.short();

    h.swamp(&["trace", &short])
        .assert()
        .success()
        .stdout(predicates::str::contains(format!("run {short}")));
    h.swamp(&["trace", &short.to_ascii_uppercase()])
        .assert()
        .success();
    h.swamp(&["trace", &run.to_string()]).assert().success();
    h.swamp(&["trace", "nosuchrun"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no run matches"));
}

/// `--stat` is the natural first look at a node, and README documents it.
#[test]
fn diff_stat_renders_the_patch_git_style() {
    let h = Harness::new().scenario("main", Scenario::claude().edits("fixed.txt", "patched\n"));
    h.swamp(&["run", "--no-brain", TASK]).assert().success();

    h.swamp(&["diff", "last", "--stat"])
        .assert()
        .success()
        .stdout(predicates::str::contains("fixed.txt"))
        .stdout(predicates::str::contains("1 file changed"));
}
