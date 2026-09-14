use crate::model::core::LimitScope;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

// DESIGN derives Eq here, which f64 cannot satisfy; PartialEq only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Failure {
    /// Provider quota exhausted for this account. Cool it, try the next one.
    RateLimited {
        #[serde(with = "time::serde::rfc3339::option")]
        resets_at: Option<OffsetDateTime>,
        scope: LimitScope,
        detected_by: Detector,
        /// The literal matching line, truncated. Makes a misclassification diagnosable.
        evidence: String,
    },
    /// Credentials for this account are dead. Out of rotation until a human fixes it.
    AuthExpired {
        detail: String,
        detected_by: Detector,
    },
    /// Transient upstream capacity problem (429-adjacent 529/503). Back off on the SAME account.
    Overloaded {
        detail: String,
    },
    /// Our own guard tripped. Do NOT fail over: another account would spend too.
    BudgetExceeded {
        limit_usd: f64,
        spent_usd: f64,
    },
    Timeout {
        after_s: u64,
    },
    /// The task itself failed. NEVER rotate: a bad prompt would burn every subscription.
    WorkerError {
        subtype: String,
        detail: String,
    },
    /// Tools were auto-denied because nobody could answer a prompt.
    PermissionDenied {
        denials: u32,
    },
    /// Process died abnormally (signal, OOM, supervisor kill).
    Crashed {
        signal: Option<i32>,
    },
    /// The stream ended with no terminal event and the pid is gone.
    Truncated {
        offset: u64,
    },
    /// No usable account at all.
    NoCapacity {
        detail: String,
    },
}

/// Which layer of the classifier fired. Journaled so `swamp doctor --schema` can report
/// the regex fallback rate, which is the early warning that a CLI changed its wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Detector {
    Telemetry,
    StructuredResult,
    Pattern,
    ExitCode,
}

impl Failure {
    /// Burn a different subscription on the same work.
    pub fn rotates_account(&self) -> bool {
        matches!(self, Self::RateLimited { .. } | Self::AuthExpired { .. })
    }
    /// Retry here with backoff, resuming the same session so the retry does not repay context.
    pub fn retries_same_account(&self) -> bool {
        matches!(
            self,
            Self::Overloaded { .. } | Self::Crashed { .. } | Self::Truncated { .. }
        )
    }
    /// Stop. The answer is the answer.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::WorkerError { .. }
                | Self::BudgetExceeded { .. }
                | Self::Timeout { .. }
                | Self::PermissionDenied { .. }
                | Self::NoCapacity { .. }
        )
    }
}
