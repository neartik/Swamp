//! WP3: the anthropic adapter, argv and classification, against the recorded sample stream.

mod common;

use camino::Utf8PathBuf;
use regex::RegexSet;
use std::collections::BTreeMap;
use std::str::FromStr;
use swamp::config::FailurePatterns;
use swamp::ids::{NodeId, NodeIds};
use swamp::model::core::{
    ChangeKind, CostBasis, EvidenceSource, FinalSummary, LimitReached, LimitScope, LimitStatus,
    NodeKind, Provider, RateLimitSnapshot, SessionHandle, Tier, Usage,
};
use swamp::model::event::WorkerEvent;
use swamp::model::failure::{Detector, Failure};
use swamp::model::node::ExitInfo;
use swamp::model::result::IsolationMode;
use swamp::worker::adapter::{McpAttach, gate_unsafe_args};
use swamp::worker::classify::MAX_LINE;
use swamp::worker::follow::read_capped_line;
use swamp::worker::{
    ExitContext, LaunchSpec, ParseState, ProviderAdapter, SessionPlan, adapter_for,
};

const SAMPLE: &str = "claude-stream-sample.jsonl";

fn claude() -> std::sync::Arc<dyn ProviderAdapter> {
    adapter_for(Provider::Anthropic)
}

fn spec() -> LaunchSpec {
    LaunchSpec {
        node: NodeIds {
            id: NodeId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
            session_uuid: uuid::Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef),
        },
        provider: Provider::Anthropic,
        exec: "claude-main".into(),
        env: BTreeMap::from([("CLAUDE_CONFIG_DIR".to_owned(), "/tmp/cfg".to_owned())]),
        model: "tier-high-model".into(),
        tier: Tier::High,
        cwd: Utf8PathBuf::from("/tmp/wt"),
        isolation: IsolationMode::Worktree,
        session: SessionPlan::New { preassigned: None },
        kind: NodeKind::Worker,
        permission_mode: "acceptEdits".into(),
        sandbox: "workspace-write".into(),
        append_system_prompt: None,
        allow_tools: Vec::new(),
        deny_tools: Vec::new(),
        mcp: None,
        last_message_path: Utf8PathBuf::from("/tmp/wt/last-message.txt"),
        extra_args: Vec::new(),
        extra: Default::default(),
        partial_messages: false,
        attempt: 1,
    }
}

fn argv_of(spec: &LaunchSpec) -> Vec<String> {
    claude()
        .build_argv(spec)
        .expect("argv")
        .into_iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn patterns() -> FailurePatterns {
    FailurePatterns {
        rate_limit: RegexSet::new([r"(?i)usage limit reached", r"(?i)\b429\b"]).unwrap(),
        auth: RegexSet::new([r"(?i)oauth token has expired"]).unwrap(),
        overloaded: RegexSet::new([r"(?i)overloaded_error"]).unwrap(),
        sources: BTreeMap::new(),
    }
}

fn parse_sample() -> (Vec<WorkerEvent>, ParseState) {
    let a = claude();
    let mut st = ParseState::default();
    let mut events = Vec::new();
    for line in common::fixture_lines(SAMPLE) {
        let po = a.parse_line(&line, &mut st);
        assert!(
            !po.noise,
            "the recorded sample must parse cleanly: {line:.80}"
        );
        events.extend(po.events);
    }
    (events, st)
}

fn final_of(subtype: &str, text: &str) -> FinalSummary {
    FinalSummary {
        ok: subtype == "success",
        subtype: subtype.to_owned(),
        text: Some(text.to_owned()),
        usage: Usage::default(),
        cost: None,
        api_error_status: None,
        num_turns: 1,
        permission_denials: 0,
        denied_tools: Vec::new(),
        ..Default::default()
    }
}

fn classify_with(
    state: &ParseState,
    exit: Option<ExitInfo>,
    deadline_hit: bool,
) -> Option<Failure> {
    let p = patterns();
    claude().classify(&ExitContext {
        exit,
        state,
        patterns: &p,
        deadline_hit,
    })
}

#[test]
fn worker_argv_is_exact_and_carries_no_mcp_config() {
    let argv = argv_of(&spec());
    assert_eq!(
        argv,
        vec![
            "claude-main",
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--model",
            "tier-high-model",
            "--session-id",
            "01234567-89ab-cdef-0123-456789abcdef",
            "--permission-mode",
            "acceptEdits",
            "--permission-prompts",
            "none",
            "--strict-mcp-config",
        ]
    );
    assert!(!argv.iter().any(|a| a == "--mcp-config"));
    assert!(!argv.iter().any(|a| a == "--include-partial-messages"));
}

#[test]
fn the_prompt_is_never_on_argv_even_at_two_megabytes() {
    let tmp = tempfile::tempdir().unwrap();
    let prompt = tmp.path().join("prompt.md");
    std::fs::write(&prompt, "x".repeat(2 * 1024 * 1024)).unwrap();
    let argv = argv_of(&spec());
    let bytes: usize = argv.iter().map(String::len).sum();
    assert!(bytes < 4096, "argv grew to {bytes} bytes");
    assert!(!argv.iter().any(|a| a.len() > 1024));
}

#[test]
fn brain_argv_adds_streaming_stdin_and_an_inline_mcp_config() {
    let mut s = spec();
    s.kind = NodeKind::Brain;
    s.mcp = Some(McpAttach {
        command: Utf8PathBuf::from("/usr/local/bin/swamp"),
        args: vec![
            "mcp-bridge".to_owned(),
            "--socket".to_owned(),
            "/tmp/ctl.sock".to_owned(),
        ],
    });
    s.allow_tools = vec!["Read".to_owned(), "Grep".to_owned()];
    s.partial_messages = true;
    let argv = argv_of(&s);
    for flag in [
        "--input-format",
        "--mcp-config",
        "--strict-mcp-config",
        "--include-partial-messages",
        "--allowed-tools",
    ] {
        assert!(argv.iter().any(|a| a == flag), "brain argv lacks {flag}");
    }
    // brain.include_partial_messages is a real switch, not decoration.
    s.partial_messages = false;
    assert!(
        !argv_of(&s)
            .iter()
            .any(|a| a == "--include-partial-messages"),
        "the flag survived include_partial_messages = false"
    );
    let cfg = argv
        .iter()
        .skip_while(|a| *a != "--mcp-config")
        .nth(1)
        .expect("inline config");
    let parsed: serde_json::Value = serde_json::from_str(cfg).expect("inline mcp config is json");
    assert_eq!(
        parsed["mcpServers"]["swamp"]["command"],
        "/usr/local/bin/swamp"
    );
    assert_eq!(parsed["mcpServers"]["swamp"]["args"][0], "mcp-bridge");
}

/// The brain runs with --permission-prompts none, so a Swamp MCP tool that is not on the
/// allow list is auto-denied and the brain cannot dispatch anything at all.
#[test]
fn the_brain_allows_every_registered_swamp_mcp_tool() {
    let mut s = spec();
    s.kind = NodeKind::Brain;
    s.isolation = IsolationMode::ReadOnly;
    s.mcp = Some(McpAttach {
        command: Utf8PathBuf::from("/usr/local/bin/swamp"),
        args: vec!["mcp-bridge".to_owned()],
    });
    s.allow_tools = vec!["Read".to_owned(), "Grep".to_owned()];
    s.deny_tools = vec!["WebFetch".to_owned()];
    let argv = argv_of(&s);

    let allowed: Vec<&String> = argv
        .iter()
        .skip_while(|a| *a != "--allowed-tools")
        .skip(1)
        .take_while(|a| !a.starts_with("--"))
        .collect();
    let registered = swamp::mcp::tools::qualified_names();
    assert!(!registered.is_empty(), "the tool registry is empty");
    for name in &registered {
        assert!(name.starts_with("mcp__swamp__"), "{name}");
        assert!(
            allowed.contains(&name),
            "{name} is missing from {allowed:?}"
        );
    }
    assert!(allowed.contains(&&"Read".to_owned()), "{allowed:?}");

    // The read-only denials and the configured ones compose into one flag: a repeated
    // variadic option overwrites, which would have dropped one of the two lists.
    let denied: Vec<&String> = argv
        .iter()
        .skip_while(|a| *a != "--disallowed-tools")
        .skip(1)
        .take_while(|a| !a.starts_with("--"))
        .collect();
    assert_eq!(
        denied,
        vec!["Edit", "Write", "MultiEdit", "NotebookEdit", "WebFetch"]
    );
    assert_eq!(
        argv.iter().filter(|a| *a == "--disallowed-tools").count(),
        1
    );
}

/// A worker has no MCP server at all, so it must not be handed MCP tool names either.
#[test]
fn a_worker_gets_no_mcp_allow_list() {
    let argv = argv_of(&spec());
    assert!(!argv.iter().any(|a| a.starts_with("mcp__")), "{argv:?}");
}

/// `LaunchSpec` no longer carries a budget at all, so the flag can never reach argv, resumed
/// session or not.
#[test]
fn argv_never_carries_a_budget_flag() {
    assert!(!argv_of(&spec()).iter().any(|a| a == "--max-budget-usd"));
    let mut s = spec();
    s.session = SessionPlan::Resume(SessionHandle {
        account: swamp::model::core::AccountId("main".into()),
        id: "sess-1".into(),
        preassigned: true,
    });
    assert!(!argv_of(&s).iter().any(|a| a == "--max-budget-usd"));
}

#[test]
fn resume_flags_appear_only_when_asked_for() {
    let plain = argv_of(&spec());
    assert!(!plain.iter().any(|a| a == "--resume"));

    let mut s = spec();
    s.session = SessionPlan::Resume(SessionHandle {
        account: swamp::model::core::AccountId("main".into()),
        id: "sess-1".into(),
        preassigned: true,
    });
    let argv = argv_of(&s);
    assert_eq!(
        argv.iter()
            .skip_while(|a| *a != "--resume")
            .nth(1)
            .map(String::as_str),
        Some("sess-1")
    );
    assert!(!argv.iter().any(|a| a == "--session-id"));
}

#[test]
fn a_system_prompt_file_is_passed_as_text_not_as_a_path() {
    let mut s = spec();
    s.append_system_prompt = Some("you are a careful worker".to_owned());
    let argv = argv_of(&s);
    assert!(!argv.iter().any(|a| a == "--append-system-prompt-file"));
    assert_eq!(
        argv.iter()
            .skip_while(|a| *a != "--append-system-prompt")
            .nth(1)
            .map(String::as_str),
        Some("you are a careful worker")
    );
}

#[test]
fn readonly_isolation_denies_the_writing_tools() {
    let mut s = spec();
    s.isolation = IsolationMode::ReadOnly;
    let argv = argv_of(&s);
    let denied: Vec<&String> = argv
        .iter()
        .skip_while(|a| *a != "--disallowed-tools")
        .skip(1)
        .collect();
    assert_eq!(denied, vec!["Edit", "Write", "MultiEdit", "NotebookEdit"]);
}

#[test]
fn dangerously_skip_permissions_is_dropped_unless_unsafe_ack_is_set() {
    let args = vec![
        "--dangerously-skip-permissions".to_owned(),
        "--effort".to_owned(),
    ];
    let (kept, refused) = gate_unsafe_args(&args, false);
    assert_eq!(kept, vec!["--effort"]);
    assert_eq!(refused, vec!["--dangerously-skip-permissions"]);

    let (kept, refused) = gate_unsafe_args(&args, true);
    assert_eq!(kept, args);
    assert!(refused.is_empty());

    let mut s = spec();
    s.extra_args = gate_unsafe_args(&args, true).0;
    assert!(
        argv_of(&s)
            .iter()
            .any(|a| a == "--dangerously-skip-permissions")
    );
}

#[test]
fn the_sample_stream_yields_the_documented_event_sequence() {
    let (events, st) = parse_sample();
    let shape: Vec<&str> = events
        .iter()
        .map(|e| match e {
            WorkerEvent::SessionStarted { .. } => "session",
            WorkerEvent::RateLimit(_) => "rate_limit",
            WorkerEvent::AssistantText { .. } => "text",
            WorkerEvent::Usage(_) => "usage",
            WorkerEvent::Final(_) => "final",
            other => panic!("unexpected event {other:?}"),
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            "session",
            "rate_limit",
            "text",
            "usage",
            "rate_limit",
            "final"
        ]
    );
    assert!(matches!(
        &events[0],
        WorkerEvent::SessionStarted { auth_hint: Some(h), .. } if h == "none"
    ));
    assert_eq!(st.unparsed, 0);
    assert_eq!(
        st.session.as_deref(),
        Some("d9dae377-a57f-40d3-8a4d-ec0caa369607")
    );
}

#[test]
fn a_rate_limit_event_keeps_every_window() {
    let (events, _) = parse_sample();
    let WorkerEvent::RateLimit(snap) = &events[1] else {
        panic!("expected a rate limit event");
    };
    assert_eq!(snap.status, LimitStatus::Allowed);
    assert_eq!(snap.windows.len(), 2, "collapsing to one window is a bug");
    assert_eq!(snap.windows[0].scope, LimitScope::FiveHour);
    assert_eq!(snap.windows[0].utilization, 0.06);
    assert_eq!(snap.windows[1].scope, LimitScope::SevenDay);
    assert_eq!(snap.windows[1].utilization, 0.64);
    assert_eq!(snap.worst_utilization(), 0.64);
    assert_eq!(snap.worst_scope(), LimitScope::SevenDay);
}

fn rate_limits(events: &[WorkerEvent]) -> Vec<RateLimitSnapshot> {
    events
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::RateLimit(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

/// A `rate_limit_event` without `unifiedWindows` carries no measurement. Synthesising a 0%
/// window from `rateLimitType` reads as a wide-open allowance and erases the real one.
#[test]
fn an_event_without_unified_windows_reports_no_window_at_all() {
    let a = claude();
    let mut st = ParseState::default();
    let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1789440000,"rateLimitType":"five_hour"}}"#;
    let snap = rate_limits(&a.parse_line(line, &mut st).events)
        .pop()
        .expect("a rate limit event");
    assert!(
        snap.windows.is_empty(),
        "a fabricated 0% window would overwrite the stored one: {:?}",
        snap.windows
    );
    assert_eq!(snap.status, LimitStatus::Rejected);
    assert_eq!(snap.limit_id.as_deref(), Some("five_hour"));
}

/// `overageStatus` and `overageDisabledReason` are plan attributes: the reference fixture
/// carries `rejected`/`member_zero_credit_limit` on an *allowed* account. Reading them as a
/// depletion event turned every ordinary five-hour limit into a permanent hard gate.
#[test]
fn a_plan_with_no_overage_is_rate_limited_not_depleted() {
    let a = claude();
    let mut st = ParseState::default();
    let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1789440000,"rateLimitType":"five_hour","overageStatus":"rejected","overageDisabledReason":"member_zero_credit_limit","isUsingOverage":false,"unifiedWindows":{"five_hour":{"utilization":1.0,"resetsAt":1789440000}}}}"#;
    let snap = rate_limits(&a.parse_line(line, &mut st).events)
        .pop()
        .expect("a rate limit event");
    assert_eq!(snap.status, LimitStatus::Rejected);
    assert_eq!(
        snap.reached,
        Some(LimitReached::RateLimit),
        "a window that resets is not depleted credits"
    );
}

/// Credits only ran out once the account was actually drawing on overage when refused.
#[test]
fn a_rejection_while_using_overage_is_depletion() {
    let a = claude();
    let mut st = ParseState::default();
    let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1789440000,"rateLimitType":"five_hour","overageStatus":"rejected","overageDisabledReason":"credit_limit_reached","isUsingOverage":true}}"#;
    let snap = rate_limits(&a.parse_line(line, &mut st).events)
        .pop()
        .expect("a rate limit event");
    assert_eq!(snap.reached, Some(LimitReached::CreditsDepleted));
}

/// WP-B acceptance 1: the sample's second event adds `seven_day_overage_included`, which is
/// not a plan limit; maxing it in used to drive Degraded off an overage window.
#[test]
fn an_overage_window_never_decides_the_worst_utilization() {
    let (events, _) = parse_sample();
    let snap = rate_limits(&events).pop().expect("a rate limit event");
    assert_eq!(snap.windows.len(), 3, "the overage window is kept");
    assert_eq!(snap.limit_id.as_deref(), Some("five_hour"));
    assert!(snap.reached.is_none(), "an allowed account is not depleted");
    let overage = snap
        .windows
        .iter()
        .find(|w| w.scope == LimitScope::Unknown)
        .expect("the overage window");
    assert_eq!(overage.utilization, 0.07);
    assert_eq!(overage.window_minutes, None, "no named size for it");
    assert_eq!(snap.worst_utilization(), 0.64);
    assert_eq!(snap.measured_utilization(), Some(0.64));
    assert_eq!(snap.tightest().map(|w| w.scope), Some(LimitScope::SevenDay));
    for w in &snap.windows {
        assert!(w.measured, "telemetry is a measurement, never an estimate");
    }
    let five = &snap.windows[0];
    assert_eq!(five.scope, LimitScope::FiveHour);
    assert_eq!(five.window_minutes, Some(300));
    assert_eq!(
        snap.windows
            .iter()
            .find(|w| w.scope == LimitScope::SevenDay)
            .and_then(|w| w.window_minutes),
        Some(10_080)
    );
}

/// The window set varies between two events of one run, so a snapshot merges per scope: a
/// key dropping out of the newer one must not erase what it measured.
#[test]
fn a_later_snapshot_does_not_erase_a_window_it_omits() {
    let (events, _) = parse_sample();
    let full = &rate_limits(&events).pop().expect("a rate limit event");
    let five_only = RateLimitSnapshot {
        status: LimitStatus::Allowed,
        windows: vec![
            full.windows
                .iter()
                .find(|w| w.scope == LimitScope::FiveHour)
                .expect("five hour")
                .clone(),
        ],
        ..Default::default()
    };
    // Before the sample's own reset instants, so nothing in it counts as already rolled.
    let now = time::OffsetDateTime::from_unix_timestamp(1_789_000_000).expect("sample epoch");
    let merged = five_only.merged_over(full, now);
    assert_eq!(merged.windows.len(), 3);
    assert_eq!(merged.worst_utilization(), 0.64);
}

/// WP-B acceptance 2: `result.usage` is the main model alone. The account paid for the haiku
/// side-call too, and only `modelUsage` reports it.
#[test]
fn the_account_total_sums_every_model_the_run_billed() {
    let (events, _) = parse_sample();
    let WorkerEvent::Final(f) = events.last().expect("final") else {
        panic!("last event is not final");
    };
    assert_eq!(f.model_usage.len(), 2, "two models billed");
    let account = f.account_usage();
    assert_eq!(account.input_tokens, f.usage.input_tokens + 899);
    assert_eq!(account.output_tokens, f.usage.output_tokens + 12);
    assert_eq!(account.cached_input_tokens, f.usage.cached_input_tokens);
    assert_eq!(account.cache_write_tokens, f.usage.cache_write_tokens);
    assert!(account.billable() > f.usage.billable());
}

#[test]
fn the_final_event_carries_reported_cost_and_run_totals() {
    let (events, st) = parse_sample();
    let WorkerEvent::Final(f) = events.last().expect("final") else {
        panic!("last event is not final");
    };
    assert!(f.ok);
    assert_eq!(f.subtype, "success");
    assert_eq!(f.permission_denials, 0);
    let cost = f.cost.expect("reported cost");
    assert_eq!(cost.basis, CostBasis::Reported);
    assert!((cost.usd - 0.153_239).abs() < 1e-9, "{}", cost.usd);
    assert_eq!(
        f.usage,
        Usage {
            input_tokens: 2,
            cached_input_tokens: 10_480,
            cache_write_tokens: 7_472,
            output_tokens: 4,
            reasoning_tokens: 0,
        }
    );
    assert_eq!(
        st.usage, f.usage,
        "the result line replaces the running sum"
    );
}

#[test]
fn an_assistant_message_with_text_and_a_tool_use_yields_two_events() {
    let line = r#"{"type":"assistant","message":{"content":[
        {"type":"text","text":"looking"},
        {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls -la"}}]}}"#;
    let mut st = ParseState::default();
    let po = claude().parse_line(line, &mut st);
    assert_eq!(
        po.events.len(),
        2,
        "returning only the first block drops work"
    );
    assert_eq!(
        po.events[0],
        WorkerEvent::AssistantText {
            text: "looking".into()
        }
    );
    assert_eq!(
        po.events[1],
        WorkerEvent::ToolCall {
            id: "t1".into(),
            name: "Bash".into(),
            summary: "ls -la".into()
        }
    );
    assert_eq!(st.tool_names.get("t1").map(String::as_str), Some("Bash"));
}

#[test]
fn an_edit_tool_use_is_advisory_evidence_of_a_file_change() {
    let line = r#"{"type":"assistant","message":{"content":[
        {"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"src/lib.rs"}}]}}"#;
    let mut st = ParseState::default();
    let po = claude().parse_line(line, &mut st);
    assert!(po.events.contains(&WorkerEvent::FileChanged {
        path: Utf8PathBuf::from("src/lib.rs"),
        kind: ChangeKind::Modify,
    }));
    assert_eq!(st.files.len(), 1);
    assert_eq!(st.files[0].source, EvidenceSource::EventStream);
}

#[test]
fn an_unknown_type_is_counted_and_never_fatal() {
    let mut st = ParseState::default();
    let po = claude().parse_line(r#"{"type":"future_thing","x":1}"#, &mut st);
    assert_eq!(po.events.len(), 1);
    assert!(matches!(po.events[0], WorkerEvent::Unknown { .. }));
    assert_eq!(st.unparsed, 1);
}

#[test]
fn an_unknown_field_on_a_known_type_is_ignored() {
    let mut st = ParseState::default();
    let po = claude().parse_line(
        r#"{"type":"assistant","brand_new":7,"message":{"brand_new":8,
            "content":[{"type":"text","text":"hi","brand_new":9}]}}"#,
        &mut st,
    );
    assert_eq!(po.events.len(), 1);
    assert_eq!(st.unparsed, 0);
}

#[test]
fn a_twenty_megabyte_line_is_capped_before_the_parser_sees_it() {
    let blob = "a".repeat(20 * 1024 * 1024);
    let line = format!("{{\"type\":\"assistant\",\"blob\":\"{blob}\"}}\n");
    let total = line.len();
    let mut buf = Vec::new();
    let capped = tokio_test::block_on(async {
        let mut rdr = tokio::io::BufReader::new(line.as_bytes());
        read_capped_line(&mut rdr, &mut buf, MAX_LINE)
            .await
            .unwrap()
    });
    assert_eq!(capped.consumed as usize, total);
    assert!(capped.complete && capped.truncated);
    assert_eq!(
        buf.len(),
        MAX_LINE,
        "the reader caps before anything parses"
    );

    let mut st = ParseState::default();
    let po = claude().parse_line(std::str::from_utf8(&buf).unwrap(), &mut st);
    assert!(po.noise);
    assert!(po.events.is_empty());
}

#[test]
fn telemetry_outranks_a_clean_exit() {
    let st = ParseState {
        last_rate_limit: Some(RateLimitSnapshot {
            status: LimitStatus::Rejected,
            windows: vec![swamp::model::core::LimitWindow {
                scope: LimitScope::SevenDay,
                utilization: 1.0,
                ..Default::default()
            }],
            resets_at: None,
            ..Default::default()
        }),
        ..ParseState::default()
    };
    let f = classify_with(
        &st,
        Some(ExitInfo {
            code: Some(0),
            signal: None,
            duration_ms: 1,
        }),
        false,
    );
    assert!(matches!(
        f,
        Some(Failure::RateLimited {
            detected_by: Detector::Telemetry,
            scope: LimitScope::SevenDay,
            ..
        })
    ));
}

#[test]
fn api_error_status_drives_rate_limit_and_overload() {
    for (status, want_rate_limited) in [(429i64, true), (529, false), (503, false)] {
        let mut f = final_of("error_during_execution", "");
        f.api_error_status = Some(status);
        let st = ParseState {
            last_final: Some(f),
            ..ParseState::default()
        };
        match classify_with(&st, None, false) {
            Some(Failure::RateLimited { detected_by, .. }) => {
                assert!(want_rate_limited);
                assert_eq!(detected_by, Detector::StructuredResult);
            }
            Some(Failure::Overloaded { .. }) => assert!(!want_rate_limited),
            other => panic!("{status} classified as {other:?}"),
        }
    }
}

#[test]
fn the_budget_subtype_is_our_own_guard_not_a_provider_failure() {
    let f = final_of("error_max_budget_usd", "");
    let st = ParseState {
        last_final: Some(f),
        ..ParseState::default()
    };
    let failure = classify_with(&st, None, false).expect("a failure");
    assert!(
        matches!(failure, Failure::WorkerError { ref subtype, .. } if subtype == "error_max_budget_usd")
    );
    assert!(failure.is_terminal());
}

#[test]
fn denials_fail_a_node_that_reports_success() {
    let mut f = final_of("success", "all done");
    f.permission_denials = 1;
    f.denied_tools = vec!["Bash".to_owned()];
    let st = ParseState {
        last_final: Some(f),
        ..ParseState::default()
    };
    assert_eq!(
        classify_with(&st, None, false),
        Some(Failure::PermissionDenied {
            denials: 1,
            tools: vec!["Bash".to_owned()],
        })
    );
}

#[test]
fn a_clean_result_is_not_a_failure() {
    let st = ParseState {
        last_final: Some(final_of("success", "done")),
        ..ParseState::default()
    };
    assert_eq!(classify_with(&st, None, false), None);
}

#[test]
fn a_configured_regex_fires_last_and_records_its_evidence() {
    let long = format!("prelude\n{} usage limit reached", "z".repeat(600));
    let st = ParseState {
        last_final: Some(final_of("error_during_execution", &long)),
        ..ParseState::default()
    };
    let Some(Failure::RateLimited {
        detected_by,
        evidence,
        ..
    }) = classify_with(&st, None, false)
    else {
        panic!("expected a pattern-detected rate limit");
    };
    assert_eq!(detected_by, Detector::Pattern);
    assert_eq!(evidence.len(), 400);
    assert!(evidence.starts_with("zzz"));
}

#[test]
fn error_during_execution_is_terminal_and_never_rotates_the_account() {
    let st = ParseState {
        last_final: Some(final_of("error_during_execution", "the build failed")),
        ..ParseState::default()
    };
    let failure = classify_with(&st, None, false).expect("a failure");
    assert_eq!(
        failure,
        Failure::WorkerError {
            subtype: "error_during_execution".into(),
            detail: "the build failed".into(),
        }
    );
    assert!(
        !failure.rotates_account(),
        "a bad prompt must not drain every subscription"
    );
    assert!(failure.is_terminal());
}

#[test]
fn a_stream_with_no_terminal_event_falls_back_to_the_exit_status() {
    let st = ParseState::default();
    let cases = [
        (
            ExitInfo {
                code: None,
                signal: Some(9),
                duration_ms: 1,
            },
            Failure::Crashed { signal: Some(9) },
        ),
        (
            ExitInfo {
                code: Some(0),
                signal: None,
                duration_ms: 1,
            },
            Failure::Truncated { offset: 0 },
        ),
        (
            ExitInfo {
                code: Some(127),
                signal: None,
                duration_ms: 1,
            },
            Failure::AuthExpired {
                detail: "exec not found (127)".into(),
                detected_by: Detector::ExitCode,
            },
        ),
    ];
    for (exit, want) in cases {
        assert_eq!(classify_with(&st, Some(exit), false), Some(want));
    }
    assert!(matches!(
        classify_with(&st, None, true),
        Some(Failure::Timeout { .. })
    ));
}

#[test]
fn stderr_is_searched_when_the_stream_says_nothing() {
    let st = ParseState {
        stderr_tail: ["boom", "OAuth token has expired"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        ..ParseState::default()
    };
    assert!(matches!(
        classify_with(
            &st,
            Some(ExitInfo {
                code: Some(1),
                signal: None,
                duration_ms: 1
            }),
            false
        ),
        Some(Failure::AuthExpired {
            detected_by: Detector::Pattern,
            ..
        })
    ));
}

/// An unset config key becomes an empty string; `--permission-mode ''` is an argv error that
/// kills claude before it emits one line, so the flag pair is dropped instead.
#[test]
fn an_empty_permission_mode_drops_the_flag_rather_than_passing_an_empty_argument() {
    let mut s = spec();
    s.permission_mode = String::new();
    let argv = argv_of(&s);
    assert!(!argv.iter().any(|a| a == "--permission-mode"), "{argv:?}");
    assert!(!argv.iter().any(String::is_empty), "{argv:?}");
    // The flag is still emitted when the key is set.
    s.permission_mode = "plan".into();
    let argv = argv_of(&s);
    let at = argv.iter().position(|a| a == "--permission-mode").unwrap();
    assert_eq!(argv[at + 1], "plan");
}

/// providers.*.tier_extra is journaled as applied; it has to actually reach the argv.
#[test]
fn tier_extra_reaches_the_argv_as_flags() {
    let mut s = spec();
    s.extra = BTreeMap::from([("effort".to_owned(), "high".to_owned())]);
    let argv = argv_of(&s);
    let at = argv
        .iter()
        .position(|a| a == "--effort")
        .unwrap_or_else(|| panic!("{argv:?}"));
    assert_eq!(argv[at + 1], "high");
}

/// A `stream_event` chunk is a partial message, not an unparsable line: counting thousands of
/// them as noise inflates the journal tenfold and tells the operator nothing.
#[test]
fn partial_message_chunks_are_neither_noise_nor_events() {
    let a = claude();
    let mut st = ParseState::default();
    let po = a.parse_line(
        r#"{"type":"stream_event","event":{"type":"content_block_delta"}}"#,
        &mut st,
    );
    assert!(po.events.is_empty());
    assert!(!po.noise);
    assert_eq!(st.unparsed, 0);
}

/// An argv error exits before the first stream line. Retrying replays it three times over,
/// so it is terminal and carries the stderr the operator needs.
#[test]
fn a_process_that_dies_before_its_first_line_is_a_terminal_launch_failure() {
    let st = ParseState {
        stderr_tail: ["error: option '--permission-mode <mode>' argument '' is invalid".to_owned()]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let f = classify_with(
        &st,
        Some(ExitInfo {
            code: Some(1),
            signal: None,
            duration_ms: 3,
        }),
        false,
    )
    .expect("a failure");
    match &f {
        Failure::WorkerError { subtype, detail } => {
            assert_eq!(subtype, "launch_failed");
            assert!(detail.contains("--permission-mode"), "{detail}");
        }
        other => panic!("{other:?}"),
    }
    assert!(f.is_terminal(), "an argv error must never be retried");
}

/// `swamp resume` re-attaches to a worker and credits its account from the stream alone. The
/// main model's line is not what the subscription paid: the side-calls are billed to it too.
#[test]
fn a_recovered_run_credits_the_account_total_and_not_the_main_model() {
    let (_events, st) = parse_sample();
    let credited = swamp::worker::account_total(&st);
    let f = st.last_final.as_ref().expect("a final summary");
    assert_eq!(credited, f.account_usage());
    assert!(
        credited.billable() > st.usage.billable(),
        "the side-call is missing from what resume would credit"
    );
}
