//! WP1: layering, profiles and validation of the config loader.

use camino::{Utf8Path, Utf8PathBuf};
use std::sync::Mutex;
use swamp::config::Config;
use swamp::error::{SwampError, exit_code};
use swamp::model::core::{AccountId, Provider, Tier, Usage};
use swamp::model::result::IsolationMode;

/// The loader reads process env, so every test that touches it runs alone.
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct Sandbox {
    _tmp: tempfile::TempDir,
    repo: Utf8PathBuf,
    user_config_dir: Utf8PathBuf,
}

impl Sandbox {
    fn new() -> Sandbox {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("utf8 tempdir");
        let repo = root.join("repo");
        let user_config_dir = root.join("userconfig");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&user_config_dir).unwrap();
        Sandbox {
            _tmp: tmp,
            repo,
            user_config_dir,
        }
    }

    fn user_config(&self, body: &str) {
        std::fs::write(self.user_config_dir.join("config.toml"), body).unwrap();
    }

    fn repo_config(&self, body: &str) {
        std::fs::create_dir_all(self.repo.join(".swamp")).unwrap();
        std::fs::write(self.repo.join(".swamp").join("config.toml"), body).unwrap();
    }

    fn write(&self, name: &str, body: &str) -> Utf8PathBuf {
        let path = self.repo.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn load(
        &self,
        explicit: Option<&Utf8Path>,
        profile: Option<&str>,
    ) -> Result<Config, SwampError> {
        self.load_with(&[], explicit, profile)
    }

    /// Runs the loader with exactly `vars` set in the SWAMP_ namespace.
    fn load_with(
        &self,
        vars: &[(&str, &str)],
        explicit: Option<&Utf8Path>,
        profile: Option<&str>,
    ) -> Result<Config, SwampError> {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved: Vec<(String, String)> = std::env::vars()
            .filter(|(k, _)| k.starts_with("SWAMP_"))
            .collect();
        unsafe {
            for (k, _) in &saved {
                std::env::remove_var(k);
            }
            std::env::set_var("SWAMP_CONFIG_DIR", self.user_config_dir.as_str());
            for (k, v) in vars {
                std::env::set_var(k, v);
            }
        }
        let out = Config::load(&self.repo, explicit, profile);
        unsafe {
            std::env::remove_var("SWAMP_CONFIG_DIR");
            for (k, _) in vars {
                std::env::remove_var(k);
            }
            for (k, v) in saved {
                std::env::set_var(k, v);
            }
        }
        out
    }
}

fn example_config() -> Utf8PathBuf {
    Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("swamp.example.toml")
}

fn err_text(e: &SwampError) -> String {
    e.to_string()
}

#[test]
fn example_config_loads_and_maps_tiers() {
    let sb = Sandbox::new();
    let example = example_config();
    let cfg = sb.load(Some(&example), None).expect("example config loads");

    assert_eq!(
        cfg.model_for(Provider::Anthropic, Tier::High, None)
            .unwrap(),
        "opus"
    );
    assert_eq!(
        cfg.model_for(Provider::Anthropic, Tier::Low, None).unwrap(),
        "haiku"
    );
    assert_eq!(cfg.limits.max_parallel, Some(4));
    assert_eq!(cfg.accounts.len(), 4);
    assert_eq!(cfg.accounts_for(Provider::Openai).len(), 1);
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
    assert_eq!(
        cfg.node_timeout(Tier::High),
        std::time::Duration::from_secs(45 * 60)
    );
    assert_eq!(cfg.node_budget_usd(Tier::High), Some(8.0));
    assert_eq!(cfg.node_budget_usd(Tier::Mid), Some(3.0));
    assert_eq!(
        cfg.provider_order(Tier::Low),
        vec![Provider::Anthropic],
        "an explicit provider_order is taken verbatim"
    );
    assert_eq!(
        cfg.tier_extra(Provider::Anthropic, Tier::High)
            .get("effort")
            .map(String::as_str),
        Some("high")
    );
}

#[test]
fn per_account_model_override_wins() {
    let sb = Sandbox::new();
    let example = example_config();
    let cfg = sb.load(Some(&example), None).unwrap();
    let alt = AccountId("alt".into());
    assert_eq!(
        cfg.model_for(Provider::Anthropic, Tier::High, Some(&alt))
            .unwrap(),
        "sonnet"
    );
    let main = AccountId("main".into());
    assert_eq!(
        cfg.model_for(Provider::Anthropic, Tier::High, Some(&main))
            .unwrap(),
        "opus"
    );
}

#[test]
fn unmapped_tier_is_an_error_not_a_default() {
    let sb = Sandbox::new();
    let path = sb.write(
        "partial.toml",
        r#"
[providers.anthropic]
models = { mid = "model-mid" }
"#,
    );
    let cfg = sb.load(Some(&path), None).unwrap();
    let err = cfg
        .model_for(Provider::Anthropic, Tier::High, None)
        .unwrap_err();
    assert!(
        matches!(
            err,
            SwampError::TierUnmapped {
                provider: Provider::Anthropic,
                tier: Tier::High
            }
        ),
        "{err}"
    );
    assert!(err_text(&err).contains("providers.<p>.models.<t>"));
}

#[test]
fn layers_stack_repo_over_user_and_env_over_both() {
    let sb = Sandbox::new();
    sb.user_config("[limits]\nmax_parallel = 2\nmax_nodes_per_run = 11\n");

    let user_only = sb.load(None, None).unwrap();
    assert_eq!(user_only.limits.max_parallel, Some(2));

    sb.repo_config("[limits]\nmax_parallel = 5\n");
    let repo_wins = sb.load(None, None).unwrap();
    assert_eq!(repo_wins.limits.max_parallel, Some(5));
    assert_eq!(
        repo_wins.limits.max_nodes_per_run,
        Some(11),
        "keys the repo layer does not set survive from the user layer"
    );

    let env_wins = sb
        .load_with(&[("SWAMP_LIMITS__MAX_PARALLEL", "9")], None, None)
        .unwrap();
    assert_eq!(env_wins.limits.max_parallel, Some(9));

    let explicit = sb.write("over.toml", "[limits]\nmax_parallel = 12\n");
    let explicit_wins = sb
        .load_with(
            &[("SWAMP_LIMITS__MAX_PARALLEL", "9")],
            Some(&explicit),
            None,
        )
        .unwrap();
    assert_eq!(explicit_wins.limits.max_parallel, Some(12));
    assert_eq!(explicit_wins.sources.len(), 3);
}

/// `config show --effective` reported version = 0 for a file declaring version = 1: the empty
/// env layer serialized a default Schema and its zero clobbered the value below it.
#[test]
fn a_layer_only_contributes_the_keys_it_sets() {
    let sb = Sandbox::new();
    sb.user_config("version = 1\n\n[limits]\nmax_parallel = 7\n");

    let cfg = sb.load(None, None).expect("the user config loads");
    assert_eq!(cfg.version, 1, "the declared version survives the merge");

    let with_env = sb
        .load_with(&[("SWAMP_LIMITS__MAX_PARALLEL", "9")], None, None)
        .expect("the env layer loads");
    assert_eq!(with_env.version, 1, "an env override sets one key, not all");
    assert_eq!(with_env.limits.max_parallel, Some(9));
}

#[test]
fn unrelated_swamp_env_vars_are_not_config() {
    let sb = Sandbox::new();
    let cfg = sb
        .load_with(&[("SWAMP_DEPTH", "1"), ("SWAMP_LOG", "debug")], None, None)
        .unwrap();
    assert_eq!(cfg.limits.max_parallel, Some(4));
}

#[test]
fn a_bad_env_override_names_the_variable() {
    let sb = Sandbox::new();
    let err = sb
        .load_with(&[("SWAMP_LIMITS__MAX_PARALLEL", "loads")], None, None)
        .unwrap_err();
    assert!(
        err_text(&err).contains("SWAMP_LIMITS__MAX_PARALLEL"),
        "{err}"
    );
}

#[test]
fn profile_applies_dotted_overrides() {
    let sb = Sandbox::new();
    let example = example_config();
    let cfg = sb.load(Some(&example), Some("cheap")).unwrap();
    assert_eq!(cfg.dispatch.default_tier, Some(Tier::Low));
    assert_eq!(cfg.limits.max_parallel, Some(2));
    assert_eq!(cfg.brain.tier, Some(Tier::Mid));

    let plain = sb.load(Some(&example), None).unwrap();
    assert_eq!(plain.dispatch.default_tier, Some(Tier::Mid));
    assert_eq!(plain.brain.tier, Some(Tier::High));
}

#[test]
fn an_unknown_profile_is_rejected() {
    let sb = Sandbox::new();
    let example = example_config();
    let err = sb.load(Some(&example), Some("nope")).unwrap_err();
    assert!(err_text(&err).contains("profiles.nope"), "{err}");
    assert!(err_text(&err).contains("cheap"), "{err}");
}

#[test]
fn unknown_keys_are_rejected_with_the_file_name() {
    let sb = Sandbox::new();
    let path = sb.write("typo.toml", "[limits]\nmax_paralel = 4\n");
    let err = sb.load(Some(&path), None).unwrap_err();
    let text = err_text(&err);
    assert!(text.contains("typo.toml"), "{text}");
    assert!(text.contains("max_paralel"), "{text}");
}

#[test]
fn validation_reports_every_problem_at_once() {
    let sb = Sandbox::new();
    let path = sb.write(
        "broken.toml",
        r#"
[cooldown]
quota_warn_at = 0.99
quota_stop_at = 0.90

[brain]
provider = "anthropic"
account = "codex-one"

[providers.anthropic]
models = { high = "model-high" }
worker = { args = ["--dangerously-skip-permissions"] }

[failure.anthropic]
rate_limit = ["(unclosed"]

[[accounts]]
id = "dup"
provider = "anthropic"
exec = "a"
max_concurrency = 0

[[accounts]]
id = "dup"
provider = "anthropic"
exec = "b"

[[accounts]]
id = "codex-one"
provider = "openai"
exec = "c"
"#,
    );
    let err = sb.load(Some(&path), None).unwrap_err();
    assert!(matches!(err, SwampError::ConfigInvalid(_)), "{err}");
    let text = err_text(&err);
    for key in [
        "accounts[0].max_concurrency",
        "accounts[1].id",
        "brain.account",
        "cooldown.quota_warn_at",
        "failure.anthropic.rate_limit[0]",
        "providers.anthropic.worker.args[0]",
    ] {
        assert!(text.contains(key), "missing `{key}` in:\n{text}");
    }
    assert_eq!(exit_code(&anyhow::Error::new(err)), 2);
}

#[test]
fn an_acked_dangerous_flag_is_allowed() {
    let sb = Sandbox::new();
    let path = sb.write(
        "acked.toml",
        r#"
[limits]
unsafe_ack = true

[providers.anthropic]
models = { high = "model-high" }
worker = { args = ["--dangerously-skip-permissions"] }
"#,
    );
    assert!(sb.load(Some(&path), None).is_ok());
}

#[test]
fn a_tier_nobody_can_serve_is_rejected() {
    let sb = Sandbox::new();
    let path = sb.write(
        "tiers.toml",
        r#"
[providers.anthropic]
models = { mid = "model-mid" }

[tiers.high]
provider_order = ["anthropic"]
"#,
    );
    let err = sb.load(Some(&path), None).unwrap_err();
    assert!(
        err_text(&err).contains("tiers.high.provider_order"),
        "{err}"
    );
}

#[test]
fn shared_isolation_forces_one_worker_and_warns() {
    let sb = Sandbox::new();
    let path = sb.write("shared.toml", "[workspace]\nisolation = \"shared\"\n");
    let cfg = sb.load(Some(&path), None).unwrap();
    assert_eq!(cfg.workspace.isolation, Some(IsolationMode::Shared));
    assert_eq!(cfg.limits.max_parallel, Some(1));
    assert_eq!(cfg.warnings.len(), 1);
    assert!(
        cfg.warnings[0].contains("max_parallel"),
        "{:?}",
        cfg.warnings
    );
}

#[test]
fn worktree_root_inside_the_repo_dot_swamp_is_rejected() {
    let sb = Sandbox::new();
    sb.repo_config(&format!(
        "[workspace]\nroot = \"{}\"\n",
        sb.repo.join(".swamp").join("worktrees")
    ));
    let err = sb.load(None, None).unwrap_err();
    assert!(err_text(&err).contains("workspace.root"), "{err}");
}

#[test]
fn estimate_cost_is_none_without_a_pricing_row() {
    let sb = Sandbox::new();
    let path = sb.write(
        "pricing.toml",
        r#"
[pricing."model-mid"]
input = 0.25
cached_input = 0.025
output = 2.00
"#,
    );
    let cfg = sb.load(Some(&path), None).unwrap();
    let usage = Usage {
        input_tokens: 1_000_000,
        cached_input_tokens: 1_000_000,
        cache_write_tokens: 500,
        output_tokens: 1_000_000,
        reasoning_tokens: 900,
    };
    let cost = cfg.estimate_cost("model-mid", &usage).unwrap();
    assert!((cost.usd - 2.275).abs() < 1e-9, "{}", cost.usd);
    assert_eq!(cost.basis, swamp::model::core::CostBasis::Estimated);
    assert_eq!(cfg.estimate_cost("model-unpriced", &usage), None);
    assert_eq!(
        cfg.estimate_cost("model-mid", &Usage::default())
            .unwrap()
            .usd,
        0.0
    );
}

#[test]
fn failure_patterns_return_the_matching_line() {
    let sb = Sandbox::new();
    let example = example_config();
    let cfg = sb.load(Some(&example), None).unwrap();
    let pats = cfg.failure_patterns(Provider::Anthropic).unwrap();

    let stderr = "starting up\nError: usage limit reached, resets at 5pm\nexiting";
    assert_eq!(
        pats.rate_limit_match(stderr),
        Some("Error: usage limit reached, resets at 5pm")
    );
    assert_eq!(pats.auth_match(stderr), None);
    assert_eq!(
        pats.overloaded_match("api error: overloaded_error"),
        Some("api error: overloaded_error")
    );
    assert_eq!(pats.rate_limit_match("all good"), None);
    assert_eq!(pats.sources["rate_limit"].len(), 5);

    let none = cfg.failure_patterns(Provider::Openai).unwrap();
    assert!(none.auth_match("not logged in").is_some());
}

#[test]
fn effective_toml_round_trips_and_hashes_stably() {
    let sb = Sandbox::new();
    let example = example_config();
    let cfg = sb.load(Some(&example), None).unwrap();

    let rendered = cfg.effective_toml();
    let reparsed = sb.write("effective.toml", &rendered);
    let again = sb.load(Some(&reparsed), None).unwrap();
    assert_eq!(again.effective_toml(), rendered);
    assert_eq!(again.sha256(), cfg.sha256());
    assert_eq!(cfg.sha256().len(), 64);

    let cheap = sb.load(Some(&example), Some("cheap")).unwrap();
    assert_ne!(cheap.sha256(), cfg.sha256());
}

/// `swamp config init` used to write `<model>` and every tier check passed it through to the
/// vendor CLI, which fails with an error naming neither swamp nor the config key.
#[test]
fn a_template_placeholder_model_is_rejected_by_name() {
    let s = Sandbox::new();
    s.repo_config(
        r#"
[providers.anthropic]
models = { high = "<model>", mid = "sonnet", low = "haiku" }
[[accounts]]
id = "main"
provider = "anthropic"
exec = "claude-main"
"#,
    );
    let e = s.load(None, None).expect_err("the placeholder is refused");
    let text = err_text(&e);
    assert!(text.contains("providers.anthropic.models.high"), "{text}");
}

/// An explicitly empty value reaches the argv as `--permission-mode ''`, which the CLI rejects
/// before it emits anything: the config is the only place it can be caught.
#[test]
fn an_empty_permission_mode_or_sandbox_is_rejected_by_name() {
    let s = Sandbox::new();
    s.repo_config(
        r#"
[brain]
permission_mode = ""
[providers.anthropic]
models = { mid = "sonnet" }
[providers.anthropic.worker]
permission_mode = ""
[providers.openai]
models = { mid = "gpt-5.6-sol" }
[providers.openai.worker]
sandbox = ""
[[accounts]]
id = "main"
provider = "anthropic"
exec = "claude-main"
"#,
    );
    let text = err_text(&s.load(None, None).expect_err("empty values are refused"));
    assert!(text.contains("brain.permission_mode"), "{text}");
    assert!(
        text.contains("providers.anthropic.worker.permission_mode"),
        "{text}"
    );
    assert!(text.contains("providers.openai.worker.sandbox"), "{text}");
}

/// Read-only isolation is enforced through `readonly_args`; every launch path builds its
/// arguments here so none of them can quietly skip it.
#[test]
fn readonly_args_are_appended_only_for_read_only_isolation() {
    let worker = swamp::config::WorkerCfg {
        permission_mode: Some("acceptEdits".into()),
        sandbox: None,
        allow_tools: Vec::new(),
        deny_tools: Vec::new(),
        system_prompt_file: None,
        args: vec!["--verbose".into()],
        readonly_args: vec!["--permission-mode".into(), "plan".into()],
    };
    assert_eq!(
        worker.args_for(IsolationMode::Worktree),
        vec!["--verbose".to_owned()]
    );
    assert_eq!(
        worker.args_for(IsolationMode::ReadOnly),
        vec![
            "--verbose".to_owned(),
            "--permission-mode".to_owned(),
            "plan".to_owned()
        ]
    );
}

/// `providers.<p>.adapter` used to deserialize and then be ignored entirely.
#[test]
fn an_unknown_provider_adapter_is_rejected() {
    let s = Sandbox::new();
    s.repo_config(
        r#"
[providers.anthropic]
adapter = "gemini-cli"
models = { mid = "sonnet" }
[[accounts]]
id = "main"
provider = "anthropic"
exec = "claude-main"
"#,
    );
    let text = err_text(
        &s.load(None, None)
            .expect_err("an unknown adapter is refused"),
    );
    assert!(text.contains("providers.anthropic.adapter"), "{text}");
}
