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

/// The default `reserve_brain_slot`: the brain holds one account back from the workers.
const TWO_RESERVING: &str = r#"
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

/// A measured window whose reset has passed is stale, not a gate: the account has to come back
/// on its own, because only a node running on it could ever refresh the reading.
#[tokio::test]
async fn a_window_past_its_reset_stops_gating_dispatch() {
    let h = harness(TWO_UNCAPPED).await;
    let now = OffsetDateTime::now_utc();
    for who in ["main", "alt"] {
        h.pool.observe_quota(
            &id(who),
            RateLimitSnapshot {
                status: LimitStatus::Warning,
                windows: vec![LimitWindow {
                    scope: LimitScope::FiveHour,
                    utilization: 0.99,
                    resets_at: Some(now - Duration::from_secs(60)),
                    measured: true,
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
    }
    assert!(
        h.pool
            .all_exhausted(Provider::Anthropic, &HashSet::new())
            .is_none(),
        "a rolled window must not leave the pool permanently exhausted"
    );
    acquire(&h.pool, 200).await.expect("the window has rolled");
}

/// A provider whose accounts are all hard-gated is exactly where failover is the only help,
/// so the predicate `retry.rs` uses has to answer for it too.
#[tokio::test]
async fn a_hard_gated_provider_reports_no_capacity_for_failover() {
    let h = harness(TWO_UNCAPPED).await;
    for who in ["main", "alt"] {
        h.pool.report(
            &id(who),
            Some(&Failure::AuthExpired {
                detected_by: Detector::ExitCode,
                detail: "token expired".into(),
            }),
            None,
        );
    }
    let Some(NoCapacity::Exhausted { reason }) =
        h.pool.all_exhausted(Provider::Anthropic, &HashSet::new())
    else {
        panic!("auth-broken accounts are capacity the pool will never regain");
    };
    assert!(reason.contains("re-authentication"), "{reason}");
}

/// Two buckets are two allowances: a snapshot for one must not inherit the other's windows.
#[test]
fn a_snapshot_never_merges_across_limit_buckets() {
    let now = OffsetDateTime::now_utc();
    let mut state = AccountState::default();
    state.apply_quota(
        RateLimitSnapshot {
            limit_id: Some("codex".into()),
            windows: vec![
                LimitWindow {
                    scope: LimitScope::FiveHour,
                    utilization: 0.97,
                    resets_at: Some(now + Duration::from_secs(3600)),
                    measured: true,
                    ..Default::default()
                },
                LimitWindow {
                    scope: LimitScope::SevenDay,
                    utilization: 0.30,
                    resets_at: Some(now + Duration::from_secs(86400)),
                    measured: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        },
        swamp::dispatch::account::QuotaSource::AppServer,
        now,
    );
    state.apply_quota(
        RateLimitSnapshot {
            limit_id: Some("codex_bengalfox".into()),
            windows: vec![LimitWindow {
                scope: LimitScope::SevenDay,
                utilization: 0.0,
                resets_at: Some(now + Duration::from_secs(86400)),
                measured: true,
                ..Default::default()
            }],
            ..Default::default()
        },
        swamp::dispatch::account::QuotaSource::AppServer,
        now,
    );
    let quota = state.quota.as_ref().expect("a snapshot");
    assert_eq!(quota.limit_id.as_deref(), Some("codex_bengalfox"));
    assert_eq!(quota.measured_utilization_at(now), Some(0.0));
    assert_eq!(
        state.quota_buckets["codex"].measured_utilization_at(now),
        Some(0.97),
        "the other bucket keeps its own windows"
    );
}

/// The token counter is keyed to the longest window, so a five-hour reading overtaking a
/// seven-day one is not a roll and must not zero what the account has spent.
#[test]
fn the_token_window_does_not_roll_when_the_tightest_scope_changes() {
    let now = OffsetDateTime::now_utc();
    let five = now + Duration::from_secs(3600);
    let seven = now + Duration::from_secs(86400);
    let snapshot = |five_util: f64| RateLimitSnapshot {
        windows: vec![
            LimitWindow {
                scope: LimitScope::FiveHour,
                utilization: five_util,
                resets_at: Some(five),
                window_minutes: Some(300),
                measured: true,
            },
            LimitWindow {
                scope: LimitScope::SevenDay,
                utilization: 0.64,
                resets_at: Some(seven),
                window_minutes: Some(10080),
                measured: true,
            },
        ],
        ..Default::default()
    };
    let mut state = AccountState::default();
    state.apply_quota(
        snapshot(0.10),
        swamp::dispatch::account::QuotaSource::Telemetry,
        now,
    );
    state.credit_tokens(&tokens(800_000));
    let rolled = state.apply_quota(
        snapshot(0.70),
        swamp::dispatch::account::QuotaSource::Telemetry,
        now,
    );
    assert!(!rolled, "no window rolled, only the tightest one changed");
    assert_eq!(state.window_tokens.billable(), 800_000);
}

/// An estimate stands in for a missing measurement, never for a live one.
#[test]
fn an_estimate_never_replaces_a_measurement_that_has_not_rolled() {
    let now = OffsetDateTime::now_utc();
    let measured = RateLimitSnapshot {
        windows: vec![LimitWindow {
            scope: LimitScope::SevenDay,
            utilization: 0.99,
            resets_at: Some(now + Duration::from_secs(86400)),
            measured: true,
            ..Default::default()
        }],
        ..Default::default()
    };
    let estimate = RateLimitSnapshot {
        windows: vec![LimitWindow {
            scope: LimitScope::SevenDay,
            utilization: 0.04,
            resets_at: Some(now + Duration::from_secs(86400)),
            measured: false,
            ..Default::default()
        }],
        ..Default::default()
    };
    let merged = estimate.merged_over(&measured, now);
    assert_eq!(merged.measured_utilization_at(now), Some(0.99));
}

/// USAGE 2.3: the app-server poll runs at most one in flight per account. Five nodes
/// terminating together must not spawn five subprocesses and five network calls.
#[tokio::test]
async fn the_app_server_probe_is_single_flight_per_account() {
    let account = AccountId("codex-main".into());
    let gate = swamp::dispatch::retry::probe_gate(&account);
    assert!(
        Arc::ptr_eq(&gate, &swamp::dispatch::retry::probe_gate(&account)),
        "one gate per account"
    );
    assert!(
        !Arc::ptr_eq(
            &gate,
            &swamp::dispatch::retry::probe_gate(&AccountId("other".into()))
        ),
        "and never shared between accounts"
    );

    let mut held = gate.lock().await;
    assert!(
        gate.try_lock().is_err(),
        "a second terminating node waits for the answer instead of probing too"
    );
    *held = Some(Instant::now());
    drop(held);
    let last = gate.lock().await;
    assert!(
        last.is_some_and(|t: Instant| t.elapsed() < Duration::from_secs(5)),
        "the late arrival can see how fresh the last probe was"
    );
}

/// USAGE 2.2/3.2: a hard reason is `Health::AuthBroken`, so `swamp accounts`, the welcome box
/// and `/usage` cannot call an account healthy that dispatch refuses.
#[tokio::test]
async fn a_hard_gated_account_is_auth_broken_for_every_surface() {
    let now = OffsetDateTime::now_utc();
    let depleted = RateLimitSnapshot {
        windows: vec![LimitWindow {
            scope: LimitScope::SevenDay,
            utilization: 0.10,
            resets_at: Some(now + Duration::from_secs(600)),
            measured: true,
            ..Default::default()
        }],
        reached: Some(LimitReached::CreditsDepleted),
        ordinary_usage_allowed: Some(false),
        ..Default::default()
    };
    let h = harness(TWO_UNCAPPED).await;
    h.pool.observe_quota(&id("main"), depleted);
    let health = |who: &str| {
        h.pool
            .snapshot()
            .into_iter()
            .find(|(_, a, _)| a == &id(who))
            .map(|(_, _, s)| s.health)
            .expect("account state")
    };
    assert_eq!(health("main"), Health::AuthBroken);
    assert_eq!(health("alt"), Health::Healthy);

    // The same gate, learned while the account was cooling: the timer is not the story.
    h.pool
        .cooldown(&id("alt"), Duration::from_secs(3600), "test");
    h.pool.observe_quota(
        &id("alt"),
        RateLimitSnapshot {
            reached: Some(LimitReached::SpendControl),
            ..Default::default()
        },
    );
    assert_eq!(health("alt"), Health::AuthBroken);
}

/// USAGE 4.7: a hard-gated account contributes no `retry_at`, even when the same terminal
/// node also cooled it. The two conditions co-occur on every provider 429 that says
/// `usage_limit_reached`, so the order of the gates is what decides the notice.
#[tokio::test]
async fn a_depleted_account_that_is_also_cooling_carries_no_retry_time() {
    let h = harness(TWO_UNCAPPED).await;
    let now = OffsetDateTime::now_utc();
    for who in ["main", "alt"] {
        h.pool.report(
            &id(who),
            Some(&Failure::RateLimited {
                resets_at: Some(now + Duration::from_secs(3600)),
                scope: LimitScope::FiveHour,
                detected_by: Detector::Telemetry,
                evidence: "usage limit reached".into(),
            }),
            None,
        );
        h.pool.observe_quota(
            &id(who),
            RateLimitSnapshot {
                windows: vec![LimitWindow {
                    scope: LimitScope::FiveHour,
                    utilization: 0.20,
                    resets_at: Some(now + Duration::from_secs(3600)),
                    measured: true,
                    ..Default::default()
                }],
                reached: Some(LimitReached::CreditsDepleted),
                ..Default::default()
            },
        );
    }
    let Some(NoCapacity::Exhausted { reason }) =
        h.pool.all_exhausted(Provider::Anthropic, &HashSet::new())
    else {
        panic!("a credits-depleted account needs a human, not a timer");
    };
    assert!(reason.contains("has no credits left"), "{reason}");
}

/// The cooldown and the health word are two independent gates: `swamp accounts enable`
/// rewrites one and leaves the other, and dispatch must still honour the live timer.
#[test]
fn a_live_cooldown_gates_whatever_the_health_word_says() {
    let now = OffsetDateTime::now_utc();
    let a = account("main", None);
    let s = AccountState {
        health: Health::Healthy,
        cooldown_until: Some(now + Duration::from_secs(4 * 3600)),
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
        None,
        "a cooling account is out of rotation whatever its health field says"
    );
}

/// DESIGN 139: the reservation is re-read on every selection. A peer that degrades after the
/// brain acquired must not strand every worker on an account the pool is holding back.
#[tokio::test]
async fn a_reservation_is_dropped_when_it_would_leave_the_workers_with_none() {
    let h = harness(TWO_RESERVING).await;
    let reserved = h
        .pool
        .reserve_for_brain(Provider::Anthropic)
        .expect("two accounts reserve one");
    let peer = if reserved == id("main") {
        "alt"
    } else {
        "main"
    };
    let lease = acquire(&h.pool, 200)
        .await
        .expect("the peer takes the node");
    assert_eq!(lease.account, id(peer));
    drop(lease);

    h.pool
        .cooldown(&id(peer), Duration::from_secs(5 * 3600), "rate limited");
    let lease = acquire(&h.pool, 200)
        .await
        .expect("holding back the only usable account helps nobody");
    assert_eq!(lease.account, reserved);
}

/// A finished or failed brain holds nothing back: the reservation ends with its lease.
#[tokio::test]
async fn the_brain_reservation_ends_with_the_brain_lease() {
    let h = harness(TWO_RESERVING).await;
    let brain = h
        .pool
        .acquire_brain(
            Provider::Anthropic,
            None,
            Instant::now() + Duration::from_millis(200),
        )
        .await
        .expect("the brain acquires");
    let held = brain.account.clone();
    drop(brain);

    // The account the brain used is now the worst candidate; a stale reservation would
    // still name it.
    h.pool
        .observe_usage(&held, NodeId::new(), tokens(5_000_000));
    let again = h
        .pool
        .reserve_for_brain(Provider::Anthropic)
        .expect("a new brain reserves again");
    assert_ne!(again, held, "the reservation did not survive its lease");
}

/// USAGE 2.3: the chat `/usage` probe takes the same per-account single flight a terminating
/// node takes, so one question never spawns two app-server subprocesses.
#[tokio::test]
async fn the_chat_probe_waits_on_the_account_probe_gate() {
    let account = AccountId("codex-usage".into());
    let max_age = Duration::from_secs(60);
    let claim = swamp::dispatch::retry::claim_probe(&account, max_age)
        .await
        .expect("the first caller probes");
    assert!(
        swamp::dispatch::retry::probe_gate(&account)
            .try_lock()
            .is_err(),
        "a second caller waits for the answer instead of probing too"
    );
    claim.stamp();
    assert!(
        swamp::dispatch::retry::claim_probe(&account, max_age)
            .await
            .is_none(),
        "a fresh reading answers for everyone"
    );
}

const BRAIN_HELD_PEER: &str = r#"
[brain]
reserve_brain_slot = false
[providers.anthropic]
models = { mid = "tier-mid" }
[[accounts]]
id = "main"
provider = "anthropic"
exec = "claude-main"
max_concurrency = 3
[[accounts]]
id = "work"
provider = "anthropic"
exec = "claude-work"
max_concurrency = 1
"#;

/// USAGE 4.7: a slot the brain holds for the whole run returns to nobody, so it must not
/// report the pool as merely busy and hide the exhausted peers from failover.
#[tokio::test]
async fn a_brain_held_account_does_not_mask_an_exhausted_pool() {
    let h = harness(BRAIN_HELD_PEER).await;
    let brain = h
        .pool
        .acquire_brain(
            Provider::Anthropic,
            Some(&id("work")),
            Instant::now() + Duration::from_millis(200),
        )
        .await
        .expect("the brain leases work");
    assert_eq!(brain.account, id("work"));

    let now = OffsetDateTime::now_utc();
    let resets_at = now + Duration::from_secs(40 * 60);
    h.pool.observe_quota(
        &id("main"),
        RateLimitSnapshot {
            status: LimitStatus::Warning,
            windows: vec![LimitWindow {
                scope: LimitScope::SevenDay,
                utilization: 0.99,
                resets_at: Some(resets_at),
                measured: true,
                ..Default::default()
            }],
            ..Default::default()
        },
    );

    let Some(NoCapacity::AllExhausted { retry_at, why }) =
        h.pool.all_exhausted(Provider::Anthropic, &HashSet::new())
    else {
        panic!("the run-long brain lease hid the exhausted pool");
    };
    assert!(
        (retry_at - resets_at).abs() < time::Duration::seconds(2),
        "the peer's reset is the retry time: {retry_at} vs {resets_at}"
    );
    assert!(why.contains("work is held by the brain"), "{why}");
    drop(brain);
}

/// USAGE 3.1: chat refreshes both halves of the pool view, not just the rows. The pool
/// adopts accounts other repos wrote while the chat runs, and `/usage` has to list them
/// under `not in config` exactly as `swamp usage` does.
#[tokio::test]
async fn the_chat_pool_refresh_picks_up_an_account_adopted_mid_session() {
    use swamp::ui::chat::app::{App, Effect};

    let h = harness(ONE_CAPPED).await;
    let mut app = App::new(
        RunId::new(),
        swamp::ui::chat::theme::Theme::plain(),
        swamp::ui::chat::blocks::WelcomeInfo::default(),
        swamp::ui::chat::input::History::load(None, 0),
        &h.pool.cfg,
    );
    app.width = 100;
    app.now = OffsetDateTime::now_utc();

    let usage_text = |app: &mut App| -> String {
        app.command("usage")
            .into_iter()
            .filter_map(|e| match e {
                Effect::Commit(body) => Some(swamp::ui::chat::blocks::text_of(&body).join("\n")),
                _ => None,
            })
            .collect::<Vec<String>>()
            .join("\n")
    };

    swamp::ui::chat::refresh_pool(&mut app, &h.pool);
    assert!(!usage_text(&mut app).contains("codex-work"));

    // Another repo's run records an account this config does not name.
    let mut theirs = swamp::dispatch::persist::StateMap::new();
    theirs.insert(
        id("codex-work"),
        AccountState {
            lifetime_nodes: 2,
            updated_at: Some(OffsetDateTime::now_utc()),
            ..Default::default()
        },
    );
    swamp::dispatch::persist::merge_state(&h.pool.state_path, &theirs).expect("their write");
    h.pool.report(&id("main"), None, None);

    swamp::ui::chat::refresh_pool(&mut app, &h.pool);
    let text = usage_text(&mut app);
    assert!(text.contains("not in config"), "{text}");
    assert!(text.contains("codex-work"), "{text}");
}
