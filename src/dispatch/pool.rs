use crate::config::Config;
use crate::config::resolve::expand_env;
use crate::dispatch::account::{Account, AccountState, Health};
use crate::dispatch::cooldown::cooldown_for;
use crate::dispatch::persist::{self, StateMap};
use crate::dispatch::policy::{SelectionPolicy, rank, score};
use crate::journal::{JournalEvent, JournalHandle};
use crate::model::core::{AccountId, Cost, Provider, RateLimitSnapshot};
use crate::model::failure::Failure;
use camino::Utf8PathBuf;
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use time::OffsetDateTime;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

const DEFAULT_MAX_PARALLEL: usize = 4;
const DEFAULT_ACCOUNT_CONCURRENCY: usize = 2;
const DEFAULT_QUOTA_WARN: f64 = 0.90;
const DEFAULT_QUOTA_STOP: f64 = 0.98;
/// Upper bound on how long a blocked acquirer sleeps when nothing tells it when to look again.
const IDLE_RECHECK: Duration = Duration::from_secs(5);

pub struct AccountPool {
    pub cfg: Arc<Config>,
    pub policy: SelectionPolicy,
    pub state_path: Utf8PathBuf,
    pub journal: JournalHandle,
    accounts: BTreeMap<AccountId, Account>,
    state: Mutex<StateMap>,
    /// limits.max_parallel, minus the brain's permit when it is reserved.
    global: Arc<Semaphore>,
    /// The brain's own slot. Taking a worker permit for it would charge the reservation twice.
    brain: Arc<Semaphore>,
    returned: Notify,
    reserved: Mutex<Option<AccountId>>,
    wakeups: AtomicU64,
    /// Rotates the candidate order, so accounts tied on every counter still alternate.
    cursor: AtomicU64,
}

/// Drop decrements inflight and notifies waiters, so a panicking node cannot leak a slot.
pub struct Lease {
    pub account: AccountId,
    pub exec: String,
    pub env: BTreeMap<String, String>,
    _permit: OwnedSemaphorePermit,
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
    AllCooling { retry_at: OffsetDateTime },
    Saturated,
    Exhausted { reason: String },
}

/// What the pool can do for this provider right now.
enum Capacity {
    Ready,
    /// Every candidate is at its concurrency ceiling; a returning lease unblocks us.
    Busy {
        next_reset: Option<OffsetDateTime>,
    },
    AllCooling {
        retry_at: OffsetDateTime,
    },
    Exhausted {
        reason: String,
    },
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
                        max_concurrency: a
                            .max_concurrency
                            .unwrap_or(DEFAULT_ACCOUNT_CONCURRENCY)
                            .max(1),
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

        let mut permits = cfg
            .limits
            .max_parallel
            .unwrap_or(DEFAULT_MAX_PARALLEL)
            .max(1);
        if cfg.brain.reserve_brain_slot == Some(true) {
            permits = permits.saturating_sub(1).max(1);
        }

        Ok(Arc::new(Self {
            policy: cfg.dispatch.policy.unwrap_or_default(),
            cfg,
            state_path,
            journal,
            accounts,
            state: Mutex::new(state),
            global: Arc::new(Semaphore::new(permits)),
            brain: Arc::new(Semaphore::new(1)),
            returned: Notify::new(),
            reserved: Mutex::new(None),
            wakeups: AtomicU64::new(0),
            cursor: AtomicU64::new(0),
        }))
    }

    /// Never busy-spins: waits on a Notify or sleeps until the earliest journaled reset.
    pub async fn acquire(
        self: &Arc<Self>,
        provider: Provider,
        exclude: &HashSet<AccountId>,
        deadline: Instant,
    ) -> Result<Lease, NoCapacity> {
        loop {
            self.wakeups.fetch_add(1, Ordering::Relaxed);
            // Registered BEFORE the capacity check: a lease returned in between must not be
            // lost to a waiter that had not subscribed yet.
            let notified = self.returned.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let wake_at = match self.capacity(provider, exclude) {
                Capacity::Ready => {
                    let permit = match tokio::time::timeout_at(
                        deadline,
                        Arc::clone(&self.global).acquire_owned(),
                    )
                    .await
                    {
                        Ok(Ok(p)) => p,
                        Ok(Err(_)) => {
                            return Err(NoCapacity::Exhausted {
                                reason: "dispatch pool is shutting down".into(),
                            });
                        }
                        Err(_) => return Err(NoCapacity::Saturated),
                    };
                    match self.take(provider, exclude, permit) {
                        Some(lease) => return Ok(lease),
                        // Someone else won the race; the permit is already back.
                        None => {
                            self.returned.notify_waiters();
                            None
                        }
                    }
                }
                Capacity::Busy { next_reset } => next_reset,
                Capacity::AllCooling { retry_at } => {
                    return Err(NoCapacity::AllCooling { retry_at });
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
            }
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
                return Err(NoCapacity::Exhausted {
                    reason: "dispatch pool is shutting down".into(),
                });
            }
            Err(_) => return Err(NoCapacity::Saturated),
        };
        let exclude = HashSet::new();
        self.take_from(provider, &exclude, chosen.as_ref(), permit)
            .ok_or_else(|| NoCapacity::Exhausted {
                reason: match &chosen {
                    Some(id) => format!("the brain's account `{}` is cooling or disabled", id.0),
                    None => self.no_account_error(provider, &exclude).to_string(),
                },
            })
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
                    entry.cooldown_until = None;
                    entry.health = health_from_quota(entry, self.quota_warn_at());
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

    /// Fed from every live WorkerEvent::RateLimit, while the worker is still running.
    pub fn observe_quota(&self, id: &AccountId, snap: RateLimitSnapshot) {
        let health = {
            let mut state = self.state.lock();
            let entry = state.entry(id.clone()).or_default();
            entry.quota = Some(snap);
            if !matches!(
                entry.health,
                Health::AuthBroken | Health::Disabled | Health::Cooling
            ) {
                entry.health = health_from_quota(entry, self.quota_warn_at());
            }
            entry.health
        };
        self.after_change(id, health);
    }

    pub fn snapshot(&self) -> Vec<(Provider, AccountId, AccountState)> {
        let state = self.state.lock();
        self.accounts
            .values()
            .map(|a| {
                (
                    a.provider,
                    a.id.clone(),
                    state.get(&a.id).cloned().unwrap_or_default(),
                )
            })
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
            entry.cooldown_until = None;
            entry.consecutive_infra_failures = 0;
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
        let state = self.state.lock();
        let stop = self.quota_stop_at();
        let best = self
            .accounts
            .values()
            .filter(|a| a.provider == p)
            .enumerate()
            .filter_map(|(i, a)| {
                let s = state.get(&a.id).cloned().unwrap_or_default();
                rank(self.policy, a, &s, stop, now, i).map(|r| (r, a.id.clone()))
            })
            .min_by(|a, b| a.0.compare(&b.0))
            .map(|(_, id)| id)?;
        *self.reserved.lock() = Some(best.clone());
        Some(best)
    }

    fn solo(&self, p: Provider) -> bool {
        self.accounts.values().filter(|a| a.provider == p).count() < 2
    }

    /// Loop iterations spent inside `acquire`. A spinning pool shows up here.
    pub fn wakeups(&self) -> u64 {
        self.wakeups.load(Ordering::Relaxed)
    }

    fn quota_warn_at(&self) -> f64 {
        self.cfg
            .cooldown
            .quota_warn_at
            .unwrap_or(DEFAULT_QUOTA_WARN)
    }

    fn quota_stop_at(&self) -> f64 {
        self.cfg
            .cooldown
            .quota_stop_at
            .unwrap_or(DEFAULT_QUOTA_STOP)
    }

    fn candidates(&self, provider: Provider, exclude: &HashSet<AccountId>) -> Vec<&Account> {
        let reserved = self.reserved.lock().clone();
        self.accounts
            .values()
            .filter(|a| a.provider == provider)
            .filter(|a| !exclude.contains(&a.id))
            .filter(|a| reserved.as_ref() != Some(&a.id))
            .collect()
    }

    fn capacity(&self, provider: Provider, exclude: &HashSet<AccountId>) -> Capacity {
        let now = OffsetDateTime::now_utc();
        let candidates = self.candidates(provider, exclude);
        if candidates.is_empty() {
            return Capacity::Exhausted {
                reason: self.no_account_error(provider, exclude).to_string(),
            };
        }
        let state = self.state.lock();
        let stop = self.quota_stop_at();
        let (mut busy, mut cooling, mut ready) = (false, None::<OffsetDateTime>, false);
        for a in candidates {
            let s = state.get(&a.id).cloned().unwrap_or_default();
            if score(self.policy, a, &s, stop, now).is_some() {
                ready = true;
                break;
            }
            match s.cooldown_until {
                Some(t) if t > now => {
                    cooling = Some(cooling.map_or(t, |c: OffsetDateTime| c.min(t)))
                }
                _ if s.inflight >= a.max_concurrency => busy = true,
                _ => {}
            }
        }
        drop(state);
        if ready {
            return Capacity::Ready;
        }
        match (busy, cooling) {
            (true, next_reset) => Capacity::Busy { next_reset },
            (false, Some(retry_at)) => Capacity::AllCooling { retry_at },
            (false, None) => Capacity::Exhausted {
                reason: self.no_account_error(provider, exclude).to_string(),
            },
        }
    }

    fn take(
        self: &Arc<Self>,
        provider: Provider,
        exclude: &HashSet<AccountId>,
        permit: OwnedSemaphorePermit,
    ) -> Option<Lease> {
        self.take_from(provider, exclude, None, permit)
    }

    fn take_from(
        self: &Arc<Self>,
        provider: Provider,
        exclude: &HashSet<AccountId>,
        pin: Option<&AccountId>,
        permit: OwnedSemaphorePermit,
    ) -> Option<Lease> {
        let now = OffsetDateTime::now_utc();
        let candidates: Vec<&Account> = match pin {
            Some(id) => self.accounts.values().filter(|a| &a.id == id).collect(),
            None => self.candidates(provider, exclude),
        };
        let stop = self.quota_stop_at();
        // Rotate the candidate order by one per selection: two accounts tied on load and on
        // lifetime nodes then alternate instead of the first name always winning.
        let turn = self.cursor.fetch_add(1, Ordering::Relaxed) as usize;
        let len = candidates.len().max(1);
        let mut state = self.state.lock();
        let best = candidates
            .iter()
            .enumerate()
            .filter_map(|(i, a)| {
                let s = state.get(&a.id).cloned().unwrap_or_default();
                let rotation = (i + len - turn % len) % len;
                rank(self.policy, a, &s, stop, now, rotation).map(|r| (r, *a))
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
        let (cooldown_until, quota) = {
            let mut state = self.state.lock();
            let entry = state.entry(id.clone()).or_default();
            entry.updated_at = Some(OffsetDateTime::now_utc());
            (entry.cooldown_until, entry.quota.clone())
        };
        crate::dispatch::emit(
            &self.journal,
            None,
            JournalEvent::AccountHealth {
                account: id.clone(),
                health,
                cooldown_until,
                quota,
            },
        );
        self.persist();
        self.returned.notify_waiters();
    }

    fn persist(&self) {
        let snapshot = self.state.lock().clone();
        match persist::merge_state(&self.state_path, &snapshot) {
            Ok(merged) => self.adopt(merged),
            Err(e) => tracing::warn!(
                "could not persist account state to {}: {e}",
                self.state_path
            ),
        }
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

fn health_from_quota(s: &AccountState, warn_at: f64) -> Health {
    let util = s.quota.as_ref().map_or(0.0, |q| q.worst_utilization());
    if util >= warn_at {
        Health::Degraded
    } else {
        Health::Healthy
    }
}

/// Wall-clock instants come from the provider; the runtime schedules on monotonic ones.
pub fn instant_of(at: OffsetDateTime) -> Instant {
    let now = OffsetDateTime::now_utc();
    Instant::now() + Duration::try_from(at - now).unwrap_or_default()
}
