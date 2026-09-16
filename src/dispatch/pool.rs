use crate::config::Config;
use crate::config::resolve::expand_env;
use crate::dispatch::account::{Account, AccountState, Health, QuotaSource, UsageLedger};
use crate::dispatch::cooldown::cooldown_for;
use crate::dispatch::persist::{self, StateMap};
use crate::dispatch::policy::{Scoring, SelectionPolicy, rank, score};
use crate::ids::NodeId;
use crate::journal::{JournalEvent, JournalHandle};
use crate::model::core::{
    AccountId, Cost, LimitReached, LimitScope, Provider, RateLimitSnapshot, Usage,
};
use crate::model::failure::Failure;
use camino::Utf8PathBuf;
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use time::OffsetDateTime;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Upper bound on how long a blocked acquirer sleeps when nothing tells it when to look again.
const IDLE_RECHECK: Duration = Duration::from_secs(5);

pub struct AccountPool {
    pub cfg: Arc<Config>,
    pub policy: SelectionPolicy,
    pub state_path: Utf8PathBuf,
    pub journal: JournalHandle,
    accounts: BTreeMap<AccountId, Account>,
    state: Mutex<StateMap>,
    /// Tokens of nodes that are still running: `share` has to be live, or a whole batch
    /// scores against the counters the pool had before the batch started.
    ledger: Mutex<UsageLedger>,
    scoring: Scoring,
    /// The brain's own slot, which is not a worker slot.
    brain: Arc<Semaphore>,
    /// The account the brain's live lease occupies, if any.
    brain_held: Mutex<Option<AccountId>>,
    returned: Notify,
    reserved: Mutex<Option<AccountId>>,
    wakeups: AtomicU64,
    /// Rotates the candidate order, so accounts tied on every counter still alternate.
    cursor: AtomicU64,
    /// `swamp run` has no live view of its own; chat renders the same notice from the journal.
    stderr_notices: AtomicBool,
    /// Coalesces the cross-process state file writes: one flush in flight at a time.
    persisting: Mutex<PersistGate>,
}

#[derive(Default)]
struct PersistGate {
    in_flight: bool,
    dirty: bool,
}

/// Drop decrements inflight and notifies waiters, so a panicking node cannot leak a slot.
pub struct Lease {
    pub account: AccountId,
    pub exec: String,
    pub env: BTreeMap<String, String>,
    _permit: Option<OwnedSemaphorePermit>,
    brain: bool,
    pool: Arc<AccountPool>,
}

impl Lease {
    /// The brain holds its lease for the whole run and reports its own spend through it.
    pub fn pool(&self) -> &Arc<AccountPool> {
        &self.pool
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if self.brain {
            self.pool.release_reservation();
            *self.pool.brain_held.lock() = None;
        }
        {
            let mut state = self.pool.state.lock();
            let entry = state.entry(self.account.clone()).or_default();
            entry.inflight = entry.inflight.saturating_sub(1);
        }
        self.pool.returned.notify_waiters();
    }
}

#[derive(Debug)]
pub enum NoCapacity {
    /// Every candidate is cooling, past its measured stop threshold, or hard-gated.
    /// `retry_at` is the earliest moment any of them could come back.
    AllExhausted {
        retry_at: OffsetDateTime,
        why: String,
    },
    Saturated,
    Exhausted {
        reason: String,
    },
    /// The caller's token fired while we waited.
    Cancelled,
}

/// What the pool can do for this provider right now.
enum Capacity {
    Ready,
    /// Every candidate is at its own concurrency ceiling; a returning lease unblocks us.
    Busy {
        next_reset: Option<OffsetDateTime>,
    },
    AllExhausted {
        retry_at: OffsetDateTime,
        why: String,
    },
    Exhausted {
        reason: String,
    },
}

/// Why one candidate cannot take work right now.
enum Block {
    /// At its own ceiling: a returning lease is what unblocks it.
    Busy,
    /// Comes back on its own, at `at`.
    Wait { at: OffsetDateTime, why: String },
    /// No timer: a human has to act.
    Hard { why: String },
}

impl AccountPool {
    pub fn new(
        cfg: Arc<Config>,
        state_path: Utf8PathBuf,
        journal: JournalHandle,
    ) -> anyhow::Result<Arc<Self>> {
        let accounts: BTreeMap<AccountId, Account> = cfg
            .accounts
            .iter()
            .map(|a| {
                (
                    a.id.clone(),
                    Account {
                        id: a.id.clone(),
                        provider: a.provider,
                        exec: a.exec.clone(),
                        env: expand_env(&a.env),
                        weight: a.weight.unwrap_or(1),
                        // Unset means unlimited: a subscription's real ceiling is the one
                        // the user writes down, not a number Swamp invented.
                        max_concurrency: a.max_concurrency.filter(|c| *c > 0),
                    },
                )
            })
            .collect();

        // A machine-wide file: entries for accounts this repo does not configure are kept.
        let mut state = match persist::load_state(&state_path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("account state at {state_path} is unreadable, starting clean: {e}");
                StateMap::new()
            }
        };
        for (id, entry) in state.iter_mut() {
            entry.inflight = 0;
            if entry.health == Health::Cooling
                && !entry
                    .cooldown_until
                    .is_some_and(|t| t > OffsetDateTime::now_utc())
            {
                entry.health = Health::Healthy;
                entry.cooldown_until = None;
            }
            tracing::debug!("loaded account state for {}", id.0);
        }
        for id in accounts.keys() {
            state.entry(id.clone()).or_default();
        }

        Ok(Arc::new(Self {
            policy: cfg.dispatch.policy.unwrap_or_default(),
            scoring: Scoring::from_config(&cfg),
            cfg,
            state_path,
            journal,
            accounts,
            state: Mutex::new(state),
            ledger: Mutex::new(UsageLedger::default()),
            brain: Arc::new(Semaphore::new(1)),
            brain_held: Mutex::new(None),
            returned: Notify::new(),
            reserved: Mutex::new(None),
            wakeups: AtomicU64::new(0),
            cursor: AtomicU64::new(0),
            stderr_notices: AtomicBool::new(false),
            persisting: Mutex::new(PersistGate::default()),
        }))
    }

    /// `swamp run` prints the blocked notice itself: it has no live view to render one in.
    pub fn notices_to_stderr(&self) {
        self.stderr_notices.store(true, Ordering::Relaxed);
    }

    /// Never busy-spins: waits on a Notify or sleeps until the earliest journaled reset.
    pub async fn acquire(
        self: &Arc<Self>,
        provider: Provider,
        exclude: &HashSet<AccountId>,
        deadline: Instant,
    ) -> Result<Lease, NoCapacity> {
        self.acquire_node(provider, exclude, deadline, None, None)
            .await
    }

    /// A blocked node waits for a window to roll instead of failing: an overnight run wants
    /// the lease it will get in 40 minutes, not an error now. `node` is what the one
    /// `NodeBlocked` line is attributed to, `cancel` is what makes `esc esc` prompt.
    pub async fn acquire_node(
        self: &Arc<Self>,
        provider: Provider,
        exclude: &HashSet<AccountId>,
        deadline: Instant,
        node: Option<NodeId>,
        cancel: Option<&CancellationToken>,
    ) -> Result<Lease, NoCapacity> {
        let mut announced = false;
        loop {
            if cancel.is_some_and(|c| c.is_cancelled()) {
                return Err(NoCapacity::Cancelled);
            }
            self.wakeups.fetch_add(1, Ordering::Relaxed);
            // Registered BEFORE the capacity check: a lease returned in between must not be
            // lost to a waiter that had not subscribed yet.
            let notified = self.returned.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let wake_at = match self.capacity(provider, exclude) {
                Capacity::Ready => match self.take(provider, exclude) {
                    Some(lease) => return Ok(lease),
                    // Someone else won the race.
                    None => {
                        self.returned.notify_waiters();
                        None
                    }
                },
                Capacity::Busy { next_reset } => next_reset,
                Capacity::AllExhausted { retry_at, why } => {
                    // Once per blocked node, never once per loop iteration.
                    if !announced {
                        announced = true;
                        tracing::warn!(
                            "every {provider} account is at its limit until {}: {why}",
                            crate::ui::fmt::clock_hm(retry_at)
                        );
                        if self.stderr_notices.load(Ordering::Relaxed) {
                            eprintln!(
                                "swamp: {}",
                                crate::ui::fmt::blocked_notice(
                                    provider,
                                    retry_at,
                                    OffsetDateTime::now_utc(),
                                    "ctrl-c to cancel",
                                )
                            );
                        }
                        crate::dispatch::emit(
                            &self.journal,
                            node,
                            JournalEvent::NodeBlocked {
                                until: retry_at,
                                why,
                            },
                        );
                    }
                    Some(retry_at)
                }
                Capacity::Exhausted { reason } => return Err(NoCapacity::Exhausted { reason }),
            };

            if Instant::now() >= deadline {
                return Err(NoCapacity::Saturated);
            }
            // A concurrency block has no reset time; sleeping to the node deadline would park
            // this task for the whole 25 minutes if a wakeup were ever missed.
            let until = wake_at
                .map_or(Instant::now() + IDLE_RECHECK, instant_of)
                .min(deadline);
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep_until(until) => {}
                _ = cancelled(cancel) => {}
            }
        }
    }

    /// Why every candidate for `provider` is unusable right now, if they all are. Cheap and
    /// non-blocking, so a caller with another provider to try decides before anyone waits.
    pub fn all_exhausted(
        &self,
        provider: Provider,
        exclude: &HashSet<AccountId>,
    ) -> Option<NoCapacity> {
        match self.capacity(provider, exclude) {
            Capacity::AllExhausted { retry_at, why } => {
                Some(NoCapacity::AllExhausted { retry_at, why })
            }
            // No timer will rescue this provider, so failover is the only thing that can.
            Capacity::Exhausted { reason } => Some(NoCapacity::Exhausted { reason }),
            _ => None,
        }
    }

    /// The brain never competes for a worker permit: `reserve_brain_slot` already paid for it.
    /// `pin` is `brain.account`, and it is honoured or the call fails; it is never ignored.
    pub async fn acquire_brain(
        self: &Arc<Self>,
        provider: Provider,
        pin: Option<&AccountId>,
        deadline: Instant,
    ) -> Result<Lease, NoCapacity> {
        let chosen = match pin {
            Some(id) => {
                if !self.accounts.contains_key(id) {
                    return Err(NoCapacity::Exhausted {
                        reason: format!("brain.account `{}` is not a configured account", id.0),
                    });
                }
                if !self.solo(provider) && self.cfg.brain.reserve_brain_slot != Some(false) {
                    *self.reserved.lock() = Some(id.clone());
                }
                Some(id.clone())
            }
            None if self.cfg.brain.reserve_brain_slot != Some(false) => {
                self.reserve_for_brain(provider)
            }
            None => None,
        };
        let permit = match tokio::time::timeout_at(
            deadline,
            Arc::clone(&self.brain).acquire_owned(),
        )
        .await
        {
            Ok(Ok(p)) => p,
            Ok(Err(_)) => {
                self.release_reservation();
                return Err(NoCapacity::Exhausted {
                    reason: "dispatch pool is shutting down".into(),
                });
            }
            Err(_) => {
                self.release_reservation();
                return Err(NoCapacity::Saturated);
            }
        };
        // The permit is held for the whole wait: a cooling account comes back on a timer, and
        // failing the command instead of waiting for it throws the run away.
        let mut permit = Some(permit);
        let exclude = HashSet::new();
        let mut announced = false;
        loop {
            // Registered BEFORE the check, exactly as `acquire_node` does it.
            let notified = self.returned.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let wake_at = match self.capacity_for(provider, &exclude, chosen.as_ref()) {
                Capacity::Ready => {
                    match self.take_from(provider, &exclude, chosen.as_ref(), None) {
                        Some(mut lease) => {
                            lease._permit = permit.take();
                            lease.brain = true;
                            *self.brain_held.lock() = Some(lease.account.clone());
                            return Ok(lease);
                        }
                        None => {
                            self.returned.notify_waiters();
                            None
                        }
                    }
                }
                Capacity::Busy { next_reset } => next_reset,
                Capacity::AllExhausted { retry_at, why } => {
                    if !announced {
                        announced = true;
                        tracing::warn!(
                            "the brain's account is at its limit until {}: {why}",
                            crate::ui::fmt::clock_hm(retry_at)
                        );
                    }
                    Some(retry_at)
                }
                // No timer will rescue this: disabled, auth-broken, or out of credits.
                Capacity::Exhausted { reason } => {
                    self.release_reservation();
                    return Err(NoCapacity::Exhausted { reason });
                }
            };
            if Instant::now() >= deadline {
                self.release_reservation();
                return Err(NoCapacity::Saturated);
            }
            let until = wake_at
                .map_or(Instant::now() + IDLE_RECHECK, instant_of)
                .min(deadline);
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep_until(until) => {}
            }
        }
    }

    pub fn report(&self, id: &AccountId, failure: Option<&Failure>, cost: Option<Cost>) {
        let now = OffsetDateTime::now_utc();
        let health = {
            let mut state = self.state.lock();
            let entry = state.entry(id.clone()).or_default();
            entry.lifetime_nodes += 1;
            if let Some(c) = cost {
                entry.lifetime_cost_usd += c.usd;
            }
            match failure {
                None => {
                    entry.consecutive_infra_failures = 0;
                    // A sibling node's cooldown, or `swamp accounts disable`, may have landed
                    // while this node was running: finishing must not undo either.
                    let gated = matches!(entry.health, Health::AuthBroken | Health::Disabled)
                        || entry.cooldown_until.is_some_and(|t| t > now);
                    if !gated {
                        entry.cooldown_until = None;
                        entry.health = health_from_quota(entry, self.quota_warn_at());
                    }
                }
                Some(f) => {
                    // Only infrastructure failures count against the account; a failing task
                    // is the task's problem and must never cool a subscription.
                    if f.rotates_account() || f.retries_same_account() {
                        if !matches!(f, Failure::Overloaded { .. }) {
                            entry.consecutive_infra_failures += 1;
                        }
                        let consecutive = entry.consecutive_infra_failures;
                        if let Some(d) = cooldown_for(f, consecutive, &self.cfg.cooldown, now) {
                            entry.cooldown_until = Some(now + d);
                            entry.health = Health::Cooling;
                        }
                    }
                    if matches!(f, Failure::AuthExpired { .. }) {
                        entry.health = Health::AuthBroken;
                    }
                }
            }
            entry.health
        };
        self.after_change(id, health);
    }

    /// Spend, without a node: a long brain session bills per turn but is one node per run.
    pub fn credit(&self, id: &AccountId, cost: Cost) {
        if cost.usd == 0.0 {
            return;
        }
        let health = {
            let mut state = self.state.lock();
            let entry = state.entry(id.clone()).or_default();
            entry.lifetime_cost_usd += cost.usd;
            entry.health
        };
        self.after_change(id, health);
    }

    /// Fed from every live WorkerEvent::RateLimit, while the worker is still running. The
    /// source is inferred; a caller that knows better says so through `observe_quota_from`.
    pub fn observe_quota(&self, id: &AccountId, snap: RateLimitSnapshot) {
        let source = self.infer_source(id, &snap);
        self.observe_quota_from(id, snap, source);
    }

    /// A whole provider reading: every bucket it reported is kept for display, and the one
    /// bucket this account bills against is what dispatch scores.
    pub fn observe_quota_read(
        &self,
        id: &AccountId,
        buckets: &BTreeMap<String, RateLimitSnapshot>,
        snap: RateLimitSnapshot,
        source: QuotaSource,
    ) {
        {
            let mut state = self.state.lock();
            state.entry(id.clone()).or_default().apply_buckets(buckets);
        }
        self.observe_quota_from(id, snap, source);
    }

    /// Records the snapshot, its provenance and the window roll it may imply.
    pub fn observe_quota_from(&self, id: &AccountId, snap: RateLimitSnapshot, source: QuotaSource) {
        let now = OffsetDateTime::now_utc();
        let (health, rolled, window, lifetime, window_key) = {
            let mut state = self.state.lock();
            let entry = state.entry(id.clone()).or_default();
            let was_gated = hard_gated(entry);
            let rolled = entry.apply_quota(snap, source, now);
            health_after_quota(entry, was_gated, self.quota_warn_at());
            (
                entry.health,
                rolled,
                entry.window_tokens,
                entry.lifetime_tokens,
                entry.window_key.clone(),
            )
        };
        if rolled {
            crate::dispatch::emit(
                &self.journal,
                None,
                JournalEvent::AccountUsage {
                    account: id.clone(),
                    window,
                    lifetime,
                    window_key,
                    rolled: true,
                    source: Some(source),
                },
            );
        }
        self.after_change(id, health);
    }

    /// `cumulative` is this node's running total, not a delta. Idempotent: calling it twice
    /// with the same value changes nothing.
    pub fn observe_usage(&self, id: &AccountId, node: NodeId, cumulative: Usage) {
        self.ledger.lock().observe(id, node, cumulative);
    }

    /// The node is terminal: fold its final total into the committed counters and forget it.
    pub fn commit_usage(&self, id: &AccountId, node: NodeId, final_total: Usage) {
        let owed = self.ledger.lock().commit(id, node, final_total);
        if owed == Usage::default() {
            return;
        }
        let estimated_window = self.estimated_window(id);
        let (window, lifetime, window_key, source, rolled) = {
            let now = OffsetDateTime::now_utc();
            let mut state = self.state.lock();
            let entry = state.entry(id.clone()).or_default();
            // No snapshot ever reaches some accounts; without this their window counter is a
            // lifetime counter and every `share` term is scored against the wrong number.
            let rolled = entry.roll_elapsed_window(estimated_window, now);
            entry.credit_tokens(&owed);
            entry.updated_at = Some(now);
            (
                entry.window_tokens,
                entry.lifetime_tokens,
                entry.window_key.clone(),
                entry.quota_source,
                rolled,
            )
        };
        crate::dispatch::emit(
            &self.journal,
            Some(node),
            JournalEvent::AccountUsage {
                account: id.clone(),
                window,
                lifetime,
                window_key,
                rolled,
                source,
            },
        );
        self.persist();
        self.returned.notify_waiters();
    }

    /// Tokens of this account's nodes that have not finished yet.
    pub fn inflight_usage(&self, id: &AccountId) -> Usage {
        self.ledger.lock().inflight(id)
    }

    /// Committed counters plus what the running nodes have spent so far, which is what every
    /// display and every `share` term is scored against.
    pub fn snapshot(&self) -> Vec<(Provider, AccountId, AccountState)> {
        let state = self.state.lock();
        let ledger = self.ledger.lock();
        self.accounts
            .values()
            .map(|a| (a.provider, a.id.clone(), live_state(&state, &ledger, &a.id)))
            .collect()
    }

    pub fn set_enabled(&self, id: &AccountId, on: bool) {
        let health = {
            let mut state = self.state.lock();
            let entry = state.entry(id.clone()).or_default();
            entry.health = if on {
                health_from_quota(entry, self.quota_warn_at())
            } else {
                Health::Disabled
            };
            entry.health
        };
        self.after_change(id, health);
    }

    pub fn clear(&self, id: &AccountId) {
        let health = {
            let mut state = self.state.lock();
            let entry = state.entry(id.clone()).or_default();
            entry.clear_gates();
            entry.health = health_from_quota(entry, self.quota_warn_at());
            entry.health
        };
        self.after_change(id, health);
    }

    pub fn cooldown(&self, id: &AccountId, d: Duration, why: &str) {
        let health = {
            let mut state = self.state.lock();
            let entry = state.entry(id.clone()).or_default();
            entry.cooldown_until = Some(OffsetDateTime::now_utc() + d);
            entry.health = Health::Cooling;
            entry.health
        };
        tracing::info!("cooling account {} for {d:?}: {why}", id.0);
        self.after_change(id, health);
    }

    /// Reserve one healthy account of `p` for the brain, excluded from worker selection.
    /// A single-account provider reserves nothing: holding back the only account would leave
    /// the workers with none at all.
    pub fn reserve_for_brain(&self, p: Provider) -> Option<AccountId> {
        if self.solo(p) {
            return None;
        }
        if let Some(id) = self.reserved.lock().clone() {
            return Some(id);
        }
        let now = OffsetDateTime::now_utc();
        let candidates: Vec<&Account> =
            self.accounts.values().filter(|a| a.provider == p).collect();
        let state = self.state.lock();
        let ledger = self.ledger.lock();
        let live = live_states(&state, &ledger, &candidates);
        let pool_window = pool_window(&live);
        let best = live
            .iter()
            .enumerate()
            .filter_map(|(i, (a, s))| {
                rank(self.policy, a, s, pool_window, &self.scoring, now, i)
                    .map(|r| (r, a.id.clone()))
            })
            .min_by(|a, b| a.0.compare(&b.0))
            .map(|(_, id)| id)?;
        drop(ledger);
        drop(state);
        *self.reserved.lock() = Some(best.clone());
        Some(best)
    }

    /// Whether holding one account back would leave the workers with none. Counting
    /// configured accounts is not enough: a disabled or auth-broken peer cannot take work.
    fn solo(&self, p: Provider) -> bool {
        self.usable(p) < 2
    }

    /// Accounts of `p` that could take a node right now, reservation ignored.
    fn usable(&self, p: Provider) -> usize {
        let candidates: Vec<&Account> =
            self.accounts.values().filter(|a| a.provider == p).collect();
        self.scorable(&candidates)
    }

    /// How many of `candidates` `score` would dispatch to right now.
    fn scorable(&self, candidates: &[&Account]) -> usize {
        let now = OffsetDateTime::now_utc();
        let state = self.state.lock();
        let ledger = self.ledger.lock();
        let live = live_states(&state, &ledger, candidates);
        let pool_window = pool_window(&live);
        live.iter()
            .filter(|(a, s)| score(self.policy, a, s, pool_window, &self.scoring, now).is_some())
            .count()
    }

    /// The brain's reservation is dropped when its lease ends or its acquire fails: an
    /// account held for a brain that is not running is an account nobody can use.
    fn release_reservation(&self) {
        *self.reserved.lock() = None;
    }

    /// Accounts the shared state file remembers but this repo's config no longer names.
    /// `/usage` lists them last; the pool never dispatches to them.
    pub fn unconfigured(&self) -> Vec<(AccountId, AccountState)> {
        self.state
            .lock()
            .iter()
            .filter(|(id, _)| !self.accounts.contains_key(id))
            .map(|(id, s)| (id.clone(), s.clone()))
            .collect()
    }

    /// Loop iterations spent inside `acquire`. A spinning pool shows up here.
    pub fn wakeups(&self) -> u64 {
        self.wakeups.load(Ordering::Relaxed)
    }

    fn quota_warn_at(&self) -> f64 {
        self.scoring.warn_at
    }

    fn quota_stop_at(&self) -> f64 {
        self.scoring.stop_at
    }

    /// Only the caller knows whether a codex snapshot came off the rollout or the app-server;
    /// everything else the pool can tell from the snapshot and the account's provider.
    fn infer_source(&self, id: &AccountId, snap: &RateLimitSnapshot) -> QuotaSource {
        if !snap.windows.is_empty() && snap.windows.iter().all(|w| !w.measured) {
            return QuotaSource::Estimated;
        }
        match self.accounts.get(id).map(|a| a.provider) {
            Some(Provider::Openai) => QuotaSource::Rollout,
            _ => QuotaSource::Telemetry,
        }
    }

    fn candidates(&self, provider: Provider, exclude: &HashSet<AccountId>) -> Vec<&Account> {
        let all: Vec<&Account> = self
            .accounts
            .values()
            .filter(|a| a.provider == provider)
            .filter(|a| !exclude.contains(&a.id))
            .collect();
        let reserved = self.reserved.lock().clone();
        let Some(reserved) = reserved else {
            return all;
        };
        let rest: Vec<&Account> = all.iter().copied().filter(|a| a.id != reserved).collect();
        // The reservation is honoured only while the workers still have someone else to use:
        // a peer that degrades later must not strand every node on an idle reserved account.
        if self.scorable(&rest) > 0 { rest } else { all }
    }

    fn capacity(&self, provider: Provider, exclude: &HashSet<AccountId>) -> Capacity {
        self.capacity_for(provider, exclude, None)
    }

    /// `pin` narrows the candidates to the one account the caller insists on, so the brain
    /// reads the same gate a worker does instead of a pool-wide verdict that ignores it.
    fn capacity_for(
        &self,
        provider: Provider,
        exclude: &HashSet<AccountId>,
        pin: Option<&AccountId>,
    ) -> Capacity {
        let now = OffsetDateTime::now_utc();
        let candidates: Vec<&Account> = match pin {
            Some(id) => self.accounts.values().filter(|a| &a.id == id).collect(),
            None => self.candidates(provider, exclude),
        };
        if candidates.is_empty() {
            return Capacity::Exhausted {
                reason: self.no_account_error(provider, exclude).to_string(),
            };
        }
        let live = {
            let state = self.state.lock();
            let ledger = self.ledger.lock();
            live_states(&state, &ledger, &candidates)
        };
        let pool_window = pool_window(&live);
        let (mut busy, mut retry_at) = (false, None::<OffsetDateTime>);
        let mut why: Vec<String> = Vec::new();
        let brain_held = self.brain_held.lock().clone();
        for (a, s) in &live {
            if score(self.policy, a, s, pool_window, &self.scoring, now).is_some() {
                return Capacity::Ready;
            }
            match self.block_reason(a, s, now) {
                // A slot the brain holds for the whole run comes back to nobody, so it must
                // not look like a lease that is about to be returned. Its siblings are
                // ordinary worker leases and do come back, so they still read as busy.
                Block::Busy if brain_held.as_ref() == Some(&a.id) && s.inflight <= 1 => {
                    why.push(format!("{} is held by the brain", a.id.0));
                }
                Block::Busy => busy = true,
                Block::Wait { at, why: w } => {
                    retry_at = Some(retry_at.map_or(at, |c: OffsetDateTime| c.min(at)));
                    why.push(w);
                }
                Block::Hard { why: w } => why.push(w),
            }
        }
        match (busy, retry_at) {
            (true, next_reset) => Capacity::Busy { next_reset },
            (false, Some(retry_at)) => Capacity::AllExhausted {
                retry_at,
                why: why.join("; "),
            },
            (false, None) if why.is_empty() => Capacity::Exhausted {
                reason: self.no_account_error(provider, exclude).to_string(),
            },
            // Hard-gated with no reset: sleeping forever helps nobody.
            (false, None) => Capacity::Exhausted {
                reason: why.join("; "),
            },
        }
    }

    /// Why one ineligible candidate is ineligible, in the order `score` gates it.
    fn block_reason(&self, a: &Account, s: &AccountState, now: OffsetDateTime) -> Block {
        let who = &a.id.0;
        // Every hard gate first, exactly as `score` gates them: a human-gated account
        // carrying a cooldown must not advertise a retry time that cannot help. The
        // provider's own reason outranks the health word it produced.
        if s.health == Health::Disabled {
            return Block::Hard {
                why: format!("{who} disabled"),
            };
        }
        if s.quota.as_ref().and_then(|q| q.ordinary_usage_allowed) == Some(false) {
            return Block::Hard {
                why: format!("{who} is refused ordinary usage by the provider"),
            };
        }
        match s.quota.as_ref().and_then(|q| q.reached) {
            Some(LimitReached::CreditsDepleted) => {
                return Block::Hard {
                    why: format!("{who} has no credits left"),
                };
            }
            Some(LimitReached::SpendControl) => {
                return Block::Hard {
                    why: format!("{who} stopped by a spend control"),
                };
            }
            _ => {}
        }
        if s.health == Health::AuthBroken {
            return Block::Hard {
                why: format!("{who} needs re-authentication"),
            };
        }
        // After the hard gates, never before: a depleted account also carrying a cooldown
        // must not advertise a retry time that brings nobody back.
        if let Some(t) = s.cooldown_until.filter(|t| *t > now) {
            return Block::Wait {
                at: t,
                why: format!("{who} cooling until {}", crate::ui::fmt::clock_day(t, now)),
            };
        }
        if a.max_concurrency.is_some_and(|c| s.inflight >= c) {
            return Block::Busy;
        }
        let stop = self.quota_stop_at();
        let util = s
            .quota
            .as_ref()
            .and_then(|q| q.measured_utilization_at(now))
            .unwrap_or(0.0);
        let scope = scope_word(
            s.quota
                .as_ref()
                .map_or(LimitScope::Unknown, |q| q.worst_scope()),
        );
        let reset = s.quota.as_ref().and_then(|q| q.soonest_reset_at(now));
        // Truncated, not rounded: 98.5% of a window is not 99% of it yet.
        let pct = (util * 100.0) as i64;
        match reset {
            // A reset in the past is not a wait: the numbers are simply stale.
            Some(at) if util >= stop => Block::Wait {
                at,
                why: format!(
                    "{who} at {pct}% of its {scope} window until {}",
                    crate::ui::fmt::clock_day(at, now)
                ),
            },
            _ if util >= stop => Block::Hard {
                why: format!("{who} at {pct}% of its {scope} window"),
            },
            _ => Block::Hard {
                why: format!("{who} is not usable"),
            },
        }
    }

    fn take(self: &Arc<Self>, provider: Provider, exclude: &HashSet<AccountId>) -> Option<Lease> {
        self.take_from(provider, exclude, None, None)
    }

    fn take_from(
        self: &Arc<Self>,
        provider: Provider,
        exclude: &HashSet<AccountId>,
        pin: Option<&AccountId>,
        permit: Option<OwnedSemaphorePermit>,
    ) -> Option<Lease> {
        let now = OffsetDateTime::now_utc();
        let candidates: Vec<&Account> = match pin {
            Some(id) => self.accounts.values().filter(|a| &a.id == id).collect(),
            None => self.candidates(provider, exclude),
        };
        // Rotate the candidate order by one per selection: two accounts tied on load and on
        // lifetime tokens then alternate instead of the first name always winning.
        let turn = self.cursor.fetch_add(1, Ordering::Relaxed) as usize;
        let len = candidates.len().max(1);
        let mut state = self.state.lock();
        let live = {
            let ledger = self.ledger.lock();
            live_states(&state, &ledger, &candidates)
        };
        // One denominator for the whole selection, computed under the state lock.
        let pool_window = pool_window(&live);
        let best = live
            .iter()
            .enumerate()
            .filter_map(|(i, (a, s))| {
                let rotation = (i + len - turn % len) % len;
                rank(self.policy, a, s, pool_window, &self.scoring, now, rotation).map(|r| (r, *a))
            })
            .min_by(|a, b| a.0.compare(&b.0))
            .map(|(r, a)| (r.score, a.id.clone(), a.exec.clone(), a.env.clone()))?;
        let (sc, id, exec, env) = best;
        let entry = state.entry(id.clone()).or_default();
        entry.inflight += 1;
        entry.last_used = Some(now);
        drop(state);

        crate::dispatch::emit(
            &self.journal,
            None,
            JournalEvent::AccountSelected {
                account: id.clone(),
                exec: exec.clone(),
                policy: self.policy,
                reason: format!("score {sc:.4}"),
                excluded: exclude.iter().cloned().collect(),
            },
        );
        Some(Lease {
            account: id,
            exec,
            env,
            _permit: permit,
            brain: false,
            pool: Arc::clone(self),
        })
    }

    fn no_account_error(
        &self,
        provider: Provider,
        exclude: &HashSet<AccountId>,
    ) -> crate::error::SwampError {
        let now = OffsetDateTime::now_utc();
        let state = self.state.lock();
        let cooling = self
            .accounts
            .values()
            .filter(|a| a.provider == provider)
            .filter(|a| {
                state
                    .get(&a.id)
                    .is_some_and(|s| s.cooldown_until.is_some_and(|t| t > now))
            })
            .count();
        crate::error::SwampError::NoAccountAvailable {
            provider,
            excluded: exclude.len(),
            cooling,
        }
    }

    fn after_change(&self, id: &AccountId, health: Health) {
        let (cooldown_until, quota, quota_observed_at, quota_source) = {
            let mut state = self.state.lock();
            let entry = state.entry(id.clone()).or_default();
            entry.updated_at = Some(OffsetDateTime::now_utc());
            (
                entry.cooldown_until,
                entry.quota.clone(),
                entry.quota_observed_at,
                entry.quota_source,
            )
        };
        crate::dispatch::emit(
            &self.journal,
            None,
            JournalEvent::AccountHealth {
                account: id.clone(),
                health,
                cooldown_until,
                quota,
                quota_observed_at,
                quota_source,
            },
        );
        self.persist();
        self.returned.notify_waiters();
    }

    /// The state file is machine-wide, locked and fsynced: doing that inline on a runtime
    /// thread parks it, and every telemetry event asks for one. Bursts coalesce into the
    /// flush that is already running, and the blocking work is handed off the worker thread.
    fn persist(&self) {
        {
            let mut gate = self.persisting.lock();
            gate.dirty = true;
            if gate.in_flight {
                return;
            }
            gate.in_flight = true;
        }
        loop {
            {
                let mut gate = self.persisting.lock();
                if !gate.dirty {
                    gate.in_flight = false;
                    return;
                }
                gate.dirty = false;
            }
            let snapshot = self.state.lock().clone();
            match blocking(|| persist::merge_state(&self.state_path, &snapshot)) {
                Ok(merged) => self.adopt(merged),
                Err(e) => tracing::warn!(
                    "could not persist account state to {}: {e}",
                    self.state_path
                ),
            }
        }
    }

    /// The window an account with no quota source at all counts against.
    fn estimated_window(&self, id: &AccountId) -> Duration {
        const DEFAULT_WINDOW: Duration = Duration::from_secs(7 * 24 * 3600);
        self.accounts
            .get(id)
            .and_then(|a| self.cfg.providers.get(&a.provider))
            .and_then(|p| p.estimated_window)
            .unwrap_or(DEFAULT_WINDOW)
    }

    /// Another process's newer entry wins, so a cooldown or a `swamp accounts disable` learned
    /// elsewhere reaches a dispatcher that is already running.
    fn adopt(&self, merged: StateMap) {
        let mut state = self.state.lock();
        for (id, theirs) in merged {
            let entry = state.entry(id).or_default();
            if theirs.updated_at > entry.updated_at {
                let inflight = entry.inflight;
                *entry = theirs;
                entry.inflight = inflight;
            }
        }
    }
}

/// Blocking file I/O, off the runtime's worker thread when there is one to step off.
fn blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(f),
        _ => f(),
    }
}

/// Committed counters plus the tokens this account's running nodes have already spent.
fn live_state(state: &StateMap, ledger: &UsageLedger, id: &AccountId) -> AccountState {
    let mut s = state.get(id).cloned().unwrap_or_default();
    let live = ledger.inflight(id);
    s.window_tokens.absorb(&live);
    s.lifetime_tokens.absorb(&live);
    s
}

fn live_states<'a>(
    state: &StateMap,
    ledger: &UsageLedger,
    candidates: &[&'a Account],
) -> Vec<(&'a Account, AccountState)> {
    candidates
        .iter()
        .map(|a| (*a, live_state(state, ledger, &a.id)))
        .collect()
}

/// The `share` denominator: one number for the whole selection, so every candidate is scored
/// against the same pool.
fn pool_window(live: &[(&Account, AccountState)]) -> u64 {
    live.iter().map(|(_, s)| s.window_tokens.billable()).sum()
}

fn scope_word(s: LimitScope) -> &'static str {
    match s {
        LimitScope::FiveHour => "five_hour",
        LimitScope::SevenDay => "seven_day",
        LimitScope::Minute => "minute",
        LimitScope::Unknown => "unknown",
    }
}

async fn cancelled(token: Option<&CancellationToken>) {
    match token {
        Some(t) => t.cancelled().await,
        None => std::future::pending().await,
    }
}

/// Re-derives health after a fresh quota reading. A gate that has just been lifted also
/// clears the `AuthBroken` it stamped; a real auth failure, which was never gated, stays,
/// and so do the words a human set.
pub fn health_after_quota(entry: &mut AccountState, was_gated: bool, warn_at: f64) {
    let lifted = was_gated && !hard_gated(entry) && entry.health == Health::AuthBroken;
    if hard_gated(entry)
        || lifted
        || !matches!(
            entry.health,
            Health::AuthBroken | Health::Disabled | Health::Cooling
        )
    {
        entry.health = health_from_quota(entry, warn_at);
    }
}

pub fn health_from_quota(s: &AccountState, warn_at: f64) -> Health {
    // USAGE 2.2: depleted credits or a spend control is not a timer, so the health every
    // surface reads has to say a human must act.
    if hard_gated(s) {
        return Health::AuthBroken;
    }
    let now = OffsetDateTime::now_utc();
    let util = s
        .quota
        .as_ref()
        .map_or(0.0, |q| q.worst_utilization_at(now));
    if util >= warn_at {
        Health::Degraded
    } else {
        Health::Healthy
    }
}

/// The provider's own refusals, which no timer lifts.
pub fn hard_gated(s: &AccountState) -> bool {
    let Some(q) = s.quota.as_ref() else {
        return false;
    };
    q.ordinary_usage_allowed == Some(false)
        || matches!(
            q.reached,
            Some(LimitReached::CreditsDepleted | LimitReached::SpendControl)
        )
}

/// Wall-clock instants come from the provider; the runtime schedules on monotonic ones.
pub fn instant_of(at: OffsetDateTime) -> Instant {
    let now = OffsetDateTime::now_utc();
    Instant::now() + Duration::try_from(at - now).unwrap_or_default()
}
