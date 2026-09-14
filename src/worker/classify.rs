use crate::model::core::LimitScope;
use crate::model::failure::{Detector, Failure};
use crate::model::node::ExitInfo;
use crate::worker::adapter::ExitContext;

pub const MAX_LINE: usize = 8 * 1024 * 1024;

const EVIDENCE_MAX: usize = 400;
const DETAIL_MAX: usize = 200;

/// Layered: telemetry, then the structured result, then config regexes, then the exit code.
pub fn classify(cx: &ExitContext<'_>) -> Option<Failure> {
    if let Some(rl) = &cx.state.last_rate_limit
        && rl.status == crate::model::core::LimitStatus::Rejected
    {
        return Some(Failure::RateLimited {
            resets_at: rl.soonest_reset(),
            scope: rl.worst_scope(),
            detected_by: Detector::Telemetry,
            evidence: "rate_limit_event status".into(),
        });
    }

    if let Some(f) = &cx.state.last_final {
        if f.api_error_status == Some(429) {
            return Some(Failure::RateLimited {
                resets_at: None,
                scope: LimitScope::Unknown,
                detected_by: Detector::StructuredResult,
                evidence: "api_error_status 429".into(),
            });
        }
        if matches!(f.api_error_status, Some(529) | Some(503)) {
            return Some(Failure::Overloaded {
                detail: format!(
                    "api_error_status {}",
                    f.api_error_status.unwrap_or_default()
                ),
            });
        }
        if f.subtype == BUDGET_SUBTYPE {
            return Some(Failure::BudgetExceeded {
                limit_usd: 0.0,
                spent_usd: f.cost.map_or(0.0, |c| c.usd),
            });
        }
        // Provider-neutral success: the adapter already decided what `ok` means on its wire.
        if f.ok && f.permission_denials == 0 {
            return None;
        }
        // A writing worker that "succeeded" with denials did not do the work it claims.
        if f.permission_denials > 0 {
            return Some(Failure::PermissionDenied {
                denials: f.permission_denials,
            });
        }
        let t = f.text.as_deref().unwrap_or("");
        if let Some(m) = cx.patterns.rate_limit_match(t) {
            return Some(Failure::RateLimited {
                resets_at: None,
                scope: LimitScope::Unknown,
                detected_by: Detector::Pattern,
                evidence: truncate(m, EVIDENCE_MAX),
            });
        }
        if let Some(m) = cx.patterns.auth_match(t) {
            return Some(Failure::AuthExpired {
                detail: truncate(m, DETAIL_MAX),
                detected_by: Detector::Pattern,
            });
        }
        if cx.patterns.overloaded_match(t).is_some() {
            return Some(Failure::Overloaded {
                detail: truncate(t, DETAIL_MAX),
            });
        }
        return Some(Failure::WorkerError {
            subtype: f.subtype.clone(),
            detail: truncate(t, EVIDENCE_MAX),
        });
    }

    if cx.deadline_hit {
        return Some(Failure::Timeout { after_s: 0 });
    }
    let tail = cx
        .state
        .stderr_tail
        .iter()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(m) = cx.patterns.rate_limit_match(&tail) {
        return Some(Failure::RateLimited {
            resets_at: None,
            scope: LimitScope::Unknown,
            detected_by: Detector::Pattern,
            evidence: truncate(m, EVIDENCE_MAX),
        });
    }
    if let Some(m) = cx.patterns.auth_match(&tail) {
        return Some(Failure::AuthExpired {
            detail: truncate(m, DETAIL_MAX),
            detected_by: Detector::Pattern,
        });
    }
    match cx.exit {
        Some(ExitInfo {
            signal: Some(s), ..
        }) => Some(Failure::Crashed { signal: Some(s) }),
        Some(ExitInfo {
            code: Some(127), ..
        }) => Some(Failure::AuthExpired {
            detail: "exec not found (127)".into(),
            detected_by: Detector::ExitCode,
        }),
        Some(ExitInfo { code: Some(0), .. }) => Some(Failure::Truncated { offset: 0 }),
        _ => Some(Failure::Crashed { signal: None }),
    }
}

/// The one provider marker DESIGN 5.6 places in this layer: our own budget guard tripped,
/// and failing over would only spend a second subscription on the same overrun.
const BUDGET_SUBTYPE: &str = "error_max_budget_usd";

/// Byte-bounded and char-boundary safe: evidence is shown to humans, never re-parsed.
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}
