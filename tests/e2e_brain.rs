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

    // And the operator's view agrees with the file. The recorded sample carries fixed reset
    // instants, so both surfaces quote its windows only while they are still current.
    let live = quota
        .windows
        .iter()
        .any(|w| w.scope == LimitScope::FiveHour && w.is_current(time::OffsetDateTime::now_utc()));
    let accounts = h.swamp(&["accounts"]).assert().success();
    let accounts = String::from_utf8_lossy(&accounts.get_output().stdout).into_owned();
    assert_eq!(
        accounts.contains("0.06"),
        live,
        "`swamp accounts` and accounts.json disagree on the five-hour window: {accounts}"
    );
    let usage = h.swamp(&["usage"]).assert().success();
    let usage = String::from_utf8_lossy(&usage.get_output().stdout).into_owned();
    assert_eq!(
        usage.contains("6%"),
        live,
        "`swamp usage` and `swamp accounts` disagree on the same window: {usage}"
    );
}

/// UI 5: a piped chat prints what its own `/help` lists. Answering `unknown command /usage`
/// to a command the same session just advertised is the one thing it must not do.
#[test]
fn a_piped_chat_runs_the_commands_its_help_lists() {
    let h = Harness::new().scenario("main", Scenario::claude());
    let out = h
        .swamp(&["chat"])
        .write_stdin("/help\n/usage\n/accounts\n/quit\n")
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    assert!(stdout.contains("/usage"), "{stdout}");
    assert!(!stdout.contains("unknown command"), "{stdout}");
    assert!(
        stdout.contains("ACCOUNT"),
        "the usage table is missing: {stdout}"
    );
}

/// `docs/BOARD.md` sections 2.1-2.3: the board and `swamp watch` call a brain orphaned when its
/// node pidfile is not live. It has to exist, and it has to name the supervisor that drives the
/// brain: a resume-per-turn brain has no process of its own between turns.
#[test]
fn the_brain_node_writes_a_pidfile_naming_its_supervisor() {
    let h = Harness::new().scenario(
        "main",
        Scenario::claude()
            .edits("worker-{n}.txt", "written by invocation {n}\n")
            .dispatches("lex", "write the lexer"),
    );
    let child = h.spawn(&["run", TASK]);
    let supervisor = child.id();
    let out = child.wait_with_output().expect("swamp run exits");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let view = h.last_view();
    let brain = view
        .nodes
        .values()
        .find(|n| n.kind == NodeKind::Brain)
        .expect("a brain node");
    let pidfile = h.last_run().pidfile(brain.id);
    let text = std::fs::read_to_string(&pidfile)
        .unwrap_or_else(|e| panic!("the brain has no pidfile at {pidfile}: {e}"));
    let pid: u32 = text
        .split_whitespace()
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("unreadable pidfile {text:?}"));
    assert_eq!(pid, supervisor, "the brain pidfile names another process");
}

/// Two identical `swamp_dispatch` calls are two dispatches with two argument files.
#[test]
fn identical_dispatch_calls_are_distinct_dispatches() {
    use swamp::journal::record::{JournalEvent, JournalLine};
    use swamp::model::dispatch::{DispatchState, Phase};

    let h = Harness::new().max_concurrency(3).scenario(
        "main",
        Scenario::claude()
            .edits("worker-{n}.txt", "written by invocation {n}\n")
            .dispatches("lex", "write the lexer")
            .dispatch_calls(2),
    );
    h.swamp(&["run", TASK]).assert().success();

    let run = h.last_run();
    let lines: Vec<JournalLine> = h
        .journal_text(run.run)
        .lines()
        .map(|l| serde_json::from_str(l).expect("a journal line"))
        .collect();

    let calls: Vec<_> = lines
        .iter()
        .filter_map(|l| match &l.event {
            JournalEvent::BrainToolCall {
                tool,
                args_path,
                call_seq,
                dispatch,
                ..
            } if tool == "swamp_dispatch" => Some((args_path.clone(), *call_seq, *dispatch)),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_ne!(calls[0].1, calls[1].1, "each call has its own sequence");
    let ids: Vec<_> = calls
        .iter()
        .map(|c| c.2.expect("a dispatch call names its dispatch"))
        .collect();
    assert_ne!(ids[0], ids[1]);
    assert_ne!(
        calls[0].0, calls[1].0,
        "the second call overwrote the first"
    );
    for (path, seq, _) in &calls {
        let seq = seq.expect("a call sequence");
        assert!(
            path.ends_with(format!("{seq}-swamp_dispatch.json")),
            "{path}"
        );
        let args = std::fs::read_to_string(path).expect("the recorded arguments");
        assert!(args.contains("write the lexer"), "{args}");
    }
    let files = std::fs::read_dir(run.dir.join("tools"))
        .expect("the tools directory")
        .count();
    assert_eq!(files, 2);

    // The record precedes its first task, and the brain was told its id.
    for id in &ids {
        let issued = lines
            .iter()
            .position(
                |l| matches!(&l.event, JournalEvent::DispatchIssued { record } if record.id == *id),
            )
            .expect("the dispatch is journaled");
        let queued = lines
            .iter()
            .position(|l| matches!(&l.event, JournalEvent::TaskQueued { dispatch, .. } if *dispatch == *id))
            .expect("its task is journaled");
        assert!(issued < queued);
    }
    let brain_stream =
        std::fs::read_to_string(run.stream(swamp::NodeId(run.run.0))).expect("the brain's stream");
    for id in &ids {
        assert!(
            brain_stream.contains(&id.to_string()),
            "swamp_dispatch did not answer with {id}"
        );
    }

    let view = h.last_view();
    let workers: Vec<_> = view
        .nodes
        .values()
        .filter(|n| n.kind == NodeKind::Worker)
        .collect();
    assert_eq!(workers.len(), 2);
    for w in &workers {
        assert_eq!(w.depth, 1, "{}", w.id);
        assert!(w.dispatch.is_some_and(|d| ids.contains(&d)), "{}", w.id);
    }
    for id in &ids {
        let d = &view.dispatches[id];
        assert_eq!(d.state, DispatchState::Settled);
        assert_eq!(d.tasks.len(), 1);
        let cost = d.cost.expect("a settled dispatch carries its cost");
        let rollup = view.rollup(swamp::journal::fold::Scope::Dispatch(*id));
        assert!(rollup.cost_complete, "{id}: {rollup:?}");
        assert!(
            (cost.usd - rollup.cost_usd).abs() < 1e-9,
            "{id}: {cost:?} vs {rollup:?}"
        );
    }

    // A waiting task is visible before it is leased or spawned.
    for logical in ids.iter().flat_map(|id| &view.dispatches[id].tasks) {
        let at = |pred: &dyn Fn(&JournalLine) -> bool| {
            lines
                .iter()
                .position(pred)
                .unwrap_or_else(|| panic!("no such line for {logical}"))
        };
        let queued = at(
            &|l| matches!(&l.event, JournalEvent::TaskQueued { logical: q, .. } if q == logical),
        );
        let spawned = at(
            &|l| matches!(&l.event, JournalEvent::NodeSpawned { node } if node.logical == *logical),
        );
        let changed = at(&|l| {
            l.node == Some(*logical) && matches!(l.event, JournalEvent::NodeStateChanged { .. })
        });
        assert!(queued < spawned && queued < changed, "{logical}");
        assert_eq!(
            view.transitions[logical][0].from,
            Phase::Queued,
            "{logical}"
        );
    }

    // Chained per node, and never out of a terminal state.
    assert!(!view.transitions.is_empty());
    for (node, chain) in &view.transitions {
        for t in chain {
            assert!(!t.from.is_terminal(), "{node}: {chain:?}");
        }
        for w in chain.windows(2) {
            assert_eq!(Phase::from(&w[0].to), w[1].from, "{node}: {chain:?}");
        }
    }
    for w in &workers {
        let phases: Vec<Phase> = view.transitions[&w.id]
            .iter()
            .map(|t| Phase::from(&t.to))
            .collect();
        assert_eq!(phases, vec![Phase::Running, Phase::Succeeded], "{}", w.id);
        assert!(view.exited.contains(&w.id), "no ProcessExited for {}", w.id);
    }
}

/// `swamp replay` renders a run and must never change what the journal folds to.
#[test]
fn replay_leaves_the_run_view_unchanged() {
    let h = Harness::new().max_concurrency(3).scenario(
        "main",
        Scenario::claude()
            .edits("worker-{n}.txt", "written by invocation {n}\n")
            .dispatches("lex", "write the lexer")
            .dispatches("parse", "write the parser"),
    );
    h.swamp(&["run", TASK]).assert().success();

    let digest = |v: &swamp::RunView| {
        format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            v.nodes,
            v.dispatches,
            v.tasks,
            v.transitions,
            v.tree()
                .iter()
                .map(|r| (r.logical, r.depth, r.state.clone(), r.attempts.clone()))
                .collect::<Vec<_>>(),
            v.totals().cost_usd,
        )
    };
    let view = h.last_view();
    let before = digest(&view);
    let journal = h.journal_text(h.last_run().run);
    h.swamp(&["replay", "last"]).assert().success();
    assert_eq!(h.journal_text(h.last_run().run), journal);
    assert_eq!(digest(&h.last_view()), before);

    // A reparse rewrites the journal: the tree and the dispatch grouping have to survive it.
    let shape = |v: &swamp::RunView| {
        let tree: Vec<_> = v
            .tree()
            .iter()
            .map(|r| (r.logical, r.depth, r.state.clone()))
            .collect();
        let dispatches: Vec<_> = v
            .dispatches
            .values()
            .map(|d| (d.id, d.state, d.tasks.clone()))
            .collect();
        format!("{tree:?}|{dispatches:?}|{:?}", v.call_seq)
    };
    h.swamp(&["replay", "last", "--reparse"]).assert().success();
    let after = h.last_view();
    assert_ne!(
        h.journal_text(h.last_run().run),
        journal,
        "the journal was rewritten"
    );
    assert_eq!(shape(&after), shape(&view));
    for t in view.tasks.keys() {
        assert_eq!(after.state_of(*t), Some(NodeState::Succeeded), "{t}");
    }
}

/// P5: the brain's reads before its dispatch, not the dispatch call, count against the budget.
#[test]
fn the_brain_reads_before_its_dispatch_are_counted_against_the_budget() {
    let h = Harness::new().scenario(
        "main",
        Scenario::claude()
            .reads_first(&["src/lib.rs", "src/main.rs", "Cargo.toml"])
            .dispatches("lex", "write the lexer"),
    );
    // The repo layer, over the harness's user layer that already has a [limits] table.
    let dot_swamp = h.paths().dot_swamp;
    std::fs::create_dir_all(&dot_swamp).expect(".swamp");
    std::fs::write(
        dot_swamp.join("config.toml"),
        "[limits]\nbrain_read_budget = 2\n",
    )
    .expect("repo config");
    h.swamp(&["run", TASK]).assert().success();

    let view = h.last_view();
    let work = view.brain_self_work().expect("a brain run");
    assert_eq!(work.calls, 3, "{work:?}");
    assert!(work.dispatched && work.over(2));

    let stdout = |args: &[&str]| {
        let out = h.swamp(args).assert().success();
        String::from_utf8_lossy(&out.get_output().stdout).into_owned()
    };
    let over = "brain  3/2 calls before the first dispatch";
    let trace = stdout(&["trace"]);
    assert!(trace.contains(over), "{trace}");
    assert!(trace.contains("over limits.brain_read_budget"), "{trace}");
    let dispatches = stdout(&["dispatches"]);
    assert!(dispatches.contains(over), "{dispatches}");
    let json: serde_json::Value =
        serde_json::from_str(&stdout(&["dispatches", "--json"])).expect("json");
    assert_eq!(json["brain"]["calls"], 3);
    assert_eq!(json["brain"]["over_budget"], true);
    let board = stdout(&["board", "--once", "--run", "last"]);
    assert!(board.contains("brain 3/2 over"), "{board}");
}
