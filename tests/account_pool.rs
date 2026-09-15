//! WP4: the account pool. Selection, concurrency, cooldowns, persistence, reservation.

use camino::Utf8PathBuf;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use swamp::config::{Config, load, resolve, validate};
use swamp::dispatch::{AccountPool, Health, NoCapacity, SelectionPolicy};
use swamp::journal::JournalHandle;
use swamp::journal::paths::RunPaths;
use swamp::journal::writer::{FsyncPolicy, Writer};
use swamp::model::core::{
    AccountId, LimitScope, LimitStatus, LimitWindow, Provider, RateLimitSnapshot,
};
use swamp::model::failure::{Detector, Failure};
use swamp::{JournalEvent, NodeId, RunId};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::Instant;

type Events = UnboundedReceiver<(Option<NodeId>, JournalEvent)>;

const TWO_ACCOUNTS: &str = r#"
[brain]
reserve_brain_slot = false
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

async fn journal(dir: &Utf8PathBuf) -> (JournalHandle, Events) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let run = RunId::new();
    let paths = RunPaths {
        run,
        dir: dir.clone(),
        sock_dir: dir.clone(),
    };
    let writer = Writer::open(&paths.journal(), FsyncPolicy::Never)
        .await
        .expect("journal writer");
    (
        JournalHandle {
            run,
            tx,
            paths: Arc::new(paths),
            writer: Arc::new(tokio::sync::Mutex::new(writer)),
        },
        rx,
    )
}

fn tmp() -> (tempfile::TempDir, Utf8PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 tempdir");
    (dir, path)
}

struct Harness {
    _dir: tempfile::TempDir,
    _events: Events,
    pool: Arc<AccountPool>,
    state_path: Utf8PathBuf,
    cfg: Arc<Config>,
}

async fn harness(extra: &str) -> Harness {
    let (dir, root) = tmp();
    let cfg = config(extra);
    let (handle, events) = journal(&root).await;
    let state_path = root.join("accounts.json");
    let pool = AccountPool::new(Arc::clone(&cfg), state_path.clone(), handle).expect("pool");
    Harness {
        _dir: dir,
        _events: events,
        pool,
        state_path,
        cfg,
    }
}

fn id(s: &str) -> AccountId {
    AccountId(s.into())
}

fn soon() -> Instant {
    Instant::now() + Duration::from_millis(150)
}

fn quota(util: f64) -> RateLimitSnapshot {
    RateLimitSnapshot {
        status: LimitStatus::Allowed,
        windows: vec![LimitWindow {
            scope: LimitScope::SevenDay,
            utilization: util,
            ..Default::default()
        }],
        resets_at: None,
        ..Default::default()
    }
}

fn rate_limited(resets_at: Option<time::OffsetDateTime>) -> Failure {
    Failure::RateLimited {
        resets_at,
        scope: LimitScope::FiveHour,
        detected_by: Detector::Telemetry,
        evidence: "usage limit reached".into(),
    }
}

async fn acquire(pool: &Arc<AccountPool>) -> Result<swamp::dispatch::Lease, NoCapacity> {
    pool.acquire(Provider::Anthropic, &HashSet::new(), soon())
        .await
}

fn health_of(pool: &Arc<AccountPool>, who: &str) -> Health {
    pool.snapshot()
        .into_iter()
        .find(|(_, a, _)| a.0 == who)
        .map(|(_, _, s)| s.health)
        .expect("account is configured")
}

#[test]
fn quota_aware_is_the_default_policy() {
    let cfg = config(TWO_ACCOUNTS);
    assert_eq!(cfg.dispatch.policy, Some(SelectionPolicy::QuotaAware));
}

#[tokio::test]
async fn round_robin_alternates_between_equal_accounts() {
    let h = harness(&format!(
        "{TWO_ACCOUNTS}\n[dispatch]\npolicy = \"round-robin\"\n"
    ))
    .await;
    let first = acquire(&h.pool).await.expect("first lease").account.clone();
    let second = acquire(&h.pool)
        .await
        .expect("second lease")
        .account
        .clone();
    assert_ne!(
        first, second,
        "round robin must not pick the same account twice"
    );
    let third = acquire(&h.pool).await.expect("third lease").account.clone();
    assert_eq!(third, first);
}

#[tokio::test]
async fn least_loaded_prefers_the_account_with_fewer_inflight() {
    let h = harness(&format!(
        "{TWO_ACCOUNTS}\n[dispatch]\npolicy = \"least-loaded\"\n"
    ))
    .await;
    // Weight only breaks the tie for the first pick; load decides the second.
    let held = h
        .pool
        .acquire(Provider::Anthropic, &HashSet::new(), soon())
        .await
        .expect("held");
    let next = acquire(&h.pool).await.expect("next lease");
    assert_ne!(held.account, next.account);
}

/// Two idle accounts scored identically and the tie fell to the map order, so every
/// sequential single-worker run went to the same subscription.
#[tokio::test]
async fn sequential_runs_alternate_between_two_idle_accounts() {
    let h = harness(&format!(
        "{TWO_ACCOUNTS}\n[dispatch]\npolicy = \"least-loaded\"\n"
    ))
    .await;
    let mut picks = Vec::new();
    for _ in 0..4 {
        let lease = acquire(&h.pool).await.expect("lease");
        let who = lease.account.clone();
        drop(lease);
        h.pool.report(&who, None, None);
        picks.push(who);
    }
    assert_eq!(picks[0], picks[2], "{picks:?}");
    assert_eq!(picks[1], picks[3], "{picks:?}");
    assert_ne!(
        picks[0], picks[1],
        "one subscription took every run: {picks:?}"
    );
}

/// What breaks the tie lives in accounts.json, so a second process keeps alternating.
#[tokio::test]
async fn a_fresh_pool_keeps_alternating_from_the_persisted_state() {
    let h = harness(TWO_ACCOUNTS).await;
    let first = acquire(&h.pool).await.expect("lease").account.clone();
    h.pool.report(&first, None, None);
    drop(h.pool);

    let (_dir2, root2) = tmp();
    let (handle, _events) = journal(&root2).await;
    let fresh = AccountPool::new(Arc::clone(&h.cfg), h.state_path.clone(), handle).expect("pool");
    let second = acquire(&fresh).await.expect("lease").account.clone();
    assert_ne!(first, second, "the second run repeated the first account");
}

#[tokio::test]
async fn quota_aware_prefers_the_less_used_account_and_degrades_without_telemetry() {
    let h = harness(&format!(
        "{TWO_ACCOUNTS}\n[dispatch]\npolicy = \"quota-aware\"\n"
    ))
    .await;
    h.pool.observe_quota(&id("alt"), quota(0.9));
    h.pool.observe_quota(&id("main"), quota(0.1));
    let lease = acquire(&h.pool).await.expect("lease");
    assert_eq!(lease.account, id("main"));
    drop(lease);

    // No telemetry at all: the score collapses to load and both are equal, so a lease is
    // still handed out rather than the pool stalling.
    let plain = harness(&format!(
        "{TWO_ACCOUNTS}\n[dispatch]\npolicy = \"quota-aware\"\n"
    ))
    .await;
    assert!(acquire(&plain.pool).await.is_ok());
}

#[tokio::test]
async fn per_account_concurrency_is_a_hard_ceiling() {
    let h = harness(
        r#"
[brain]
reserve_brain_slot = false
[providers.anthropic]
models = { mid = "tier-mid" }
[[accounts]]
id = "main"
provider = "anthropic"
exec = "claude-main"
max_concurrency = 2
"#,
    )
    .await;
    let _a = acquire(&h.pool).await.expect("first");
    let _b = acquire(&h.pool).await.expect("second");
    let third = acquire(&h.pool).await;
    assert!(
        matches!(third, Err(NoCapacity::Saturated)),
        "a third lease must not be issued"
    );
}

const ONE_ACCOUNT_SOLO: &str = r#"
[brain]
reserve_brain_slot = false
[providers.anthropic]
models = { mid = "tier-mid" }
[[accounts]]
id = "main"
provider = "anthropic"
exec = "claude-main"
max_concurrency = 1
"#;

#[tokio::test]
async fn acquire_waits_without_spinning() {
    let h = harness(ONE_ACCOUNT_SOLO).await;
    let _a = acquire(&h.pool).await.expect("first");
    let before = h.pool.wakeups();
    let denied = h
        .pool
        .acquire(
            Provider::Anthropic,
            &HashSet::new(),
            Instant::now() + Duration::from_millis(200),
        )
        .await;
    assert!(matches!(denied, Err(NoCapacity::Saturated)));
    assert!(
        h.pool.wakeups() - before <= 4,
        "acquire spun: {} wakeups",
        h.pool.wakeups() - before
    );
}

#[tokio::test]
async fn a_panicking_task_releases_its_permit_and_slot() {
    let h = harness(ONE_ACCOUNT_SOLO).await;
    let pool = Arc::clone(&h.pool);
    let joined = tokio::spawn(async move {
        let _lease = acquire(&pool).await.expect("lease");
        panic!("worker task exploded");
    })
    .await;
    assert!(joined.is_err(), "the task is expected to panic");

    let inflight: usize = h.pool.snapshot().iter().map(|(_, _, s)| s.inflight).sum();
    assert_eq!(inflight, 0, "a panic must not leak an inflight slot");
    assert!(acquire(&h.pool).await.is_ok(), "the permit must come back");
}

#[tokio::test]
async fn a_rate_limit_cools_the_account_until_the_provider_reset() {
    let h = harness(TWO_ACCOUNTS).await;
    let resets_at = time::OffsetDateTime::now_utc() + Duration::from_secs(20 * 60);
    h.pool
        .report(&id("main"), Some(&rate_limited(Some(resets_at))), None);

    let state = h
        .pool
        .snapshot()
        .into_iter()
        .find(|(_, a, _)| a.0 == "main")
        .map(|(_, _, s)| s)
        .expect("main");
    assert_eq!(state.health, Health::Cooling);
    let until = state.cooldown_until.expect("cooldown_until");
    assert!(
        (until - resets_at).abs() < time::Duration::seconds(2),
        "{until} should track the provider reset {resets_at}"
    );

    // Only the limited account is parked.
    assert_eq!(health_of(&h.pool, "alt"), Health::Healthy);
    assert_eq!(acquire(&h.pool).await.expect("lease").account, id("alt"));
}

#[tokio::test]
async fn a_task_failure_never_cools_an_account() {
    let h = harness(TWO_ACCOUNTS).await;
    h.pool.report(
        &id("main"),
        Some(&Failure::WorkerError {
            subtype: "error_during_execution".into(),
            detail: "the tests failed".into(),
        }),
        None,
    );
    assert_eq!(health_of(&h.pool, "main"), Health::Healthy);
}

#[tokio::test]
async fn consecutive_crashes_trip_the_circuit_breaker() {
    let h = harness(&format!(
        "{TWO_ACCOUNTS}\n[cooldown]\nbreaker_threshold = 3\n"
    ))
    .await;
    for _ in 0..2 {
        h.pool.report(
            &id("main"),
            Some(&Failure::Crashed { signal: Some(9) }),
            None,
        );
    }
    assert_eq!(health_of(&h.pool, "main"), Health::Healthy);
    h.pool.report(
        &id("main"),
        Some(&Failure::Crashed { signal: Some(9) }),
        None,
    );
    assert_eq!(health_of(&h.pool, "main"), Health::Cooling);
}

#[tokio::test]
async fn a_success_clears_the_breaker_count() {
    let h = harness(&format!(
        "{TWO_ACCOUNTS}\n[cooldown]\nbreaker_threshold = 2\n"
    ))
    .await;
    h.pool
        .report(&id("main"), Some(&Failure::Crashed { signal: None }), None);
    h.pool.report(&id("main"), None, None);
    h.pool
        .report(&id("main"), Some(&Failure::Crashed { signal: None }), None);
    assert_eq!(health_of(&h.pool, "main"), Health::Healthy);
}

#[tokio::test]
async fn proactive_quota_stop_blocks_new_leases_but_not_a_running_one() {
    let h = harness(
        r#"
[brain]
reserve_brain_slot = false
[cooldown]
quota_warn_at = 0.90
quota_stop_at = 0.98
[providers.anthropic]
models = { mid = "tier-mid" }
[[accounts]]
id = "main"
provider = "anthropic"
exec = "claude-main"
max_concurrency = 2
"#,
    )
    .await;
    let running = acquire(&h.pool).await.expect("lease");
    h.pool.observe_quota(&id("main"), quota(0.99));

    assert!(matches!(
        acquire(&h.pool).await,
        Err(NoCapacity::Exhausted { .. })
    ));
    assert_eq!(running.account, id("main"), "the live lease is untouched");
    let inflight: usize = h.pool.snapshot().iter().map(|(_, _, s)| s.inflight).sum();
    assert_eq!(inflight, 1);
}

#[tokio::test]
async fn quota_past_the_warning_threshold_only_degrades() {
    let h = harness(TWO_ACCOUNTS).await;
    h.pool.observe_quota(&id("main"), quota(0.95));
    assert_eq!(health_of(&h.pool, "main"), Health::Degraded);
    assert!(acquire(&h.pool).await.is_ok());
}

/// Every account cooling is a wait, not a failure: the node's own deadline is what ends it.
#[tokio::test]
async fn every_account_cooling_blocks_instead_of_failing() {
    let h = harness(TWO_ACCOUNTS).await;
    for who in ["main", "alt"] {
        h.pool.report(&id(who), Some(&rate_limited(None)), None);
    }
    let Some(NoCapacity::AllExhausted { retry_at, why }) =
        h.pool.all_exhausted(Provider::Anthropic, &HashSet::new())
    else {
        panic!("every account is cooling");
    };
    assert!(retry_at > time::OffsetDateTime::now_utc());
    assert!(why.contains("cooling until"), "{why}");
    match acquire(&h.pool).await {
        Err(NoCapacity::Saturated) => {}
        Err(other) => panic!("expected Saturated, got {other:?}"),
        Ok(lease) => panic!("expected a wait, got a lease on {}", lease.account.0),
    }
}

#[tokio::test]
async fn cooldowns_survive_a_new_pool_on_the_same_state_file() {
    let h = harness(TWO_ACCOUNTS).await;
    h.pool.report(&id("main"), Some(&rate_limited(None)), None);
    drop(h.pool);

    let (_dir2, root2) = tmp();
    let (handle, _events) = journal(&root2).await;
    let fresh = AccountPool::new(Arc::clone(&h.cfg), h.state_path.clone(), handle).expect("pool");
    assert_eq!(health_of(&fresh, "main"), Health::Cooling);
    assert_eq!(acquire(&fresh).await.expect("lease").account, id("alt"));
}

#[tokio::test]
async fn a_fresh_pool_does_not_inherit_stale_inflight_counts() {
    let h = harness(TWO_ACCOUNTS).await;
    let lease = acquire(&h.pool).await.expect("lease");
    h.pool.report(&lease.account, None, None);
    std::mem::forget(lease);

    let (_dir2, root2) = tmp();
    let (handle, _events) = journal(&root2).await;
    let fresh = AccountPool::new(Arc::clone(&h.cfg), h.state_path.clone(), handle).expect("pool");
    let inflight: usize = fresh.snapshot().iter().map(|(_, _, s)| s.inflight).sum();
    assert_eq!(inflight, 0);
}

#[tokio::test]
async fn a_reserved_brain_account_is_out_of_the_worker_pool() {
    let h = harness(
        r#"
[brain]
reserve_brain_slot = true
provider = "anthropic"
[providers.anthropic]
models = { mid = "tier-mid" }
[[accounts]]
id = "main"
provider = "anthropic"
exec = "claude-main"
max_concurrency = 2
"#,
    )
    .await;
    // The only account of a provider is never held back: reserving it would leave the
    // workers with nothing to lease at all.
    assert_eq!(h.pool.reserve_for_brain(Provider::Anthropic), None);
    assert!(acquire(&h.pool).await.is_ok(), "the solo account is usable");

    let two = harness(TWO_ACCOUNTS).await;
    let reserved = two
        .pool
        .reserve_for_brain(Provider::Anthropic)
        .expect("a brain account");
    let worker = acquire(&two.pool).await.expect("a worker lease");
    assert_ne!(
        worker.account, reserved,
        "a worker batch must not take the brain's account"
    );
    let second = acquire(&two.pool).await.expect("a second worker lease");
    assert_ne!(second.account, reserved);
}

/// The brain's permit comes out of its own slot, never out of the worker budget, and the
/// account it leases is the one `brain.account` names.
#[tokio::test]
async fn the_brain_lease_does_not_consume_a_worker_permit() {
    let h = harness(
        r#"
[brain]
reserve_brain_slot = false
[providers.anthropic]
models = { mid = "tier-mid" }
[[accounts]]
id = "main"
provider = "anthropic"
exec = "claude-main"
max_concurrency = 4
[[accounts]]
id = "alt"
provider = "anthropic"
exec = "claude-alt"
max_concurrency = 4
"#,
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let brain = h
        .pool
        .acquire_brain(Provider::Anthropic, Some(&id("main")), deadline)
        .await
        .expect("a brain lease");
    assert_eq!(brain.account, id("main"));

    // each account allows 4 concurrent workers and reserve_brain_slot is off in this
    // fixture, so four worker leases must still be available with the brain holding its own.
    let mut leases = Vec::new();
    for _ in 0..4 {
        leases.push(acquire(&h.pool).await.expect("a worker lease"));
    }
    assert_eq!(leases.len(), 4);
}

#[tokio::test]
async fn disable_and_clear_move_an_account_in_and_out_of_rotation() {
    let h = harness(TWO_ACCOUNTS).await;
    h.pool.set_enabled(&id("main"), false);
    assert_eq!(health_of(&h.pool, "main"), Health::Disabled);
    assert_eq!(acquire(&h.pool).await.expect("lease").account, id("alt"));

    h.pool
        .cooldown(&id("alt"), Duration::from_secs(600), "manual");
    assert_eq!(health_of(&h.pool, "alt"), Health::Cooling);
    h.pool.clear(&id("alt"));
    assert_eq!(health_of(&h.pool, "alt"), Health::Healthy);

    h.pool.set_enabled(&id("main"), true);
    assert_eq!(health_of(&h.pool, "main"), Health::Healthy);
}

#[tokio::test]
async fn expired_auth_takes_the_account_out_until_a_human_fixes_it() {
    let h = harness(TWO_ACCOUNTS).await;
    h.pool.report(
        &id("main"),
        Some(&Failure::AuthExpired {
            detail: "token expired".into(),
            detected_by: Detector::Pattern,
        }),
        None,
    );
    assert_eq!(health_of(&h.pool, "main"), Health::AuthBroken);
    assert_eq!(acquire(&h.pool).await.expect("lease").account, id("alt"));
}

#[tokio::test]
async fn reporting_accumulates_lifetime_counters() {
    let h = harness(TWO_ACCOUNTS).await;
    h.pool.report(
        &id("main"),
        None,
        Some(swamp::model::core::Cost {
            usd: 1.5,
            basis: swamp::model::core::CostBasis::Reported,
        }),
    );
    h.pool.report(&id("main"), None, None);
    let state = h
        .pool
        .snapshot()
        .into_iter()
        .find(|(_, a, _)| a.0 == "main")
        .map(|(_, _, s)| s)
        .expect("main");
    assert_eq!(state.lifetime_nodes, 2);
    assert!((state.lifetime_cost_usd - 1.5).abs() < f64::EPSILON);
}

/// Counting configured accounts is not enough: a peer that cannot take work is not a peer, and
/// holding back the only usable account leaves every worker with nothing to lease.
#[tokio::test]
async fn the_brain_never_reserves_the_last_usable_account() {
    let h = harness(TWO_ACCOUNTS).await;
    h.pool.set_enabled(&id("alt"), false);
    assert_eq!(
        h.pool.reserve_for_brain(Provider::Anthropic),
        None,
        "main is the only account left that can run anything"
    );
    let lease = acquire(&h.pool).await.expect("a worker lease");
    assert_eq!(lease.account, id("main"));
}
