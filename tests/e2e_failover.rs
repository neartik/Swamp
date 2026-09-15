//! WP8: who rotates, who does not, and what never reaches the disk.

mod support;

use predicates::prelude::PredicateBooleanExt;

use support::{Harness, Scenario, epoch_in};
use swamp::dispatch::account::Health;
use swamp::model::core::{AccountId, NodeState};
use swamp::model::failure::Failure;

const TASK: &str = "rewrite the retry loop";
const SECRET: &str = "sk-ant-fake123";

/// WP8 acceptance 3: A rate-limited subscription cools for as long as the provider says, and the work moves
/// to the next one.
#[test]
fn a_rate_limited_account_cools_and_the_node_rotates_to_the_next_one() {
    let resets_at = epoch_in(1800);
    let h = Harness::new()
        .with_accounts(2, 0)
        .prefer("main")
        .scenario("main", Scenario::claude_rate_limited(resets_at))
        .scenario("alt", Scenario::claude().edits("fixed.txt", "patched\n"));

    h.swamp(&["run", "--no-brain", TASK]).assert().success();

    let view = h.last_view();
    assert_eq!(view.nodes.len(), 2, "one attempt per account");
    assert_eq!(view.tree().len(), 1, "both attempts serve one logical node");
    let attempts: Vec<_> = view.nodes.values().collect();
    let logical: std::collections::BTreeSet<_> = attempts.iter().map(|n| n.logical).collect();
    assert_eq!(logical.len(), 1, "one logical node");

    let first = attempts
        .iter()
        .find(|n| n.attempt == 1)
        .expect("a first attempt");
    let second = attempts
        .iter()
        .find(|n| n.attempt == 2)
        .expect("a second attempt");
    assert_eq!(first.account.as_ref().map(|a| a.0.as_str()), Some("main"));
    assert_eq!(second.account.as_ref().map(|a| a.0.as_str()), Some("alt"));
    assert_eq!(
        second.retry_of,
        Some(first.id),
        "the retry names its parent"
    );
    assert!(
        matches!(
            first.state,
            NodeState::Failed {
                failure: Failure::RateLimited { .. }
            }
        ),
        "{:?}",
        first.state
    );
    assert_eq!(second.state, NodeState::Succeeded);
    assert_eq!(
        second.argv.first().map(String::as_str),
        Some("claude-alt"),
        "the second attempt launched the other wrapper: {:?}",
        second.argv
    );
    assert_eq!(h.invocations("main").len(), 1);
    assert_eq!(h.invocations("alt").len(), 1);

    // The provider named a reset time, so the cooldown is that time, not the backoff curve.
    let state = h.accounts_state();
    let main = state
        .get(&AccountId("main".into()))
        .expect("main is in accounts.json");
    assert_eq!(main.health, Health::Cooling);
    let until = main.cooldown_until.expect("a cooldown").unix_timestamp();
    assert!(
        (until - resets_at).abs() <= 60,
        "cooldown_until {until} is not derived from resets_at {resets_at}"
    );
    assert_eq!(
        state.get(&AccountId("alt".into())).map(|a| a.health),
        Some(Health::Healthy)
    );

    h.swamp(&["accounts"])
        .assert()
        .success()
        .stdout(predicates::str::contains("cooling"));

    // The cooldown is machine-wide state, not run state: a different repo sees it too.
    let elsewhere = h.root.join("other-repo");
    std::fs::create_dir_all(&elsewhere).expect("the second repo");
    support::init_repo(&elsewhere);
    let mut cmd = assert_cmd::Command::new(support::swamp_exe());
    cmd.current_dir(&elsewhere);
    for (k, v) in h.env() {
        cmd.env(k, v);
    }
    cmd.arg("accounts")
        .assert()
        .success()
        .stdout(predicates::str::contains("cooling"));
}

/// WP8 acceptance 4: A failing task is the task's problem. Rotating would burn a second subscription on the
/// same bad prompt.
#[test]
fn a_task_failure_never_rotates_the_account() {
    let h = Harness::new()
        .with_accounts(2, 0)
        .prefer("main")
        .scenario("main", Scenario::claude_task_error());

    h.swamp(&["run", "--no-brain", TASK])
        .assert()
        .code(4)
        .stdout(predicates::str::contains("error_during_execution"));

    let view = h.last_view();
    assert_eq!(view.nodes.len(), 1, "exactly one node, no retry");
    let node = view.nodes.values().next().expect("the node");
    assert!(
        matches!(
            node.state,
            NodeState::Failed {
                failure: Failure::WorkerError { .. }
            }
        ),
        "{:?}",
        node.state
    );
    assert_eq!(h.invocations("main").len(), 1);
    assert!(h.invocations("alt").is_empty(), "alt was never invoked");

    // And the account is untouched: a bad prompt must never cool a subscription.
    let state = h.accounts_state();
    assert_eq!(
        state.get(&AccountId("main".into())).map(|a| a.health),
        Some(Health::Healthy)
    );
    assert!(
        state
            .get(&AccountId("main".into()))
            .and_then(|a| a.cooldown_until)
            .is_none()
    );
}

/// WP8 acceptance 9: `subtype: "success"` with denials is not a success: the worker was stopped from doing
/// the work it reports.
#[test]
fn auto_denied_tool_calls_fail_the_node_and_show_up_in_the_trace() {
    let h = Harness::new().scenario("main", Scenario::claude_permission_denied(3));

    h.swamp(&["run", "--no-brain", TASK]).assert().code(4);

    let view = h.last_view();
    let node = view.nodes.values().next().expect("the node");
    assert!(
        matches!(
            &node.state,
            NodeState::Failed {
                failure: Failure::PermissionDenied { denials: 3, .. }
            }
        ),
        "{:?}",
        node.state
    );
    assert_eq!(h.invocations("main").len(), 1, "denials do not rotate");

    h.swamp(&["trace", "last"])
        .assert()
        .success()
        .stdout(predicates::str::contains("3 tool calls were auto-denied"));
}

/// A denial is only actionable if the trace names the tool that was refused: the cause of a
/// worker that could not run its tests is one line, not a dig through stream.jsonl.
#[test]
fn the_trace_names_the_tools_that_were_auto_denied() {
    let h = Harness::new().scenario(
        "main",
        Scenario::claude_denied_tools(&["Bash", "Bash", "Edit"]),
    );

    h.swamp(&["run", "--no-brain", TASK]).assert().code(4);

    let view = h.last_view();
    let node = view.nodes.values().next().expect("the node");
    let NodeState::Failed {
        failure: Failure::PermissionDenied { tools, .. },
    } = &node.state
    else {
        panic!("{:?}", node.state)
    };
    assert_eq!(
        tools,
        &["Bash".to_owned(), "Bash".to_owned(), "Edit".to_owned()]
    );

    h.swamp(&["trace", "last"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "3 tool calls were auto-denied: Bash x2, Edit",
        ));
}

/// WP8 acceptance 12: A worker that echoes a token must not put it in the journal.
#[test]
fn a_leaked_token_is_redacted_out_of_the_journal() {
    let h = Harness::new().scenario("main", Scenario::claude_leaks(SECRET));

    h.swamp(&["run", "--no-brain", TASK]).assert().success();

    let run = h.last_run();
    let journal = h.journal_text(run.run);
    assert!(
        !journal.contains(SECRET),
        "the token reached journal.jsonl: {journal}"
    );
    assert!(journal.contains("[redacted]"), "nothing was masked");
}

/// WP8 acceptance 12, second half: `stream.jsonl` and `stderr.log` are the worker's own fds: DESIGN 5.1
/// hands the files to the process and DESIGN 7.1 keeps them verbatim, so the redactor never
/// sees those bytes. Masking them needs a live pipe, which is the design decision this
/// architecture exists to avoid.
#[test]
#[ignore = "stream.jsonl and stderr.log are the worker's own fds (DESIGN 5.1, 7.1): the redactor never sees them"]
fn a_leaked_token_is_redacted_out_of_the_raw_files() {
    let h = Harness::new().scenario("main", Scenario::claude_leaks(SECRET));
    h.swamp(&["run", "--no-brain", TASK]).assert().success();

    let run = h.last_run();
    let view = h.last_view();
    let node = view.nodes.values().next().expect("the node");
    let stream = std::fs::read_to_string(run.stream(node.id)).expect("stream.jsonl");
    assert!(!stream.contains(SECRET), "the token reached stream.jsonl");
    let stderr = std::fs::read_to_string(run.stderr(node.id)).expect("stderr.log");
    assert!(!stderr.contains(SECRET), "the token reached stderr.log");
}

/// `~/.swamp/accounts.json` is machine-wide, so it keeps entries for ids this repo does not
/// configure. They were invisible: an old `main` with three infra failures sat in the file
/// with nothing to name it and no way to drop it.
#[test]
fn state_for_an_account_that_left_the_config_is_listed_and_droppable() {
    let h = Harness::new();
    let path = h.paths().accounts_state();
    let mut state = swamp::dispatch::persist::StateMap::new();
    state.insert(
        AccountId("retired".into()),
        swamp::dispatch::AccountState {
            consecutive_infra_failures: 3,
            lifetime_nodes: 12,
            ..Default::default()
        },
    );
    state.insert(AccountId("main".into()), Default::default());
    swamp::dispatch::persist::save_state(&path, &state).expect("writing accounts.json");

    h.swamp(&["accounts"])
        .assert()
        .success()
        .stdout(predicates::str::contains("not in config"))
        .stdout(predicates::str::contains("retired"));

    // Nothing drops it on its own, and dropping one entry leaves the others alone.
    h.swamp(&["accounts", "reset", "retired"])
        .assert()
        .success()
        .stdout(predicates::str::contains("dropped"));
    let after = h.accounts_state();
    assert!(
        !after.contains_key(&AccountId("retired".into())),
        "{after:?}"
    );
    assert!(after.contains_key(&AccountId("main".into())), "{after:?}");

    h.swamp(&["accounts"])
        .assert()
        .success()
        .stdout(predicates::str::contains("not in config").not());
    h.swamp(&["accounts", "reset", "retired"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no recorded state"));
}

/// USAGE 4.7: a pool with nothing left is a wait, not a silent stall. The node blocks until
/// its own `--timeout`, and `swamp run` says so on stderr before it starts waiting.
#[test]
fn every_account_at_its_limit_prints_one_notice_on_stderr() {
    let resets_at = epoch_in(3600);
    let h = Harness::new()
        .with_accounts(2, 0)
        .scenario("main", Scenario::claude_rate_limited(resets_at))
        .scenario("alt", Scenario::claude_rate_limited(resets_at));

    // Burns both subscriptions. The cooldown is machine-wide, so the next run starts blocked.
    h.swamp(&["run", "--no-brain", TASK]).assert().code(3);

    let out = h
        .swamp(&["run", "--no-brain", "--timeout", "3s", TASK])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "every anthropic account is at its limit",
        ))
        .stderr(predicates::str::contains("earliest reset"))
        .stderr(predicates::str::contains("ctrl-c to cancel"))
        .get_output()
        .clone();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr
            .lines()
            .filter(|l| l.contains("is at its limit"))
            .count(),
        1,
        "one notice per blocked node, not one per recheck: {stderr}"
    );

    // Nothing ran: the wait ended at the node deadline, and no wrapper was invoked again.
    assert_eq!(h.invocations("main").len(), 1);
    assert_eq!(h.invocations("alt").len(), 1);
    let blocked = h
        .journal_text(*h.runs().first().expect("the blocked run"))
        .lines()
        .filter(|l| l.contains("node_blocked"))
        .count();
    assert_eq!(blocked, 1, "one NodeBlocked journal line");
}

/// USAGE 2.1: every cross-process hand-off is keyed on `updated_at`. A disable that does not
/// stamp it is invisible to a running supervisor, which then writes its own entry over it.
#[test]
fn disabling_an_account_stamps_it_for_a_running_supervisor() {
    let h = Harness::new();
    let before = time::OffsetDateTime::now_utc() - time::Duration::seconds(1);

    h.swamp(&["accounts", "disable", "main"]).assert().success();
    let state = h.accounts_state();
    let main = state.get(&AccountId("main".into())).expect("main");
    assert_eq!(main.health, Health::Disabled);
    let stamped = main.updated_at.expect("disable stamps the entry");
    assert!(stamped > before, "{stamped} is not the moment of the edit");

    // A supervisor that loaded the file earlier yields to it, instead of erasing it.
    let path = h.paths().accounts_state();
    let mut stale = swamp::dispatch::persist::StateMap::new();
    stale.insert(
        AccountId("main".into()),
        swamp::dispatch::AccountState {
            updated_at: Some(before),
            ..Default::default()
        },
    );
    let merged = swamp::dispatch::persist::merge_state(&path, &stale).expect("merge");
    assert_eq!(merged[&AccountId("main".into())].health, Health::Disabled);
}
