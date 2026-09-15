//! Journal fixtures the chat tests fold, and the pieces an `App` needs offline.

use crate::ids::{NodeId, RunId};
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

pub fn config() -> crate::config::Config {
    let schema: crate::config::Schema = toml::from_str(
        r#"
version = 1
[limits]
max_parallel = 4
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
        cwd: "/Users/quentin/projects/swamp/Swamp".to_owned(),
        brain: "anthropic/main · claude-opus-4-20250514 · tier high".to_owned(),
        workers: "4 accounts · 3 ready, 1 cooling · max 4 parallel · budget $10.00".to_owned(),
        run: run_id().short(),
    }
}
