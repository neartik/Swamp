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
    for w in &workers {
        assert_eq!(w.parent, Some(brain.id), "worker {} is not a child", w.id);
        assert_eq!(w.state, NodeState::Succeeded);
        assert!(w.work.as_ref().is_some_and(|work| !work.empty));
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
