//! WP-B: per-account token accounting, the window roll, and what the shared state file keeps.

use camino::Utf8PathBuf;
use swamp::dispatch::account::{AccountState, QuotaSource, UsageLedger, WindowKey};
use swamp::dispatch::persist::{StateMap, load_state, merge_state};
use swamp::ids::{NodeId, RunId};
use swamp::journal::fold::RunView;
use swamp::journal::record::{JournalEvent, JournalLine};
use swamp::model::core::{
    AccountId, LimitScope, LimitStatus, LimitWindow, RateLimitSnapshot, Usage,
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
    };
    let new_key = WindowKey {
        scope: LimitScope::SevenDay,
        resets_at: now + time::Duration::days(7),
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
