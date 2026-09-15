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
    assert!(errors[0].detail.contains("not on PATH"), "{}", errors[0].detail);
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
    assert!(
        errors[0].detail.contains("providers.<p>.models.<t>"),
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
    assert!(errors(&acked).is_empty(), "an acknowledged flag is accepted");
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
    std::fs::write(dir.join("ctl.sock"), "").expect("socket");

    let removed = swamp::doctor::reap(&f.paths).await.expect("reap");
    assert!(removed >= 1, "the stale socket is removed");
    assert!(!dir.join("ctl.sock").exists());
}
