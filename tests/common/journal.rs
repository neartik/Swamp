//! The journal lines the fold, trace and board suites share.

use camino::Utf8PathBuf;
use std::str::FromStr;
use swamp::dispatch::policy::Ineligible;
use swamp::ids::{CallSeq, DispatchId, NodeId, RunId};
use swamp::journal::record::{JournalEvent, JournalLine};
use swamp::model::core::{
    AccountId, Cost, CostBasis, NodeKind, NodeState, Provider, Tier, Usage, WorkspaceRef,
};
use swamp::model::dispatch::{DispatchCounts, DispatchRecord, Phase, TaskRef};
use swamp::model::event::WorkerEvent;
use swamp::model::failure::{Detector, Failure};
use swamp::model::node::NodeRecord;
use time::OffsetDateTime;

const CROCKFORD: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

pub fn ulid_text(i: usize) -> String {
    let hi = CROCKFORD[(i / 32) % 32] as char;
    let lo = CROCKFORD[i % 32] as char;
    format!("01ARZ3NDEKTSV4RRFFQ69G5F{hi}{lo}")
}

pub fn nid(i: usize) -> NodeId {
    NodeId::from_str(&ulid_text(i)).expect("node id")
}

pub fn rid(i: usize) -> RunId {
    RunId::from_str(&ulid_text(i)).expect("run id")
}

pub fn at(offset: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + offset).expect("timestamp")
}

pub fn record(id: NodeId, logical: NodeId, parent: Option<NodeId>, attempt: u32) -> NodeRecord {
    NodeRecord {
        id,
        run_id: rid(0),
        parent,
        logical,
        attempt,
        retry_of: None,
        kind: if parent.is_none() {
            NodeKind::Brain
        } else {
            NodeKind::Worker
        },
        title: format!("task {}", logical.short()),
        prompt_path: Utf8PathBuf::from("prompt.md"),
        prompt_sha256: String::new(),
        provider: Provider::Anthropic,
        account: None,
        exec: None,
        argv: Vec::new(),
        model: None,
        tier: Tier::Mid,
        workspace: WorkspaceRef::ReadOnly {
            path: Utf8PathBuf::from("/repo"),
        },
        session: None,
        state: NodeState::Queued,
        created_at: at(0),
        started_at: None,
        ended_at: None,
        usage: Usage::default(),
        cost: None,
        exit: None,
        files: Vec::new(),
        work: None,
        summary: None,
        stream_offset: 0,
        unparsed_lines: 0,
        depth: 1,
        dispatch: None,
    }
}

pub fn spawned(id: NodeId, logical: NodeId, parent: Option<NodeId>, attempt: u32) -> JournalEvent {
    JournalEvent::NodeSpawned {
        node: Box::new(record(id, logical, parent, attempt)),
    }
}

pub fn line(seq: u64, node: Option<NodeId>, event: JournalEvent) -> JournalLine {
    JournalLine {
        seq,
        at: at(seq as i64),
        run: rid(0),
        node,
        event,
    }
}

pub fn finished(state: NodeState, cost: Option<f64>) -> JournalEvent {
    JournalEvent::NodeFinished {
        state,
        exit: None,
        usage: Usage {
            input_tokens: 100,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 10,
            reasoning_tokens: 0,
        },
        cost: cost.map(|usd| Cost {
            usd,
            basis: CostBasis::Reported,
        }),
        work: None,
        summary: None,
        files: Vec::new(),
        unparsed_lines: 0,
    }
}

pub fn did(i: usize) -> DispatchId {
    DispatchId::from_str(&ulid_text(i)).expect("dispatch id")
}

pub fn changed(from: Phase, to: NodeState, why: &str) -> JournalEvent {
    JournalEvent::NodeStateChanged {
        from,
        to,
        why: why.to_owned(),
    }
}

pub fn leased(account: &str) -> NodeState {
    NodeState::Leased {
        account: AccountId(account.into()),
    }
}

pub fn running(pid: i32) -> NodeState {
    NodeState::Running {
        pid,
        pgid: pid,
        since: at(0),
    }
}

pub fn rate_limited() -> Failure {
    Failure::RateLimited {
        resets_at: None,
        scope: swamp::model::core::LimitScope::FiveHour,
        detected_by: Detector::Telemetry,
        evidence: "usage limit".into(),
    }
}

pub fn attempt(id: NodeId, logical: NodeId, n: u32, d: DispatchId) -> JournalEvent {
    let mut rec = record(id, logical, Some(nid(0)), n);
    rec.dispatch = Some(d);
    rec.depth = 1;
    rec.state = leased("main");
    JournalEvent::NodeSpawned {
        node: Box::new(rec),
    }
}

pub fn started(pid: i32) -> JournalEvent {
    JournalEvent::ProcessStarted {
        pid,
        pgid: pid,
        argv: vec!["claude".into()],
        env_overrides: Default::default(),
        cwd: Utf8PathBuf::from("/wt"),
    }
}

pub fn exited() -> JournalEvent {
    JournalEvent::ProcessExited {
        code: Some(0),
        signal: None,
    }
}

pub fn issued(
    caller: NodeId,
    d: DispatchId,
    seq: Option<u64>,
    tasks: &[(NodeId, &str)],
) -> JournalEvent {
    JournalEvent::DispatchIssued {
        record: Box::new(DispatchRecord {
            id: d,
            run: rid(0),
            caller,
            call_seq: seq.map(CallSeq),
            wait: true,
            max_wait_s: Some(600),
            tasks: tasks
                .iter()
                .map(|(logical, title)| TaskRef {
                    logical: *logical,
                    title: (*title).to_owned(),
                    tier: Tier::Mid,
                    provider: Provider::Anthropic,
                })
                .collect(),
            at: at(0),
        }),
    }
}

pub fn queued(logical: NodeId, d: DispatchId, title: &str, depth: u32) -> JournalEvent {
    JournalEvent::TaskQueued {
        logical,
        dispatch: d,
        title: title.to_owned(),
        tier: Tier::Mid,
        depth,
    }
}

pub fn tool_call(seq: u64, d: DispatchId) -> JournalEvent {
    JournalEvent::BrainToolCall {
        tool: "swamp_dispatch".into(),
        args_sha256: String::new(),
        args_path: Utf8PathBuf::from(format!("tools/{seq}-swamp_dispatch.json")),
        call_seq: Some(CallSeq(seq)),
        dispatch: Some(d),
    }
}

/// A fails over, its second attempt runs E one level down, B is blocked first, C is rejected
/// and D is still queued.
pub fn schema_2() -> Vec<JournalLine> {
    let (brain, a, b, c, d, e) = (nid(0), nid(1), nid(2), nid(3), nid(4), nid(5));
    let (a1, a2, b1, e1) = (nid(11), nid(12), nid(21), nid(51));
    let (d1, d2, d3) = (did(40), did(41), did(42));
    let mut nested = record(e1, e, Some(a), 1);
    (nested.dispatch, nested.depth, nested.state) = (Some(d3), 2, leased("alt"));
    let events: Vec<(Option<NodeId>, JournalEvent)> = vec![
        (
            None,
            JournalEvent::RunStarted {
                swamp_version: "0.1.0".into(),
                schema: 2,
                argv: vec!["swamp".into(), "run".into()],
                cwd: Utf8PathBuf::from("/repo"),
                repo: Some(Utf8PathBuf::from("/repo")),
                base: Some("HEAD".into()),
                config_sha256: "abc".into(),
                task: Some("t".into()),
            },
        ),
        (Some(brain), spawned(brain, brain, None, 1)),
        (
            Some(brain),
            JournalEvent::NodeUsage {
                usage: Usage::default(),
                cost: Some(Cost {
                    usd: 1.0,
                    basis: CostBasis::Reported,
                }),
            },
        ),
        (Some(brain), tool_call(1, d1)),
        (
            Some(brain),
            issued(brain, d1, Some(1), &[(a, "lex"), (b, "parse")]),
        ),
        (Some(a), queued(a, d1, "lex", 1)),
        (Some(b), queued(b, d1, "parse", 1)),
        (
            Some(a),
            changed(Phase::Queued, leased("main"), "leased main"),
        ),
        (Some(a1), attempt(a1, a, 1, d1)),
        (Some(a1), started(101)),
        (Some(a1), changed(Phase::Leased, running(101), "pid 101")),
        (Some(a1), exited()),
        (
            Some(a1),
            finished(
                NodeState::Failed {
                    failure: rate_limited(),
                },
                Some(0.1),
            ),
        ),
        (
            Some(a1),
            changed(
                Phase::Running,
                NodeState::Failed {
                    failure: rate_limited(),
                },
                "rate_limited",
            ),
        ),
        (
            Some(a),
            changed(Phase::Leased, NodeState::Queued, "rotating"),
        ),
        (
            Some(b),
            JournalEvent::NodeBlocked {
                until: at(600),
                why: "main cooling".into(),
                ineligible: vec![(AccountId("main".into()), Ineligible::Cooling)],
            },
        ),
        (
            Some(b),
            changed(
                Phase::Queued,
                NodeState::Blocked {
                    until: at(600),
                    why: "main cooling".into(),
                },
                "main cooling",
            ),
        ),
        (Some(a), changed(Phase::Queued, leased("alt"), "leased alt")),
        (Some(a2), attempt(a2, a, 2, d1)),
        (Some(a2), started(102)),
        (Some(a2), changed(Phase::Leased, running(102), "pid 102")),
        (Some(a), issued(a, d3, None, &[(e, "tokens")])),
        (Some(e), queued(e, d3, "tokens", 2)),
        (Some(e), changed(Phase::Queued, leased("alt"), "leased alt")),
        (
            Some(e1),
            JournalEvent::NodeSpawned {
                node: Box::new(nested),
            },
        ),
        (Some(e1), finished(NodeState::Succeeded, Some(0.05))),
        (
            Some(e1),
            changed(Phase::Leased, NodeState::Succeeded, "succeeded"),
        ),
        (
            Some(e),
            changed(Phase::Leased, NodeState::Succeeded, "succeeded"),
        ),
        (
            Some(a),
            JournalEvent::DispatchSettled {
                dispatch: d3,
                counts: DispatchCounts {
                    succeeded: 1,
                    ..DispatchCounts::default()
                },
                cost: Some(Cost {
                    usd: 0.05,
                    basis: CostBasis::Reported,
                }),
            },
        ),
        (Some(a2), exited()),
        (Some(a2), finished(NodeState::Succeeded, Some(0.5))),
        (
            Some(a2),
            changed(Phase::Running, NodeState::Succeeded, "succeeded"),
        ),
        (
            Some(a),
            changed(Phase::Leased, NodeState::Succeeded, "succeeded"),
        ),
        (
            Some(b),
            changed(Phase::Blocked, leased("main"), "leased main"),
        ),
        (Some(b1), attempt(b1, b, 1, d1)),
        (Some(b1), finished(NodeState::Succeeded, Some(0.25))),
        (
            Some(b1),
            changed(Phase::Leased, NodeState::Succeeded, "succeeded"),
        ),
        (
            Some(b),
            changed(Phase::Leased, NodeState::Succeeded, "succeeded"),
        ),
        (
            Some(brain),
            JournalEvent::DispatchSettled {
                dispatch: d1,
                counts: DispatchCounts {
                    succeeded: 2,
                    ..DispatchCounts::default()
                },
                cost: Some(Cost {
                    usd: 0.85,
                    basis: CostBasis::Reported,
                }),
            },
        ),
        (Some(brain), tool_call(2, d2)),
        (
            Some(brain),
            issued(brain, d2, Some(2), &[(c, "docs"), (d, "bench")]),
        ),
        (
            Some(c),
            JournalEvent::DispatchRejected {
                dispatch: d2,
                logical: c,
                reason: Failure::WorkerError {
                    subtype: "max_nodes_per_run".into(),
                    detail: "run already spawned 3 nodes".into(),
                },
            },
        ),
        (Some(d), queued(d, d2, "bench", 1)),
    ];
    events
        .into_iter()
        .enumerate()
        .map(|(i, (node, event))| line(i as u64, node, event))
        .collect()
}

/// A tool call as the brain CLI's own stream reports it.
pub fn brain_call(name: &str) -> JournalEvent {
    JournalEvent::NodeEvent {
        offset: 0,
        event: WorkerEvent::ToolCall {
            id: format!("toolu_{name}"),
            name: name.to_owned(),
            summary: String::new(),
        },
    }
}

/// `schema_2` with `reads` brain reads before its first dispatch, swamp's own dispatch call
/// among them as the CLI names it, and two more reads after the dispatch.
pub fn schema_2_with_reads(reads: usize) -> Vec<JournalLine> {
    let brain = nid(0);
    let lines = schema_2();
    let first = lines
        .iter()
        .position(|l| matches!(l.event, JournalEvent::BrainToolCall { .. }))
        .expect("schema_2 dispatches");
    let mut before: Vec<(Option<NodeId>, JournalEvent)> = (0..reads)
        .map(|i| {
            (
                Some(brain),
                brain_call(if i % 2 == 0 { "Read" } else { "Grep" }),
            )
        })
        .collect();
    before.push((Some(brain), brain_call("mcp__swamp__swamp_dispatch")));
    let after = [
        (Some(brain), brain_call("Read")),
        (Some(brain), brain_call("Bash")),
    ];
    let mut events: Vec<(Option<NodeId>, JournalEvent)> = Vec::new();
    for (i, l) in lines.into_iter().enumerate() {
        if i == first {
            events.append(&mut before);
        }
        let issued = matches!(l.event, JournalEvent::DispatchIssued { .. });
        events.push((l.node, l.event));
        if issued && !after.is_empty() {
            events.extend(after.iter().cloned());
        }
    }
    events
        .into_iter()
        .enumerate()
        .map(|(i, (node, event))| line(i as u64, node, event))
        .collect()
}
