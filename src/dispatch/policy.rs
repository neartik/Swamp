#![allow(dead_code, unused_variables)]

use crate::dispatch::account::{Account, AccountState};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SelectionPolicy {
    RoundRobin,
    /// v1 default: the only telemetry that feeds QuotaAware exists for one provider.
    #[default]
    LeastLoaded,
    QuotaAware,
}

/// Lower is better. None means ineligible.
pub fn score(
    policy: SelectionPolicy,
    a: &Account,
    s: &AccountState,
    quota_stop_at: f64,
    now: OffsetDateTime,
) -> Option<f64> {
    todo!("WP4")
}
