//! Quota for OpenAI accounts. `codex exec --json` carries none, so it is read out of band:
//! the rollout file the run appends to (free, live), then the app-server (richer, network).

use crate::model::core::{
    LimitReached, LimitScope, LimitStatus, LimitWindow, RateLimitSnapshot, Usage,
};
use camino::{Utf8Path, Utf8PathBuf};
use parking_lot::Mutex;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::OnceLock;
use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// `codex exec` never waits this long for a local handshake; past it the probe is a timeout,
/// which renders as a stale row and never as an error.
pub const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const ROLLOUT_DEPTH: usize = 4;

/// One `rate_limits` object: the rollout spells it snake_case, the app-server camelCase.
#[derive(Debug, Default, Clone, Copy, Deserialize)]
pub struct RateLimits {
    #[serde(default)]
    pub primary: Option<RateWindow>,
    #[serde(default)]
    pub secondary: Option<RateWindow>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct RateWindow {
    /// 0..100, ALWAYS. Ingesting it raw would put every codex account past `quota_stop_at`.
    #[serde(default, alias = "usedPercent")]
    pub used_percent: f64,
    #[serde(default, alias = "windowMinutes")]
    pub window_minutes: Option<u32>,
    #[serde(default, alias = "resetsInSeconds")]
    pub resets_in_seconds: Option<i64>,
}

/// One quota bucket. `codex_bengalfox` at 0% on a model family this account never runs must
/// not make `codex` at 32% look better, so buckets are kept apart and never maxed together.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Bucket {
    #[serde(default, alias = "limitId")]
    pub limit_id: Option<String>,
    #[serde(default, alias = "limitName")]
    pub limit_name: Option<String>,
    #[serde(default, alias = "ordinaryUsageAllowed")]
    pub ordinary_usage_allowed: Option<bool>,
    #[serde(default, alias = "rateLimitReachedType")]
    pub reached: Option<String>,
    #[serde(default, alias = "planName")]
    pub plan: Option<String>,
    #[serde(flatten)]
    pub limits: RateLimits,
}

/// Every bucket the provider reported, plus which model family each one bills.
#[derive(Debug, Default, Clone)]
pub struct RateLimitsRead {
    pub buckets: BTreeMap<String, RateLimitSnapshot>,
    pub names: BTreeMap<String, String>,
    pub plan: Option<String>,
}

impl RateLimitsRead {
    /// `accounts[].limit_id` when set, else the bucket billing this account's model, else
    /// `codex`, else the first.
    pub fn select(&self, limit_id: Option<&str>, model: Option<&str>) -> Option<RateLimitSnapshot> {
        let by_model = model.and_then(|m| {
            self.names
                .iter()
                .find(|(_, name)| name.as_str() == m)
                .map(|(id, _)| id.clone())
        });
        let id = limit_id
            .map(str::to_owned)
            .filter(|id| self.buckets.contains_key(id))
            .or(by_model)
            .or_else(|| {
                self.buckets
                    .contains_key("codex")
                    .then(|| "codex".to_owned())
            })
            .or_else(|| self.buckets.keys().next().cloned())?;
        self.buckets.get(&id).cloned()
    }
}

/// The last `token_count` of a rollout: the whole thread's tokens and its quota, both live.
#[derive(Debug, Default, Clone)]
pub struct RolloutSample {
    pub quota: Option<RateLimitSnapshot>,
    pub total_tokens: Option<Usage>,
}

/// Scope comes from the window length, NEVER from the field name: `primary` on the `codex`
/// bucket is a 7-day window today, and mapping it to FiveHour by position is simply wrong.
pub fn scope_of(window_minutes: Option<u32>) -> LimitScope {
    match window_minutes {
        Some(m) if m <= 60 => LimitScope::Minute,
        Some(240..=420) => LimitScope::FiveHour,
        Some(9000..=11000) => LimitScope::SevenDay,
        _ => LimitScope::Unknown,
    }
}

fn window_of(w: &RateWindow, now: OffsetDateTime) -> LimitWindow {
    LimitWindow {
        scope: scope_of(w.window_minutes),
        utilization: (w.used_percent / 100.0).clamp(0.0, 1.0),
        resets_at: w
            .resets_in_seconds
            .and_then(|s| time::Duration::checked_seconds_f64(s as f64))
            .map(|d| now + d),
        window_minutes: w.window_minutes,
        measured: true,
    }
}

fn reached_of(word: Option<&str>) -> Option<LimitReached> {
    let w = word?;
    if w.contains("credits_depleted") || w.contains("usage_limit_reached") {
        return Some(LimitReached::CreditsDepleted);
    }
    if w.contains("spend_control") {
        return Some(LimitReached::SpendControl);
    }
    if w.contains("rate_limit") {
        return Some(LimitReached::RateLimit);
    }
    None
}

/// A bucket becomes a snapshot. A null `secondary` yields no second window rather than a
/// zeroed one, which would read as a wide-open five-hour allowance.
pub fn snapshot_of(b: &Bucket, now: OffsetDateTime) -> RateLimitSnapshot {
    let windows: Vec<LimitWindow> = [b.limits.primary, b.limits.secondary]
        .into_iter()
        .flatten()
        .map(|w| window_of(&w, now))
        .collect();
    let reached = reached_of(b.reached.as_deref());
    RateLimitSnapshot {
        status: match (&reached, b.ordinary_usage_allowed) {
            (Some(_), _) | (_, Some(false)) => LimitStatus::Rejected,
            _ => LimitStatus::Allowed,
        },
        windows,
        resets_at: None,
        limit_id: b.limit_id.clone(),
        ordinary_usage_allowed: b.ordinary_usage_allowed,
        reached,
        plan: b.plan.clone(),
    }
}

/// Accepts a whole JSON-RPC response or the bare result object.
pub fn parse_rate_limits(value: &serde_json::Value, now: OffsetDateTime) -> RateLimitsRead {
    let result = value.get("result").unwrap_or(value);
    let plan = result
        .get("planName")
        .or_else(|| result.get("plan_name"))
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let raw = result
        .get("rateLimitsByLimitId")
        .or_else(|| result.get("rate_limits_by_limit_id"));
    let mut out = RateLimitsRead {
        plan: plan.clone(),
        ..RateLimitsRead::default()
    };
    let Some(map) = raw.and_then(|v| v.as_object()) else {
        return out;
    };
    for (id, value) in map {
        let Ok(mut bucket) = serde_json::from_value::<Bucket>(value.clone()) else {
            continue;
        };
        bucket.limit_id.get_or_insert_with(|| id.clone());
        if bucket.plan.is_none() {
            bucket.plan.clone_from(&plan);
        }
        if let Some(name) = &bucket.limit_name {
            out.names.insert(id.clone(), name.clone());
        }
        out.buckets.insert(id.clone(), snapshot_of(&bucket, now));
    }
    out
}

/// The rollout's own `rate_limits` plus `info.total_token_usage`, from the LAST `token_count`.
pub fn parse_rollout(text: &str, now: OffsetDateTime) -> RolloutSample {
    let mut out = RolloutSample::default();
    for line in text.lines() {
        let t = line.trim();
        if !t.starts_with('{') || !t.contains("token_count") {
            continue;
        }
        let Ok(line) = serde_json::from_str::<RolloutLine>(t) else {
            continue;
        };
        let Some(payload) = line.payload else {
            continue;
        };
        if payload.kind.as_deref() != Some("token_count") {
            continue;
        }
        if let Some(limits) = payload.rate_limits {
            let bucket = Bucket {
                limits,
                ..Bucket::default()
            };
            let snap = snapshot_of(&bucket, now);
            out.quota = (!snap.windows.is_empty()).then_some(snap);
        }
        if let Some(info) = payload.info {
            out.total_tokens = Some(usage_of(&info.total_token_usage));
        }
    }
    out
}

/// codex counts the cached prompt inside `input_tokens`; everything downstream uses the
/// Anthropic split, where the two are disjoint.
fn usage_of(u: &RolloutUsage) -> Usage {
    Usage {
        input_tokens: u.input_tokens.saturating_sub(u.cached_input_tokens),
        cached_input_tokens: u.cached_input_tokens,
        cache_write_tokens: 0,
        output_tokens: u.output_tokens,
        reasoning_tokens: u.reasoning_output_tokens,
    }
}

/// `$CODEX_HOME/sessions/<YYYY>/<MM>/<DD>/rollout-<local-ISO>-<thread_id>.jsonl`, tailed while
/// the run is still appending to it.
pub fn tail_rollout(codex_home: &Utf8Path, thread_id: &str) -> Option<RolloutSample> {
    let path = find_rollout(&codex_home.join("sessions"), thread_id, ROLLOUT_DEPTH)?;
    let text = std::fs::read_to_string(&path).ok()?;
    Some(parse_rollout(&text, OffsetDateTime::now_utc()))
}

fn find_rollout(dir: &Utf8Path, thread_id: &str, depth: usize) -> Option<Utf8PathBuf> {
    let suffix = format!("-{thread_id}.jsonl");
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) else {
            continue;
        };
        if path.is_dir() {
            dirs.push(path);
            continue;
        }
        let name = path.file_name().unwrap_or_default();
        if name.starts_with("rollout-") && name.ends_with(&suffix) {
            return Some(path);
        }
    }
    if depth == 0 {
        return None;
    }
    // Newest day first: the run that is appending right now is under the latest date.
    dirs.sort();
    dirs.iter()
        .rev()
        .find_map(|d| find_rollout(d, thread_id, depth - 1))
}

/// Swamp's own counters against a configured window size. Never a measurement, so it can
/// only deprioritise an account: Swamp does not know an OpenAI plan's real ceiling.
/// The window boundary is quantised on the epoch rather than taken from the account's
/// `window_started_at`: that field is written by the roll this estimate triggers, so keying on
/// it would make every observation a fresh window and zero the counter it just read.
pub fn estimated(
    window_tokens: &Usage,
    window: std::time::Duration,
    window_limit: u64,
    now: OffsetDateTime,
) -> Option<RateLimitSnapshot> {
    if window_limit == 0 {
        return None;
    }
    let secs = window.as_secs();
    if secs == 0 {
        return None;
    }
    let minutes = (secs / 60) as u32;
    let started = now.unix_timestamp() - now.unix_timestamp().rem_euclid(secs as i64);
    let resets_at = OffsetDateTime::from_unix_timestamp(started + secs as i64).ok();
    Some(RateLimitSnapshot {
        status: LimitStatus::Allowed,
        windows: vec![LimitWindow {
            scope: scope_of(Some(minutes)),
            utilization: (window_tokens.billable() as f64 / window_limit as f64).clamp(0.0, 1.0),
            resets_at,
            window_minutes: Some(minutes),
            measured: false,
        }],
        ..RateLimitSnapshot::default()
    })
}

/// `CODEX_HOME` is owned by the user's wrapper and Swamp cannot read a wrapper: the
/// app-server's `initialize` result echoes it, and the answer is cached per account.
pub async fn resolve_codex_home(exec: &str, env: &BTreeMap<String, String>) -> Option<Utf8PathBuf> {
    if let Some(home) = env.get("CODEX_HOME") {
        return Some(Utf8PathBuf::from(home));
    }
    let key = home_key(exec, env);
    if let Some(hit) = homes().lock().get(&key) {
        return hit.clone();
    }
    let found = match probe(exec, env).await {
        Ok((home, _)) => home,
        Err(e) => {
            tracing::debug!("cannot resolve CODEX_HOME for {exec}: {e:#}");
            None
        }
    };
    homes().lock().insert(key, found.clone());
    found
}

/// The resolved home depends on the whole environment the app-server is spawned with, so two
/// accounts sharing an executable must not share a cache entry.
fn home_key(exec: &str, env: &BTreeMap<String, String>) -> String {
    let mut key = exec.to_owned();
    for (k, v) in env {
        key.push('\u{0}');
        key.push_str(k);
        key.push('=');
        key.push_str(v);
    }
    key
}

/// `initialize` -> `initialized` -> `account/rateLimits/read` on stdio. Free of model tokens,
/// but it hits the network, so it is never on a timer. `account/read` is NOT called: it
/// returns the account email and Swamp has no use for it.
pub async fn read_rate_limits(
    exec: &str,
    env: &BTreeMap<String, String>,
) -> anyhow::Result<RateLimitsRead> {
    let (home, read) = probe(exec, env).await?;
    if home.is_some() && !env.contains_key("CODEX_HOME") {
        homes().lock().insert(home_key(exec, env), home);
    }
    read.ok_or_else(|| anyhow::anyhow!("{exec} app-server returned no rate limits"))
}

fn homes() -> &'static Mutex<BTreeMap<String, Option<Utf8PathBuf>>> {
    static HOMES: OnceLock<Mutex<BTreeMap<String, Option<Utf8PathBuf>>>> = OnceLock::new();
    HOMES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

async fn probe(
    exec: &str,
    env: &BTreeMap<String, String>,
) -> anyhow::Result<(Option<Utf8PathBuf>, Option<RateLimitsRead>)> {
    tokio::time::timeout(PROBE_TIMEOUT, talk(exec, env))
        .await
        .map_err(|_| anyhow::anyhow!("{exec} app-server did not answer in {PROBE_TIMEOUT:?}"))?
}

async fn talk(
    exec: &str,
    env: &BTreeMap<String, String>,
) -> anyhow::Result<(Option<Utf8PathBuf>, Option<RateLimitsRead>)> {
    let mut child = tokio::process::Command::new(exec)
        .arg("app-server")
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("no stdin on {exec} app-server"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("no stdout on {exec} app-server"))?;
    let mut lines = BufReader::new(stdout).lines();

    // Written in one go and stdin closed behind them: a wrapper that reads its stdin to EOF
    // before answering would otherwise hold the probe until it times out.
    let init = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "clientInfo": { "name": "swamp", "version": env!("CARGO_PKG_VERSION") } }
    });
    let ready = serde_json::json!({"jsonrpc": "2.0", "method": "initialized", "params": {}});
    let call = serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "account/rateLimits/read", "params": {}
    });
    stdin
        .write_all(format!("{init}\n{ready}\n{call}\n").as_bytes())
        .await?;
    stdin.shutdown().await?;
    drop(stdin);

    let mut home = None;
    let mut limits = None;
    while let Some(line) = lines.next_line().await? {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match value.get("id").and_then(serde_json::Value::as_u64) {
            Some(1) => {
                home = result_of(&value)
                    .and_then(|r| {
                        r.get("codexHome")
                            .or_else(|| r.get("codex_home"))
                            .and_then(|v| v.as_str())
                    })
                    .map(Utf8PathBuf::from);
            }
            Some(2) => {
                limits = result_of(&value).map(|r| parse_rate_limits(r, OffsetDateTime::now_utc()));
                break;
            }
            _ => {}
        }
    }
    let _ = child.kill().await;
    Ok((home, limits))
}

fn result_of(value: &serde_json::Value) -> Option<&serde_json::Value> {
    if let Some(e) = value.get("error") {
        tracing::debug!("app-server error: {e}");
        return None;
    }
    value.get("result")
}

#[derive(Deserialize)]
struct RolloutLine {
    #[serde(default)]
    payload: Option<RolloutPayload>,
}

#[derive(Deserialize)]
struct RolloutPayload {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    rate_limits: Option<RateLimits>,
    #[serde(default)]
    info: Option<RolloutInfo>,
}

#[derive(Deserialize)]
struct RolloutInfo {
    #[serde(default)]
    total_token_usage: RolloutUsage,
}

#[derive(Deserialize, Default)]
struct RolloutUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    reasoning_output_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_789_400_000).unwrap()
    }

    fn fixture(name: &str) -> String {
        let path = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("docs/ref")
            .join(name);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"))
    }

    #[test]
    fn a_seven_day_primary_never_becomes_a_five_hour_window() {
        let limits: RateLimits = serde_json::from_str(
            r#"{"primary":{"used_percent":32.0,"window_minutes":10080,"resets_in_seconds":433200},
                "secondary":null}"#,
        )
        .expect("rate_limits");
        let snap = snapshot_of(
            &Bucket {
                limits,
                ..Bucket::default()
            },
            now(),
        );
        assert_eq!(snap.windows.len(), 1, "a null secondary is not a window");
        assert_eq!(snap.windows[0].scope, LimitScope::SevenDay);
        assert!((snap.windows[0].utilization - 0.32).abs() < 1e-9);
        assert_eq!(snap.windows[0].window_minutes, Some(10080));
        assert!(snap.windows[0].measured);
    }

    #[test]
    fn the_rollout_tail_wins_and_carries_the_thread_total() {
        let sample = parse_rollout(&fixture("codex-rollout-sample.jsonl"), now());
        let quota = sample.quota.expect("rate limits");
        assert_eq!(quota.windows.len(), 1);
        assert!(
            (quota.worst_utilization() - 0.32).abs() < 1e-9,
            "the LAST token_count wins"
        );
        let tokens = sample.total_tokens.expect("total_token_usage");
        assert_eq!(tokens.cached_input_tokens, 24_000);
        assert_eq!(
            tokens.input_tokens, 7_000,
            "cached tokens are not billed twice"
        );
        assert_eq!(tokens.output_tokens, 120);
    }

    #[test]
    fn buckets_are_kept_apart_and_selected_by_id_then_model() {
        let value: serde_json::Value =
            serde_json::from_str(&fixture("codex-ratelimits-sample.json")).expect("json");
        let read = parse_rate_limits(&value, now());
        assert_eq!(read.buckets.len(), 2);
        assert_eq!(read.plan.as_deref(), Some("plus"));

        let chosen = read.select(None, None).expect("default bucket");
        assert_eq!(chosen.limit_id.as_deref(), Some("codex"));
        assert!((chosen.worst_utilization() - 0.32).abs() < 1e-9);
        assert_eq!(chosen.ordinary_usage_allowed, Some(true));

        let mini = read
            .select(None, Some("gpt-5.1-codex-mini"))
            .expect("bucket for the model");
        assert_eq!(mini.limit_id.as_deref(), Some("codex_bengalfox"));
        assert_eq!(mini.worst_utilization(), 0.0);

        let pinned = read.select(Some("codex_bengalfox"), Some("gpt-5.1-codex"));
        assert_eq!(
            pinned.and_then(|s| s.limit_id).as_deref(),
            Some("codex_bengalfox"),
            "accounts[].limit_id outranks the model match"
        );
    }

    #[test]
    fn a_depleted_bucket_is_rejected_with_a_reason() {
        let read = parse_rate_limits(
            &serde_json::json!({
                "result": {"rateLimitsByLimitId": {"codex": {
                    "limitId": "codex", "ordinaryUsageAllowed": false,
                    "rateLimitReachedType": "codex_credits_depleted",
                    "primary": {"usedPercent": 100.0, "windowMinutes": 10080}
                }}}
            }),
            now(),
        );
        let snap = read.select(None, None).expect("bucket");
        assert_eq!(snap.reached, Some(LimitReached::CreditsDepleted));
        assert_eq!(snap.ordinary_usage_allowed, Some(false));
        assert_eq!(snap.status, LimitStatus::Rejected);
    }

    #[test]
    fn an_estimate_is_never_measured_and_needs_a_configured_ceiling() {
        let spent = Usage {
            input_tokens: 500,
            output_tokens: 500,
            ..Usage::default()
        };
        assert!(
            estimated(&spent, std::time::Duration::from_secs(7 * 86400), 0, now()).is_none(),
            "0 means no estimate, not 100%"
        );
        let snap = estimated(
            &spent,
            std::time::Duration::from_secs(7 * 86400),
            10_000,
            now(),
        )
        .expect("estimate");
        assert_eq!(snap.windows[0].scope, LimitScope::SevenDay);
        assert!((snap.windows[0].utilization - 0.1).abs() < 1e-9);
        assert!(!snap.windows[0].measured);
        assert_eq!(
            snap.measured_utilization(),
            None,
            "an estimate never parks an account"
        );
    }

    /// The estimated window is keyed to a fixed boundary, so two observations inside one
    /// window agree and the counter is not re-rolled (and zeroed) on every node.
    #[test]
    fn two_estimates_in_one_window_agree_and_the_next_window_moves_on() {
        let spent = Usage {
            input_tokens: 1_000,
            ..Usage::default()
        };
        let window = std::time::Duration::from_secs(7 * 86400);
        let first = estimated(&spent, window, 10_000, now()).expect("estimate");
        let later =
            estimated(&spent, window, 10_000, now() + time::Duration::hours(6)).expect("estimate");
        assert_eq!(first.windows[0].resets_at, later.windows[0].resets_at);

        let next = estimated(&spent, window, 10_000, now() + time::Duration::days(8))
            .expect("estimate")
            .windows[0]
            .resets_at;
        assert!(next > first.windows[0].resets_at, "the window has to roll");
    }

    /// Two accounts sharing an executable differ by their env overlay, so the resolved
    /// CODEX_HOME cannot be cached under the executable alone.
    #[test]
    fn the_codex_home_cache_key_separates_two_env_overlays() {
        let personal = BTreeMap::from([("HOME".to_owned(), "/p".to_owned())]);
        let work = BTreeMap::from([("HOME".to_owned(), "/w".to_owned())]);
        assert_ne!(home_key("codex", &personal), home_key("codex", &work));
        assert_eq!(home_key("codex", &personal), home_key("codex", &personal));
    }

    #[test]
    fn a_rollout_is_found_by_thread_id_under_the_dated_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let day = home.join("sessions/2026/09/15");
        std::fs::create_dir_all(&day).expect("mkdir");
        let thread = "01a0a1ec-4635-7720-ab6f-c88e0351c2d6";
        std::fs::write(
            day.join(format!("rollout-2026-09-15T20-10-11-{thread}.jsonl")),
            fixture("codex-rollout-sample.jsonl"),
        )
        .expect("write");
        let sample = tail_rollout(&home, thread).expect("sample");
        assert!(sample.quota.is_some());
        assert!(tail_rollout(&home, "no-such-thread").is_none());
    }
}
