//! WP3: the openai adapter, argv and the recorded `codex exec --json` stream.

mod common;

use camino::Utf8PathBuf;
use std::collections::BTreeMap;
use std::str::FromStr;
use swamp::ids::{NodeId, NodeIds};
use swamp::model::core::{
    AccountId, ChangeKind, EvidenceSource, NodeKind, Provider, SessionHandle, Tier, Usage,
};
use swamp::model::event::WorkerEvent;
use swamp::model::result::IsolationMode;
use swamp::worker::adapter::McpAttach;
use swamp::worker::{LaunchSpec, ParseState, ProviderAdapter, SessionPlan, adapter_for};

const SAMPLE: &str = "codex-stream-sample.jsonl";

fn codex() -> std::sync::Arc<dyn ProviderAdapter> {
    adapter_for(Provider::Openai)
}

fn spec(cwd: &str) -> LaunchSpec {
    LaunchSpec {
        node: NodeIds {
            id: NodeId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
            session_uuid: uuid::Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef),
        },
        provider: Provider::Openai,
        exec: "codex-main".into(),
        env: BTreeMap::from([("CODEX_HOME".to_owned(), "/tmp/codex".to_owned())]),
        model: "tier-mid-model".into(),
        tier: Tier::Mid,
        cwd: Utf8PathBuf::from(cwd),
        isolation: IsolationMode::Worktree,
        session: SessionPlan::New { preassigned: None },
        kind: NodeKind::Worker,
        permission_mode: "acceptEdits".into(),
        sandbox: "workspace-write".into(),
        append_system_prompt: None,
        allow_tools: Vec::new(),
        deny_tools: Vec::new(),
        mcp: None,
        last_message_path: Utf8PathBuf::from("/tmp/node/last-message.txt"),
        extra_args: Vec::new(),
        extra: Default::default(),
        partial_messages: false,
        attempt: 1,
    }
}

fn argv_of(spec: &LaunchSpec) -> Vec<String> {
    codex()
        .build_argv(spec)
        .expect("argv")
        .into_iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn parse_sample() -> (Vec<WorkerEvent>, ParseState, u32) {
    let a = codex();
    let mut st = ParseState::default();
    let mut events = Vec::new();
    let mut noisy = 0;
    for line in common::fixture_lines(SAMPLE) {
        let po = a.parse_line(&line, &mut st);
        if po.noise {
            noisy += 1;
            assert!(po.events.is_empty(), "noise must not also emit events");
        }
        events.extend(po.events);
    }
    (events, st, noisy)
}

#[test]
fn worker_argv_is_exact_and_ends_with_the_stdin_marker() {
    let (_tmp, repo) = common::tmp_repo();
    let argv = argv_of(&spec(repo.as_str()));
    assert_eq!(
        argv,
        vec![
            "codex-main",
            "exec",
            "--json",
            "-m",
            "tier-mid-model",
            "-C",
            repo.as_str(),
            "-s",
            "workspace-write",
            "-o",
            "/tmp/node/last-message.txt",
            "-c",
            "approval_policy=\"never\"",
            "-",
        ]
    );
    assert_eq!(argv.last().map(String::as_str), Some("-"));
}

/// `codex exec` has no -a/--ask-for-approval: it is top-level only, and passing it here
/// kills every openai worker at argv parsing.
#[test]
fn codex_exec_argv_never_carries_the_top_level_approval_flag() {
    let (_tmp, repo) = common::tmp_repo();
    for s in [
        spec(repo.as_str()),
        {
            let mut s = spec(repo.as_str());
            s.kind = NodeKind::Brain;
            s.mcp = Some(McpAttach {
                command: Utf8PathBuf::from("/usr/local/bin/swamp"),
                args: vec!["mcp-bridge".to_owned()],
            });
            s
        },
        {
            let mut s = spec(repo.as_str());
            s.isolation = IsolationMode::ReadOnly;
            s
        },
    ] {
        let argv = argv_of(&s);
        assert!(!argv.iter().any(|a| a == "-a" || a == "--ask-for-approval"));
        assert!(argv.iter().any(|a| a == "approval_policy=\"never\""));
    }
}

#[test]
fn the_prompt_is_never_on_argv_even_at_two_megabytes() {
    let (_tmp, repo) = common::tmp_repo();
    let prompt = repo.join("prompt.md");
    std::fs::write(&prompt, "x".repeat(2 * 1024 * 1024)).unwrap();
    let argv = argv_of(&spec(repo.as_str()));
    let bytes: usize = argv.iter().map(String::len).sum();
    assert!(bytes < 4096, "argv grew to {bytes} bytes");
}

#[test]
fn brain_argv_declares_the_swamp_mcp_server_through_config_overrides() {
    let (_tmp, repo) = common::tmp_repo();
    let mut s = spec(repo.as_str());
    s.kind = NodeKind::Brain;
    s.mcp = Some(McpAttach {
        command: Utf8PathBuf::from("/usr/local/bin/swamp"),
        args: vec![
            "mcp-bridge".to_owned(),
            "--socket".to_owned(),
            "/tmp/ctl.sock".to_owned(),
        ],
    });
    let argv = argv_of(&s);
    assert!(
        argv.iter()
            .any(|a| a == "mcp_servers.swamp.command=\"/usr/local/bin/swamp\"")
    );
    assert!(
        argv.iter()
            .any(|a| a == "mcp_servers.swamp.args=[\"mcp-bridge\",\"--socket\",\"/tmp/ctl.sock\"]")
    );
}

#[test]
fn resume_is_a_subcommand_and_readonly_switches_the_sandbox() {
    let (_tmp, repo) = common::tmp_repo();
    let mut s = spec(repo.as_str());
    s.session = SessionPlan::Resume(SessionHandle {
        account: AccountId("codex-main".into()),
        id: "01a0a1ec-4635-7720-ab6f-c88e0351c2d6".into(),
        preassigned: false,
    });
    s.isolation = IsolationMode::ReadOnly;
    let argv = argv_of(&s);
    assert_eq!(
        &argv[1..4],
        &["exec", "resume", "01a0a1ec-4635-7720-ab6f-c88e0351c2d6"]
    );
    assert_eq!(
        argv.iter()
            .skip_while(|a| *a != "-s")
            .nth(1)
            .map(String::as_str),
        Some("read-only")
    );
}

#[test]
fn skip_git_repo_check_appears_only_outside_a_repository() {
    let (_tmp, repo) = common::tmp_repo();
    assert!(
        !argv_of(&spec(repo.as_str()))
            .iter()
            .any(|a| a == "--skip-git-repo-check")
    );
    let bare = tempfile::tempdir().unwrap();
    let bare = bare.path().to_str().unwrap();
    assert!(
        argv_of(&spec(bare))
            .iter()
            .any(|a| a == "--skip-git-repo-check")
    );
}

#[test]
fn the_first_stdout_line_is_prose_and_is_noise_not_an_error() {
    let mut st = ParseState::default();
    let po = codex().parse_line("Reading additional input from stdin...", &mut st);
    assert!(po.noise);
    assert!(po.events.is_empty());
}

#[test]
fn the_sample_stream_yields_session_text_and_a_final() {
    let (events, st, noisy) = parse_sample();
    assert_eq!(noisy, 1, "only the prose preamble is noise");
    assert_eq!(events.len(), 3, "{events:#?}");
    assert_eq!(
        events[0],
        WorkerEvent::SessionStarted {
            session: "01a0a1ec-4635-7720-ab6f-c88e0351c2d6".into(),
            model: None,
            auth_hint: None,
        }
    );
    assert_eq!(
        events[1],
        WorkerEvent::AssistantText {
            text: "pong".into()
        }
    );
    let WorkerEvent::Final(f) = &events[2] else {
        panic!("expected a final event");
    };
    assert!(f.ok);
    assert_eq!(f.cost, None, "codex reports no cost; never invent one");
    // Codex reports 15_300 total prompt tokens INCLUDING 12_160 cache reads; Usage keeps the
    // two disjoint the way claude reports them, or estimate_cost bills the cache twice.
    assert_eq!(
        f.usage,
        Usage {
            input_tokens: 3_140,
            cached_input_tokens: 12_160,
            cache_write_tokens: 0,
            output_tokens: 5,
            reasoning_tokens: 0,
        }
    );
    assert_eq!(st.usage, f.usage);
    assert_eq!(st.last_rate_limit, None, "codex carries no quota telemetry");
}

#[test]
fn a_completed_file_change_item_yields_one_event_per_path() {
    let line = r#"{"type":"item.completed","item":{"id":"i1","type":"file_change","changes":[
        {"path":"src/lib.rs","kind":"update"},{"path":"src/new.rs","kind":"add"}]}}"#;
    let mut st = ParseState::default();
    let po = codex().parse_line(line, &mut st);
    assert_eq!(
        po.events.as_slice(),
        [
            WorkerEvent::FileChanged {
                path: Utf8PathBuf::from("src/lib.rs"),
                kind: ChangeKind::Modify
            },
            WorkerEvent::FileChanged {
                path: Utf8PathBuf::from("src/new.rs"),
                kind: ChangeKind::Add
            },
        ]
    );
    assert_eq!(st.files.len(), 2);
    assert!(
        st.files
            .iter()
            .all(|f| f.source == EvidenceSource::EventStream)
    );
}

#[test]
fn an_unknown_type_is_counted_and_never_fatal() {
    let mut st = ParseState::default();
    let po = codex().parse_line(r#"{"type":"turn.rethought","x":1}"#, &mut st);
    assert_eq!(po.events.len(), 1);
    assert!(matches!(po.events[0], WorkerEvent::Unknown { .. }));
    assert_eq!(st.unparsed, 1);

    let po = codex().parse_line(
        r#"{"type":"item.completed","brand_new":1,"item":{"id":"i","type":"agent_message","text":"hi","brand_new":2}}"#,
        &mut st,
    );
    assert_eq!(po.events.len(), 1);
    assert_eq!(st.unparsed, 1, "a new field on a known type is not drift");
}

/// `providers.openai.worker.sandbox` has no built-in default; `-s ''` is rejected by clap
/// with exit 2 before codex reads a single byte of the prompt.
#[test]
fn an_empty_sandbox_drops_the_flag_rather_than_passing_an_empty_argument() {
    let mut s = spec("/tmp/wt");
    s.sandbox = String::new();
    let argv = argv_of(&s);
    assert!(!argv.iter().any(|a| a == "-s"), "{argv:?}");
    assert!(!argv.iter().any(String::is_empty), "{argv:?}");

    // read-only isolation still forces the sandbox regardless of the config.
    s.isolation = IsolationMode::ReadOnly;
    let argv = argv_of(&s);
    let at = argv.iter().position(|a| a == "-s").expect("-s");
    assert_eq!(argv[at + 1], "read-only");
}

/// providers.openai.tier_extra is journaled as applied; it has to actually reach the argv.
#[test]
fn tier_extra_reaches_the_argv_as_config_overrides() {
    let mut s = spec("/tmp/wt");
    s.extra = BTreeMap::from([("model_reasoning_effort".to_owned(), "high".to_owned())]);
    let argv = argv_of(&s);
    assert!(
        argv.iter().any(|a| a == "model_reasoning_effort=\"high\""),
        "{argv:?}"
    );
}

/// WP-B acceptance 6: `thread.failed` used to fall through to `Unknown`, so the thread ended
/// with no Final at all and a hard failure was misread as a truncated stream.
#[test]
fn a_failed_thread_is_a_worker_error_and_not_a_truncated_stream() {
    let mut st = ParseState::default();
    let po = codex().parse_line(
        r#"{"type":"thread.failed","error":{"code":"internal","message":"thread aborted"}}"#,
        &mut st,
    );
    assert!(!po.noise, "a known event type is not drift");
    assert_eq!(st.unparsed, 0);
    let WorkerEvent::Final(f) = &po.events[0] else {
        panic!("expected a final event, got {:?}", po.events);
    };
    assert!(!f.ok);
    assert_eq!(f.subtype, "thread.failed");
    assert_eq!(f.text.as_deref(), Some("thread aborted"));

    let patterns = swamp::config::FailurePatterns {
        rate_limit: regex::RegexSet::empty(),
        auth: regex::RegexSet::empty(),
        overloaded: regex::RegexSet::empty(),
        sources: BTreeMap::new(),
    };
    let failure = codex().classify(&swamp::worker::ExitContext {
        exit: Some(swamp::model::node::ExitInfo {
            code: Some(0),
            signal: None,
            duration_ms: 1,
        }),
        state: &st,
        patterns: &patterns,
        deadline_hit: false,
    });
    match failure {
        Some(swamp::model::failure::Failure::WorkerError { subtype, .. }) => {
            assert_eq!(subtype, "thread.failed");
        }
        other => panic!("expected a worker error, got {other:?}"),
    }
}

/// WP-B acceptance 3: `used_percent` is 0..100 and the scope comes from the window length.
/// `primary` on the `codex` bucket is a seven-day window, whatever its name suggests.
#[test]
fn a_codex_rate_limits_payload_is_one_seven_day_window() {
    use swamp::model::core::LimitScope;
    use swamp::worker::codex_quota;

    let text = std::fs::read_to_string(
        Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/ref/codex-rollout-sample.jsonl"),
    )
    .expect("the rollout fixture");
    let now = time::OffsetDateTime::now_utc();
    let quota = codex_quota::parse_rollout(&text, now)
        .quota
        .expect("rate limits");
    assert_eq!(quota.windows.len(), 1, "a null secondary is not a window");
    assert_eq!(quota.windows[0].scope, LimitScope::SevenDay);
    assert!(
        (quota.windows[0].utilization - 0.32).abs() < 1e-9,
        "0..100 is divided by 100"
    );
    assert!(quota.windows[0].utilization <= 1.0);
    assert!(
        !quota
            .windows
            .iter()
            .any(|w| w.scope == LimitScope::FiveHour),
        "primary is not five_hour by position"
    );
    assert_eq!(
        quota.measured_utilization(),
        Some(quota.worst_utilization())
    );
}

/// swamp.example.toml used to hand the user the same `-c approval_policy="never"` the adapter
/// already emits, so copying it doubled the override on every codex argv.
#[test]
fn the_example_config_does_not_repeat_the_approval_override() {
    let path = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("swamp.example.toml");
    let text = std::fs::read_to_string(&path).expect("swamp.example.toml");
    let schema: swamp::config::Schema = toml::from_str(&text).expect("the example parses");
    let cfg = swamp::config::resolve::from_schema(swamp::config::load::merge(vec![
        swamp::config::load::default_layer(),
        swamp::config::load::Layer {
            origin: "example".into(),
            schema,
        },
    ]));
    let worker = &cfg
        .providers
        .get(&Provider::Openai)
        .expect("the example configures the openai provider")
        .worker;

    let (_tmp, repo) = common::tmp_repo();
    let mut s = spec(repo.as_str());
    s.extra_args = worker.args.clone();
    let argv = argv_of(&s);
    assert_eq!(
        argv.iter()
            .filter(|a| a.starts_with("approval_policy="))
            .count(),
        1,
        "the adapter owns the override: {argv:?}"
    );
}

/// A rollout file is appended to by another process, and the app-server's JSON is provider
/// data too: a countdown past the end of the calendar must drop the reset, not abort the node
/// that was tailing the file.
#[test]
fn an_out_of_range_reset_countdown_drops_the_reset_instead_of_panicking() {
    use swamp::worker::codex_quota;

    let line = r#"{"payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":5.0,"window_minutes":10080,"resets_in_seconds":9000000000000000000}}}}"#;
    let now = time::OffsetDateTime::now_utc();
    let quota = codex_quota::parse_rollout(line, now)
        .quota
        .expect("the window still parses");
    assert_eq!(quota.windows.len(), 1);
    assert!(
        quota.windows[0].resets_at.is_none(),
        "an unrepresentable reset is no reset: {:?}",
        quota.windows[0].resets_at
    );
    assert!((quota.windows[0].utilization - 0.05).abs() < 1e-9);
}

/// USAGE 4.8: `accounts[].limit_id` exists precisely because the auto-pick chose the wrong
/// bucket, and `retry.rs` honours it first. A display probe that preferred the recorded bucket
/// moved the account back onto the bucket the config says it must not use.
#[test]
fn an_explicit_limit_id_wins_over_the_recorded_bucket() {
    use swamp::worker::codex_quota::probe_limit_id;

    let config = |pin: Option<&str>| {
        let text = format!(
            r#"
[providers.openai]
models = {{ high = "a", mid = "b", low = "c" }}

[[accounts]]
id = "codex-main"
provider = "openai"
exec = "codex-main"
{}
"#,
            pin.map(|l| format!("limit_id = \"{l}\""))
                .unwrap_or_default()
        );
        let schema: swamp::config::Schema = toml::from_str(&text).expect("the config parses");
        swamp::config::resolve::from_schema(swamp::config::load::merge(vec![
            swamp::config::load::default_layer(),
            swamp::config::load::Layer {
                origin: "test".into(),
                schema,
            },
        ]))
    };
    let id = AccountId("codex-main".into());

    assert_eq!(
        probe_limit_id(&config(Some("codex")), &id, Some("codex_bengalfox")),
        Some("codex".to_owned()),
        "the configured bucket is the one dispatch bills against"
    );
    assert_eq!(
        probe_limit_id(&config(None), &id, Some("codex_bengalfox")),
        Some("codex_bengalfox".to_owned()),
        "with no pin the probe still reuses the recorded bucket"
    );
    assert_eq!(probe_limit_id(&config(None), &id, None), None);
}
