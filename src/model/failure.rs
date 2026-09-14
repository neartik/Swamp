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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    enum Class {
        Rotate,
        Retry,
        Terminal,
    }

    /// No wildcard arm: a new variant must be classified or this stops compiling.
    fn class(f: &Failure) -> Class {
        match f {
            Failure::RateLimited { .. } => Class::Rotate,
            Failure::AuthExpired { .. } => Class::Rotate,
            Failure::Overloaded { .. } => Class::Retry,
            Failure::Crashed { .. } => Class::Retry,
            Failure::Truncated { .. } => Class::Retry,
            Failure::BudgetExceeded { .. } => Class::Terminal,
            Failure::Timeout { .. } => Class::Terminal,
            Failure::WorkerError { .. } => Class::Terminal,
            Failure::PermissionDenied { .. } => Class::Terminal,
            Failure::NoCapacity { .. } => Class::Terminal,
        }
    }

    fn every_variant() -> Vec<Failure> {
        vec![
            Failure::RateLimited {
                resets_at: Some(time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()),
                scope: LimitScope::FiveHour,
                detected_by: Detector::Telemetry,
                evidence: "usage limit reached".into(),
            },
            Failure::AuthExpired {
                detail: "token expired".into(),
                detected_by: Detector::Pattern,
            },
            Failure::Overloaded {
                detail: "529".into(),
            },
            Failure::BudgetExceeded {
                limit_usd: 3.0,
                spent_usd: 3.5,
            },
            Failure::Timeout { after_s: 1500 },
            Failure::WorkerError {
                subtype: "error_during_execution".into(),
                detail: "build failed".into(),
            },
            Failure::PermissionDenied { denials: 4 },
            Failure::Crashed { signal: Some(9) },
            Failure::Truncated { offset: 8192 },
            Failure::NoCapacity {
                detail: "all cooling".into(),
            },
        ]
    }

    #[test]
    fn every_variant_is_classified_exactly_once() {
        for f in every_variant() {
            let hits = [
                f.rotates_account(),
                f.retries_same_account(),
                f.is_terminal(),
            ];
            assert_eq!(
                hits.iter().filter(|h| **h).count(),
                1,
                "{f:?} is in {hits:?} policy sets, expected exactly one"
            );
            let expected = match class(&f) {
                Class::Rotate => [true, false, false],
                Class::Retry => [false, true, false],
                Class::Terminal => [false, false, true],
            };
            assert_eq!(hits, expected, "{f:?}");
        }
    }

    #[test]
    fn rotation_is_limited_to_rate_limit_and_auth() {
        let rotating: Vec<_> = every_variant()
            .into_iter()
            .filter(Failure::rotates_account)
            .map(|f| class(&f))
            .collect();
        assert_eq!(rotating, vec![Class::Rotate, Class::Rotate]);
    }

    #[test]
    fn failures_round_trip() {
        let all = every_variant();
        for f in &all {
            let json = serde_json::to_string(f).unwrap();
            assert_eq!(&serde_json::from_str::<Failure>(&json).unwrap(), f);
        }
        insta::assert_json_snapshot!(all);
    }
}
