//! WP7: the health checks, and above all the one that catches two names for one subscription.

mod common;

use camino::Utf8PathBuf;
use std::collections::BTreeMap;
use swamp::config::{Config, load, resolve};
use swamp::doctor::{Check, Level, checks};
use swamp::journal::paths::Paths;

struct Fixture {
    _tmp: tempfile::TempDir,
    _home: tempfile::TempDir,
    repo: Utf8PathBuf,
    paths: Paths,
    bin: Utf8PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let (tmp, repo) = common::tmp_repo();
        let home = tempfile::tempdir().expect("home");
        let home = Utf8PathBuf::from_path_buf(home.path().to_path_buf()).expect("utf8 home");
        let bin = repo.join("bin");
        std::fs::create_dir_all(&bin).expect("bin dir");
        let holder = tempfile::tempdir().expect("home holder");
        let home = if home.is_dir() {
            home
        } else {
            Utf8PathBuf::from_path_buf(holder.path().to_path_buf()).expect("utf8")
        };
        let paths = Paths {
            repo: repo.clone(),
            dot_swamp: repo.join(".swamp"),
            home_swamp: home.join(".swamp"),
        };
        Fixture {
            _tmp: tmp,
            _home: holder,
            repo,
            paths,
            bin,
        }
    }

    /// An executable that exists: `which` resolves an absolute path without touching PATH.
    fn exec(&self, name: &str) -> String {
        let path = self.bin.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write exec");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        path.to_string()
    }
}

fn config(text: &str) -> Config {
    let schema = toml::from_str(text).expect("test config parses");
    let layers = vec![
        load::default_layer(),
        load::Layer {
            origin: "test".into(),
            schema,
        },
    ];
    resolve::from_schema(load::merge(layers))
}

fn healthy(f: &Fixture) -> Config {
    let main = f.exec("worker-main");
    let alt = f.exec("worker-alt");
    config(&format!(
        r#"
[providers.anthropic]
models = {{ high = "tier-high", mid = "tier-mid", low = "tier-low" }}

[[accounts]]
id = "main"
provider = "anthropic"
exec = "{main}"
env = {{ TEST_CONFIG_DIR = "/tmp/main" }}

[[accounts]]
id = "alt"
provider = "anthropic"
exec = "{alt}"
env = {{ TEST_CONFIG_DIR = "/tmp/alt" }}
"#
    ))
}

fn errors(checks: &[Check]) -> Vec<&Check> {
    checks.iter().filter(|c| c.level == Level::Error).collect()
}

fn warnings(checks: &[Check]) -> Vec<&Check> {
    checks.iter().filter(|c| c.level == Level::Warn).collect()
}

#[tokio::test]
async fn a_good_setup_reports_no_errors() {
    let f = Fixture::new();
    let cfg = healthy(&f);
    let out = checks(&cfg, &f.paths, false, false).await;
    assert!(
        errors(&out).is_empty(),
        "unexpected errors: {:?}",
        errors(&out)
            .iter()
            .map(|c| format!("{}: {}", c.name, c.detail))
            .collect::<Vec<_>>()
    );
    assert!(out.iter().any(|c| c.name == "providers/anthropic"));
}

#[tokio::test]
async fn a_missing_exec_is_one_error_that_names_the_fix() {
    let f = Fixture::new();
    let mut cfg = healthy(&f);
    cfg.accounts[1].exec = "definitely-not-on-path-swamp".into();
    let out = checks(&cfg, &f.paths, false, false).await;
    let errors = errors(&out);
    assert_eq!(errors.len(), 1, "{:?}", errors.len());
    assert_eq!(errors[0].name, "accounts/alt");
    assert!(
        errors[0].detail.contains("not on PATH"),
        "{}",
        errors[0].detail
    );
    assert!(errors[0].detail.contains("wrapper"), "{}", errors[0].detail);
}

#[tokio::test]
async fn a_tier_with_no_model_is_one_error_that_names_the_key() {
    let f = Fixture::new();
    let mut cfg = healthy(&f);
    cfg.providers
        .get_mut(&swamp::model::core::Provider::Anthropic)
        .expect("provider")
        .models
        .remove(&swamp::model::core::Tier::High);
    let out = checks(&cfg, &f.paths, false, false).await;
    let errors = errors(&out);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].name, "providers/anthropic/high");
    // The key the user has to set, spelled the way the config spells it, in a real file.
    assert!(
        errors[0]
            .detail
            .contains("providers.anthropic.models.high in .swamp/config.toml"),
        "{}",
        errors[0].detail
    );
}

#[tokio::test]
async fn an_unwritable_dot_swamp_is_one_error() {
    let f = Fixture::new();
    std::fs::write(&f.paths.dot_swamp, "not a directory").expect("block .swamp");
    let cfg = healthy(&f);
    let out = checks(&cfg, &f.paths, false, false).await;
    let errors = errors(&out);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].name, "environment/.swamp");
}

/// The most valuable check in the tool: two accounts, one quota.
#[tokio::test]
async fn two_accounts_on_one_subscription_are_an_error_that_names_both() {
    let f = Fixture::new();
    let mut cfg = healthy(&f);
    let shared = cfg.accounts[0].exec.clone();
    let env: BTreeMap<String, String> = cfg.accounts[0].env.clone();
    cfg.accounts[1].exec = shared;
    cfg.accounts[1].env = env;

    let out = checks(&cfg, &f.paths, false, false).await;
    let errors = errors(&out);
    assert_eq!(errors.len(), 1, "exactly one failing check");
    assert_eq!(errors[0].name, "accounts/collision");
    assert!(errors[0].detail.contains("main"), "{}", errors[0].detail);
    assert!(errors[0].detail.contains("alt"), "{}", errors[0].detail);
    assert!(
        errors[0].detail.contains("ONE subscription"),
        "the message must say the two accounts are one subscription: {}",
        errors[0].detail
    );
}

#[tokio::test]
async fn an_unacknowledged_dangerous_flag_is_an_error() {
    let f = Fixture::new();
    let mut cfg = healthy(&f);
    cfg.providers
        .get_mut(&swamp::model::core::Provider::Anthropic)
        .expect("provider")
        .worker
        .args = vec!["--dangerously-skip-everything".into()];
    let out = checks(&cfg, &f.paths, false, false).await;
    let failing = errors(&out);
    assert_eq!(failing.len(), 1);
    assert_eq!(failing[0].name, "providers/anthropic/args");
    assert!(
        failing[0].detail.contains("unsafe_ack"),
        "{}",
        failing[0].detail
    );

    cfg.limits.unsafe_ack = Some(true);
    let acked = checks(&cfg, &f.paths, false, false).await;
    assert!(
        errors(&acked).is_empty(),
        "an acknowledged flag is accepted"
    );
}

#[tokio::test]
async fn a_heavy_build_directory_with_no_link_warns() {
    let f = Fixture::new();
    let heavy = f.repo.join("target");
    std::fs::create_dir_all(&heavy).expect("target dir");
    let file = std::fs::File::create(heavy.join("big.bin")).expect("big file");
    // Sparse: the check reads metadata, so this costs no disk.
    file.set_len(2 << 30).expect("set_len");
    drop(file);

    let cfg = healthy(&f);
    let out = checks(&cfg, &f.paths, false, false).await;
    assert!(errors(&out).is_empty());
    let warning = warnings(&out)
        .into_iter()
        .find(|c| c.name == "workspace/link")
        .expect("a warning about the unlinked build directory");
    assert!(warning.detail.contains("link"), "{}", warning.detail);
}

#[tokio::test]
async fn schema_drift_across_recent_runs_fails_the_check() {
    use swamp::ids::{NodeId, RunId};
    use swamp::journal::record::{JournalEvent, JournalLine};
    use swamp::model::core::{LimitScope, NodeState, Usage};
    use swamp::model::failure::{Detector, Failure};

    let f = Fixture::new();
    let run = RunId::new();
    let dir = f.paths.dot_swamp.join("runs").join(run.to_string());
    std::fs::create_dir_all(&dir).expect("run dir");

    let mut text = String::new();
    for i in 0..4 {
        let line = JournalLine {
            seq: i,
            at: time::OffsetDateTime::now_utc(),
            run,
            node: Some(NodeId::new()),
            event: JournalEvent::NodeFinished {
                state: NodeState::Failed {
                    failure: Failure::RateLimited {
                        resets_at: None,
                        scope: LimitScope::Unknown,
                        detected_by: Detector::Pattern,
                        evidence: "429 too many requests".into(),
                    },
                },
                exit: None,
                usage: Usage::default(),
                cost: None,
                work: None,
                summary: None,
                files: Vec::new(),
                unparsed_lines: 3,
            },
        };
        text.push_str(&serde_json::to_string(&line).expect("json"));
        text.push('\n');
    }
    std::fs::write(dir.join("journal.jsonl"), text).expect("journal");

    let cfg = healthy(&f);
    let out = checks(&cfg, &f.paths, false, true).await;
    let drift = out
        .iter()
        .find(|c| c.name == "protocol/schema")
        .expect("the schema check ran");
    assert_eq!(drift.level, Level::Error, "{}", drift.detail);
    assert!(drift.detail.contains("regex fallback"), "{}", drift.detail);

    // Without --schema the check does not run at all.
    let quiet = checks(&cfg, &f.paths, false, false).await;
    assert!(!quiet.iter().any(|c| c.name == "protocol/schema"));
}

#[tokio::test]
async fn reap_removes_stale_sockets_and_pidfiles() {
    let f = Fixture::new();
    let run = swamp::ids::RunId::new();
    let dir = f.paths.dot_swamp.join("runs").join(run.to_string());
    std::fs::create_dir_all(&dir).expect("run dir");
    std::fs::write(dir.join("journal.jsonl"), "").expect("journal");
    let socket = f.paths.run_paths(run).socket();
    std::fs::create_dir_all(socket.parent().expect("sock dir")).expect("sock dir");
    std::fs::write(&socket, "").expect("socket");

    let removed = swamp::doctor::reap(&f.paths).await.expect("reap");
    assert!(removed.runs >= 1, "the stale socket is removed");
    assert!(!socket.exists());
}

/// A killed session whose repository is gone leaves nothing behind that names its run, so the
/// socket directory is swept on its own. A socket that still answers belongs to a live run.
#[tokio::test]
async fn reap_sweeps_the_global_socket_directory_and_spares_live_sockets() {
    let f = Fixture::new();
    let sock_dir = f.paths.sock_dir();
    std::fs::create_dir_all(&sock_dir).expect("sock dir");

    let dead = sock_dir.join("deadaa.sock");
    drop(std::os::unix::net::UnixListener::bind(&dead).expect("bind dead"));
    let live = sock_dir.join("liveaa.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&live).expect("bind live");

    let removed = swamp::doctor::reap(&f.paths).await.expect("reap");
    assert_eq!(removed.sockets, 1, "only the unanswered socket is removed");
    assert!(!dead.exists(), "the stale socket is gone");
    assert!(live.exists(), "a socket that accepts a connection is kept");
}

/// Swamp launches workers with `--permission-prompts none`: under it these modes auto-deny
/// every Bash call the configuration did not allow by name, so the worker cannot run the
/// tests it was sent to run. Allowing Bash is the fix, not switching to `auto`, which denies
/// the file writes instead.
#[tokio::test]
async fn a_permission_mode_that_denies_bash_warns_until_bash_is_allowed() {
    let f = Fixture::new();
    let mut cfg = healthy(&f);
    let anthropic = swamp::model::core::Provider::Anthropic;

    for mode in ["acceptEdits", "plan", "manual", "dontAsk"] {
        cfg.providers
            .get_mut(&anthropic)
            .expect("the anthropic provider")
            .worker
            .permission_mode = Some(mode.to_owned());
        let out = checks(&cfg, &f.paths, false, false).await;
        let warned: Vec<&Check> = warnings(&out)
            .into_iter()
            .filter(|c| c.name == "providers/anthropic/permission_mode")
            .collect();
        assert_eq!(warned.len(), 1, "{mode} was not flagged");
        assert!(warned[0].detail.contains("Bash"), "{}", warned[0].detail);
        assert!(
            warned[0].detail.contains("allow_tools"),
            "the warning names the fix: {}",
            warned[0].detail
        );
    }

    // acceptEdits plus an allowed Bash is the recommendation: writes land and tests run.
    let worker = &mut cfg
        .providers
        .get_mut(&anthropic)
        .expect("the anthropic provider")
        .worker;
    worker.permission_mode = Some("acceptEdits".to_owned());
    worker.allow_tools = vec!["Bash".to_owned()];
    let out = checks(&cfg, &f.paths, false, false).await;
    assert!(
        !out.iter()
            .any(|c| c.name == "providers/anthropic/permission_mode"),
        "acceptEdits with Bash allowed must not warn"
    );

    // The same allowance spelled as a raw flag in worker.args counts too.
    let worker = &mut cfg
        .providers
        .get_mut(&anthropic)
        .expect("the anthropic provider")
        .worker;
    worker.allow_tools.clear();
    worker.args = vec!["--allowedTools".to_owned(), "Bash".to_owned()];
    let out = checks(&cfg, &f.paths, false, false).await;
    assert!(
        !out.iter()
            .any(|c| c.name == "providers/anthropic/permission_mode"),
        "--allowedTools Bash in worker.args must not warn"
    );
}

/// The brain runs the same way, and needs Bash for git and for the tests it verifies with.
#[tokio::test]
async fn the_brain_is_checked_the_same_way_as_a_worker() {
    let f = Fixture::new();
    let mut cfg = healthy(&f);
    cfg.brain.permission_mode = Some("acceptEdits".to_owned());
    cfg.brain.allow_tools.clear();
    let out = checks(&cfg, &f.paths, false, false).await;
    let warned: Vec<&Check> = warnings(&out)
        .into_iter()
        .filter(|c| c.name == "brain/permission_mode")
        .collect();
    assert_eq!(warned.len(), 1, "the brain was not flagged");
    assert!(
        warned[0].detail.contains("brain.allow_tools"),
        "{}",
        warned[0].detail
    );

    cfg.brain.allow_tools = vec!["Bash".to_owned()];
    let out = checks(&cfg, &f.paths, false, false).await;
    assert!(
        !out.iter().any(|c| c.name == "brain/permission_mode"),
        "a brain that may run Bash must not warn"
    );
}

/// USAGE 1.7: the level follows the quota SOURCE, not merely the presence of a snapshot. An
/// estimated snapshot is the case the check exists to flag.
#[tokio::test]
async fn the_quota_check_grades_the_source_and_prints_the_age() {
    use swamp::dispatch::account::{AccountState, QuotaSource};
    use swamp::model::core::{AccountId, LimitScope, LimitWindow, RateLimitSnapshot};

    let f = Fixture::new();
    let cfg = healthy(&f);
    let now = time::OffsetDateTime::now_utc();
    let snapshot = |utilization: f64, measured: bool| RateLimitSnapshot {
        windows: vec![LimitWindow {
            scope: LimitScope::SevenDay,
            utilization,
            resets_at: Some(now + time::Duration::hours(4)),
            window_minutes: Some(10080),
            measured,
        }],
        ..RateLimitSnapshot::default()
    };
    let mut state = swamp::dispatch::persist::StateMap::new();
    let mut live = AccountState::default();
    live.apply_quota(snapshot(0.13, true), QuotaSource::Telemetry, now);
    state.insert(AccountId("main".into()), live);
    let mut guessed = AccountState::default();
    guessed.apply_quota(snapshot(0.02, false), QuotaSource::Estimated, now);
    state.insert(AccountId("alt".into()), guessed);
    std::fs::create_dir_all(&f.paths.home_swamp).expect("state dir");
    swamp::dispatch::persist::save_state(&f.paths.accounts_state(), &state).expect("state file");

    let out = checks(&cfg, &f.paths, false, false).await;
    let check = |name: &str| {
        out.iter()
            .find(|c| c.name == format!("providers/{name}/quota"))
            .unwrap_or_else(|| panic!("no quota check for {name}"))
    };
    assert_eq!(check("main").level, Level::Ok);
    assert!(check("main").detail.contains("quota telemetry live"));
    assert!(check("main").detail.contains("observed "));
    assert_eq!(
        check("alt").level,
        Level::Warn,
        "an estimated snapshot is not a quota source: {}",
        check("alt").detail
    );
    assert!(check("alt").detail.contains("no quota source"));
}

/// The §9 sample is the interface teams gate CI on, so it must show the collision at the level
/// the code emits and the header the code prints.
#[test]
fn the_design_doc_doctor_sample_matches_what_the_code_emits() {
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let doc = std::fs::read_to_string(root.join("docs/DESIGN.md")).expect("DESIGN.md");
    let sample = doc
        .split("$ swamp doctor\n")
        .nth(1)
        .and_then(|rest| rest.split("```").next())
        .expect("the §9 doctor sample");
    assert!(
        sample.contains(&format!("{} accounts/collision", Level::Error.label())),
        "the collision is an Error on `accounts/collision` in src/doctor.rs: {sample}"
    );
    assert!(
        !sample.contains("rustc"),
        "swamp doctor prints no rustc version"
    );
    assert!(
        sample.contains("swamp 0.1.0 - macos aarch64"),
        "the header is `swamp <version> - <os> <arch>`: {sample}"
    );
    assert!(
        sample.contains("/quota") && sample.contains("no quota source"),
        "§9 has to show the per-account `providers/<id>/quota` check doctor emits: {sample}"
    );
    assert!(
        !sample.contains("no cost and no quota telemetry"),
        "an openai account does get quota telemetry, from the rollout or the app-server: {sample}"
    );
    // Counted, not spelled: the tally has to follow the sample whenever a check is added.
    let count = |label: &str| {
        sample
            .lines()
            .filter(|l| l.trim_start().starts_with(label))
            .count()
    };
    let warnings = count(Level::Warn.label());
    let errors = count(Level::Error.label());
    let plural = |n: usize, w: &str| format!("{n} {w}{}", if n == 1 { "" } else { "s" });
    assert!(
        sample.trim_end().ends_with(&format!(
            "{}, {}.",
            plural(warnings, "warning"),
            plural(errors, "error")
        )),
        "the tally has to match the {warnings} WARN and {errors} ERROR lines: {sample}"
    );
}

/// A window whose reset has passed measures an allowance that already rolled: `score` and the
/// usage table both drop it, so doctor must not quote it as evidence the account is spent.
#[tokio::test]
async fn doctor_never_quotes_a_window_whose_reset_has_passed() {
    use swamp::dispatch::account::{AccountState, QuotaSource};
    use swamp::dispatch::persist::{StateMap, save_state};
    use swamp::model::core::{AccountId, LimitScope, LimitStatus, LimitWindow, RateLimitSnapshot};
    use time::OffsetDateTime;

    let f = Fixture::new();
    let cfg = healthy(&f);
    let now = OffsetDateTime::now_utc();
    let window = |resets_at| LimitWindow {
        scope: LimitScope::SevenDay,
        utilization: 0.98,
        resets_at: Some(resets_at),
        window_minutes: Some(10_080),
        measured: true,
    };
    let snapshot = |resets_at| RateLimitSnapshot {
        status: LimitStatus::Allowed,
        windows: vec![window(resets_at)],
        ..RateLimitSnapshot::default()
    };

    let mut state = StateMap::new();
    let mut stale = AccountState::default();
    stale.apply_quota(
        snapshot(now - time::Duration::days(2)),
        QuotaSource::Telemetry,
        now,
    );
    state.insert(AccountId("main".into()), stale);
    let mut live = AccountState::default();
    live.apply_quota(
        snapshot(now + time::Duration::days(2)),
        QuotaSource::Telemetry,
        now,
    );
    state.insert(AccountId("alt".into()), live);
    std::fs::create_dir_all(&f.paths.home_swamp).expect("home dir");
    save_state(&f.paths.accounts_state(), &state).expect("state file");

    let out = checks(&cfg, &f.paths, false, false).await;
    let line = |id: &str| {
        out.iter()
            .find(|c| c.name == format!("providers/{id}/quota"))
            .map(|c| c.detail.clone())
            .expect("a quota line")
    };
    assert!(
        !line("main").contains("98%"),
        "a rolled window is not evidence: {}",
        line("main")
    );
    assert!(
        line("alt").contains("7d 98%"),
        "a live window is still reported: {}",
        line("alt")
    );
}

/// SWAMP_CONFIG_DIR and XDG_CONFIG_HOME move the user layer somewhere the docs never name, so
/// "no config file was found" has to say which paths were tried.
#[tokio::test]
async fn config_sources_names_the_user_path_it_resolved() {
    let f = Fixture::new();
    let cfg = healthy(&f);
    assert!(cfg.sources.is_empty(), "the fixture loads no file");

    let out = checks(&cfg, &f.paths, false, false).await;
    let line = out
        .iter()
        .find(|c| c.name == "config/sources")
        .expect("the config/sources check");
    let expected = swamp::config::load::user_config_path().expect("a user config path");
    assert!(line.detail.contains(expected.as_str()), "{}", line.detail);
    assert!(
        line.detail.contains("<repo>/.swamp/config.toml"),
        "{}",
        line.detail
    );
}
