//! WP-D: quota-aware selection. The five worked examples of `docs/USAGE.md` §4.9, the
//! eligibility gates of §4.3, and the blocked-wait loop of §4.7.

use camino::Utf8PathBuf;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use swamp::config::{Config, load, resolve, validate};
use swamp::dispatch::policy::{Rank, Scoring, SelectionPolicy, rank, score};
use swamp::dispatch::{Account, AccountPool, AccountState, Health, NoCapacity};
use swamp::journal::JournalHandle;
use swamp::journal::paths::RunPaths;
use swamp::journal::writer::{FsyncPolicy, Writer};
use swamp::model::core::{
    AccountId, LimitReached, LimitScope, LimitStatus, LimitWindow, Provider, RateLimitSnapshot,
    Usage,
};
use swamp::model::failure::{Detector, Failure};
use swamp::{JournalEvent, NodeId, RunId};
use time::OffsetDateTime;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

type Events = UnboundedReceiver<(Option<NodeId>, JournalEvent)>;

// ---------------------------------------------------------------- scoring fixtures

fn account(id: &str, max_concurrency: Option<usize>) -> Account {
    Account {
        id: AccountId(id.into()),
        provider: Provider::Anthropic,
        exec: format!("claude-{id}"),
        env: BTreeMap::new(),
        weight: 1,
        max_concurrency,
    }
}

fn window(scope: LimitScope, utilization: f64, measured: bool) -> LimitWindow {
    LimitWindow {
        scope,
        utilization,
        measured,
        ..Default::default()
    }
}

fn quota(windows: Vec<LimitWindow>) -> RateLimitSnapshot {
    RateLimitSnapshot {
        status: LimitStatus::Allowed,
        windows,
        ..Default::default()
    }
}

fn tokens(billable: u64) -> Usage {
    Usage {
        input_tokens: billable,
        ..Default::default()
    }
}

/// Idle for more than an hour, which is where every worked example starts.
fn idle_state(inflight: usize, window_billable: u64, now: OffsetDateTime) -> AccountState {
    AccountState {
        inflight,
        window_tokens: tokens(window_billable),
        last_used: Some(now - Duration::from_secs(7200)),
        ..Default::default()
    }
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// Scores a whole candidate set the way `take_from` does: one `pool_window` for all of them.
fn scores(
    policy: SelectionPolicy,
    pool: &[(Account, AccountState)],
    now: OffsetDateTime,
) -> Vec<(String, Option<f64>)> {
    let cfg = Scoring::default();
    let total: u64 = pool
        .iter()
        .map(|(_, s)| s.window_tokens.billable())
        .sum::<u64>();
    pool.iter()
        .map(|(a, s)| {
            (
                a.id.0.clone(),
                score(policy, a, s, total, &cfg, now).map(round3),
            )
        })
        .collect()
}

fn winner(pool: &[(Account, AccountState)], now: OffsetDateTime) -> String {
    let cfg = Scoring::default();
    let total: u64 = pool.iter().map(|(_, s)| s.window_tokens.billable()).sum();
    pool.iter()
        .enumerate()
        .filter_map(|(i, (a, s))| {
            rank(SelectionPolicy::QuotaAware, a, s, total, &cfg, now, i)
                .map(|r| (r, a.id.0.clone()))
        })
        .min_by(|a, b| a.0.compare(&b.0))
        .map(|(_, id)| id)
        .expect("an eligible account")
}

// ---------------------------------------------------------------- §4.9, the five examples

/// 1. Utilization decides between two idle accounts.
#[test]
fn utilization_decides_between_two_idle_accounts() {
    let now = OffsetDateTime::now_utc();
    let pool = vec![
        (
            account("main", Some(3)),
            AccountState {
                quota: Some(quota(vec![
                    window(LimitScope::FiveHour, 0.13, true),
                    window(LimitScope::SevenDay, 0.05, true),
                ])),
                ..idle_state(0, 412_000, now)
            },
        ),
        (
            account("alt", Some(2)),
            AccountState {
                quota: Some(quota(vec![
                    window(LimitScope::FiveHour, 0.03, true),
                    window(LimitScope::SevenDay, 0.65, true),
                ])),
                ..idle_state(0, 1_200_000, now)
            },
        ),
    ];
    assert_eq!(
        scores(SelectionPolicy::QuotaAware, &pool, now),
        vec![
            ("main".to_owned(), Some(0.083)),
            ("alt".to_owned(), Some(0.417))
        ]
    );
    assert_eq!(winner(&pool, now), "main");
}

/// 2. Load matters, but utilization outranks it; the cap still gates.
#[test]
fn load_matters_but_utilization_outranks_it() {
    let now = OffsetDateTime::now_utc();
    let busy_main = |max: Option<usize>, inflight: usize| {
        (
            account("main", max),
            AccountState {
                quota: Some(quota(vec![
                    window(LimitScope::FiveHour, 0.13, true),
                    window(LimitScope::SevenDay, 0.05, true),
                ])),
                ..idle_state(inflight, 412_000, now)
            },
        )
    };
    let alt = (
        account("alt", Some(2)),
        AccountState {
            quota: Some(quota(vec![
                window(LimitScope::FiveHour, 0.03, true),
                window(LimitScope::SevenDay, 0.65, true),
            ])),
            ..idle_state(0, 1_200_000, now)
        },
    );

    let two_of_three = vec![busy_main(Some(3), 2), alt.clone()];
    assert_eq!(
        scores(SelectionPolicy::QuotaAware, &two_of_three, now),
        vec![
            ("main".to_owned(), Some(0.283)),
            ("alt".to_owned(), Some(0.417))
        ]
    );
    assert_eq!(winner(&two_of_three, now), "main");

    // At its own ceiling `main` is ineligible outright and the node goes to `alt`.
    let three_of_three = vec![busy_main(Some(3), 3), alt.clone()];
    assert_eq!(
        scores(SelectionPolicy::QuotaAware, &three_of_three, now),
        vec![("main".to_owned(), None), ("alt".to_owned(), Some(0.417))]
    );
    assert_eq!(winner(&three_of_three, now), "alt");

    // Uncapped, three nodes deep, `main` is still the better account: crowding, not a number.
    let uncapped = vec![busy_main(None, 3), alt];
    assert_eq!(
        scores(SelectionPolicy::QuotaAware, &uncapped, now),
        vec![
            ("main".to_owned(), Some(0.308)),
            ("alt".to_owned(), Some(0.417))
        ]
    );
    assert_eq!(winner(&uncapped, now), "main");
}

/// 3. The near-exhaustion penalty is decisive, and past `stop` the account is gone.
#[test]
fn the_near_exhaustion_penalty_is_decisive() {
    let now = OffsetDateTime::now_utc();
    let alt_at = |seven_day: f64| {
        (
            account("alt", Some(2)),
            AccountState {
                quota: Some(quota(vec![
                    window(LimitScope::FiveHour, 0.03, true),
                    window(LimitScope::SevenDay, seven_day, true),
                ])),
                ..idle_state(0, 1_200_000, now)
            },
        )
    };
    let main = (
        account("main", Some(3)),
        AccountState {
            quota: Some(quota(vec![
                window(LimitScope::FiveHour, 0.13, true),
                window(LimitScope::SevenDay, 0.05, true),
            ])),
            ..idle_state(0, 412_000, now)
        },
    );

    let warned = vec![main.clone(), alt_at(0.93)];
    assert_eq!(
        scores(SelectionPolicy::QuotaAware, &warned, now),
        vec![
            ("main".to_owned(), Some(0.083)),
            ("alt".to_owned(), Some(0.932))
        ]
    );
    assert_eq!(winner(&warned, now), "main");

    let stopped = vec![main, alt_at(0.985)];
    assert_eq!(
        scores(SelectionPolicy::QuotaAware, &stopped, now),
        vec![("main".to_owned(), Some(0.083)), ("alt".to_owned(), None)]
    );
}

/// 4. Two OpenAI accounts with no quota telemetry at all: `share` alone balances the pool.
#[test]
fn share_alone_balances_a_pool_with_no_telemetry() {
    let now = OffsetDateTime::now_utc();
    let pool = vec![
        (account("codex-main", None), idle_state(0, 15_200, now)),
        (account("codex-alt", None), idle_state(0, 480_000, now)),
    ];
    assert_eq!(
        scores(SelectionPolicy::QuotaAware, &pool, now),
        vec![
            ("codex-main".to_owned(), Some(-0.015)),
            ("codex-alt".to_owned(), Some(0.125))
        ]
    );
    assert_eq!(winner(&pool, now), "codex-main");

    // A fresh machine ties on score and on lifetime tokens, so rotation decides.
    let fresh = vec![
        (account("codex-main", None), idle_state(0, 0, now)),
        (account("codex-alt", None), idle_state(0, 0, now)),
    ];
    assert_eq!(
        scores(SelectionPolicy::QuotaAware, &fresh, now),
        vec![
            ("codex-main".to_owned(), Some(-0.020)),
            ("codex-alt".to_owned(), Some(-0.020))
        ]
    );
    let cfg = Scoring::default();
    let ranked: Vec<Rank> = fresh
        .iter()
        .enumerate()
        .map(|(i, (a, s))| rank(SelectionPolicy::QuotaAware, a, s, 0, &cfg, now, i).unwrap())
        .collect();
    assert_eq!(ranked[0].compare(&ranked[1]), std::cmp::Ordering::Less);
}

// ---------------------------------------------------------------- pool fixtures

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

struct Harness {
    _dir: tempfile::TempDir,
    events: Events,
    pool: Arc<AccountPool>,
}

async fn harness(extra: &str) -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 tempdir");
    let (tx, events) = tokio::sync::mpsc::unbounded_channel();
    let run = RunId::new();
    let paths = RunPaths {
        run,
        dir: root.clone(),
        sock_dir: root.clone(),
    };
    let writer = Writer::open(&paths.journal(), FsyncPolicy::Never)
        .await
        .expect("journal writer");
    let handle = JournalHandle {
        run,
        tx,
        paths: Arc::new(paths),
        writer: Arc::new(tokio::sync::Mutex::new(writer)),
    };
    let cfg = config(extra);
    let pool = AccountPool::new(cfg, root.join("accounts.json"), handle).expect("pool");
    Harness {
        _dir: dir,
        events,
        pool,
    }
}

fn id(s: &str) -> AccountId {
    AccountId(s.into())
}

const TWO_UNCAPPED: &str = r#"
[brain]
reserve_brain_slot = false
[providers.anthropic]
models = { mid = "tier-mid" }
[[accounts]]
id = "alt"
provider = "anthropic"
exec = "claude-alt"
[[accounts]]
id = "main"
provider = "anthropic"
exec = "claude-main"
"#;

const ONE_CAPPED: &str = r#"
[brain]
reserve_brain_slot = false
[providers.anthropic]
models = { mid = "tier-mid" }
[[accounts]]
id = "main"
provider = "anthropic"
exec = "claude-main"
max_concurrency = 2
"#;

async fn acquire(pool: &Arc<AccountPool>, ms: u64) -> Result<swamp::dispatch::Lease, NoCapacity> {
    pool.acquire(
        Provider::Anthropic,
        &HashSet::new(),
        Instant::now() + Duration::from_millis(ms),
    )
    .await
}

fn drain(events: &mut Events) -> Vec<(Option<NodeId>, JournalEvent)> {
    let mut out = Vec::new();
    while let Ok(e) = events.try_recv() {
        out.push(e);
    }
    out
}

// ---------------------------------------------------------------- §4.3 gates and §4.7 waits

/// 5. Everything exhausted: Swamp waits for the earliest reset, it does not fail.
#[tokio::test]
async fn everything_exhausted_waits_for_the_earliest_reset() {
    let h = harness(TWO_UNCAPPED).await;
    let now = OffsetDateTime::now_utc();
    let resets_at = now + Duration::from_secs(40 * 60);
    h.pool.observe_quota(
        &id("main"),
        RateLimitSnapshot {
            status: LimitStatus::Warning,
            windows: vec![LimitWindow {
                scope: LimitScope::SevenDay,
                utilization: 0.985,
                resets_at: Some(resets_at),
                measured: true,
                ..Default::default()
            }],
            ..Default::default()
        },
    );
    h.pool.report(
        &id("alt"),
        Some(&Failure::RateLimited {
            resets_at: Some(now + Duration::from_secs(80 * 60)),
            scope: LimitScope::FiveHour,
            detected_by: Detector::Telemetry,
            evidence: "usage limit reached".into(),
        }),
        None,
    );

    let Some(NoCapacity::AllExhausted { retry_at, why }) =
        h.pool.all_exhausted(Provider::Anthropic, &HashSet::new())
    else {
        panic!("every account is out");
    };
    assert!(
        (retry_at - resets_at).abs() < time::Duration::seconds(2),
        "the earliest reset wins: {retry_at} vs {resets_at}"
    );
    assert!(
        why.contains("main at 98% of its seven_day window until"),
        "{why}"
    );
    assert!(why.contains("alt cooling until"), "{why}");

    // The account past its measured stop is degraded before dispatch stops using it.
    let health = h
        .pool
        .snapshot()
        .into_iter()
        .find(|(_, a, _)| a == &id("main"))
        .map(|(_, _, s)| s.health)
        .expect("main");
    assert_eq!(health, Health::Degraded);
}

/// 6. An estimate is a guess: it deprioritises an account, it never parks one.
#[test]
fn an_estimated_window_never_parks_an_account() {
    let now = OffsetDateTime::now_utc();
    let a = account("codex-main", None);
    let estimated = AccountState {
        quota: Some(quota(vec![window(LimitScope::SevenDay, 0.99, false)])),
        ..idle_state(0, 0, now)
    };
    let measured = AccountState {
        quota: Some(quota(vec![window(LimitScope::SevenDay, 0.99, true)])),
        ..idle_state(0, 0, now)
    };
    let cfg = Scoring::default();
    assert!(score(SelectionPolicy::QuotaAware, &a, &estimated, 0, &cfg, now).is_some());
    assert_eq!(
        score(SelectionPolicy::QuotaAware, &a, &measured, 0, &cfg, now),
        None
    );
}

/// 7. The provider's own gate outranks every percentage.
#[test]
fn ordinary_usage_not_allowed_is_a_hard_gate() {
    let now = OffsetDateTime::now_utc();
    let a = account("codex-main", None);
    let s = AccountState {
        quota: Some(RateLimitSnapshot {
            windows: vec![window(LimitScope::SevenDay, 0.01, true)],
            ordinary_usage_allowed: Some(false),
            ..Default::default()
        }),
        ..idle_state(0, 0, now)
    };
    assert_eq!(
        score(
            SelectionPolicy::QuotaAware,
            &a,
            &s,
            0,
            &Scoring::default(),
            now
        ),
        None
    );
}

/// 8. Depleted credits are not a timer: the account is out, and it contributes no `retry_at`.
#[tokio::test]
async fn depleted_credits_are_out_and_carry_no_retry_time() {
    let now = OffsetDateTime::now_utc();
    let a = account("main", None);
    let s = AccountState {
        quota: Some(RateLimitSnapshot {
            windows: vec![LimitWindow {
                scope: LimitScope::SevenDay,
                utilization: 0.10,
                resets_at: Some(now + Duration::from_secs(600)),
                measured: true,
                ..Default::default()
            }],
            reached: Some(LimitReached::CreditsDepleted),
            ..Default::default()
        }),
        ..idle_state(0, 0, now)
    };
    assert_eq!(
        score(
            SelectionPolicy::QuotaAware,
            &a,
            &s,
            0,
            &Scoring::default(),
            now
        ),
        None
    );

    let h = harness(TWO_UNCAPPED).await;
    h.pool.observe_quota(
        &id("main"),
        RateLimitSnapshot {
            windows: vec![LimitWindow {
                scope: LimitScope::SevenDay,
                utilization: 0.10,
                // A reset time the pool must NOT wait for: nobody is coming.
                resets_at: Some(now + Duration::from_secs(600)),
                measured: true,
                ..Default::default()
            }],
            reached: Some(LimitReached::CreditsDepleted),
            ..Default::default()
        },
    );
    let cooling_until = now + Duration::from_secs(3600);
    h.pool.report(
        &id("alt"),
        Some(&Failure::RateLimited {
            resets_at: Some(cooling_until),
            scope: LimitScope::FiveHour,
            detected_by: Detector::Telemetry,
            evidence: "usage limit reached".into(),
        }),
        None,
    );
    let Some(NoCapacity::AllExhausted { retry_at, why }) =
        h.pool.all_exhausted(Provider::Anthropic, &HashSet::new())
    else {
        panic!("every account is out");
    };
    assert!(
        (retry_at - cooling_until).abs() < time::Duration::seconds(2),
        "a credits-depleted account must not set retry_at: {retry_at}"
    );
    assert!(why.contains("main has no credits left"), "{why}");
}

/// 9. An uncapped pool takes every concurrent acquirer; a capped one queues them, and none
/// of the queued ones fails.
#[tokio::test]
async fn concurrent_acquires_spread_without_failing() {
    let h = harness(TWO_UNCAPPED).await;
    // `alt` has already spent a window's worth of tokens, so `share` sends work to `main`.
    h.pool
        .observe_usage(&id("alt"), NodeId::new(), tokens(1_000_000));
    let mut leases = Vec::new();
    for _ in 0..10 {
        leases.push(
            acquire(&h.pool, 2_000)
                .await
                .expect("no cap means no queue"),
        );
    }
    let on_main = leases.iter().filter(|l| l.account == id("main")).count();
    assert!(
        on_main > 5,
        "share must send most of the batch to the idle account, got {on_main}/10"
    );
    drop(leases);

    let capped = harness(ONE_CAPPED).await;
    let mut tasks = Vec::new();
    for _ in 0..10 {
        let pool = Arc::clone(&capped.pool);
        tasks.push(tokio::spawn(async move {
            pool.acquire(
                Provider::Anthropic,
                &HashSet::new(),
                Instant::now() + Duration::from_secs(30),
            )
            .await
        }));
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
    let done = tasks.iter().filter(|t| t.is_finished()).count();
    assert_eq!(
        done, 2,
        "max_concurrency = 2 means two leases and eight waits"
    );
    for t in tasks {
        if t.is_finished() {
            t.await.expect("join").expect("a lease");
        } else {
            t.abort();
        }
    }
}

/// 10. One `NodeBlocked` per blocked node, not one per loop iteration, and no spinning.
#[tokio::test]
async fn a_blocked_node_journals_one_line_and_does_not_spin() {
    let mut h = harness(TWO_UNCAPPED).await;
    for who in ["main", "alt"] {
        h.pool.cooldown(&id(who), Duration::from_secs(1800), "test");
    }
    drain(&mut h.events);
    let node = NodeId::new();
    let before = h.pool.wakeups();
    let got = h
        .pool
        .acquire_node(
            Provider::Anthropic,
            &HashSet::new(),
            Instant::now() + Duration::from_millis(400),
            Some(node),
            None,
        )
        .await;
    assert!(matches!(got, Err(NoCapacity::Saturated)), "expected a wait");
    assert!(
        h.pool.wakeups() - before <= 4,
        "acquire spun: {} wakeups",
        h.pool.wakeups() - before
    );
    let blocked: Vec<_> = drain(&mut h.events)
        .into_iter()
        .filter(|(n, e)| matches!(e, JournalEvent::NodeBlocked { .. }) && *n == Some(node))
        .collect();
    assert_eq!(blocked.len(), 1, "one NodeBlocked per blocked node");
}

/// 11. `esc esc` while blocked comes back at once, and it is a cancellation, not a capacity
/// problem.
#[tokio::test]
async fn a_cancelled_token_ends_the_wait_promptly() {
    let h = harness(TWO_UNCAPPED).await;
    for who in ["main", "alt"] {
        h.pool.cooldown(&id(who), Duration::from_secs(3600), "test");
    }
    let cancel = CancellationToken::new();
    let fired = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        fired.cancel();
    });
    let started = std::time::Instant::now();
    let got = h
        .pool
        .acquire_node(
            Provider::Anthropic,
            &HashSet::new(),
            Instant::now() + Duration::from_secs(600),
            Some(NodeId::new()),
            Some(&cancel),
        )
        .await;
    assert!(
        matches!(got, Err(NoCapacity::Cancelled)),
        "expected a cancellation"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a cancelled wait must end at once, took {:?}",
        started.elapsed()
    );
}

/// The ledger is the live half of `share`: a running node's tokens count before it commits,
/// and they are counted exactly once when it does.
#[tokio::test]
async fn usage_is_cumulative_live_and_committed_once() {
    let h = harness(TWO_UNCAPPED).await;
    let node = NodeId::new();
    for total in [100u64, 200, 200] {
        h.pool.observe_usage(&id("main"), node, tokens(total));
    }
    let live = h
        .pool
        .snapshot()
        .into_iter()
        .find(|(_, a, _)| a == &id("main"))
        .map(|(_, _, s)| s)
        .expect("main");
    assert_eq!(live.window_tokens.billable(), 200);
    assert_eq!(live.lifetime_tokens.billable(), 200);

    h.pool.commit_usage(&id("main"), node, tokens(200));
    // A stale observation after the commit changes nothing.
    h.pool.observe_usage(&id("main"), node, tokens(50));
    let after = h
        .pool
        .snapshot()
        .into_iter()
        .find(|(_, a, _)| a == &id("main"))
        .map(|(_, _, s)| s)
        .expect("main");
    assert_eq!(after.window_tokens.billable(), 200);
    assert_eq!(after.lifetime_tokens.billable(), 200);
}
