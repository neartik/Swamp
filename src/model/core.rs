use crate::model::failure::Failure;
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use time::OffsetDateTime;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Anthropic,
    Openai,
}

/// Ordered so `Tier::High > Tier::Low` works for policy checks.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Low,
    Mid,
    High,
}

impl Provider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Openai => "openai",
        }
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Provider {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "anthropic" => Ok(Self::Anthropic),
            "openai" => Ok(Self::Openai),
            other => anyhow::bail!("unknown provider `{other}`, expected anthropic or openai"),
        }
    }
}

impl Tier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Mid => "mid",
            Self::High => "high",
        }
    }
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Tier {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Ok(Self::Low),
            "mid" => Ok(Self::Mid),
            "high" => Ok(Self::High),
            other => anyhow::bail!("unknown tier `{other}`, expected low, mid or high"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Root,
    Brain,
    Worker,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum NodeState {
    Queued,
    /// Every eligible account is cooling; the scheduler sleeps until `until`.
    Blocked {
        #[serde(with = "time::serde::rfc3339")]
        until: OffsetDateTime,
        why: String,
    },
    Leased {
        account: AccountId,
    },
    Running {
        pid: i32,
        pgid: i32,
        #[serde(with = "time::serde::rfc3339")]
        since: OffsetDateTime,
    },
    /// Process outlived a supervisor crash, or vice versa. Recovery decides adopt vs resume.
    Orphaned {
        pid: i32,
        stream_offset: u64,
    },
    Succeeded,
    Failed {
        failure: Failure,
    },
    Cancelled {
        by: CancelSource,
    },
}

impl NodeState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed { .. } | Self::Cancelled { .. }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelSource {
    User,
    Brain,
    Timeout,
    Shutdown,
}

/// A resume handle. ALWAYS carried with the account that minted it: a claude session id
/// created under CLAUDE_CONFIG_DIR=A does not exist under B. If this invariant breaks,
/// resume silently starts a fresh conversation while Swamp believes it has context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHandle {
    pub account: AccountId,
    pub id: String,
    pub preassigned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceRef {
    /// Fresh `git worktree` on a `swamp/<run>/<node>` branch. Default for anything that writes.
    Worktree {
        path: Utf8PathBuf,
        branch: String,
        base: String,
    },
    /// The user's real checkout, serialized behind a repo-wide write mutex. Opt-in.
    Shared { path: Utf8PathBuf },
    /// The checkout with provider-level write denial. Used for the brain and review nodes.
    ReadOnly { path: Utf8PathBuf },
}

impl WorkspaceRef {
    pub fn path(&self) -> &camino::Utf8Path {
        match self {
            Self::Worktree { path, .. } | Self::Shared { path } | Self::ReadOnly { path } => path,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
}

impl Usage {
    pub fn absorb(&mut self, o: &Usage) {
        self.input_tokens += o.input_tokens;
        self.cached_input_tokens += o.cached_input_tokens;
        self.cache_write_tokens += o.cache_write_tokens;
        self.output_tokens += o.output_tokens;
        self.reasoning_tokens += o.reasoning_tokens;
    }
    /// Per-field max. Two processes count the same account independently and the shared
    /// file has to keep the higher number, never a stale one.
    pub fn take_max(&mut self, o: &Usage) {
        self.input_tokens = self.input_tokens.max(o.input_tokens);
        self.cached_input_tokens = self.cached_input_tokens.max(o.cached_input_tokens);
        self.cache_write_tokens = self.cache_write_tokens.max(o.cache_write_tokens);
        self.output_tokens = self.output_tokens.max(o.output_tokens);
        self.reasoning_tokens = self.reasoning_tokens.max(o.reasoning_tokens);
    }
    pub fn billable(&self) -> u64 {
        self.input_tokens + self.cache_write_tokens + self.output_tokens
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    pub usd: f64,
    pub basis: CostBasis,
}

/// `Reported` = the CLI told us (claude `total_cost_usd`). Note that on a subscription this is
/// list-price equivalence, not money billed. `Estimated` = we multiplied tokens by the
/// `[pricing]` table because the CLI reports none. Absent cost renders as `-`, never `$0.00`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostBasis {
    Reported,
    Estimated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Add,
    Modify,
    Delete,
    Rename,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: Utf8PathBuf,
    pub kind: ChangeKind,
    #[serde(default)]
    pub added: u32,
    #[serde(default)]
    pub removed: u32,
    /// Git is authoritative. EventStream is a live-progress estimate and may be wrong.
    pub source: EvidenceSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    Git,
    EventStream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitScope {
    FiveHour,
    SevenDay,
    Minute,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitStatus {
    Allowed,
    Warning,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitReached {
    /// codex `rate_limit_reached`. Cool the account, it comes back.
    RateLimit,
    /// Credits are gone: a human must act, so there is no timer.
    CreditsDepleted,
    /// codex `spend_control_reached`. Handled like CreditsDepleted.
    SpendControl,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LimitWindow {
    pub scope: LimitScope,
    /// 0.0 ..= 1.0, ALWAYS. codex reports 0..100 and is divided at the boundary.
    pub utilization: f64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub resets_at: Option<OffsetDateTime>,
    /// The provider's own window length. 300 -> five_hour, 10080 -> seven_day. Kept so a
    /// window Swamp has no scope for is still labelled honestly in the UI.
    #[serde(default)]
    pub window_minutes: Option<u32>,
    /// False when this number came from Swamp's own counters, not the provider.
    #[serde(default = "measured_default")]
    pub measured: bool,
}

/// Every window Swamp stored before estimates existed came from a provider.
fn measured_default() -> bool {
    true
}

impl LimitWindow {
    /// A window with no reset time never expires; one whose reset has passed has rolled.
    pub fn is_current(&self, now: OffsetDateTime) -> bool {
        self.resets_at.is_none_or(|t| t > now)
    }

    /// Nominal span in minutes, for ordering windows by length.
    pub fn minutes(&self) -> u32 {
        self.window_minutes.unwrap_or(match self.scope {
            LimitScope::Minute => 1,
            LimitScope::FiveHour => 300,
            LimitScope::SevenDay => 10080,
            LimitScope::Unknown => 0,
        })
    }
}

impl Default for LimitWindow {
    fn default() -> Self {
        Self {
            scope: LimitScope::Unknown,
            utilization: 0.0,
            resets_at: None,
            window_minutes: None,
            measured: true,
        }
    }
}

/// The real sample carries five_hour = 0.06 AND seven_day = 0.64 in the same event.
/// Collapsing to one window throws away the one that is actually near exhaustion,
/// so every window is kept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitSnapshot {
    pub status: LimitStatus,
    pub windows: Vec<LimitWindow>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub resets_at: Option<OffsetDateTime>,
    /// codex `limit_id` / claude `rateLimitType`. Names which bucket this is.
    #[serde(default)]
    pub limit_id: Option<String>,
    /// codex `ordinaryUsageAllowed`. The authoritative gate: a client must NOT infer
    /// recovery from percentages or reset times, so `Some(false)` wins over both.
    #[serde(default)]
    pub ordinary_usage_allowed: Option<bool>,
    /// Why the limit was reached, when the provider says. Decides cooldown vs park.
    #[serde(default)]
    pub reached: Option<LimitReached>,
    /// Plan name, display only.
    #[serde(default)]
    pub plan: Option<String>,
}

impl Default for RateLimitSnapshot {
    fn default() -> Self {
        Self {
            status: LimitStatus::Allowed,
            windows: Vec::new(),
            resets_at: None,
            limit_id: None,
            ordinary_usage_allowed: None,
            reached: None,
            plan: None,
        }
    }
}

impl RateLimitSnapshot {
    /// An `Unknown` window is an overage-inclusive or unmapped bucket: it is never a plan
    /// limit, so it is skipped as soon as one named window exists.
    pub fn named(&self) -> impl Iterator<Item = &LimitWindow> {
        let any_named = self.windows.iter().any(|w| w.scope != LimitScope::Unknown);
        self.windows
            .iter()
            .filter(move |w| !any_named || w.scope != LimitScope::Unknown)
    }
    pub fn worst_utilization(&self) -> f64 {
        self.named().map(|w| w.utilization).fold(0.0, f64::max)
    }
    /// None when every window is an estimate: only a measured number may park an account.
    pub fn measured_utilization(&self) -> Option<f64> {
        self.named()
            .filter(|w| w.measured)
            .map(|w| w.utilization)
            .fold(None, |acc: Option<f64>, u| {
                Some(acc.map_or(u, |a| a.max(u)))
            })
    }
    /// Windows that still describe the present. A window whose `resets_at` has passed measures
    /// an allowance that has already rolled, and for Anthropic nothing can refresh it until a
    /// node runs on that account, so a stale number must never keep gating dispatch.
    pub fn current(&self, now: OffsetDateTime) -> impl Iterator<Item = &LimitWindow> {
        self.named().filter(move |w| w.is_current(now))
    }
    pub fn worst_utilization_at(&self, now: OffsetDateTime) -> f64 {
        self.current(now).map(|w| w.utilization).fold(0.0, f64::max)
    }
    /// `measured_utilization`, ignoring windows that have already rolled.
    pub fn measured_utilization_at(&self, now: OffsetDateTime) -> Option<f64> {
        self.current(now)
            .filter(|w| w.measured)
            .map(|w| w.utilization)
            .fold(None, |acc: Option<f64>, u| {
                Some(acc.map_or(u, |a| a.max(u)))
            })
    }
    /// The window §4 scores against: the one closest to exhaustion.
    pub fn tightest(&self) -> Option<&LimitWindow> {
        self.named()
            .max_by(|a, b| a.utilization.total_cmp(&b.utilization))
    }
    /// `tightest`, ignoring windows that have already rolled.
    pub fn tightest_at(&self, now: OffsetDateTime) -> Option<&LimitWindow> {
        self.current(now)
            .max_by(|a, b| a.utilization.total_cmp(&b.utilization))
    }
    pub fn soonest_reset(&self) -> Option<OffsetDateTime> {
        self.named()
            .filter_map(|w| w.resets_at)
            .min()
            .or(self.resets_at)
    }
    /// The earliest reset still ahead of us. A window that has already rolled cannot be
    /// waited for, and taking it would turn a timed wait into a hard failure.
    pub fn soonest_reset_at(&self, now: OffsetDateTime) -> Option<OffsetDateTime> {
        self.current(now)
            .filter_map(|w| w.resets_at)
            .min()
            .or(self.resets_at)
            .filter(|t| *t > now)
    }
    pub fn worst_scope(&self) -> LimitScope {
        self.tightest().map_or(LimitScope::Unknown, |w| w.scope)
    }

    /// Per-scope merge, not replacement: two `rate_limit_event`s of one run carry different
    /// window sets, and a key dropping out of the newer one must not erase what it measured.
    /// An estimate never replaces a measurement that has not rolled yet, and a window whose
    /// reset has passed is dropped rather than carried forward forever.
    pub fn merged_over(&self, prev: &RateLimitSnapshot, now: OffsetDateTime) -> RateLimitSnapshot {
        let mut out = self.clone();
        for old in prev.windows.iter().filter(|w| w.is_current(now)) {
            match out.windows.iter_mut().find(|w| w.scope == old.scope) {
                Some(w) if !w.measured && old.measured => *w = old.clone(),
                Some(_) => {}
                None => out.windows.push(old.clone()),
            }
        }
        out.windows.retain(|w| w.is_current(now));
        out.windows.sort_by_key(|w| w.scope);
        out
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FinalSummary {
    pub ok: bool,
    /// Raw provider terminal marker: claude `result.subtype`, codex `turn.completed|failed`.
    pub subtype: String,
    pub text: Option<String>,
    pub usage: Usage,
    pub cost: Option<Cost>,
    pub api_error_status: Option<i64>,
    #[serde(default)]
    pub num_turns: u32,
    /// Under `--permission-prompts none` anything that would prompt is denied. A worker then
    /// writes a confident summary of work it never did. Non-empty is a failure signal.
    #[serde(default)]
    pub permission_denials: u32,
    /// The tool names behind those denials, so the cause is visible without stream.jsonl.
    #[serde(default)]
    pub denied_tools: Vec<String>,
    /// claude `modelUsage`: per-model totals including side-calls `usage` never reports.
    #[serde(default)]
    pub model_usage: BTreeMap<String, Usage>,
}

impl FinalSummary {
    /// What the ACCOUNT spent. `usage` is the main model only; a haiku side-call is billed
    /// to the same subscription and appears only in `modelUsage`.
    pub fn account_usage(&self) -> Usage {
        if self.model_usage.is_empty() {
            return self.usage;
        }
        let mut out = Usage::default();
        for u in self.model_usage.values() {
            out.absorb(u);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::str::FromStr;

    fn usage(v: [u64; 5]) -> Usage {
        Usage {
            input_tokens: v[0],
            cached_input_tokens: v[1],
            cache_write_tokens: v[2],
            output_tokens: v[3],
            reasoning_tokens: v[4],
        }
    }

    fn absorbed(a: &Usage, b: &Usage) -> Usage {
        let mut out = *a;
        out.absorb(b);
        out
    }

    proptest! {
        #[test]
        fn absorb_is_commutative(a in any::<[u32; 5]>(), b in any::<[u32; 5]>()) {
            let (a, b) = (usage(a.map(u64::from)), usage(b.map(u64::from)));
            prop_assert_eq!(absorbed(&a, &b), absorbed(&b, &a));
        }

        #[test]
        fn absorb_is_associative(
            a in any::<[u32; 5]>(), b in any::<[u32; 5]>(), c in any::<[u32; 5]>()
        ) {
            let (a, b, c) = (usage(a.map(u64::from)), usage(b.map(u64::from)), usage(c.map(u64::from)));
            prop_assert_eq!(
                absorbed(&absorbed(&a, &b), &c),
                absorbed(&a, &absorbed(&b, &c))
            );
        }
    }

    #[test]
    fn billable_excludes_cached_and_reasoning() {
        assert_eq!(usage([10, 5, 3, 7, 11]).billable(), 20);
    }

    #[test]
    fn worst_utilization_keeps_the_near_exhausted_window() {
        let snap = RateLimitSnapshot {
            status: LimitStatus::Allowed,
            windows: vec![
                LimitWindow {
                    scope: LimitScope::FiveHour,
                    utilization: 0.06,
                    resets_at: None,
                    ..Default::default()
                },
                LimitWindow {
                    scope: LimitScope::SevenDay,
                    utilization: 0.64,
                    resets_at: None,
                    ..Default::default()
                },
            ],
            resets_at: None,
            ..Default::default()
        };
        assert_eq!(snap.worst_utilization(), 0.64);
        assert_eq!(snap.worst_scope(), LimitScope::SevenDay);
    }

    #[test]
    fn empty_snapshot_has_no_worst_scope() {
        let snap = RateLimitSnapshot {
            status: LimitStatus::Allowed,
            windows: vec![],
            resets_at: None,
            ..Default::default()
        };
        assert_eq!(snap.worst_utilization(), 0.0);
        assert_eq!(snap.worst_scope(), LimitScope::Unknown);
    }

    #[test]
    fn soonest_reset_prefers_the_earliest_window() {
        let early = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let late = time::OffsetDateTime::from_unix_timestamp(1_700_003_600).unwrap();
        let snap = RateLimitSnapshot {
            status: LimitStatus::Warning,
            windows: vec![
                LimitWindow {
                    scope: LimitScope::SevenDay,
                    utilization: 0.5,
                    resets_at: Some(late),
                    ..Default::default()
                },
                LimitWindow {
                    scope: LimitScope::FiveHour,
                    utilization: 0.1,
                    resets_at: Some(early),
                    ..Default::default()
                },
            ],
            resets_at: Some(late),
            ..Default::default()
        };
        assert_eq!(snap.soonest_reset(), Some(early));
    }

    #[test]
    fn tier_is_ordered_by_capability() {
        assert!(Tier::High > Tier::Low);
        assert!(Tier::Mid > Tier::Low);
    }

    #[test]
    fn provider_and_tier_round_trip_through_strings() {
        for p in [Provider::Anthropic, Provider::Openai] {
            assert_eq!(Provider::from_str(&p.to_string()).unwrap(), p);
            assert_eq!(
                serde_json::to_string(&p).unwrap(),
                format!("\"{}\"", p.as_str())
            );
        }
        for t in [Tier::Low, Tier::Mid, Tier::High] {
            assert_eq!(Tier::from_str(&t.to_string()).unwrap(), t);
            assert_eq!(Tier::from_str(&t.as_str().to_ascii_uppercase()).unwrap(), t);
        }
        assert!(Provider::from_str("mistral").is_err());
        assert!(Tier::from_str("medium").is_err());
    }

    #[test]
    fn workspace_ref_exposes_its_path() {
        let w = WorkspaceRef::Worktree {
            path: "/tmp/wt".into(),
            branch: "swamp/a/b".into(),
            base: "HEAD".into(),
        };
        assert_eq!(w.path(), "/tmp/wt");
        assert_eq!(WorkspaceRef::Shared { path: "/r".into() }.path(), "/r");
        assert_eq!(WorkspaceRef::ReadOnly { path: "/r".into() }.path(), "/r");
    }

    #[test]
    fn node_state_terminality() {
        let terminal = [
            NodeState::Succeeded,
            NodeState::Failed {
                failure: Failure::Timeout { after_s: 1 },
            },
            NodeState::Cancelled {
                by: CancelSource::User,
            },
        ];
        for s in terminal {
            assert!(s.is_terminal(), "{s:?}");
        }
        assert!(!NodeState::Queued.is_terminal());
        assert!(
            !NodeState::Leased {
                account: AccountId("main".into())
            }
            .is_terminal()
        );
    }
}
