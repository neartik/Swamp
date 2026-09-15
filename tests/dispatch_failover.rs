//! WP4: the attempt loop. Who rotates, who retries, and who must never do either.

use async_trait::async_trait;
use camino::Utf8PathBuf;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use swamp::config::{Config, load, resolve, validate};
use swamp::dispatch::{AccountPool, Dispatcher, Health, NodeCtx, NodeRunner, run_node};
use swamp::journal::JournalHandle;
use swamp::journal::paths::{Paths, RunPaths};
use swamp::journal::writer::{FsyncPolicy, Writer};
use swamp::model::core::{
    AccountId, LimitScope, NodeKind, Provider, SessionHandle, Tier, WorkspaceRef,
};
use swamp::model::failure::{Detector, Failure};
use swamp::model::node::WorkResultRef;
use swamp::model::result::{IsolationMode, TaskRequest};
use swamp::worker::RunOutcome;
use swamp::worker::adapter::{LaunchSpec, SessionPlan};
use swamp::workspace::{Git, NodeWorktree, WorkspaceManager};
use swamp::{JournalEvent, NodeId, NodeIds, RunId, SwampError};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

type Events = UnboundedReceiver<(Option<NodeId>, JournalEvent)>;

const TWO_ACCOUNTS: &str = r#"
[brain]
reserve_brain_slot = false
[dispatch]
max_attempts = 3
cross_provider_failover = false
[providers.anthropic]
models = { low = "tier-low", mid = "tier-mid", high = "tier-high" }
[[accounts]]
id = "alt"
provider = "anthropic"
exec = "claude-alt"
max_concurrency = 2
[[accounts]]
id = "main"
provider = "anthropic"
exec = "claude-main"
max_concurrency = 2
"#;

// ---------------------------------------------------------------- scripted runner

#[derive(Clone, Debug, PartialEq)]
enum Session {
    New(Option<String>),
    Resume(SessionHandle),
}

#[derive(Clone, Debug)]
struct Call {
    exec: String,
    cwd: Utf8PathBuf,
    attempt: u32,
    model: String,
    session: Session,
    provider: Provider,
}

impl Call {
    fn account(&self) -> AccountId {
        AccountId(self.exec.trim_start_matches("claude-").to_owned())
    }
}

struct Scripted {
    root: Utf8PathBuf,
    outcomes: Mutex<VecDeque<RunOutcome>>,
    calls: Mutex<Vec<Call>>,
    worktrees: Mutex<Vec<Utf8PathBuf>>,
    delay: Option<Duration>,
}

impl Scripted {
    fn new(root: &Utf8PathBuf, outcomes: Vec<RunOutcome>) -> Arc<Self> {
        Arc::new(Self {
            root: root.clone(),
            outcomes: Mutex::new(outcomes.into()),
            calls: Mutex::new(Vec::new()),
            worktrees: Mutex::new(Vec::new()),
            delay: None,
        })
    }
    fn slow(root: &Utf8PathBuf, delay: Duration) -> Arc<Self> {
        let me = Self::new(root, Vec::new());
        Arc::new(Self {
            root: me.root.clone(),
            outcomes: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
            worktrees: Mutex::new(Vec::new()),
            delay: Some(delay),
        })
    }
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("calls").clone()
    }
    fn worktrees(&self) -> Vec<Utf8PathBuf> {
        self.worktrees.lock().expect("worktrees").clone()
    }
}

#[async_trait]
impl NodeRunner for Scripted {
    async fn workspace(&self, logical: NodeId, attempt: u32) -> anyhow::Result<NodeWorktree> {
        let path = self
            .root
            .join("worktrees")
            .join(logical.short())
            .join(attempt.to_string());
        std::fs::create_dir_all(&path)?;
        self.worktrees.lock().expect("worktrees").push(path.clone());
        Ok(NodeWorktree {
            node: logical,
            path,
            branch: format!("swamp/{}/{attempt}", logical.short()),
            base: "HEAD".into(),
        })
    }

    async fn run(
        &self,
        spec: &LaunchSpec,
        _timeout: Duration,
        _cancel: CancellationToken,
    ) -> anyhow::Result<RunOutcome> {
        self.calls.lock().expect("calls").push(Call {
            exec: spec.exec.clone(),
            cwd: spec.cwd.clone(),
            attempt: spec.attempt,
            model: spec.model.clone(),
            session: match &spec.session {
                SessionPlan::New { preassigned } => Session::New(preassigned.clone()),
                SessionPlan::Resume(h) => Session::Resume(h.clone()),
            },
            provider: spec.provider,
        });
        if let Some(d) = self.delay {
            tokio::time::sleep(d).await;
        }
        Ok(self
            .outcomes
            .lock()
            .expect("outcomes")
            .pop_front()
            .unwrap_or_else(success))
    }

    async fn finalize(
        &self,
        _wt: &NodeWorktree,
        _title: &str,
        _tier: Tier,
    ) -> anyhow::Result<Option<WorkResultRef>> {
        Ok(None)
    }
}

// ---------------------------------------------------------------- fixtures

fn success() -> RunOutcome {
    RunOutcome {
        failure: None,
        exit: None,
        session: None,
        usage: Default::default(),
        cost: None,
        summary: Some("done".into()),
        files: Vec::new(),
        rate_limit: None,
        stream_offset: 0,
        unparsed_lines: 0,
        permission_denials: 0,
    }
}

fn failed(f: Failure) -> RunOutcome {
    RunOutcome {
        failure: Some(f),
        ..success()
    }
}

fn with_session(mut out: RunOutcome, account: &str, id: &str) -> RunOutcome {
    out.session = Some(SessionHandle {
        account: AccountId(account.into()),
        id: id.into(),
        preassigned: false,
    });
    out
}

fn rate_limited() -> Failure {
    Failure::RateLimited {
        resets_at: None,
        scope: LimitScope::FiveHour,
        detected_by: Detector::Telemetry,
        evidence: "usage limit reached".into(),
    }
}

fn config(extra: &str) -> Arc<Config> {
    let schema = toml::from_str(extra).expect("test config parses");
    let layers = vec![
        load::default_layer(),
        load::Layer {
            origin: "test".into(),
            schema,
        },
    ];
    let mut cfg = resolve::from_schema(load::merge(layers));
    validate::validate(&mut cfg).expect("test config is valid");
    Arc::new(cfg)
}

struct Fixture {
    _dir: tempfile::TempDir,
    _events: Events,
    root: Utf8PathBuf,
    cfg: Arc<Config>,
    journal: JournalHandle,
    pool: Arc<AccountPool>,
}

async fn fixture(extra: &str) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 tempdir");
    let cfg = config(extra);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let run = RunId::new();
    let paths = RunPaths {
        run,
        dir: root.join("run"),
        sock_dir: root.join("sock"),
    };
    std::fs::create_dir_all(&paths.dir).expect("run dir");
    let writer = Writer::open(&paths.journal(), FsyncPolicy::Never)
        .await
        .expect("journal writer");
    let journal = JournalHandle {
        run,
        tx,
        paths: Arc::new(paths),
        writer: Arc::new(tokio::sync::Mutex::new(writer)),
    };
    let pool = AccountPool::new(
        Arc::clone(&cfg),
        root.join("accounts.json"),
        journal.clone(),
    )
    .expect("pool");
    Fixture {
        _dir: dir,
        _events: rx,
        root,
        cfg,
        journal,
        pool,
    }
}

impl Fixture {
    fn ctx(&self, runner: Arc<dyn NodeRunner>, deadline: Duration) -> NodeCtx {
        NodeCtx {
            cfg: Arc::clone(&self.cfg),
            pool: Arc::clone(&self.pool),
            runner,
            journal: self.journal.clone(),
            provider_order: vec![Provider::Anthropic],
            cross_provider: false,
            max_attempts: self.cfg.dispatch.max_attempts.unwrap_or(3),
            deadline: Instant::now() + deadline,
            parent: None,
            logical: NodeId::new(),
            cancel: CancellationToken::new(),
        }
    }

    async fn dispatcher(&self, runner: Arc<dyn NodeRunner>) -> Arc<Dispatcher> {
        let exec = Arc::new(swamp::worker::Executor {
            journal: self.journal.clone(),
            cfg: Arc::clone(&self.cfg),
        });
        let ws = WorkspaceManager::new(
            Git {
                root: self.root.clone(),
            },
            Arc::new(Paths {
                repo: self.root.clone(),
                dot_swamp: self.root.join(".swamp"),
                home_swamp: self.root.join("home"),
            }),
            Arc::clone(&self.cfg),
            self.journal.clone(),
        )
        .await
        .expect("workspace manager");
        Dispatcher::with_runner(
            Arc::clone(&self.cfg),
            Arc::clone(&self.pool),
            exec,
            ws,
            self.journal.clone(),
            runner,
        )
    }
}

fn task(title: &str) -> TaskRequest {
    TaskRequest {
        title: title.into(),
        prompt: "do the thing".into(),
        tier: Some(Tier::Mid),
        provider: None,
        isolation: None,
        account: None,
        deps: Vec::new(),
    }
}

fn spec() -> LaunchSpec {
    LaunchSpec {
        node: NodeIds {
            id: NodeId::new(),
            session_uuid: uuid::Uuid::new_v4(),
        },
        provider: Provider::Anthropic,
        exec: String::new(),
        env: Default::default(),
        model: String::new(),
        tier: Tier::Mid,
        cwd: "/nonexistent".into(),
        isolation: IsolationMode::Worktree,
        session: SessionPlan::New { preassigned: None },
        kind: NodeKind::Worker,
        permission_mode: String::new(),
        sandbox: String::new(),
        budget_usd: None,
        append_system_prompt: None,
        allow_tools: Vec::new(),
        deny_tools: Vec::new(),
        mcp: None,
        last_message_path: "/nonexistent".into(),
        extra_args: Vec::new(),
        extra: Default::default(),
        partial_messages: false,
        attempt: 1,
    }
}

fn health_of(pool: &Arc<AccountPool>, who: &AccountId) -> Health {
    pool.snapshot()
        .into_iter()
        .find(|(_, a, _)| a == who)
        .map(|(_, _, s)| s.health)
        .expect("a configured account")
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn a_rate_limit_fails_over_to_the_next_account() {
    let f = fixture(TWO_ACCOUNTS).await;
    let runner = Scripted::new(&f.root, vec![failed(rate_limited()), success()]);
    let cx = f.ctx(runner.clone(), Duration::from_secs(30));
    let out = run_node(&cx, spec(), &task("port the parser")).await;

    let calls = runner.calls();
    assert_eq!(calls.len(), 2, "one attempt per account");
    assert_ne!(
        calls[0].exec, calls[1].exec,
        "the retry must rotate accounts"
    );
    assert_eq!((calls[0].attempt, calls[1].attempt), (1, 2));
    assert!(calls.iter().all(|c| c.provider == Provider::Anthropic));
    assert!(calls.iter().all(|c| c.model == "tier-mid"), "{calls:?}");
    assert!(out.failure.is_none());

    assert_eq!(out.attempts.len(), 2);
    assert_eq!(out.attempts[0].logical, out.attempts[1].logical);
    assert_eq!(out.attempts[0].logical, cx.logical);
    assert_eq!(out.attempts[1].retry_of, Some(out.attempts[0].id));
    assert_ne!(out.attempts[0].id, out.attempts[1].id);
    assert_eq!(out.attempts[1].attempt, 2);

    assert_eq!(health_of(&f.pool, &calls[0].account()), Health::Cooling);
    assert_eq!(health_of(&f.pool, &calls[1].account()), Health::Healthy);
}

#[tokio::test]
async fn a_task_failure_never_burns_a_second_subscription() {
    let f = fixture(TWO_ACCOUNTS).await;
    let runner = Scripted::new(
        &f.root,
        vec![failed(Failure::WorkerError {
            subtype: "error_during_execution".into(),
            detail: "the tests failed".into(),
        })],
    );
    let cx = f.ctx(runner.clone(), Duration::from_secs(30));
    let out = run_node(&cx, spec(), &task("fix the tests")).await;

    let calls = runner.calls();
    assert_eq!(
        calls.len(),
        1,
        "a failing task must not be retried anywhere"
    );
    assert_eq!(out.attempts.len(), 1);
    assert!(matches!(out.failure, Some(Failure::WorkerError { .. })));

    for (_, _, state) in f.pool.snapshot() {
        assert_eq!(state.health, Health::Healthy);
        assert!(state.cooldown_until.is_none());
        assert_eq!(state.consecutive_infra_failures, 0);
    }
    let used = calls[0].account();
    let nodes: u64 = f
        .pool
        .snapshot()
        .into_iter()
        .filter(|(_, a, _)| *a == used)
        .map(|(_, _, s)| s.lifetime_nodes)
        .sum();
    assert_eq!(nodes, 1, "the account was used exactly once");
}

#[tokio::test(start_paused = true)]
async fn an_overload_retries_the_same_account_and_resumes_its_session() {
    let f = fixture(TWO_ACCOUNTS).await;
    let runner = Scripted::new(
        &f.root,
        vec![
            with_session(
                failed(Failure::Overloaded {
                    detail: "529".into(),
                }),
                "alt",
                "session-one",
            ),
            success(),
        ],
    );
    let cx = f.ctx(runner.clone(), Duration::from_secs(600));
    let out = run_node(&cx, spec(), &task("retry me")).await;
    assert!(out.failure.is_none());

    let calls = runner.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0].exec, calls[1].exec,
        "overload stays on the account"
    );
    assert_eq!(calls[0].session, Session::New(None));
    match &calls[1].session {
        Session::Resume(h) => {
            assert_eq!(h.id, "session-one");
            assert_eq!(h.account, calls[1].account());
        }
        other => panic!("attempt 2 should resume the session, got {other:?}"),
    }
}

#[tokio::test]
async fn a_session_handle_never_crosses_accounts() {
    let f = fixture(TWO_ACCOUNTS).await;
    // The handle is minted on the first account, which is then rate limited away.
    let runner = Scripted::new(
        &f.root,
        vec![
            with_session(failed(rate_limited()), "alt", "session-one"),
            success(),
        ],
    );
    let cx = f.ctx(runner.clone(), Duration::from_secs(30));
    let out = run_node(&cx, spec(), &task("rotate me")).await;
    assert!(out.failure.is_none());

    let calls = runner.calls();
    assert_ne!(calls[0].exec, calls[1].exec);
    match &calls[1].session {
        Session::New(preassigned) => assert!(
            preassigned.is_some(),
            "a rotated attempt gets a fresh preassigned session"
        ),
        Session::Resume(h) => panic!("a handle from another account leaked: {h:?}"),
    }
    assert_ne!(
        out.attempts[1].session.as_ref().map(|h| h.id.clone()),
        Some("session-one".to_owned())
    );
}

#[tokio::test]
async fn every_attempt_runs_in_a_fresh_worktree() {
    let f = fixture(TWO_ACCOUNTS).await;
    let runner = Scripted::new(&f.root, vec![failed(rate_limited()), success()]);
    let cx = f.ctx(runner.clone(), Duration::from_secs(30));
    let out = run_node(&cx, spec(), &task("two trees")).await;

    let trees = runner.worktrees();
    assert_eq!(trees.len(), 2, "one worktree per attempt");
    assert_ne!(trees[0], trees[1]);

    let calls = runner.calls();
    assert_eq!(calls[0].cwd, trees[0]);
    assert_eq!(calls[1].cwd, trees[1]);
    assert_ne!(
        calls[1].cwd, calls[0].cwd,
        "attempt 2 never reuses attempt 1"
    );

    for record in &out.attempts {
        assert!(matches!(record.workspace, WorkspaceRef::Worktree { .. }));
    }
}

#[tokio::test]
async fn the_prompt_is_written_once_per_attempt_and_hashed() {
    let f = fixture(TWO_ACCOUNTS).await;
    let runner = Scripted::new(&f.root, vec![failed(rate_limited()), success()]);
    let cx = f.ctx(runner.clone(), Duration::from_secs(30));
    let out = run_node(&cx, spec(), &task("hash me")).await;
    for record in &out.attempts {
        assert_eq!(
            std::fs::read_to_string(&record.prompt_path).expect("prompt on disk"),
            "do the thing"
        );
        assert_eq!(record.prompt_sha256.len(), 64);
    }
    assert_ne!(out.attempts[0].prompt_path, out.attempts[1].prompt_path);
}

#[tokio::test]
async fn every_account_cooling_gives_up_with_no_account_available() {
    let f = fixture(TWO_ACCOUNTS).await;
    for who in ["main", "alt"] {
        f.pool
            .report(&AccountId(who.into()), Some(&rate_limited()), None);
    }
    let runner = Scripted::new(&f.root, vec![success()]);
    let cx = f.ctx(runner.clone(), Duration::from_millis(100));
    let out = run_node(&cx, spec(), &task("nowhere to run")).await;

    assert!(runner.calls().is_empty(), "nothing may be spawned");
    let detail = match out.failure {
        Some(Failure::NoCapacity { detail }) => detail,
        other => panic!("expected NoCapacity, got {other:?}"),
    };
    let expected = SwampError::NoAccountAvailable {
        provider: Provider::Anthropic,
        excluded: 0,
        cooling: 2,
    };
    assert_eq!(detail, expected.to_string());
    assert_eq!(swamp::exit_code(&anyhow::Error::new(expected)), 3);
}

#[tokio::test(start_paused = true)]
async fn attempts_are_capped_by_max_attempts() {
    let f = fixture(&format!("{TWO_ACCOUNTS}\n[limits]\nmax_parallel = 8\n")).await;
    let runner = Scripted::new(
        &f.root,
        vec![
            failed(Failure::Crashed { signal: Some(9) }),
            failed(Failure::Crashed { signal: Some(9) }),
            failed(Failure::Crashed { signal: Some(9) }),
            success(),
        ],
    );
    let mut cx = f.ctx(runner.clone(), Duration::from_secs(600));
    cx.max_attempts = 2;
    let out = run_node(&cx, spec(), &task("keep crashing")).await;
    assert_eq!(runner.calls().len(), 2);
    assert!(matches!(
        out.failure,
        Some(Failure::WorkerError { ref subtype, .. }) if subtype == "attempts_exhausted"
    ));
}

#[tokio::test]
async fn the_dispatcher_rejects_dependent_tasks() {
    let f = fixture(TWO_ACCOUNTS).await;
    let runner = Scripted::new(&f.root, vec![]);
    let disp = f.dispatcher(runner.clone()).await;
    let mut t = task("second step");
    t.deps = vec![NodeId::new()];

    let result = disp.dispatch_one(NodeId::new(), t).await;
    assert!(!result.ok);
    assert!(runner.calls().is_empty());
    let detail = match result.failure {
        Some(Failure::WorkerError { detail, .. }) => detail,
        other => panic!("expected a rejection, got {other:?}"),
    };
    assert!(detail.contains("deps"), "{detail}");
    assert!(detail.contains("v1"), "{detail}");
}

#[tokio::test]
async fn the_dispatcher_enforces_max_depth() {
    let f = fixture(&format!("{TWO_ACCOUNTS}\n[limits]\nmax_depth = 0\n")).await;
    let runner = Scripted::new(&f.root, vec![]);
    let disp = f.dispatcher(runner.clone()).await;
    let result = disp.dispatch_one(NodeId::new(), task("too deep")).await;
    assert!(!result.ok);
    assert!(runner.calls().is_empty(), "a capped node is never spawned");
    let detail = match result.failure {
        Some(Failure::WorkerError { detail, .. }) => detail,
        other => panic!("expected a rejection, got {other:?}"),
    };
    assert!(detail.contains("max_depth"), "{detail}");
}

#[tokio::test]
async fn the_dispatcher_enforces_max_nodes_per_run() {
    let f = fixture(&format!(
        "{TWO_ACCOUNTS}\n[limits]\nmax_nodes_per_run = 1\n"
    ))
    .await;
    let runner = Scripted::new(&f.root, vec![]);
    let disp = f.dispatcher(runner.clone()).await;
    let parent = NodeId::new();
    assert!(disp.dispatch_one(parent, task("first")).await.ok);
    let second = disp.dispatch_one(parent, task("second")).await;
    assert!(!second.ok);
    assert_eq!(runner.calls().len(), 1);
    let detail = match second.failure {
        Some(Failure::WorkerError { detail, .. }) => detail,
        other => panic!("expected a rejection, got {other:?}"),
    };
    assert!(detail.contains("max_nodes_per_run"), "{detail}");
}

#[tokio::test]
async fn a_batch_returns_within_max_wait_with_running_nodes() {
    let f = fixture(TWO_ACCOUNTS).await;
    let runner = Scripted::slow(&f.root, Duration::from_secs(30));
    let disp = f.dispatcher(runner.clone()).await;
    let started = std::time::Instant::now();
    let results = disp
        .dispatch_batch(
            NodeId::new(),
            vec![task("slow one"), task("slow two")],
            Duration::from_millis(120),
        )
        .await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the batch blocked"
    );
    assert_eq!(results.len(), 2);
    for r in &results {
        assert_eq!(r.state, "running");
        assert!(!r.ok);
        assert!(r.failure.is_none());
        assert!(disp.result(r.node).is_some());
    }
}

#[tokio::test]
async fn a_completed_batch_carries_the_worker_result() {
    let f = fixture(TWO_ACCOUNTS).await;
    let runner = Scripted::new(&f.root, vec![success(), success()]);
    let disp = f.dispatcher(runner.clone()).await;
    let results = disp
        .dispatch_batch(
            NodeId::new(),
            vec![task("one"), task("two")],
            Duration::from_secs(10),
        )
        .await;
    assert_eq!(results.len(), 2);
    for r in &results {
        assert!(r.ok, "{:?}", r.failure);
        assert_eq!(r.state, "succeeded");
        assert_eq!(r.attempts, 1);
        assert_eq!(r.summary.as_deref(), Some("done"));
        assert_eq!(r.model.as_deref(), Some("tier-mid"));
        assert!(r.account.is_some());
    }
    assert_eq!(runner.calls().len(), 2);
}

#[tokio::test]
async fn cross_provider_failover_is_opt_in() {
    let toml = format!(
        "{TWO_ACCOUNTS}\n[providers.openai]\nmodels = {{ mid = \"other-mid\" }}\n\
         [[accounts]]\nid = \"codex\"\nprovider = \"openai\"\nexec = \"codex-main\"\n"
    );
    for (cross, want) in [(false, None), (true, Some(Provider::Openai))] {
        let f = fixture(&toml).await;
        for who in ["main", "alt"] {
            f.pool
                .report(&AccountId(who.into()), Some(&rate_limited()), None);
        }
        let runner = Scripted::new(&f.root, vec![success()]);
        let mut cx = f.ctx(runner.clone(), Duration::from_millis(100));
        cx.provider_order = vec![Provider::Anthropic, Provider::Openai];
        cx.cross_provider = cross;

        let out = run_node(&cx, spec(), &task("spill over")).await;
        match want {
            None => {
                assert!(
                    runner.calls().is_empty(),
                    "opt-in means nothing moves by default"
                );
                assert!(matches!(out.failure, Some(Failure::NoCapacity { .. })));
            }
            Some(p) => {
                let calls = runner.calls();
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].provider, p);
                assert_eq!(calls[0].exec, "codex-main");
                assert!(out.failure.is_none());
            }
        }
    }
}

#[tokio::test]
async fn max_parallel_dispatch_serializes_a_batch() {
    let f = fixture(&format!(
        "{TWO_ACCOUNTS}\n[limits]\nmax_parallel_dispatch = 1\n"
    ))
    .await;
    let runner = Scripted::slow(&f.root, Duration::from_secs(30));
    let disp = f.dispatcher(runner.clone()).await;
    let results = disp
        .dispatch_batch(
            NodeId::new(),
            vec![task("one"), task("two")],
            Duration::from_millis(150),
        )
        .await;
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| r.state == "running"));
    assert_eq!(runner.calls().len(), 1, "only one node runs at a time");
}

#[tokio::test]
async fn cancelling_an_unknown_node_is_an_error() {
    let f = fixture(TWO_ACCOUNTS).await;
    let disp = f.dispatcher(Scripted::new(&f.root, vec![])).await;
    assert!(disp.cancel(NodeId::new()).await.is_err());
}

/// `/cancel <node>` kills the process group; the classifier only sees SIGTERM and used to
/// call that a crash, which retries on the same account with a brand new worktree.
#[tokio::test]
async fn a_cancelled_node_is_never_respawned() {
    let f = fixture(TWO_ACCOUNTS).await;
    let runner = Scripted::new(
        &f.root,
        vec![
            failed(Failure::Crashed { signal: Some(15) }),
            failed(Failure::Crashed { signal: Some(15) }),
            failed(Failure::Crashed { signal: Some(15) }),
        ],
    );
    let cx = f.ctx(runner.clone(), Duration::from_secs(30));
    cx.cancel.cancel();
    let out = run_node(&cx, spec(), &task("cancelled")).await;

    assert_eq!(runner.calls().len(), 0, "a cancelled node never spawns");
    assert_eq!(
        out.failure,
        Some(Failure::Cancelled {
            by: swamp::model::core::CancelSource::User
        })
    );
    assert!(out.failure.as_ref().expect("failure").is_terminal());
}

/// Cancellation arriving while the worker runs outranks whatever the classifier made of the
/// kill signal: one attempt, one worktree, and a cancelled node rather than a failed one.
#[tokio::test]
async fn cancellation_during_a_run_stops_the_attempt_loop() {
    let f = fixture(TWO_ACCOUNTS).await;
    let runner = Scripted::slow(&f.root, Duration::from_millis(200));
    let cx = f.ctx(runner.clone(), Duration::from_secs(30));
    let token = cx.cancel.clone();
    let request = task("cancelled");
    let (out, ()) = tokio::join!(run_node(&cx, spec(), &request), async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();
    });

    assert_eq!(runner.calls().len(), 1, "{:?}", runner.calls());
    assert_eq!(runner.worktrees().len(), 1, "no retry worktree");
    assert_eq!(out.attempts.len(), 1);
    assert!(matches!(
        out.attempts[0].state,
        swamp::model::core::NodeState::Cancelled { .. }
    ));
    assert!(matches!(out.failure, Some(Failure::Cancelled { .. })));
}

/// Quota telemetry has to reach the pool, or `quota_stop_at`, `Degraded` health and the
/// quota-aware policy are all arithmetic on a permanent zero.
#[tokio::test]
async fn a_rate_limit_snapshot_from_the_worker_reaches_the_pool() {
    use swamp::model::core::{LimitStatus, LimitWindow, RateLimitSnapshot};

    let f = fixture(TWO_ACCOUNTS).await;
    let mut out = success();
    out.rate_limit = Some(RateLimitSnapshot {
        status: LimitStatus::Allowed,
        windows: vec![LimitWindow {
            scope: LimitScope::FiveHour,
            utilization: 0.99,
            resets_at: None,
        }],
        resets_at: None,
    });
    let runner = Scripted::new(&f.root, vec![out]);
    let cx = f.ctx(runner.clone(), Duration::from_secs(30));
    let done = run_node(&cx, spec(), &task("telemetry")).await;
    let used = done.attempts.last().expect("an attempt").account.clone();

    let (_, _, state) = f
        .pool
        .snapshot()
        .into_iter()
        .find(|(_, id, _)| Some(id) == used.as_ref())
        .expect("the leased account");
    assert_eq!(
        state.quota.as_ref().map(|q| q.worst_utilization()),
        Some(0.99),
        "the snapshot the worker reported was dropped"
    );
    assert_eq!(state.health, Health::Degraded);
}
