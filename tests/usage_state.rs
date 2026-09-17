//! WP-B: per-account token accounting, the window roll, and what the shared state file keeps.

use camino::Utf8PathBuf;
use swamp::dispatch::account::{AccountState, QuotaSource, UsageLedger, WindowKey};
use swamp::dispatch::persist::{StateMap, load_state, merge_state, update_state};
use swamp::ids::{NodeId, RunId};
use swamp::journal::fold::RunView;
use swamp::journal::record::{JournalEvent, JournalLine};
use swamp::model::core::{
    AccountId, LimitReached, LimitScope, LimitStatus, LimitWindow, RateLimitSnapshot, Usage,
};
use time::OffsetDateTime;

fn tokens(billable: u64) -> Usage {
    Usage {
        input_tokens: billable,
        ..Usage::default()
    }
}

fn snapshot(resets_at: OffsetDateTime) -> RateLimitSnapshot {
    RateLimitSnapshot {
        status: LimitStatus::Allowed,
        windows: vec![LimitWindow {
            scope: LimitScope::SevenDay,
            utilization: 0.32,
            resets_at: Some(resets_at),
            window_minutes: Some(10_080),
            measured: true,
        }],
        ..Default::default()
    }
}

/// WP-B acceptance 4: the interface takes a running total, so three observations of one node
/// are worth 200 tokens and not 500.
#[test]
fn cumulative_observations_are_idempotent_and_commit_once() {
    let account = AccountId("codex-main".into());
    let node = NodeId::new();
    let mut ledger = UsageLedger::default();
    let mut state = AccountState::default();

    for total in [100, 200, 200] {
        ledger.observe(&account, node, tokens(total));
    }
    assert_eq!(ledger.inflight(&account).billable(), 200);
    assert_eq!(
        state.window_tokens.billable(),
        0,
        "nothing is committed yet"
    );

    let folded = ledger.commit(&account, node, tokens(200));
    state.credit_tokens(&folded);
    assert_eq!(state.window_tokens.billable(), 200);
    assert_eq!(state.lifetime_tokens.billable(), 200);
    assert_eq!(
        ledger.inflight(&account).billable(),
        0,
        "a committed node is not inflight"
    );

    // A late event for a node that already settled must not move either counter.
    ledger.observe(&account, node, tokens(50));
    state.credit_tokens(&ledger.commit(&account, node, tokens(50)));
    assert_eq!(ledger.inflight(&account).billable(), 0);
    assert_eq!(state.window_tokens.billable(), 200);
    assert_eq!(state.lifetime_tokens.billable(), 200);

    // Two nodes on one account add up while both run.
    let other = NodeId::new();
    ledger.observe(&account, other, tokens(70));
    assert_eq!(ledger.inflight(&account).billable(), 70);
}

/// claude reports usage per assistant message and re-reports the whole cached prefix every
/// time, so the running sum peaks well above what the node really spent. The result line is
/// the measurement: the ledger has to settle back down to it, not keep the peak.
#[test]
fn the_provider_total_settles_the_per_message_estimate_down() {
    let account = AccountId("claude-alt".into());
    let node = NodeId::new();
    let mut ledger = UsageLedger::default();
    let mut state = AccountState::default();
    let cached = |n: u64| Usage {
        cached_input_tokens: n,
        ..Usage::default()
    };

    let mut running = Usage::default();
    for message in [35_597u64, 35_597] {
        running.absorb(&cached(message));
        ledger.observe(&account, node, running);
    }
    assert_eq!(ledger.inflight(&account).cached_input_tokens, 71_194);

    ledger.settle(&account, node, cached(35_597));
    assert_eq!(
        ledger.inflight(&account).cached_input_tokens,
        35_597,
        "the result line supersedes the per-message sum"
    );

    state.credit_tokens(&ledger.commit(&account, node, cached(35_597)));
    assert_eq!(state.lifetime_tokens.cached_input_tokens, 35_597);
    assert_eq!(state.window_tokens.cached_input_tokens, 35_597);

    // A node that never reported a total keeps the estimate: crediting nothing is worse.
    let orphan = NodeId::new();
    ledger.observe(&account, orphan, tokens(120));
    assert_eq!(
        ledger.commit(&account, orphan, Usage::default()).billable(),
        120
    );
}

/// One account, one allowance: a bucket an earlier reading left behind must not keep answering
/// with a number the live snapshot contradicts.
#[test]
fn a_telemetry_reading_drops_the_buckets_it_supersedes() {
    let now = OffsetDateTime::now_utc();
    let resets = now + time::Duration::days(3);
    let seven = |utilization: f64| RateLimitSnapshot {
        windows: vec![LimitWindow {
            scope: LimitScope::SevenDay,
            utilization,
            resets_at: Some(resets),
            window_minutes: Some(10_080),
            measured: true,
        }],
        limit_id: Some("seven_day".into()),
        ..Default::default()
    };
    let mut state = AccountState::default();
    state.apply_quota(seven(0.80), QuotaSource::AppServer, now);
    assert_eq!(state.quota_buckets.len(), 1);

    state.apply_quota(seven(0.92), QuotaSource::Telemetry, now);
    assert!(
        state.quota_buckets.is_empty(),
        "a bucket quoting 80% next to a live 92% is two answers to one question"
    );
    assert_eq!(
        state
            .quota
            .as_ref()
            .and_then(|q| q.measured_utilization_at(now)),
        Some(0.92)
    );
}

/// WP-B acceptance 5: `resets_at` moving forward is what a rolled window looks like on the
/// wire, for both providers, and it is the only thing that zeroes `window_tokens`.
#[test]
fn a_window_rolls_only_when_its_reset_moves() {
    let now = OffsetDateTime::from_unix_timestamp(1_789_400_000).expect("now");
    let first = now + time::Duration::days(7);
    let mut state = AccountState::default();

    assert!(state.roll_window(WindowKey::of(&snapshot(first)), now));
    state.credit_tokens(&tokens(1_200));
    assert_eq!(state.window_started_at, Some(now));
    assert_eq!(state.window_tokens.billable(), 1_200);

    // The same window again: the counter keeps counting.
    assert!(!state.roll_window(WindowKey::of(&snapshot(first)), now));
    assert_eq!(state.window_tokens.billable(), 1_200);

    let later = now + time::Duration::hours(1);
    let rolled = state.roll_window(
        WindowKey::of(&snapshot(first + time::Duration::days(7))),
        later,
    );
    assert!(rolled, "a later reset is a new window");
    assert_eq!(state.window_tokens, Usage::default(), "the window zeroed");
    assert_eq!(state.window_started_at, Some(later));
    assert_eq!(
        state.lifetime_tokens.billable(),
        1_200,
        "lifetime never rolls"
    );

    // A snapshot with no reset time keys nothing and therefore rolls nothing.
    assert!(!state.roll_window(WindowKey::of(&RateLimitSnapshot::default()), later));
}

/// The roll and the commit both reach `swamp trace` and `/status` through the journal, so
/// `swamp replay --reparse` re-derives them like everything else.
#[test]
fn account_usage_folds_into_the_run_view() {
    let account = AccountId("claude-main".into());
    let at = OffsetDateTime::from_unix_timestamp(1_789_400_000).expect("at");
    let key = WindowKey {
        scope: LimitScope::SevenDay,
        resets_at: at + time::Duration::days(7),
        window_minutes: Some(10_080),
        estimated: false,
    };
    let mut view = RunView::default();
    view.apply(&JournalLine {
        seq: 1,
        at,
        run: RunId::new(),
        node: None,
        event: JournalEvent::AccountUsage {
            account: account.clone(),
            window: tokens(412_000),
            lifetime: tokens(8_100_000),
            window_key: Some(key.clone()),
            rolled: true,
            source: Some(QuotaSource::Telemetry),
        },
    });
    let folded = &view.accounts[&account];
    assert_eq!(folded.window_tokens.billable(), 412_000);
    assert_eq!(folded.lifetime_tokens.billable(), 8_100_000);
    assert_eq!(folded.window_key.as_ref(), Some(&key));
    assert_eq!(folded.window_started_at, Some(at));
    assert_eq!(folded.quota_source, Some(QuotaSource::Telemetry));

    let line = serde_json::to_string(&JournalEvent::AccountUsage {
        account,
        window: tokens(1),
        lifetime: tokens(1),
        window_key: None,
        rolled: false,
        source: None,
    })
    .expect("json");
    assert!(line.contains(r#""ev":"account_usage""#), "{line}");
}

/// WP-B acceptance 7: the state file is machine-wide. Lifetime counters only ever grow, and
/// a window another process has already rolled must not come back from the file.
#[test]
fn a_merge_keeps_the_larger_lifetime_and_never_resurrects_a_rolled_window() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.path().join("accounts.json")).expect("utf8");
    let account = AccountId("codex-main".into());
    let now = OffsetDateTime::now_utc();
    let old_key = WindowKey {
        scope: LimitScope::SevenDay,
        resets_at: now,
        window_minutes: Some(10_080),
        estimated: false,
    };
    let new_key = WindowKey {
        scope: LimitScope::SevenDay,
        resets_at: now + time::Duration::days(7),
        window_minutes: Some(10_080),
        estimated: false,
    };

    let mut theirs = StateMap::new();
    theirs.insert(
        account.clone(),
        AccountState {
            lifetime_nodes: 4,
            lifetime_tokens: Usage {
                input_tokens: 900,
                output_tokens: 10,
                ..Usage::default()
            },
            window_tokens: tokens(500),
            window_key: Some(old_key),
            updated_at: Some(now - std::time::Duration::from_secs(60)),
            ..Default::default()
        },
    );
    merge_state(&path, &theirs).expect("their write");

    let mut mine = StateMap::new();
    mine.insert(
        account.clone(),
        AccountState {
            lifetime_nodes: 2,
            lifetime_tokens: Usage {
                input_tokens: 100,
                output_tokens: 90,
                ..Usage::default()
            },
            window_tokens: tokens(7),
            window_key: Some(new_key.clone()),
            updated_at: Some(now),
            ..Default::default()
        },
    );
    let merged = merge_state(&path, &mine).expect("my write");
    let got = &merged[&account];
    assert_eq!(
        got.lifetime_nodes, 4,
        "lifetime_nodes already worked this way"
    );
    assert_eq!(
        got.lifetime_tokens.input_tokens, 900,
        "per field, the larger"
    );
    assert_eq!(got.lifetime_tokens.output_tokens, 90);
    assert_eq!(
        got.window_tokens.billable(),
        7,
        "the rolled window starts again at its own count"
    );
    assert_eq!(got.window_key.as_ref(), Some(&new_key));

    // Same window on both sides: the higher count wins, so neither process loses its spend.
    let mut same = StateMap::new();
    same.insert(
        account.clone(),
        AccountState {
            window_tokens: tokens(3),
            window_key: Some(new_key),
            updated_at: Some(now + std::time::Duration::from_secs(1)),
            ..Default::default()
        },
    );
    let merged = merge_state(&path, &same).expect("third write");
    assert_eq!(merged[&account].window_tokens.billable(), 7);
    assert_eq!(load_state(&path).expect("reload").len(), 1);
}

/// An old `accounts.json`, written before any of these fields existed, still loads.
#[test]
fn a_state_file_from_before_token_accounting_still_loads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.path().join("accounts.json")).expect("utf8");
    std::fs::write(
        &path,
        r#"{"main":{"inflight":0,"health":"healthy","cooldown_until":null,
             "consecutive_infra_failures":0,"quota":null,"last_used":null,
             "lifetime_nodes":3,"lifetime_cost_usd":1.5}}"#,
    )
    .expect("write");
    let state = load_state(&path).expect("load");
    let got = &state[&AccountId("main".into())];
    assert_eq!(got.lifetime_nodes, 3);
    assert_eq!(got.lifetime_tokens, Usage::default());
    assert!(got.window_key.is_none());
    assert!(got.quota_source.is_none());
    assert!(got.quota_buckets.is_empty());

    // USAGE 2.1: every field defaults, so the smallest hand-written entry is a valid one.
    std::fs::write(&path, r#"{"main":{}}"#).expect("write");
    let state = load_state(&path).expect("a hand-written entry loads");
    let got = &state[&AccountId("main".into())];
    assert_eq!(got.lifetime_nodes, 0);
    assert!(got.cooldown_until.is_none());
    assert!(got.last_used.is_none());
}

/// `AccountPool::new` tolerates an unreadable state file and starts clean. If the write path
/// tolerated it too, the first persist would replace a machine-wide file - every other repo's
/// account included - with this repo's accounts alone.
#[test]
fn a_write_never_replaces_a_state_file_it_could_not_parse() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.path().join("accounts.json")).expect("utf8");
    std::fs::write(&path, "{ not json at all").expect("write");

    let mut mine = StateMap::new();
    mine.insert(AccountId("main".into()), AccountState::default());
    assert!(merge_state(&path, &mine).is_err(), "a parse error is fatal");
    assert!(
        update_state(&path, &[AccountId("main".into())], |_, _| {}).is_err(),
        "a parse error is fatal"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("the file survives"),
        "{ not json at all"
    );
}

/// WP-B acceptance 8: quota telemetry is percentages and counters. No credential, no email,
/// and `account/read`, which would return the account's email, is never called.
#[test]
fn nothing_in_this_package_reads_or_records_an_identity() {
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let email = regex::Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}").expect("re");

    for name in [
        "docs/ref/codex-rollout-sample.jsonl",
        "docs/ref/codex-ratelimits-sample.json",
        "docs/ref/claude-stream-sample.jsonl",
    ] {
        let text = std::fs::read_to_string(root.join(name)).expect(name);
        assert!(!email.is_match(&text), "{name} carries an email address");
        for word in [
            "auth.json",
            "access_token",
            "refresh_token",
            "api_key",
            "sk-",
        ] {
            assert!(!text.contains(word), "{name} carries {word}");
        }
    }

    for name in [
        "src/worker/codex_quota.rs",
        "src/dispatch/retry.rs",
        "src/dispatch/account.rs",
        "src/journal/record.rs",
    ] {
        let text = std::fs::read_to_string(root.join(name)).expect(name);
        assert!(
            !text.contains("\"account/read\""),
            "{name} calls account/read, which returns the account email"
        );
        assert!(!text.contains("auth.json"), "{name} reads auth.json");
    }
}

/// One snapshot carries provenance, a merge and a possible roll: the pool applies all three
/// together, so a percentage can never be stored without the moment it was observed.
#[test]
fn applying_a_snapshot_records_its_provenance_and_rolls_once() {
    let now = OffsetDateTime::from_unix_timestamp(1_789_400_000).expect("now");
    let resets = now + time::Duration::days(7);
    let mut state = AccountState::default();

    let mut first = snapshot(resets);
    first.limit_id = Some("codex".into());
    assert!(state.apply_quota(first, QuotaSource::Rollout, now));
    state.credit_tokens(&tokens(900));
    assert_eq!(state.quota_observed_at, Some(now));
    assert_eq!(state.quota_source, Some(QuotaSource::Rollout));
    assert!(state.quota_buckets.contains_key("codex"));

    // The same window, seen again a minute later: provenance moves, the counter does not.
    let later = now + time::Duration::minutes(1);
    let mut again = snapshot(resets);
    again.limit_id = Some("codex".into());
    assert!(!state.apply_quota(again, QuotaSource::AppServer, later));
    assert_eq!(state.window_tokens.billable(), 900);
    assert_eq!(state.quota_observed_at, Some(later));
    assert_eq!(state.quota_source, Some(QuotaSource::AppServer));
}

/// USAGE 2.2 and 4.3: `ordinary_usage_allowed` is the authoritative gate, and a codex rollout
/// carries a percentage and nothing else. A rollout tail must therefore never lift a refusal
/// the app-server read, or Swamp keeps leasing an account the provider has already refused.
#[test]
fn a_rollout_reading_cannot_lift_the_provider_gate() {
    let now = OffsetDateTime::from_unix_timestamp(1_789_400_000).expect("now");
    let resets = now + time::Duration::days(7);
    let mut state = AccountState::default();

    let mut refused = snapshot(resets);
    refused.ordinary_usage_allowed = Some(false);
    refused.reached = Some(LimitReached::CreditsDepleted);
    state.apply_quota(refused, QuotaSource::AppServer, now);
    assert!(swamp::dispatch::pool::hard_gated(&state));

    // `parse_rollout` builds `Bucket { limits, ..default() }`: both gate fields are unset.
    let later = now + time::Duration::minutes(1);
    state.apply_quota(snapshot(resets), QuotaSource::Rollout, later);
    let q = state.quota.as_ref().expect("a snapshot");
    assert_eq!(q.ordinary_usage_allowed, Some(false));
    assert_eq!(q.reached, Some(LimitReached::CreditsDepleted));
    assert!(
        swamp::dispatch::pool::hard_gated(&state),
        "a rollout tail lifted the provider's hard gate"
    );
    assert_eq!(
        swamp::dispatch::pool::health_from_quota(&state, 0.9),
        swamp::dispatch::account::Health::AuthBroken
    );

    // The app-server can state the gate, so it is what clears it.
    let mut allowed = snapshot(resets);
    allowed.ordinary_usage_allowed = Some(true);
    state.apply_quota(allowed, QuotaSource::AppServer, later);
    assert!(!swamp::dispatch::pool::hard_gated(&state));
}

/// codex reports its reset as a countdown, so the absolute instant Swamp derives moves
/// forward with every read of the same unchanged window. Re-keying on that would zero
/// `window_tokens` after every single node.
#[test]
fn re_reading_one_codex_window_is_not_a_roll() {
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(root.join("docs/ref/codex-ratelimits-sample.json"))
        .expect("sample");
    let value: serde_json::Value = serde_json::from_str(&text).expect("sample parses");
    let now = OffsetDateTime::from_unix_timestamp(1_789_400_000).expect("now");
    let later = now + time::Duration::minutes(5);

    let first = swamp::worker::codex_quota::parse_rate_limits(&value, now);
    let second = swamp::worker::codex_quota::parse_rate_limits(&value, later);
    let (a, b) = (
        first.select(None, None).expect("a bucket"),
        second.select(None, None).expect("a bucket"),
    );
    assert_ne!(
        a.windows[0].resets_at, b.windows[0].resets_at,
        "the derived reset drifts with the age of the reading"
    );

    let mut state = AccountState::default();
    assert!(state.apply_quota(a, QuotaSource::AppServer, now));
    state.credit_tokens(&tokens(900_000));
    assert!(
        !state.apply_quota(b, QuotaSource::AppServer, later),
        "the same provider window is not a new window"
    );
    assert_eq!(state.window_tokens.billable(), 900_000);
}

/// USAGE 2.1 and 2.3: every bucket the provider reported is kept for display, not just the
/// one this account bills against.
#[test]
fn every_reported_bucket_is_kept_for_display() {
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(root.join("docs/ref/codex-ratelimits-sample.json"))
        .expect("sample");
    let value: serde_json::Value = serde_json::from_str(&text).expect("sample parses");
    let now = OffsetDateTime::from_unix_timestamp(1_789_400_000).expect("now");
    let read = swamp::worker::codex_quota::parse_rate_limits(&value, now);
    assert!(read.buckets.len() > 1, "the sample carries several buckets");

    let mut state = AccountState::default();
    state.apply_buckets(&read.buckets);
    state.apply_quota(
        read.select(None, None).expect("a bucket"),
        QuotaSource::AppServer,
        now,
    );
    let kept: Vec<&str> = state.quota_buckets.keys().map(String::as_str).collect();
    let reported: Vec<&str> = read.buckets.keys().map(String::as_str).collect();
    assert_eq!(kept, reported);
    assert_eq!(
        state.quota.as_ref().and_then(|q| q.limit_id.clone()),
        Some("codex".to_owned()),
        "the selected bucket is still the one dispatch scores"
    );
}

/// USAGE 2.1: an account no snapshot ever reaches still rolls its window on wall time, or
/// `window_tokens` is a lifetime counter and the `share` term compares the wrong numbers.
#[test]
fn an_account_with_no_quota_source_rolls_on_wall_time() {
    let window = std::time::Duration::from_secs(7 * 24 * 3600);
    let now = OffsetDateTime::from_unix_timestamp(1_789_400_000).expect("now");
    let mut state = AccountState::default();

    assert!(state.roll_elapsed_window(window, now), "the first window");
    state.credit_tokens(&tokens(1_000));
    let key = state.window_key.clone().expect("a key");
    assert!(key.resets_at > now);

    // Still inside it an hour later: the counter keeps counting.
    let hour = now + time::Duration::hours(1);
    assert!(!state.roll_elapsed_window(window, hour));
    assert_eq!(state.window_tokens.billable(), 1_000);
    assert_eq!(state.window_key, Some(key.clone()));

    // Past the boundary: a new window, zeroed, with lifetime untouched.
    let after = key.resets_at + time::Duration::minutes(1);
    assert!(state.roll_elapsed_window(window, after));
    assert_eq!(state.window_tokens, Usage::default());
    assert_eq!(state.lifetime_tokens.billable(), 1_000);

    // A live measured window owns the roll: wall time must not touch it.
    let mut measured = AccountState::default();
    measured.apply_quota(
        snapshot(now + time::Duration::days(3)),
        QuotaSource::Telemetry,
        now,
    );
    measured.credit_tokens(&tokens(500));
    assert!(!measured.roll_elapsed_window(window, now));
    assert_eq!(measured.window_tokens.billable(), 500);
}

/// USAGE 2.1: once the stored window has expired the wall-time roll is unconditional. The
/// half-window drift tolerance is for two readings of a live window, and letting it suppress
/// the roll left two windows of spend in a counter labelled "this window".
#[test]
fn an_expired_window_rolls_even_when_the_grid_boundary_is_near_it() {
    let window = std::time::Duration::from_secs(7 * 24 * 3600);
    let now = OffsetDateTime::from_unix_timestamp(1_789_400_000).expect("now");
    let expired = now - time::Duration::hours(1);
    let mut state = AccountState::default();
    state.apply_quota(snapshot(expired), QuotaSource::Telemetry, expired);
    state.credit_tokens(&tokens(1_000));

    // The epoch grid boundary lands 2.4 days from the stale reset, inside the 3.5 day slack.
    assert!(state.roll_elapsed_window(window, now), "the window expired");
    assert_eq!(state.window_tokens, Usage::default());
    assert_eq!(state.window_started_at, Some(now));
    assert_eq!(state.lifetime_tokens.billable(), 1_000);
    let key = state.window_key.clone().expect("a fresh key");
    assert!(key.resets_at > now, "the new window is still ahead of us");
}

/// Cost accumulates per process from what the file held at startup, so the merge has to take
/// the larger of the two totals like every other lifetime counter.
#[test]
fn a_concurrent_run_cannot_erase_another_processes_cost() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.path().join("accounts.json")).expect("utf8");
    let account = AccountId("claude-main".into());
    let now = OffsetDateTime::now_utc();

    let mut theirs = StateMap::new();
    theirs.insert(
        account.clone(),
        AccountState {
            lifetime_cost_usd: 12.40,
            updated_at: Some(now - time::Duration::minutes(5)),
            ..AccountState::default()
        },
    );
    merge_state(&path, &theirs).expect("their write");

    let mut ours = StateMap::new();
    ours.insert(
        account.clone(),
        AccountState {
            lifetime_cost_usd: 0.30,
            updated_at: Some(now),
            ..AccountState::default()
        },
    );
    let merged = merge_state(&path, &ours).expect("our write");
    assert_eq!(merged[&account].lifetime_cost_usd, 12.40);
}

/// WP-B acceptance 7 again, from the other side: a newer entry in the file wins the
/// last-writer-wins fields, but never carries away the counters this process kept.
#[test]
fn a_newer_entry_in_the_file_still_reconciles_our_counters() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.path().join("accounts.json")).expect("utf8");
    let account = AccountId("codex-main".into());
    let now = OffsetDateTime::now_utc();
    let key = WindowKey {
        scope: LimitScope::SevenDay,
        resets_at: now + time::Duration::days(3),
        window_minutes: Some(10_080),
        estimated: false,
    };

    let mut theirs = StateMap::new();
    theirs.insert(
        account.clone(),
        AccountState {
            lifetime_nodes: 1,
            lifetime_tokens: tokens(1_950_000),
            window_tokens: tokens(40),
            window_key: Some(key.clone()),
            cooldown_until: Some(now + std::time::Duration::from_secs(600)),
            // Written while we were blocked on the lock.
            updated_at: Some(now + std::time::Duration::from_secs(1)),
            ..Default::default()
        },
    );
    merge_state(&path, &theirs).expect("their write");

    let mut mine = StateMap::new();
    mine.insert(
        account.clone(),
        AccountState {
            lifetime_nodes: 9,
            lifetime_tokens: tokens(2_000_000),
            window_tokens: tokens(60),
            window_key: Some(key),
            updated_at: Some(now),
            ..Default::default()
        },
    );
    let merged = merge_state(&path, &mine).expect("my write");
    let got = &merged[&account];
    assert_eq!(
        got.lifetime_tokens.input_tokens, 2_000_000,
        "per field, the larger"
    );
    assert_eq!(got.lifetime_nodes, 9);
    assert_eq!(got.window_tokens.billable(), 60);
    assert_eq!(
        got.cooldown_until,
        Some(now + std::time::Duration::from_secs(600)),
        "the newer entry still wins what only it can know"
    );
}

/// USAGE 2.1: the wall-time grid key is a stand-in for a window nobody has measured. The
/// first real reading adopts it, instead of zeroing the tokens the node just spent.
#[test]
fn the_first_measured_reading_adopts_the_estimated_window() {
    let now = OffsetDateTime::now_utc();
    let mut state = AccountState::default();
    assert!(
        state.roll_elapsed_window(std::time::Duration::from_secs(5 * 3600), now),
        "an account with no key starts one"
    );
    state.credit_tokens(&tokens(50_000));

    let rolled = state.apply_quota(
        snapshot(now + time::Duration::days(7)),
        QuotaSource::Rollout,
        now,
    );
    assert!(!rolled, "adopting a measured window is not a roll");
    assert_eq!(
        state.window_tokens.billable(),
        50_000,
        "the tokens this window already counted survive"
    );
    assert_eq!(
        state.window_key.as_ref().map(|k| k.estimated),
        Some(false),
        "and the key is the measured one from here on"
    );

    // A measured window really rolling still zeroes the counter.
    let rolled = state.apply_quota(
        snapshot(now + time::Duration::days(14)),
        QuotaSource::Rollout,
        now,
    );
    assert!(rolled, "a measured window moving on is a roll");
    assert_eq!(state.window_tokens.billable(), 0);
}

/// `swamp accounts clear` and `AccountPool::clear` share this mutation: clearing only the
/// cooldown left the provider gates, which carry no timer, in place forever.
#[test]
fn clearing_an_account_lifts_the_provider_gates_and_the_stale_windows() {
    let now = OffsetDateTime::now_utc();
    let mut gated = snapshot(now - time::Duration::hours(1));
    gated.limit_id = Some("codex".into());
    gated.reached = Some(swamp::model::core::LimitReached::CreditsDepleted);
    gated.ordinary_usage_allowed = Some(false);

    let mut entry = AccountState {
        health: swamp::dispatch::account::Health::AuthBroken,
        cooldown_until: Some(now + time::Duration::minutes(5)),
        consecutive_infra_failures: 3,
        ..AccountState::default()
    };
    entry.apply_quota(gated, QuotaSource::AppServer, now);
    assert!(swamp::dispatch::pool::hard_gated(&entry));

    entry.clear_gates();
    assert!(!swamp::dispatch::pool::hard_gated(&entry));
    assert!(entry.cooldown_until.is_none());
    assert_eq!(entry.consecutive_infra_failures, 0);
    let quota = entry.quota.as_ref().expect("the reading is kept");
    assert!(
        quota.windows.is_empty(),
        "a rolled window still gates score"
    );
    assert!(
        entry
            .quota_buckets
            .values()
            .all(|q| q.reached.is_none() && q.windows.is_empty()),
        "every displayed bucket is cleared too"
    );
    entry.health = swamp::dispatch::pool::health_from_quota(&entry, 0.9);
    assert_eq!(entry.health, swamp::dispatch::account::Health::Healthy);
}

/// `swamp usage --probe` writes a reading straight onto the file; without this the stored
/// health word contradicts the snapshot printed under it.
#[test]
fn a_probed_reading_re_derives_health_the_way_the_pool_does() {
    let now = OffsetDateTime::now_utc();
    let mut entry = AccountState::default();

    let mut refused = snapshot(now + time::Duration::hours(1));
    refused.ordinary_usage_allowed = Some(false);
    let was_gated = swamp::dispatch::pool::hard_gated(&entry);
    entry.apply_quota(refused, QuotaSource::AppServer, now);
    swamp::dispatch::pool::health_after_quota(&mut entry, was_gated, 0.9);
    assert_eq!(entry.health, swamp::dispatch::account::Health::AuthBroken);

    let mut allowed = snapshot(now + time::Duration::hours(1));
    allowed.ordinary_usage_allowed = Some(true);
    let was_gated = swamp::dispatch::pool::hard_gated(&entry);
    entry.apply_quota(allowed, QuotaSource::AppServer, now);
    swamp::dispatch::pool::health_after_quota(&mut entry, was_gated, 0.9);
    assert_eq!(entry.health, swamp::dispatch::account::Health::Healthy);
}

/// USAGE 2.3: claude's `limit_id` is the event's `rateLimitType`, a label and not a bucket -
/// a rejection names whichever limit was hit. Keying the per-scope merge on it let one such
/// event replace the stored snapshot wholesale, erasing a 99% window and routing the pool
/// straight back into a near-exhausted account.
#[test]
fn a_claude_event_naming_another_limit_still_merges_the_stored_windows() {
    let now = OffsetDateTime::from_unix_timestamp(1_789_400_000).expect("now");
    let mut state = AccountState::default();
    state.apply_quota(
        RateLimitSnapshot {
            status: LimitStatus::Allowed,
            windows: vec![
                LimitWindow {
                    scope: LimitScope::FiveHour,
                    utilization: 0.06,
                    resets_at: Some(now + time::Duration::hours(2)),
                    window_minutes: Some(300),
                    measured: true,
                },
                LimitWindow {
                    scope: LimitScope::SevenDay,
                    utilization: 0.99,
                    resets_at: Some(now + time::Duration::days(3)),
                    window_minutes: Some(10_080),
                    measured: true,
                },
            ],
            limit_id: Some("five_hour".to_owned()),
            ..Default::default()
        },
        QuotaSource::Telemetry,
        now,
    );

    // The rejection names the seven-day limit and carries no `unifiedWindows` at all.
    state.apply_quota(
        RateLimitSnapshot {
            status: LimitStatus::Rejected,
            windows: Vec::new(),
            resets_at: Some(now + time::Duration::days(3)),
            limit_id: Some("seven_day".to_owned()),
            reached: Some(LimitReached::RateLimit),
            ..Default::default()
        },
        QuotaSource::Telemetry,
        now + time::Duration::minutes(1),
    );

    let quota = state.quota.as_ref().expect("a snapshot");
    assert_eq!(quota.windows.len(), 2, "both windows survive the rejection");
    assert_eq!(
        quota.measured_utilization(),
        Some(0.99),
        "the stored seven-day reading is what still gates dispatch"
    );
    assert_eq!(quota.reached, Some(LimitReached::RateLimit));
    assert!(
        state.quota_buckets.is_empty(),
        "a rejection label is not a bucket, and `swamp usage --json` prints what it holds: {:?}",
        state.quota_buckets.keys().collect::<Vec<_>>()
    );
}

/// The other half of the same rule: for codex two `limit_id`s really are two allowances, so a
/// reading of one bucket must never inherit the other's windows.
#[test]
fn a_codex_reading_of_another_bucket_does_not_inherit_its_windows() {
    let now = OffsetDateTime::from_unix_timestamp(1_789_400_000).expect("now");
    let mut state = AccountState::default();
    let mut first = snapshot(now + time::Duration::days(3));
    first.limit_id = Some("codex".to_owned());
    state.apply_quota(first, QuotaSource::AppServer, now);

    let other = RateLimitSnapshot {
        status: LimitStatus::Allowed,
        windows: Vec::new(),
        limit_id: Some("codex_bengalfox".to_owned()),
        ..Default::default()
    };
    state.apply_quota(
        other,
        QuotaSource::AppServer,
        now + time::Duration::minutes(1),
    );
    assert!(
        state.quota.as_ref().is_some_and(|q| q.windows.is_empty()),
        "a second bucket starts from its own reading"
    );
}
