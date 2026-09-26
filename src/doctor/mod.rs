//! `swamp doctor`: environment, accounts, permissions, workspace and schema checks.

mod accounts;
mod env;
mod permissions;
mod reap;
mod workspace;

pub use accounts::probe_account;
pub use permissions::permission_checks;
pub use reap::{Reaped, reap};

use crate::config::Config;
use crate::journal::paths::Paths;
use crate::journal::record::{JournalEvent, JournalLine};
use crate::model::failure::{Detector, Failure};

/// Above this share of unparsed stream lines the adapters have drifted from the CLIs.
const UNPARSED_MAX: f64 = 0.02;
/// Above this share of pattern-matched classifications the structured signals are gone.
const PATTERN_MAX: f64 = 0.25;
const RECENT_RUNS: usize = 20;

pub struct Check {
    pub name: String,
    pub level: Level,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Note,
    Warn,
    Error,
}

impl Level {
    pub fn label(&self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Note => "note",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }
}

impl Check {
    pub(crate) fn new(name: impl Into<String>, level: Level, detail: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            level,
            detail: detail.into(),
        }
    }
}

pub async fn checks(cfg: &Config, paths: &Paths, probe: bool, schema: bool) -> Vec<Check> {
    let mut out = Vec::new();
    env::environment(paths, &mut out).await;
    accounts::accounts(cfg, paths, probe, &mut out).await;
    accounts::quota(cfg, paths, &mut out);
    accounts::tiers(cfg, &mut out);
    permissions::unsafe_args(cfg, &mut out);
    permissions::permission_modes(cfg, &mut out);
    workspace::workspace(cfg, paths, &mut out);
    env::config_sources(cfg, &mut out);
    if schema {
        out.push(schema_drift(paths));
    }
    out
}

/// `--schema`: how often the classifier fell back to regexes, and how many raw lines the
/// adapters could not read. Both are the early warning that a vendor changed wording.
fn schema_drift(paths: &Paths) -> Check {
    let mut events = 0u64;
    let mut unparsed = 0u64;
    let mut detections = 0u64;
    let mut pattern = 0u64;
    let runs = paths.list_runs().unwrap_or_default();
    for run in runs.into_iter().take(RECENT_RUNS) {
        let journal = paths.run_paths(run).journal();
        let Ok(text) = std::fs::read_to_string(&journal) else {
            continue;
        };
        for line in text.lines() {
            let Ok(l) = serde_json::from_str::<JournalLine>(line) else {
                continue;
            };
            match &l.event {
                JournalEvent::NodeEvent { .. } => events += 1,
                JournalEvent::NodeFinished {
                    state,
                    unparsed_lines,
                    ..
                } => {
                    unparsed += u64::from(*unparsed_lines);
                    if let crate::model::core::NodeState::Failed { failure } = state {
                        count_detector(failure, &mut detections, &mut pattern);
                    }
                }
                JournalEvent::NodeRetry { reason, .. } => {
                    count_detector(reason, &mut detections, &mut pattern)
                }
                _ => {}
            }
        }
    }

    let total = events + unparsed;
    if total == 0 {
        return Check::new(
            "protocol/schema",
            Level::Note,
            "no recorded runs to measure adapter drift against",
        );
    }
    let unparsed_ratio = unparsed as f64 / total as f64;
    let pattern_ratio = if detections == 0 {
        0.0
    } else {
        pattern as f64 / detections as f64
    };
    let detail = format!(
        "{unparsed} of {total} stream lines unparsed ({:.1}%), {pattern} of {detections} \
         classifications used the regex fallback ({:.1}%)",
        unparsed_ratio * 100.0,
        pattern_ratio * 100.0
    );
    if unparsed_ratio > UNPARSED_MAX || pattern_ratio > PATTERN_MAX {
        return Check::new(
            "protocol/schema",
            Level::Error,
            format!(
                "{detail}: the adapters have drifted from the CLIs. Re-record docs/ref fixtures \
                 and fix the adapter, then `swamp replay --reparse` the affected runs"
            ),
        );
    }
    Check::new("protocol/schema", Level::Ok, detail)
}

fn count_detector(f: &Failure, detections: &mut u64, pattern: &mut u64) {
    let detected_by = match f {
        Failure::RateLimited { detected_by, .. } | Failure::AuthExpired { detected_by, .. } => {
            Some(detected_by)
        }
        _ => None,
    };
    if let Some(d) = detected_by {
        *detections += 1;
        if *d == Detector::Pattern {
            *pattern += 1;
        }
    }
}
