//! Journal fixtures the chat tests fold, and the pieces an `App` needs offline.

use crate::ids::{DispatchId, NodeId, RunId};
use crate::journal::fold::RunView;
use crate::journal::record::{JournalEvent, JournalLine, SCHEMA_VERSION};
use crate::model::core::{
    AccountId, ChangeKind, Cost, CostBasis, EvidenceSource, FileChange, NodeKind, NodeState,
    Provider, Tier, Usage, WorkspaceRef,
};
use crate::model::failure::Failure;
use crate::model::node::{NodeRecord, WorkResultRef};
use crate::ui::chat::app::App;
use crate::ui::chat::blocks::WelcomeInfo;
use crate::ui::chat::input::History;
use crate::ui::chat::theme::Theme;
use camino::Utf8PathBuf;
use std::str::FromStr;
use time::OffsetDateTime;

pub fn run_id() -> RunId {
    RunId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("run id")
}

/// `id(0)` is the brain: the run's root node, per `brain::build`.
pub fn id(n: u8) -> NodeId {
    if n == 0 {
        return NodeId(run_id().0);
    }
    NodeId::from_str(&format!("01ARZ3NDEKTSV4RRFFQ69G5F{n:02}")).expect("node id")
}

pub fn at(offset: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + offset).expect("time")
}

pub fn now() -> OffsetDateTime {
    at(200)
}

pub fn view_of(lines: Vec<JournalLine>) -> RunView {
    let mut view = RunView::default();
    for l in &lines {
        view.apply(l);
    }
    view
}

fn line(seq: u64, node: Option<NodeId>, event: JournalEvent) -> JournalLine {
    JournalLine {
        seq,
        at: at(seq as i64),
        run: run_id(),
        node,
        event,
    }
}

fn header() -> JournalLine {
    line(
        1,
        None,
        JournalEvent::RunStarted {
            swamp_version: "0.1.0".into(),
            schema: SCHEMA_VERSION,
            argv: vec!["swamp".into(), "chat".into()],
            cwd: Utf8PathBuf::from("/repo"),
            repo: Some(Utf8PathBuf::from("/repo")),
            base: Some("9f3c1ad".into()),
            config_sha256: "abc".into(),
            task: None,
        },
    )
}

fn brain() -> NodeRecord {
    NodeRecord {
        kind: NodeKind::Brain,
        title: "brain".into(),
        parent: None,
        tier: Tier::High,
        model: Some("claude-opus-4-20250514".into()),
        ..record(
            id(0),
            "brain",
            NodeState::Running {
                pid: 1,
                pgid: 1,
                since: at(1),
            },
        )
    }
}

fn record(node: NodeId, title: &str, state: NodeState) -> NodeRecord {
    NodeRecord {
        id: node,
        run_id: run_id(),
        parent: Some(id(0)),
        logical: node,
        attempt: 1,
        retry_of: None,
        kind: NodeKind::Worker,
        title: title.to_owned(),
        prompt_path: Utf8PathBuf::from("prompt.md"),
        prompt_sha256: String::new(),
        provider: Provider::Anthropic,
        account: Some(AccountId("main".into())),
        exec: Some("claude-main".into()),
        argv: Vec::new(),
        model: Some("claude-sonnet-4-20250514".into()),
        tier: Tier::Low,
        workspace: WorkspaceRef::Worktree {
            path: Utf8PathBuf::from("/wt"),
            branch: format!("swamp/9g5fav/{}-1", node.short()),
            base: "HEAD".into(),
        },
        session: None,
        state,
        created_at: at(2),
        started_at: Some(at(10)),
        ended_at: None,
        usage: Usage {
            input_tokens: 1_000,
            output_tokens: 400,
            ..Usage::default()
        },
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

fn changed(path: &str) -> FileChange {
    FileChange {
        path: Utf8PathBuf::from(path),
        kind: ChangeKind::Modify,
        added: 21,
        removed: 1,
        source: EvidenceSource::Git,
    }
}

fn cost(usd: f64) -> Option<Cost> {
    Some(Cost {
        usd,
        basis: CostBasis::Reported,
    })
}

/// Two workers still running: the live board.
pub fn running() -> Vec<JournalLine> {
    let mut first = record(
        id(1),
        "add pagination to /users",
        NodeState::Running {
            pid: 10,
            pgid: 10,
            since: at(10),
        },
    );
    first.cost = cost(0.08);
    let mut second = record(
        id(2),
        "backfill the users index",
        NodeState::Running {
            pid: 11,
            pgid: 11,
            since: at(24),
        },
    );
    second.account = Some(AccountId("alt".into()));
    second.tier = Tier::Mid;
    second.cost = cost(0.11);
    vec![
        header(),
        line(
            2,
            Some(id(0)),
            JournalEvent::NodeSpawned {
                node: Box::new(brain()),
            },
        ),
        line(
            3,
            Some(id(1)),
            JournalEvent::NodeSpawned {
                node: Box::new(first),
            },
        ),
        line(
            4,
            Some(id(2)),
            JournalEvent::NodeSpawned {
                node: Box::new(second),
            },
        ),
    ]
}

/// The same two workers, one landed and one denied Bash twice.
pub fn fixture() -> Vec<JournalLine> {
    let mut lines = running();
    let mut ok = record(id(1), "add pagination to /users", NodeState::Succeeded);
    ok.ended_at = Some(at(140));
    ok.cost = cost(0.14);
    ok.files = vec![changed("src/api/users.rs"), changed("src/api/mod.rs")];
    ok.work = Some(WorkResultRef {
        head: "abc1234".into(),
        branch: format!("swamp/9g5fav/{}-1", id(1).short()),
        patch: Utf8PathBuf::from("/patch"),
        insertions: 21,
        deletions: 1,
        empty: false,
        files: Vec::new(),
    });
    let mut failed = record(
        id(2),
        "backfill the users index",
        NodeState::Failed {
            failure: Failure::PermissionDenied {
                denials: 2,
                tools: vec!["Bash".into(), "Bash".into()],
            },
        },
    );
    failed.account = Some(AccountId("alt".into()));
    failed.tier = Tier::Mid;
    failed.ended_at = Some(at(136));
    failed.cost = cost(0.09);
    lines.push(line(
        5,
        Some(id(1)),
        JournalEvent::NodeSpawned { node: Box::new(ok) },
    ));
    lines.push(line(
        6,
        Some(id(2)),
        JournalEvent::NodeSpawned {
            node: Box::new(failed),
        },
    ));
    lines
}

/// A later node, so a second dispatch has something of its own to admit.
pub fn third() -> Vec<JournalLine> {
    let mut node = record(
        id(3),
        "rewrite the seed script",
        NodeState::Running {
            pid: 12,
            pgid: 12,
            since: at(150),
        },
    );
    node.tier = Tier::Mid;
    vec![line(
        7,
        Some(id(3)),
        JournalEvent::NodeSpawned {
            node: Box::new(node),
        },
    )]
}

/// Every anthropic account at its limit: the one line the pool journals per blocked node.
pub fn blocked() -> Vec<JournalLine> {
    vec![line(
        8,
        Some(id(4)),
        JournalEvent::NodeBlocked {
            until: at(2_660),
            why: "main cooling until 22:57".into(),
            ineligible: Vec::new(),
        },
    )]
}

pub fn config() -> crate::config::Config {
    let schema: crate::config::Schema = toml::from_str(
        r#"
version = 1
[dispatch]
default_tier = "mid"
[providers.anthropic]
models = { high = "claude-opus-4-20250514", mid = "claude-sonnet-4-20250514", low = "claude-haiku-4-20250514" }
[[accounts]]
id = "main"
provider = "anthropic"
exec = "claude-main"
"#,
    )
    .expect("fixture config parses");
    let layers = vec![
        crate::config::load::default_layer(),
        crate::config::load::Layer {
            origin: "test".into(),
            schema,
        },
    ];
    let mut cfg = crate::config::resolve::from_schema(crate::config::load::merge(layers));
    crate::config::validate::validate(&mut cfg).expect("fixture config is valid");
    cfg
}

/// An `App` with a fixed clock, a plain theme and no history file.
pub fn app(width: u16) -> App {
    let cfg = config();
    let mut app = App::new(
        run_id(),
        Theme::plain(),
        welcome(),
        History::load(None, 10),
        &cfg,
    );
    app.width = width;
    app.rows = 24;
    app.now = now();
    app
}

pub fn welcome() -> WelcomeInfo {
    WelcomeInfo {
        cwd: "/Users/me/projects/example/repo-one".to_owned(),
        brain: "anthropic/main · claude-opus-4-20250514 · tier high".to_owned(),
        workers: "4 accounts · 3 ready, 1 cooling · 12% of the tightest window used".to_owned(),
        run: run_id().short(),
    }
}

// ---------------------------------------------------------------- P4 fixture

/// A node id ending in `tail`: `nid("09").short()` is `9g5f09`.
pub fn nid(tail: &str) -> NodeId {
    NodeId::from_str(&format!(
        "01ARZ3NDEKTSV4RRFFQ69G5F{}",
        tail.to_ascii_uppercase()
    ))
    .expect("node id")
}

pub fn did(tail: &str) -> DispatchId {
    DispatchId::from_str(&format!(
        "01ARZ3NDEKTSV4RRFFQ69G5F{}",
        tail.to_ascii_uppercase()
    ))
    .expect("dispatch id")
}

pub const TERMS: &str = "score .41 = util .93\u{d7}.50 + load .33\u{d7}.30 + share .12\u{d7}.15 \
                         \u{2212} weight .00 \u{2212} idle .02";

/// Logical ids of the tasks that ran: the attempts carry the ids the rows show.
pub fn p4_task(n: u8) -> NodeId {
    match n {
        1 => nid("11"),
        2 => nid("12"),
        3 => nid("04"),
        4 => nid("0a"),
        5 => nid("15"),
        _ => nid("0c"),
    }
}

fn p4_line(seq: u64, offset: i64, node: Option<NodeId>, event: JournalEvent) -> JournalLine {
    JournalLine {
        seq,
        at: at(offset),
        run: run_id(),
        node,
        event,
    }
}

struct Attempt {
    id: NodeId,
    logical: NodeId,
    attempt: u32,
    account: &'static str,
    model: &'static str,
    tier: Tier,
    title: &'static str,
    started: i64,
    tokens: u64,
    usd: f64,
}

impl Attempt {
    fn record(&self) -> NodeRecord {
        let mut r = record(
            self.id,
            self.title,
            NodeState::Running {
                pid: 100 + self.started as i32,
                pgid: 100 + self.started as i32,
                since: at(self.started),
            },
        );
        r.logical = self.logical;
        r.attempt = self.attempt;
        r.retry_of = (self.attempt > 1).then_some(nid("08"));
        r.account = Some(AccountId(self.account.to_owned()));
        r.exec = Some(format!("claude-{}", self.account));
        r.model = Some(self.model.to_owned());
        r.tier = self.tier;
        r.created_at = at(0);
        r.started_at = Some(at(self.started));
        r.usage = Usage {
            input_tokens: self.tokens,
            ..Usage::default()
        };
        r.cost = cost(self.usd);
        r.dispatch = Some(did("18"));
        r
    }
}

const SONNET: &str = "claude-sonnet-4-5-20250929";
const OPUS: &str = "claude-opus-4-1-20250805";

/// `docs/BOARD.md` §1 of the P4 spec: two dispatches, a retry, a blocked task and a rejection.
/// Now is 22:16:40 UTC, `at(200)`; dispatch #1 went out at 22:13:20, `at(0)`.
pub fn p4_journal() -> Vec<JournalLine> {
    use crate::dispatch::policy::{Ineligible, SelectionPolicy};
    use crate::ids::CallSeq;
    use crate::model::core::{LimitScope, Provider};
    use crate::model::dispatch::{DispatchCounts, DispatchRecord, Phase, TaskRef};
    use crate::model::failure::Detector;

    let brain = NodeRecord {
        model: Some(OPUS.into()),
        started_at: Some(at(-52)),
        usage: Usage {
            input_tokens: 214_000,
            ..Usage::default()
        },
        cost: cost(0.09),
        state: NodeState::Running {
            pid: 1,
            pgid: 1,
            since: at(-52),
        },
        ..brain()
    };
    let tasks = [
        (p4_task(1), "add pagination to /users", Tier::Mid),
        (p4_task(2), "backfill the users index", Tier::High),
        (p4_task(3), "rebuild the index", Tier::Mid),
        (p4_task(4), "write the changelog", Tier::Low),
        (p4_task(5), "add the /users route", Tier::Low),
    ];
    let issued = |dispatch: DispatchId, seq: u64, at_: i64, tasks: &[(NodeId, &str, Tier)]| {
        JournalEvent::DispatchIssued {
            record: Box::new(DispatchRecord {
                id: dispatch,
                run: run_id(),
                caller: id(0),
                call_seq: Some(CallSeq(seq)),
                wait: true,
                max_wait_s: None,
                tasks: tasks
                    .iter()
                    .map(|(logical, title, tier)| TaskRef {
                        logical: *logical,
                        title: (*title).to_owned(),
                        tier: *tier,
                        provider: Provider::Anthropic,
                    })
                    .collect(),
                at: at(at_),
            }),
        }
    };
    let first = Attempt {
        id: nid("01"),
        logical: p4_task(1),
        attempt: 1,
        account: "main",
        model: SONNET,
        tier: Tier::Mid,
        title: "add pagination to /users",
        started: 10,
        tokens: 118_000,
        usd: 0.08,
    };
    let limited = Attempt {
        id: nid("08"),
        logical: p4_task(2),
        attempt: 1,
        account: "main",
        model: OPUS,
        tier: Tier::High,
        title: "backfill the users index",
        started: -9,
        tokens: 0,
        usd: 0.01,
    };
    let retry = Attempt {
        id: nid("09"),
        attempt: 2,
        account: "alt",
        started: 32,
        tokens: 223_000,
        usd: 0.21,
        ..limited
    };
    let route = Attempt {
        id: nid("05"),
        logical: p4_task(5),
        attempt: 1,
        account: "main",
        model: SONNET,
        tier: Tier::Low,
        title: "add the /users route",
        started: 20,
        tokens: 96_000,
        usd: 0.04,
    };
    let rate_limited = crate::model::failure::Failure::RateLimited {
        resets_at: None,
        scope: LimitScope::FiveHour,
        detected_by: Detector::Telemetry,
        evidence: "usage limit reached".into(),
    };
    let finished = |state: NodeState, a: &Attempt| JournalEvent::NodeFinished {
        state,
        exit: None,
        usage: Usage {
            input_tokens: a.tokens,
            ..Usage::default()
        },
        cost: cost(a.usd),
        work: None,
        summary: None,
        files: Vec::new(),
        unparsed_lines: 0,
    };
    let spawned = |a: &Attempt| JournalEvent::NodeSpawned {
        node: Box::new(a.record()),
    };

    let mut lines = vec![
        p4_line(1, -60, None, header().event),
        p4_line(
            2,
            -52,
            Some(id(0)),
            JournalEvent::NodeSpawned {
                node: Box::new(brain),
            },
        ),
        p4_line(3, 0, Some(id(0)), issued(did("18"), 1, 0, &tasks)),
    ];
    let mut seq = 4;
    for (logical, title, tier) in &tasks {
        lines.push(p4_line(
            seq,
            0,
            Some(*logical),
            JournalEvent::TaskQueued {
                logical: *logical,
                dispatch: did("18"),
                title: (*title).to_owned(),
                tier: *tier,
                depth: 1,
            },
        ));
        seq += 1;
    }
    let mut push = |offset: i64, node: NodeId, event: JournalEvent| {
        lines.push(p4_line(seq, offset, Some(node), event));
        seq += 1;
    };
    push(
        1,
        p4_task(3),
        JournalEvent::NodeBlocked {
            until: at(2_480),
            why: "main at capacity, alt quota stop".into(),
            ineligible: vec![
                (AccountId("main".into()), Ineligible::AtCapacity),
                (AccountId("alt".into()), Ineligible::QuotaStop),
            ],
        },
    );
    push(-9, limited.id, spawned(&limited));
    push(10, first.id, spawned(&first));
    push(20, route.id, spawned(&route));
    push(
        32,
        limited.id,
        finished(
            NodeState::Failed {
                failure: rate_limited.clone(),
            },
            &limited,
        ),
    );
    push(
        32,
        limited.id,
        JournalEvent::NodeRetry {
            attempt: 2,
            reason: rate_limited,
            rotate: true,
        },
    );
    push(32, retry.id, spawned(&retry));
    push(
        32,
        retry.id,
        JournalEvent::AccountSelected {
            account: AccountId("alt".into()),
            exec: "claude-alt".into(),
            policy: SelectionPolicy::QuotaAware,
            reason: TERMS.to_owned(),
            excluded: vec![AccountId("main".into()), AccountId("codex-main".into())],
        },
    );
    push(150, route.id, finished(NodeState::Succeeded, &route));
    push(
        150,
        p4_task(5),
        JournalEvent::NodeStateChanged {
            from: Phase::Running,
            to: NodeState::Succeeded,
            why: "finished".into(),
        },
    );
    push(
        159,
        id(0),
        issued(
            did("1c"),
            2,
            159,
            &[(p4_task(6), "probe the migration", Tier::Low)],
        ),
    );
    push(
        159,
        p4_task(6),
        JournalEvent::DispatchRejected {
            dispatch: did("1c"),
            logical: p4_task(6),
            reason: crate::model::failure::Failure::WorkerError {
                subtype: "max_nodes_per_run".into(),
                detail: "the run already holds 32 nodes".into(),
            },
        },
    );
    push(
        159,
        id(0),
        JournalEvent::DispatchSettled {
            dispatch: did("1c"),
            counts: DispatchCounts {
                rejected: 1,
                ..DispatchCounts::default()
            },
            cost: None,
        },
    );
    lines
}
