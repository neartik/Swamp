use crate::config::Config;
use crate::dispatch::account::{Account, AccountState, Health};
use crate::model::core::LimitReached;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub const DEFAULT_QUOTA_WARN: f64 = 0.90;
pub const DEFAULT_QUOTA_STOP: f64 = 0.98;
pub const DEFAULT_NEAR_EXHAUSTION_PENALTY: f64 = 2.0;
pub const DEFAULT_QUOTA_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SelectionPolicy {
    RoundRobin,
    LeastLoaded,
    /// Balances measured utilization, live load and token share, so a pool with no provider
    /// telemetry at all still rotates.
    #[default]
    QuotaAware,
}

/// `[dispatch.weights]`. Every term is in `[0, 1]`, so these are directly comparable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Weights {
    pub util: f64,
    pub load: f64,
    pub share: f64,
    pub weight: f64,
    pub idle: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            util: 0.50,
            load: 0.30,
            share: 0.15,
            weight: 0.05,
            idle: 0.02,
        }
    }
}

/// Everything selection needs out of the config, resolved once per pool.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scoring {
    pub weights: Weights,
    pub penalty: f64,
    pub warn_at: f64,
    pub stop_at: f64,
}

impl Default for Scoring {
    fn default() -> Self {
        Self {
            weights: Weights::default(),
            penalty: DEFAULT_NEAR_EXHAUSTION_PENALTY,
            warn_at: DEFAULT_QUOTA_WARN,
            stop_at: DEFAULT_QUOTA_STOP,
        }
    }
}

impl Scoring {
    pub fn from_config(cfg: &Config) -> Self {
        let d = Scoring::default();
        let w = cfg.dispatch.weights.unwrap_or_default();
        Self {
            weights: Weights {
                util: w.util.unwrap_or(d.weights.util),
                load: w.load.unwrap_or(d.weights.load),
                share: w.share.unwrap_or(d.weights.share),
                weight: w.weight.unwrap_or(d.weights.weight),
                idle: w.idle.unwrap_or(d.weights.idle),
            },
            penalty: cfg.dispatch.near_exhaustion_penalty.unwrap_or(d.penalty),
            warn_at: cfg.cooldown.quota_warn_at.unwrap_or(d.warn_at),
            stop_at: cfg.cooldown.quota_stop_at.unwrap_or(d.stop_at),
        }
    }

    /// Raw utilization below the knee, and a steep climb above it: at `stop_at` the penalty
    /// adds a full `penalty` to a term no other term can outweigh.
    pub fn penalised(&self, util: f64) -> f64 {
        if util < self.warn_at {
            return util;
        }
        let span = (self.stop_at - self.warn_at).max(f64::EPSILON);
        util + self.penalty * (util - self.warn_at) / span
    }
}

fn saturation(a: &Account, s: &AccountState) -> f64 {
    match a.max_concurrency {
        Some(c) if c > 0 => s.inflight as f64 / c as f64,
        _ => 0.0,
    }
}

/// Rises with every live node and never reaches 1, so an uncapped account cannot soak up a
/// whole batch while an idle capped one still starts level with it.
fn crowding(s: &AccountState) -> f64 {
    s.inflight as f64 / (s.inflight as f64 + 1.0)
}

pub fn load(a: &Account, s: &AccountState) -> f64 {
    saturation(a, s).max(crowding(s))
}

/// The measured quantities every policy scores from, read once so `score` and `explain`
/// can never drift apart.
struct Inputs {
    util: f64,
    load: f64,
    share: f64,
    idle: f64,
}

fn inputs(a: &Account, s: &AccountState, pool_window: u64, now: OffsetDateTime) -> Inputs {
    Inputs {
        util: s
            .quota
            .as_ref()
            .map_or(0.0, |q| q.worst_utilization_at(now)),
        load: load(a, s),
        share: s.window_tokens.billable() as f64 / pool_window.max(1) as f64,
        idle: s.last_used.map_or(1.0, |t| {
            ((now - t).as_seconds_f64() / 3600.0).clamp(0.0, 1.0)
        }),
    }
}

/// Lower is better. None means ineligible right now; every `None` here is a hard gate.
pub fn score(
    policy: SelectionPolicy,
    a: &Account,
    s: &AccountState,
    pool_window: u64,
    cfg: &Scoring,
    now: OffsetDateTime,
) -> Option<f64> {
    if matches!(s.health, Health::Disabled | Health::AuthBroken) {
        return None;
    }
    // Two independent gates, never one nested inside the other: `swamp accounts enable`
    // rewrites health without touching the timer, and a live cooldown still means wait.
    if s.cooldown_until.is_some_and(|t| t > now) {
        return None;
    }
    // The provider's own authoritative gate: a client must not infer recovery from
    // percentages or reset times, so this outranks both.
    if s.quota.as_ref().and_then(|q| q.ordinary_usage_allowed) == Some(false) {
        return None;
    }
    // Depleted credits or a spend control is not a timer: a human has to act.
    if matches!(
        s.quota.as_ref().and_then(|q| q.reached),
        Some(LimitReached::CreditsDepleted | LimitReached::SpendControl)
    ) {
        return None;
    }
    if let Some(c) = a.max_concurrency
        && s.inflight >= c
    {
        return None;
    }
    // Proactive, before any provider error, and on MEASURED windows only: an estimate is a
    // guess and must never park a working subscription.
    // A window whose reset has passed measures an allowance that has already rolled: gating
    // on it strands the account forever, because only a node running on it can refresh it.
    if s.quota
        .as_ref()
        .and_then(|q| q.measured_utilization_at(now))
        .is_some_and(|u| u >= cfg.stop_at)
    {
        return None;
    }

    Some(
        terms(policy, a, s, pool_window, cfg, now)
            .iter()
            .map(|t| t.contribution)
            .sum(),
    )
}

/// One term of a score, the way the dispatch reason prints it. `weight` is `Some` when the
/// term reads `value` times `weight`, `None` when only its signed contribution is meaningful.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Term {
    pub name: &'static str,
    pub value: f64,
    pub weight: Option<f64>,
    /// What the term added to the score, sign included.
    pub contribution: f64,
}

/// The terms behind a score, in the order the reason prints them. The eligibility gates are
/// deliberately not applied: a caller re-scoring a rejected account still wants its numbers.
pub fn terms(
    policy: SelectionPolicy,
    a: &Account,
    s: &AccountState,
    pool_window: u64,
    cfg: &Scoring,
    now: OffsetDateTime,
) -> Vec<Term> {
    let i = inputs(a, s, pool_window, now);
    let w = &cfg.weights;
    let weight = a.weight as f64;
    match policy {
        SelectionPolicy::RoundRobin => vec![Term {
            name: "idle",
            value: i.idle,
            weight: None,
            contribution: -i.idle,
        }],
        SelectionPolicy::LeastLoaded => vec![
            Term {
                name: "load",
                value: i.load,
                weight: None,
                contribution: i.load,
            },
            Term {
                name: "weight",
                value: weight,
                weight: None,
                contribution: -0.01 * weight,
            },
        ],
        SelectionPolicy::QuotaAware => vec![
            // The value shown is the raw utilization every other surface shows; the knee at
            // `warn_at` lives in the contribution.
            Term {
                name: "util",
                value: i.util,
                weight: Some(w.util),
                contribution: w.util * cfg.penalised(i.util),
            },
            Term {
                name: "load",
                value: i.load,
                weight: Some(w.load),
                contribution: w.load * i.load,
            },
            Term {
                name: "share",
                value: i.share,
                weight: Some(w.share),
                contribution: w.share * i.share,
            },
            Term {
                name: "weight",
                value: weight - 1.0,
                weight: None,
                contribution: -w.weight * (weight - 1.0),
            },
            Term {
                name: "idle",
                value: i.idle,
                weight: None,
                contribution: -w.idle * i.idle,
            },
        ],
    }
}

/// `.41`, `1.68`, `-.07`: two decimals with the leading zero dropped, the form the board
/// footer uses.
fn num(v: f64) -> String {
    let s = format!("{:.2}", v.abs());
    let mag = s.strip_prefix('0').unwrap_or(&s);
    if v < 0.0 && mag != ".00" {
        format!("-{mag}")
    } else {
        mag.to_string()
    }
}

fn term_text(t: &Term) -> String {
    match t.weight {
        Some(w) => format!("{} {}\u{d7}{}", t.name, num(t.value), num(w)),
        None => format!("{} {}", t.name, num(t.contribution.abs())),
    }
}

/// The term the runner-up gave the most away on.
fn lost_on(won: &[Term], lost: &[Term]) -> Option<&'static str> {
    won.iter()
        .zip(lost)
        .map(|(w, l)| (l.contribution - w.contribution, w.name))
        .filter(|(d, _)| *d > TIE)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, name)| name)
}

/// Two scores this close are a tie: they are small rationals, so this only absorbs
/// floating-point noise.
const TIE: f64 = 1e-9;

/// A total order over eligible accounts. A tied score is settled by the tokens the account
/// has spent over its life and then by a rotating cursor, so two idle accounts alternate
/// instead of the first name always winning. Cost left this chain: it reads 0.0 for every
/// OpenAI account, which silently ranked them all equal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rank {
    pub score: f64,
    pub lifetime_billable: u64,
    pub rotation: usize,
}

impl Rank {
    /// Lower is better, like `score`.
    pub fn compare(&self, other: &Rank) -> std::cmp::Ordering {
        if (self.score - other.score).abs() > TIE {
            return self.score.total_cmp(&other.score);
        }
        self.lifetime_billable
            .cmp(&other.lifetime_billable)
            .then_with(|| self.rotation.cmp(&other.rotation))
    }
}

/// `score` plus its tie-breakers. `rotation` is the caller's position in a list rotated by a
/// per-pool cursor, which is what makes sequential runs alternate.
pub fn rank(
    policy: SelectionPolicy,
    a: &Account,
    s: &AccountState,
    pool_window: u64,
    cfg: &Scoring,
    now: OffsetDateTime,
    rotation: usize,
) -> Option<Rank> {
    Some(Rank {
        score: score(policy, a, s, pool_window, cfg, now)?,
        lifetime_billable: s.lifetime_tokens.billable(),
        rotation,
    })
}

/// The one line `JournalEvent::AccountSelected.reason` carries: the score, the terms that
/// make it up, and the runner-up with the term it lost on. Recorded at dispatch, so the
/// board shows the numbers as they were then rather than a re-scored guess.
pub fn explain(
    policy: SelectionPolicy,
    winner: (&Account, &AccountState),
    runner_up: Option<(&Account, &AccountState)>,
    pool_window: u64,
    cfg: &Scoring,
    now: OffsetDateTime,
) -> String {
    let won = terms(policy, winner.0, winner.1, pool_window, cfg, now);
    let total: f64 = won.iter().map(|t| t.contribution).sum();
    let mut out = format!("score {}", num(total));
    for (i, t) in won.iter().enumerate() {
        out.push_str(if i == 0 { " = " } else { " " });
        // Sign, not magnitude: a term the score subtracts stays subtracted at zero.
        if t.contribution.is_sign_negative() {
            out.push_str("\u{2212} ");
        } else if i > 0 {
            out.push_str("+ ");
        }
        out.push_str(&term_text(t));
    }
    let Some((other, state)) = runner_up else {
        return out;
    };
    let lost = terms(policy, other, state, pool_window, cfg, now);
    let theirs: f64 = lost.iter().map(|t| t.contribution).sum();
    let who = &other.id.0;
    if (theirs - total).abs() <= TIE {
        out.push_str(&format!(
            "; {who} tied at {} and lost on the tie-break",
            num(theirs)
        ));
        return out;
    }
    out.push_str(&format!("; {who} scored {}", num(theirs)));
    if let Some(name) = lost_on(&won, &lost) {
        out.push_str(&format!(" and lost on {name}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::core::{
        AccountId, LimitScope, LimitStatus, LimitWindow, Provider, RateLimitSnapshot,
    };
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn account(id: &str, max_concurrency: Option<usize>) -> Account {
        Account {
            id: AccountId(id.into()),
            provider: Provider::Anthropic,
            exec: format!("claude-{id}"),
            env: BTreeMap::new(),
            weight: 1,
            max_concurrency,
        }
    }

    fn util(v: f64) -> RateLimitSnapshot {
        RateLimitSnapshot {
            status: LimitStatus::Allowed,
            windows: vec![LimitWindow {
                scope: LimitScope::SevenDay,
                utilization: v,
                ..Default::default()
            }],
            resets_at: None,
            ..Default::default()
        }
    }

    fn scoring() -> Scoring {
        Scoring::default()
    }

    #[test]
    fn a_saturated_or_parked_account_is_ineligible() {
        let now = OffsetDateTime::now_utc();
        let a = account("main", Some(2));
        let cfg = scoring();
        let mut s = AccountState {
            inflight: 2,
            ..Default::default()
        };
        assert_eq!(
            score(SelectionPolicy::LeastLoaded, &a, &s, 0, &cfg, now),
            None
        );

        s.inflight = 0;
        for health in [Health::Disabled, Health::AuthBroken] {
            s.health = health;
            assert_eq!(
                score(SelectionPolicy::LeastLoaded, &a, &s, 0, &cfg, now),
                None
            );
        }

        s.health = Health::Cooling;
        s.cooldown_until = Some(now + Duration::from_secs(60));
        assert_eq!(
            score(SelectionPolicy::LeastLoaded, &a, &s, 0, &cfg, now),
            None
        );

        // An expired cooldown is usable again without anyone clearing the flag.
        s.cooldown_until = Some(now - Duration::from_secs(60));
        assert!(score(SelectionPolicy::LeastLoaded, &a, &s, 0, &cfg, now).is_some());
    }

    /// An account with no ceiling is bounded by headroom, never by a number.
    #[test]
    fn an_uncapped_account_is_never_saturated() {
        let now = OffsetDateTime::now_utc();
        let a = account("main", None);
        let s = AccountState {
            inflight: 99,
            ..Default::default()
        };
        let got =
            score(SelectionPolicy::LeastLoaded, &a, &s, 0, &scoring(), now).expect("eligible");
        assert!(got < 1.0, "crowding must stay under 1: {got}");
    }

    #[test]
    fn quota_stop_makes_an_account_ineligible_before_any_error() {
        let now = OffsetDateTime::now_utc();
        let a = account("main", Some(2));
        let s = AccountState {
            quota: Some(util(0.99)),
            ..Default::default()
        };
        let mut cfg = scoring();
        assert_eq!(
            score(SelectionPolicy::LeastLoaded, &a, &s, 0, &cfg, now),
            None
        );
        cfg.stop_at = 1.0;
        assert!(score(SelectionPolicy::LeastLoaded, &a, &s, 0, &cfg, now).is_some());
    }

    #[test]
    fn quota_aware_degrades_to_least_loaded_without_telemetry() {
        let now = OffsetDateTime::now_utc();
        let (a, b) = (account("a", Some(2)), account("b", Some(2)));
        let cfg = scoring();
        let idle = AccountState::default();
        let busy = AccountState {
            inflight: 1,
            ..Default::default()
        };
        let sa = score(SelectionPolicy::QuotaAware, &a, &idle, 0, &cfg, now).unwrap();
        let sb = score(SelectionPolicy::QuotaAware, &b, &busy, 0, &cfg, now).unwrap();
        assert!(sa < sb, "{sa} < {sb}");
    }

    /// Two idle accounts scored identically, so selection fell back to the map order and
    /// every sequential run went to the same one.
    #[test]
    fn a_tie_goes_to_the_account_with_fewer_lifetime_tokens() {
        let now = OffsetDateTime::now_utc();
        let (a, b) = (account("alt", Some(2)), account("main", Some(2)));
        let cfg = scoring();
        let used = AccountState {
            lifetime_tokens: crate::model::core::Usage {
                input_tokens: 5_000,
                ..Default::default()
            },
            ..Default::default()
        };
        let fresh = AccountState::default();
        let ra = rank(SelectionPolicy::LeastLoaded, &a, &used, 0, &cfg, now, 0).unwrap();
        let rb = rank(SelectionPolicy::LeastLoaded, &b, &fresh, 0, &cfg, now, 1).unwrap();
        assert_eq!(ra.compare(&rb), std::cmp::Ordering::Greater);
        assert_eq!(rb.compare(&ra), std::cmp::Ordering::Less);
    }

    /// Equal on every counter: the rotating cursor decides, and it is deterministic.
    #[test]
    fn a_full_tie_falls_back_to_the_rotating_cursor() {
        let now = OffsetDateTime::now_utc();
        let (a, b) = (account("alt", Some(2)), account("main", Some(2)));
        let cfg = scoring();
        let idle = AccountState::default();
        let first = rank(SelectionPolicy::LeastLoaded, &a, &idle, 0, &cfg, now, 1).unwrap();
        let second = rank(SelectionPolicy::LeastLoaded, &b, &idle, 0, &cfg, now, 0).unwrap();
        assert_eq!(first.compare(&second), std::cmp::Ordering::Greater);
    }

    /// Load still outranks lifetime history: a busy account is never preferred.
    #[test]
    fn lifetime_history_never_outweighs_current_load() {
        let now = OffsetDateTime::now_utc();
        let (a, b) = (account("alt", Some(2)), account("main", Some(2)));
        let cfg = scoring();
        let idle_but_used = AccountState {
            lifetime_tokens: crate::model::core::Usage {
                input_tokens: 99_000,
                ..Default::default()
            },
            ..Default::default()
        };
        let busy_and_fresh = AccountState {
            inflight: 1,
            ..Default::default()
        };
        let ra = rank(
            SelectionPolicy::LeastLoaded,
            &a,
            &idle_but_used,
            0,
            &cfg,
            now,
            0,
        )
        .unwrap();
        let rb = rank(
            SelectionPolicy::LeastLoaded,
            &b,
            &busy_and_fresh,
            0,
            &cfg,
            now,
            1,
        )
        .unwrap();
        assert_eq!(ra.compare(&rb), std::cmp::Ordering::Less);
    }

    /// BOARD 3.4: the recorded reason is the footer, term by term, and it still opens with
    /// the word every older reader matched on.
    #[test]
    fn explain_prints_every_term_and_the_runner_up() {
        let now = OffsetDateTime::now_utc();
        let cfg = scoring();
        let (a, b) = (account("alt", Some(4)), account("main", Some(4)));
        let cool = AccountState {
            inflight: 1,
            quota: Some(util(0.40)),
            last_used: Some(now - Duration::from_secs(7200)),
            ..Default::default()
        };
        let hot = AccountState {
            quota: Some(util(0.80)),
            last_used: Some(now - Duration::from_secs(7200)),
            ..cool.clone()
        };
        let got = explain(
            SelectionPolicy::QuotaAware,
            (&a, &cool),
            Some((&b, &hot)),
            0,
            &cfg,
            now,
        );
        assert_eq!(
            got,
            "score .33 = util .40\u{d7}.50 + load .50\u{d7}.30 + share .00\u{d7}.15 \
             \u{2212} weight .00 \u{2212} idle .02; main scored .53 and lost on util"
        );
        assert!(got.starts_with("score "), "{got}");
    }

    /// A pool of one has no runner-up, and a policy with fewer terms prints fewer.
    #[test]
    fn explain_drops_what_it_has_no_number_for() {
        let now = OffsetDateTime::now_utc();
        let cfg = scoring();
        let a = account("main", Some(4));
        let s = AccountState {
            inflight: 1,
            last_used: Some(now - Duration::from_secs(7200)),
            ..Default::default()
        };
        assert_eq!(
            explain(SelectionPolicy::LeastLoaded, (&a, &s), None, 0, &cfg, now),
            "score .49 = load .50 \u{2212} weight .01"
        );
        assert_eq!(
            explain(SelectionPolicy::RoundRobin, (&a, &s), None, 0, &cfg, now),
            "score -1.00 = \u{2212} idle 1.00"
        );
    }

    /// Two accounts equal on every term: the reason has to say the tie-break decided, not
    /// invent a term one of them lost on.
    #[test]
    fn explain_names_the_tie_break_when_no_term_separates_them() {
        let now = OffsetDateTime::now_utc();
        let cfg = scoring();
        let (a, b) = (account("alt", Some(2)), account("main", Some(2)));
        let idle = AccountState::default();
        let got = explain(
            SelectionPolicy::QuotaAware,
            (&a, &idle),
            Some((&b, &idle)),
            0,
            &cfg,
            now,
        );
        assert!(
            got.ends_with("main tied at -.02 and lost on the tie-break"),
            "{got}"
        );
    }

    /// The board parses the recorded reason back out of the journal, so every string
    /// `explain` can emit has to classify as the term form, minus sign and all.
    #[test]
    fn every_explanation_parses_back_as_the_term_form() {
        use crate::ui::board::model::{Reason, ReasonForm};

        let now = OffsetDateTime::now_utc();
        let cfg = scoring();
        let (a, b) = (account("alt", Some(4)), account("main", Some(4)));
        let s = AccountState {
            inflight: 1,
            quota: Some(util(0.40)),
            last_used: Some(now - Duration::from_secs(7200)),
            ..Default::default()
        };
        let cases = [
            explain(
                SelectionPolicy::QuotaAware,
                (&a, &s),
                Some((&b, &s)),
                0,
                &cfg,
                now,
            ),
            explain(SelectionPolicy::LeastLoaded, (&a, &s), None, 0, &cfg, now),
            explain(SelectionPolicy::RoundRobin, (&a, &s), None, 0, &cfg, now),
        ];
        for raw in cases {
            let r = Reason::parse(&raw);
            assert_eq!(r.form, ReasonForm::Terms, "{raw}");
            assert!(r.score.is_some(), "{raw}");
            // sanitize must leave the × and − the footer is built from alone.
            assert_eq!(r.text, raw);
        }
    }

    #[test]
    fn the_penalty_knee_starts_at_the_warning_threshold() {
        let cfg = scoring();
        assert!((cfg.penalised(0.50) - 0.50).abs() < 1e-9);
        assert!((cfg.penalised(0.90) - 0.90).abs() < 1e-9);
        assert!((cfg.penalised(0.93) - 1.68).abs() < 1e-9);
        assert!((cfg.penalised(0.98) - 2.98).abs() < 1e-9);
    }
}
