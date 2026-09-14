#![allow(dead_code, unused_variables)]

use crate::config::Config;
use crate::dispatch::account::AccountState;
use crate::dispatch::policy::SelectionPolicy;
use crate::journal::JournalHandle;
use crate::model::core::{AccountId, Cost, Provider, RateLimitSnapshot};
use crate::model::failure::Failure;
use camino::Utf8PathBuf;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::Instant;

pub struct AccountPool {
    pub cfg: Arc<Config>,
    pub policy: SelectionPolicy,
    pub state_path: Utf8PathBuf,
    pub journal: JournalHandle,
}

/// Drop decrements inflight and notifies waiters, so a panicking node cannot leak a slot.
pub struct Lease {
    pub account: AccountId,
    pub exec: String,
    pub env: BTreeMap<String, String>,
    _permit: OwnedSemaphorePermit,
    pool: Arc<AccountPool>,
}

pub enum NoCapacity {
    AllCooling { retry_at: OffsetDateTime },
    Saturated,
    Exhausted { reason: String },
}

impl AccountPool {
    pub fn new(
        cfg: Arc<Config>,
        state_path: Utf8PathBuf,
        journal: JournalHandle,
    ) -> anyhow::Result<Arc<Self>> {
        todo!("WP4")
    }
    /// Never busy-spins: waits on a Notify or sleeps until the earliest journaled reset.
    pub async fn acquire(
        self: &Arc<Self>,
        provider: Provider,
        exclude: &HashSet<AccountId>,
        deadline: Instant,
    ) -> Result<Lease, NoCapacity> {
        todo!("WP4")
    }
    pub fn report(&self, id: &AccountId, failure: Option<&Failure>, cost: Option<Cost>) {
        todo!("WP4")
    }
    /// Fed from every live WorkerEvent::RateLimit, while the worker is still running.
    pub fn observe_quota(&self, id: &AccountId, snap: RateLimitSnapshot) {
        todo!("WP4")
    }
    pub fn snapshot(&self) -> Vec<(Provider, AccountId, AccountState)> {
        todo!("WP4")
    }
    pub fn set_enabled(&self, id: &AccountId, on: bool) {
        todo!("WP4")
    }
    pub fn clear(&self, id: &AccountId) {
        todo!("WP4")
    }
    pub fn cooldown(&self, id: &AccountId, d: Duration, why: &str) {
        todo!("WP4")
    }
    /// Reserve one healthy account of `p` for the brain, excluded from worker selection.
    pub fn reserve_for_brain(&self, p: Provider) -> Option<AccountId> {
        todo!("WP4")
    }
}
