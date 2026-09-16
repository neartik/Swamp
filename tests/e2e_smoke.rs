//! WP8: the smallest runnable thing, end to end, through the real binary and fake CLIs.

mod support;

use predicates::prelude::*;
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

/// A worker inherits the operator's own CLAUDE.md unless Swamp says otherwise, and then it
/// orchestrates instead of working: it spawns subagents, asks questions nobody can answer and
/// returns boilerplate. The role prompt and the tool policy both have to reach the argv.
#[test]
fn a_worker_is_launched_with_its_role_and_the_configured_tool_policy() {
    let h = Harness::new()
        .scenario("main", Scenario::claude().edits("fixed.txt", "patched\n"))
        .with_toml(
            r#"
[providers.anthropic.worker]
permission_mode = "acceptEdits"
allow_tools = ["Bash"]
deny_tools = ["Task", "Agent"]
"#,
        );

    h.swamp(&["run", "--no-brain", TASK]).assert().success();

    let call = h.invocations("main").pop().expect("one invocation");
    assert_eq!(call.arg("--permission-mode"), Some("acceptEdits"));
    assert_eq!(call.arg("--permission-prompts"), Some("none"));
    assert_eq!(
        call.arg("--allowed-tools"),
        Some("Bash"),
        "acceptEdits denies Bash unless it is allowed by name: {:?}",
        call.argv
    );
    let denied = call.argv.iter().position(|a| a == "--disallowed-tools");
    let denied = denied.expect("the deny list reaches the argv");
    assert_eq!(&call.argv[denied + 1..denied + 3], ["Task", "Agent"]);

    let role = call
        .arg("--append-system-prompt")
        .expect("the worker role prompt");
    assert!(role.contains("Swamp worker"), "{role}");
    assert!(
        role.contains(&call.cwd),
        "the role names the worktree: {role}"
    );
    assert!(role.contains("Do not spawn subagents"), "{role}");
    assert!(role.contains("Never ask a question"), "{role}");

    // The task itself stays on stdin, byte for byte: the role rides on the flag.
    let run = h.last_run();
    let node = h.last_view().nodes.values().next().expect("the node").id;
    assert_eq!(
        std::fs::read_to_string(run.prompt(node)).expect("prompt.md"),
        TASK
    );
}

/// DESIGN 7.1 lists `nodes/<short>/result.json`, and the brain's `swamp_result` reads the same
/// NodeResult. Neither was written, and the file list was empty for a node with a patch.
#[test]
fn a_finished_node_writes_result_json_with_the_files_git_saw() {
    // The recorded stream announces no edit at all, so files can only come from the diff.
    let h = Harness::new().scenario("main", Scenario::claude().edits("fixed.txt", "patched\n"));

    h.swamp(&["run", "--no-brain", TASK]).assert().success();

    let run = h.last_run();
    let node = h.last_view().nodes.values().next().expect("the node").id;
    let body = std::fs::read_to_string(run.result(node)).expect("nodes/<short>/result.json");
    let result: serde_json::Value = serde_json::from_str(&body).expect("valid json");

    assert_eq!(result["state"], "succeeded");
    assert_eq!(result["ok"], true);
    assert_eq!(result["node"], node.0.to_string());
    let files = result["files"].as_array().expect("a file list");
    assert_eq!(files.len(), 1, "{body}");
    assert_eq!(files[0]["path"], "fixed.txt");
    assert_eq!(files[0]["source"], "git", "git is authoritative: {body}");
    assert_eq!(result["insertions"], 1);

    // The same list reaches the tree the brain and the user read.
    let node = h.last_view().nodes.remove(&node).expect("the node");
    assert_eq!(
        node.files
            .iter()
            .map(|f| f.path.as_str())
            .collect::<Vec<_>>(),
        vec!["fixed.txt"]
    );
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

    // The account's quota is read out of band once the node ends, so `exec` is no longer
    // the wrapper's only invocation.
    let call = h
        .invocations("codex")
        .into_iter()
        .find(|c| c.argv.get(1).map(String::as_str) == Some("exec"))
        .expect("one exec invocation");
    assert_eq!(call.arg("-m"), Some(MID));
    // `codex exec` has no --append-system-prompt, so the worker role rides in front of the task.
    assert!(call.stdin.contains("Swamp worker"), "{}", call.stdin);
    assert!(call.stdin.trim_end().ends_with(TASK), "{}", call.stdin);
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

/// Config discovery used to append `.swamp/config.toml` to the raw cwd while path discovery
/// walked up to the git root: from any subdirectory the project config was silently ignored.
#[test]
fn the_repo_config_is_read_from_a_subdirectory() {
    let h = Harness::new();
    h.install();
    std::fs::create_dir_all(h.repo.join(".swamp")).expect("dot swamp");
    std::fs::write(
        h.repo.join(".swamp").join("config.toml"),
        "[dispatch]\ndefault_tier = \"low\"\n",
    )
    .expect("repo config");
    let sub = h.repo.join("crates").join("api");
    std::fs::create_dir_all(&sub).expect("subdir");

    for dir in [&h.repo, &sub] {
        let out = h
            .swamp(&["config", "show"])
            .current_dir(dir)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("default_tier = \"low\""),
            "the repo config was not read from {dir}: {text}"
        );
    }
}

/// A CLI that rejects its own argv exits before writing a single stream line. The run has to
/// fail fast with the stderr in hand, not retry an argv error three times.
#[test]
fn a_worker_that_dies_before_its_first_line_fails_fast_with_its_stderr() {
    let h = Harness::new();
    let mut cmd = h.swamp(&["run", "--no-brain", "--tier", "mid", TASK]);
    // install() has just linked the wrapper to the shared fake binary; unlink before writing
    // or the write follows the symlink and replaces the fake for every other test.
    let wrapper = h.bin.join(&h.accounts[0].exec);
    std::fs::remove_file(&wrapper).expect("unlink the wrapper");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\ncat > /dev/null\n\
         echo \"error: option '--permission-mode <mode>' argument '' is invalid\" >&2\nexit 1\n",
    )
    .expect("wrapper");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    let started = std::time::Instant::now();
    let out = cmd.assert().code(4).get_output().clone();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "the run hung instead of failing fast"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--permission-mode"), "{stderr}");

    let view = h.last_view();
    let node = view.nodes.values().next().expect("one node");
    assert!(matches!(node.state, NodeState::Failed { .. }));
    assert_eq!(view.nodes.len(), 1, "an argv error is never retried");
}

/// `--stat` printed git's summary line with the clauses git omits, and a bare stat line for a
/// node that changed nothing at all.
#[test]
fn diff_stat_matches_git_and_says_so_when_there_is_no_patch() {
    // Two files, insertions only: git prints no "0 deletions(-)" clause and neither do we.
    let h = Harness::new().scenario(
        "main",
        Scenario::claude()
            .edits("one.txt", "added\n")
            .edits("two.txt", "added\n"),
    );
    h.swamp(&["run", "--no-brain", TASK]).assert().success();
    h.swamp(&["diff", "last", "--stat"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "2 files changed, 2 insertions(+)\n",
        ))
        .stdout(predicates::str::contains("deletion").not());

    // A worker that changed nothing has no patch to stat.
    let h = h.scenario("main", Scenario::claude());
    h.swamp(&["run", "--no-brain", TASK]).assert().success();
    let node = h
        .last_view()
        .nodes
        .values()
        .next()
        .expect("the node")
        .id
        .short();
    h.swamp(&["diff", "last", "--stat"])
        .assert()
        .success()
        .stdout(predicates::str::contains(format!(
            "no patch recorded for {node}"
        )));
}

/// `swamp diff <id>` and `swamp adopt <id>` used to see only the run `last` points at, so a
/// node from any earlier run answered "no node matches", and `last` was not a node spec at all.
#[test]
fn diff_adopt_and_trace_find_a_node_from_an_older_run() {
    let h = Harness::new().scenario(
        "main",
        Scenario::claude().edits("first.txt", "from the first run\n"),
    );
    h.swamp(&["run", "--no-brain", TASK]).assert().success();
    let older = h.last_view();
    let first = older.nodes.values().next().expect("the first node");
    let first_id = first.id.short();

    let h = h.scenario(
        "main",
        Scenario::claude().edits("second.txt", "from the second run\n"),
    );
    h.swamp(&["run", "--no-brain", TASK]).assert().success();
    assert_eq!(h.runs().len(), 2, "two recorded runs");
    assert_ne!(
        h.last_run().run,
        first.run_id,
        "the node is not in the last run"
    );

    h.swamp(&["diff", &first_id])
        .assert()
        .success()
        .stdout(predicates::str::contains("first.txt"));
    h.swamp(&["trace", "--node", &first_id]).assert().success();
    h.swamp(&["adopt", &first_id, "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains("adopted"));

    // `last` is a run alias everywhere, node commands included: one node, so it is that node.
    h.swamp(&["diff", "last"])
        .assert()
        .success()
        .stdout(predicates::str::contains("second.txt"));
    h.swamp(&["diff", "--", "-2"])
        .assert()
        .success()
        .stdout(predicates::str::contains("first.txt"));

    // A prefix that matches a node in both runs names neither.
    h.swamp(&["diff", "0"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("ambiguous"));
}

/// The event stream names absolute worktree paths and counts nothing, and its list was
/// preferred over git's: `result.json` carried `/…/worktrees/…/calc.py` with `0, 0`, and
/// `--stat` printed those paths with `| 0` and no insertion counts.
#[test]
fn the_file_list_is_gits_relative_paths_and_real_counts() {
    // The stream announces calc.py twice, the way two edits to one file look.
    let h = Harness::new().scenario(
        "main",
        Scenario::claude_edits_announced(&["calc.py", "tests/test_calc.py", "calc.py"]),
    );

    h.swamp(&["run", "--no-brain", TASK]).assert().success();

    let run = h.last_run();
    let node = h.last_view().nodes.values().next().expect("the node").id;
    let body = std::fs::read_to_string(run.result(node)).expect("result.json");
    let result: serde_json::Value = serde_json::from_str(&body).expect("valid json");
    let files = result["files"].as_array().expect("a file list");

    assert_eq!(files.len(), 2, "git counts files, not tool calls: {body}");
    let mut paths: Vec<&str> = files.iter().map(|f| f["path"].as_str().unwrap()).collect();
    paths.sort_unstable();
    assert_eq!(paths, ["calc.py", "tests/test_calc.py"], "{body}");
    for f in files {
        assert_eq!(f["source"], "git", "{body}");
        assert_eq!(f["added"], 1, "{body}");
        assert_eq!(f["removed"], 0, "{body}");
        assert_eq!(f["kind"], "add", "{body}");
    }
    assert_eq!(result["insertions"], 2, "{body}");

    h.swamp(&["diff", "last", "--stat"])
        .assert()
        .success()
        .stdout(predicates::str::contains(" calc.py            |    1 +\n"))
        .stdout(predicates::str::contains(" tests/test_calc.py |    1 +\n"))
        .stdout(predicates::str::contains(
            "2 files changed, 2 insertions(+)",
        ))
        .stdout(predicates::str::contains("worktrees").not());

    // The same list is what the tree renders.
    h.swamp(&["trace", "last"])
        .assert()
        .success()
        .stdout(predicates::str::contains("+2 -0   2 files"));
}

/// A setup refusal used to happen after the run was created and a node had been spawned:
/// the user got an empty trace block, a `failed` node and a run dir for work that never
/// started. The check belongs in front of all of it.
#[test]
fn a_dirty_tree_is_refused_before_any_run_exists() {
    let h = Harness::new().scenario("main", Scenario::claude().edits("fixed.txt", "patched\n"));
    std::fs::write(h.repo.join("api.rs"), "uncommitted\n").expect("dirty the checkout");

    h.swamp(&["run", "--no-brain", TASK])
        .assert()
        .code(1)
        .stdout(predicates::str::is_empty())
        .stderr(predicates::str::contains(
            "refusing to run: working tree is dirty",
        ));

    assert!(
        h.runs().is_empty(),
        "a refusal created a run: {:?}",
        h.runs()
    );
    assert!(h.invocations("main").is_empty(), "a worker was spawned");

    // --include-dirty is still the way through, and it does create a run.
    h.swamp(&["run", "--no-brain", "--include-dirty", TASK])
        .assert()
        .success();
    assert_eq!(h.runs().len(), 1);
}

/// `--help` promised the origin of every key; `--effective` prints the merged TOML under the
/// layers it was built from. The two have to describe the same command.
#[test]
fn config_show_effective_prints_what_its_help_promises() {
    let h = Harness::new();
    h.install();
    let help = String::from_utf8(
        h.swamp(&["config", "show", "--help"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .expect("utf8");
    assert!(
        !help.contains("origin of every key"),
        "nothing annotates a key with its layer: {help}"
    );
    assert!(help.contains("which layers it was built from"), "{help}");

    let text = String::from_utf8(
        h.swamp(&["config", "show", "--effective"])
            .current_dir(&h.repo)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .expect("utf8");
    assert!(text.contains("lowest priority first"), "{text}");
    assert!(text.contains("[dispatch]"), "{text}");
}

/// `--config` is a global flag and the highest-priority layer, so the command that counts the
/// layers must count the stack this invocation actually ran on.
#[test]
fn config_validate_counts_the_layer_the_invocation_added() {
    let h = Harness::new();
    h.install();
    let extra = h.repo.join("candidate.toml");
    std::fs::write(&extra, "[limits]\nworker_timeout = \"3m\"\n").expect("candidate layer");

    let plain = String::from_utf8(
        h.swamp(&["config", "validate"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .expect("utf8");
    assert!(plain.contains("ok: 2 layers"), "{plain}");

    let with_extra = String::from_utf8(
        h.swamp(&["--config", extra.as_str(), "config", "validate"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .expect("utf8");
    assert!(with_extra.contains("ok: 3 layers"), "{with_extra}");
}
