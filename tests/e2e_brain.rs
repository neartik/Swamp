//! WP8: the brain dispatches through the in-process MCP server, over the stdio bridge.

mod support;

use support::{Harness, Scenario};
use swamp::model::core::{NodeKind, NodeState};

const TASK: &str = "split the parser work in two";

/// WP8 acceptance 6: A real `swamp_dispatch` tool call: the brain is a process, the bridge is a process, and
/// the two workers it creates are children of the brain node.
#[test]
fn the_brain_dispatches_two_workers_and_they_hang_off_its_node() {
    let h = Harness::new().max_concurrency(3).scenario(
        "main",
        Scenario::claude()
            .edits("worker-{n}.txt", "written by invocation {n}\n")
            .dispatches("lex", "write the lexer")
            .dispatches("parse", "write the parser"),
    );

    h.swamp(&["run", TASK]).assert().success();

    let view = h.last_view();
    let brain = view
        .nodes
        .values()
        .find(|n| n.kind == NodeKind::Brain)
        .expect("a brain node");
    assert_eq!(brain.id.0, h.last_run().run.0, "the brain is the run root");

    let workers: Vec<_> = view
        .nodes
        .values()
        .filter(|n| n.kind == NodeKind::Worker)
        .collect();
    assert_eq!(workers.len(), 2, "two worker nodes");
    let run = h.last_run();
    for w in &workers {
        assert_eq!(w.parent, Some(brain.id), "worker {} is not a child", w.id);
        assert_eq!(w.state, NodeState::Succeeded);
        assert!(w.work.as_ref().is_some_and(|work| !work.empty));
        // What swamp_result hands back is this file, and its file list comes from the diff.
        let body = std::fs::read_to_string(run.result(w.id)).expect("result.json");
        let result: serde_json::Value = serde_json::from_str(&body).expect("valid json");
        assert_eq!(result["state"], "succeeded", "{body}");
        let files = result["files"].as_array().expect("a file list");
        assert_eq!(
            files.len(),
            1,
            "a node with a patch lists its files: {body}"
        );
        assert_eq!(files[0]["source"], "git");
    }
    let titles: std::collections::BTreeSet<&str> =
        workers.iter().map(|w| w.title.as_str()).collect();
    assert_eq!(
        titles,
        ["lex", "parse"].into_iter().collect(),
        "the brain's own titles reached the tree"
    );

    assert!(
        !h.is_dirty(),
        "the brain reads the user's checkout, it never writes to it"
    );

    let tree = view.tree();
    assert_eq!(tree.len(), 3, "brain plus two children: {tree:?}");
    assert_eq!(tree[0].logical, brain.id);
    assert!(tree[1].depth == 1 && tree[2].depth == 1, "{tree:?}");

    // The turn reached the brain as one stream-json user line on fd0.
    let stdin = h.brain_stdin("main");
    let turn: serde_json::Value =
        serde_json::from_str(stdin.first().expect("a user turn")).expect("valid json");
    assert_eq!(turn["type"], "user");
    assert_eq!(turn["message"]["content"][0]["text"], TASK);

    // The MCP bridge is spawned by absolute path: the brain's cwd is not ours.
    let brain_call = h
        .invocations("main")
        .into_iter()
        .find(|c| c.arg("--mcp-config").is_some())
        .expect("the brain invocation");
    let config: serde_json::Value =
        serde_json::from_str(brain_call.arg("--mcp-config").expect("the flag")).expect("json");
    let command = config["mcpServers"]["swamp"]["command"]
        .as_str()
        .expect("a bridge command");
    assert!(
        std::path::Path::new(command).is_absolute(),
        "the bridge command `{command}` is not absolute"
    );
    assert_eq!(config["mcpServers"]["swamp"]["args"][0], "mcp-bridge");
    assert!(
        std::path::Path::new(
            config["mcpServers"]["swamp"]["args"][2]
                .as_str()
                .expect("a socket path")
        )
        .is_absolute(),
        "the control socket is not an absolute path"
    );

    // Every Swamp tool is on the allow list: --permission-prompts none auto-denies anything
    // that is not, and an auto-denied swamp_dispatch leaves the brain with nothing to do.
    let allowed: Vec<&String> = brain_call
        .argv
        .iter()
        .skip_while(|a| *a != "--allowed-tools")
        .skip(1)
        .take_while(|a| !a.starts_with("--"))
        .collect();
    for name in swamp::mcp::tools::qualified_names() {
        assert!(
            allowed.contains(&&name),
            "{name} is missing from {allowed:?}"
        );
    }

    // Three invocations of one wrapper: one brain, two workers, none of them with MCP.
    let calls = h.invocations("main");
    assert_eq!(calls.len(), 3, "one brain and two workers");
    assert_eq!(
        calls
            .iter()
            .filter(|c| c.arg("--mcp-config").is_none())
            .count(),
        2,
        "workers get no MCP server at all"
    );
}

/// The brain spends a real subscription: a whole run's planning went to one account and
/// `swamp accounts` still showed it at 0 nodes and $0, so neither the operator nor the
/// history tie-break in selection could see what the brain had burned.
#[test]
fn the_brain_credits_its_node_cost_and_quota_to_its_account() {
    use swamp::model::core::{AccountId, LimitScope};

    // `prefer` decides which account the brain reserves; the worker gets the other one.
    let h = Harness::new().with_accounts(2, 0).prefer("alt").scenario(
        "alt",
        Scenario::claude().dispatches("lex", "write the lexer"),
    );

    h.swamp(&["run", TASK]).assert().success();

    let state = h.accounts_state();
    let brain = state
        .get(&AccountId("alt".into()))
        .expect("the brain's account is in accounts.json");
    assert_eq!(brain.lifetime_nodes, 1, "the brain is one node per run");
    assert!(
        brain.lifetime_cost_usd > 0.0,
        "the brain's spend never reached the pool: {brain:?}"
    );
    assert!(brain.updated_at.is_some(), "{brain:?}");
    let quota = brain
        .quota
        .as_ref()
        .expect("the brain's rate-limit telemetry");
    assert!(
        quota
            .windows
            .iter()
            .any(|w| w.scope == LimitScope::FiveHour && w.utilization > 0.0),
        "{quota:?}"
    );
    assert_eq!(
        state
            .get(&AccountId("main".into()))
            .map(|s| s.lifetime_nodes),
        Some(1),
        "the worker's account is counted exactly once"
    );

    // And the operator's view agrees with the file.
    h.swamp(&["accounts"])
        .assert()
        .success()
        .stdout(predicates::str::contains("0.06"));
}
