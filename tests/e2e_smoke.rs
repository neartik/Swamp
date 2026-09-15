//! WP8: the smallest runnable thing, end to end, through the real binary and fake CLIs.

mod support;

use support::{Harness, MID, Scenario};
use swamp::model::core::NodeState;

const TASK: &str = "fix the flaky test in api.rs";

/// WP8 acceptance 1: `swamp run --no-brain --tier mid`: one process, one worktree, one node, one diff.
#[test]
fn a_no_brain_run_journals_one_node_and_captures_its_patch() {
    let h = Harness::new().scenario("main", Scenario::claude().edits("fixed.txt", "patched\n"));

    h.swamp(&["run", "--no-brain", "--tier", "mid", TASK])
        .assert()
        .success();

    let run = h.last_run();
    let journal = h.journal_text(run.run);
    assert!(journal.contains(r#""ev":"run_started""#), "{journal}");
    assert!(journal.contains(r#""ev":"run_finished""#), "{journal}");
    // WP2 renamed the NodeSpawned payload key to break a flatten collision with the line's node.
    assert!(journal.contains(r#""ev":"node_spawned""#), "{journal}");
    assert!(journal.contains(r#""record":{"#), "{journal}");

    let view = h.last_view();
    assert_eq!(view.nodes.len(), 1, "exactly one worker node");
    assert_eq!(view.tree().len(), 1, "the node is the root of the tree");
    let node = view.nodes.values().next().expect("the node");
    assert_eq!(node.state, NodeState::Succeeded);
    assert_eq!(node.model.as_deref(), Some(MID), "the configured mid model");
    assert_eq!(node.account.as_ref().map(|a| a.0.as_str()), Some("main"));
    assert_eq!(node.tier, swamp::model::core::Tier::Mid);

    let prompt = std::fs::read_to_string(run.prompt(node.id)).expect("prompt.md");
    assert_eq!(prompt, TASK, "the prompt is the task, byte for byte");

    let stream = std::fs::read(run.stream(node.id)).expect("stream.jsonl");
    let fixture = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs/ref/claude-stream-sample.jsonl"),
    )
    .expect("the recorded fixture");
    assert_eq!(stream, fixture, "the raw stream is kept verbatim");

    let work = node.work.as_ref().expect("a work result");
    assert!(!work.empty, "the worker changed a file");
    assert!(work.branch.starts_with("swamp/"), "branch {}", work.branch);
    let patch = std::fs::read_to_string(&work.patch).expect("patch.diff");
    assert!(patch.contains("fixed.txt"), "{patch}");
    assert_eq!(work.patch, run.patch(node.id));

    let worktree = node.workspace.path();
    assert!(
        worktree.starts_with(h.worktree_root()),
        "{worktree} is not under {}",
        h.worktree_root()
    );
    assert!(
        !worktree.starts_with(&h.repo),
        "worktrees live outside the repo"
    );

    let exclude = std::fs::read_to_string(h.repo.join(".git/info/exclude")).expect("exclude");
    assert!(exclude.contains(".swamp/"), "{exclude}");
    assert!(!h.is_dirty(), "the user's checkout is untouched");

    h.swamp(&["trace", "last"])
        .assert()
        .success()
        .stdout(predicates::str::contains(TASK));
}

/// WP8 acceptance 2: The same path on codex, including the banner line the CLI writes before its JSON.
#[test]
fn a_codex_run_succeeds_and_keeps_the_banner_line_out_of_the_stream() {
    let h = Harness::new()
        .with_accounts(0, 1)
        .scenario("codex", Scenario::codex().edits("fixed.rs", "patched\n"));

    h.swamp(&["run", "--no-brain", "--provider", "openai", TASK])
        .assert()
        .success();

    let run = h.last_run();
    let view = h.last_view();
    let node = view.nodes.values().next().expect("the node");
    assert_eq!(node.state, NodeState::Succeeded);
    assert_eq!(node.provider, swamp::model::core::Provider::Openai);
    assert_eq!(
        node.summary.as_deref(),
        Some("pong"),
        "codex -o last message"
    );

    let noise = std::fs::read_to_string(run.noise(node.id)).expect("noise.log");
    let lines: Vec<&str> = noise.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "exactly one unparsable line: {noise}");
    assert!(lines[0].contains("Reading additional input"), "{noise}");
    assert_eq!(node.unparsed_lines, 1);

    let call = h.invocations("codex").pop().expect("one invocation");
    assert_eq!(call.argv.get(1).map(String::as_str), Some("exec"));
    assert_eq!(call.arg("-m"), Some(MID));
}

/// WP8 acceptance 5: Three tasks at once: three worktrees, three branches, three patches, one clean checkout.
#[test]
fn parallel_runs_stay_in_their_own_worktrees() {
    let h = Harness::new()
        .with_accounts(3, 0)
        .scenario("main", Scenario::claude().edits("from-main.txt", "main\n"))
        .scenario("alt", Scenario::claude().edits("from-alt.txt", "alt\n"))
        .scenario(
            "third",
            Scenario::claude().edits("from-third.txt", "third\n"),
        );

    let children: Vec<std::process::Child> = ["main", "alt", "third"]
        .iter()
        .map(|id| h.spawn(&["run", "--no-brain", "--account", id, TASK]))
        .collect();
    for child in children {
        let out = child.wait_with_output().expect("a parallel run finishes");
        assert!(
            out.status.success(),
            "a parallel run failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    assert_eq!(h.runs().len(), 3, "three runs");
    let mut worktrees = std::collections::BTreeSet::new();
    let mut branches = std::collections::BTreeSet::new();
    for run in h.runs() {
        let view = h.view(run);
        assert_eq!(view.nodes.len(), 1);
        let node = view.nodes.values().next().expect("the node");
        assert_eq!(node.state, NodeState::Succeeded);
        let work = node.work.as_ref().expect("a work result");
        assert!(work.patch.is_file(), "no patch at {}", work.patch);
        worktrees.insert(node.workspace.path().to_string());
        branches.insert(work.branch.clone());
    }
    assert_eq!(
        worktrees.len(),
        3,
        "three distinct worktrees: {worktrees:?}"
    );
    assert_eq!(branches.len(), 3, "three distinct branches: {branches:?}");
    assert_eq!(
        h.git(&["branch", "--list", "swamp/*"]).lines().count(),
        3,
        "three swamp branches in the repo"
    );

    for name in ["from-main.txt", "from-alt.txt", "from-third.txt"] {
        assert!(
            !h.repo.join(name).exists(),
            "{name} leaked into the user's checkout"
        );
    }
    assert!(!h.is_dirty(), "the user's checkout is untouched");
}

/// WP8 acceptance 11: Doctor is the first-run check, and it is the one that catches two names pointing at
/// one subscription.
#[test]
fn doctor_passes_on_a_healthy_setup_and_fails_on_a_shared_config_dir() {
    let h = Harness::new().with_accounts(2, 0);
    h.swamp(&["doctor"]).assert().success();

    let twin = Harness::new();
    let shared = twin.accounts[0].config_dir.clone();
    let twin = twin.with_toml(&format!(
        "\n[[accounts]]\nid = \"twin\"\nprovider = \"anthropic\"\nexec = \"claude-main\"\n\
         env = {{ CLAUDE_CONFIG_DIR = \"{shared}\" }}\n"
    ));
    twin.swamp(&["doctor"])
        .assert()
        .failure()
        .stdout(predicates::str::contains("main and twin"))
        .stdout(predicates::str::contains("ONE subscription"));
}

/// WP8 acceptance 13: No network, by construction: nothing in the dependency tree can open a socket to a
/// provider. The fixtures and the fakes are the only sources of provider bytes.
#[test]
fn the_dependency_tree_contains_no_http_client() {
    let lock = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.lock"),
    )
    .expect("Cargo.lock");
    let names: Vec<&str> = lock
        .lines()
        .filter_map(|l| l.strip_prefix("name = \""))
        .filter_map(|l| l.strip_suffix('"'))
        .collect();
    for banned in [
        "reqwest",
        "hyper",
        "hyper-util",
        "curl",
        "ureq",
        "isahc",
        "surf",
        "attohttpc",
        "h2",
        "h3",
        "tungstenite",
        "tokio-tungstenite",
        "native-tls",
        "openssl",
        "rustls",
        "eventsource-stream",
        "async-openai",
        "aws-sdk-bedrockruntime",
    ] {
        assert!(
            !names.contains(&banned),
            "`{banned}` is in the dependency tree: swamp must not be able to reach a provider \
             over the network"
        );
    }
}
