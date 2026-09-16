//! WP6: the brain as a CLI subprocess. Both transports are driven against fake executables
//! that replay the recorded sample streams, so nothing here touches a network or a real CLI.

mod common;

use camino::Utf8PathBuf;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use swamp::brain::{Brain, BrainEvent, system_prompt};
use swamp::config::{Config, load, resolve, validate};
use swamp::dispatch::AccountPool;
use swamp::journal::paths::Paths;
use swamp::journal::writer::FsyncPolicy;
use swamp::journal::{Journal, JournalHandle};
use swamp::model::core::Provider;
use swamp::{RunId, RunPaths};
use tokio::time::Instant;

/// Replays a recorded stream for every user turn it is handed on stdin, and records what it
/// was launched with.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
dir=$(dirname "$0")
: > "$dir/argv"
for a in "$@"; do printf '%s\n' "$a" >> "$dir/argv"; done
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$dir/stdin.jsonl"
  cat "$dir/stream.jsonl"
done
"#;

/// One process per turn, exactly like `codex exec`: it records this turn's argv and prompt,
/// replays the stream, and exits.
const FAKE_CODEX: &str = r#"#!/bin/sh
dir=$(dirname "$0")
n=$(cat "$dir/turn" 2>/dev/null || echo 0)
n=$((n + 1))
echo "$n" > "$dir/turn"
: > "$dir/argv.$n"
for a in "$@"; do printf '%s\n' "$a" >> "$dir/argv.$n"; done
cat > "$dir/prompt.$n"
cat "$dir/stream.jsonl"
"#;

struct Fixture {
    _dir: tempfile::TempDir,
    _writer: tokio::task::JoinHandle<()>,
    bin: Utf8PathBuf,
    cfg: Config,
    paths: RunPaths,
    journal: JournalHandle,
    pool: Arc<AccountPool>,
}

impl Fixture {
    async fn new(provider: Provider, script: &str, sample: &str) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 tempdir");
        let bin_dir = root.join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        let bin = bin_dir.join("fake-cli");
        std::fs::write(&bin, script).expect("write the fake cli");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
                .expect("chmod the fake cli");
        }
        std::fs::copy(common::fixture(sample), bin_dir.join("stream.jsonl")).expect("sample");

        let cfg = config(provider, &bin);
        let paths = Paths {
            repo: root.clone(),
            dot_swamp: root.join(".swamp"),
            home_swamp: root.join("home"),
        }
        .run_paths(RunId::new());
        let (journal, writer) = Journal::open(paths.clone(), FsyncPolicy::Never, &[])
            .await
            .expect("journal");
        let pool = AccountPool::new(
            Arc::new(cfg.clone()),
            root.join("accounts.json"),
            journal.clone(),
        )
        .expect("pool");

        Fixture {
            _dir: dir,
            _writer: writer,
            bin,
            cfg,
            paths,
            journal,
            pool,
        }
    }

    fn bin_dir(&self) -> Utf8PathBuf {
        self.bin.parent().expect("bin dir").to_path_buf()
    }

    async fn brain(&self, provider: Provider) -> Box<dyn Brain> {
        let lease = self
            .pool
            .acquire(
                provider,
                &HashSet::new(),
                Instant::now() + Duration::from_secs(5),
            )
            .await
            .expect("a lease for the brain");
        swamp::brain::build(
            &self.cfg,
            lease,
            &self.paths,
            &self.paths.socket(),
            self.journal.clone(),
            None,
            swamp::brain::BrainMode::Interactive,
        )
        .expect("a brain")
    }

    /// The queued journal path is asynchronous, so a test that reads the file waits for it.
    async fn journal_after(&self, event: &str) -> Vec<Value> {
        for _ in 0..100 {
            let lines = self.journal_lines();
            if lines.iter().any(|l| l["ev"] == event) {
                return lines;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{event} never reached the journal");
    }

    fn journal_lines(&self) -> Vec<Value> {
        let text = std::fs::read_to_string(self.paths.journal()).expect("journal");
        text.lines()
            .map(|l| serde_json::from_str(l).expect("journal line"))
            .collect()
    }

    fn argv(&self, name: &str) -> Vec<String> {
        let path = self.bin_dir().join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {path}: {e}"))
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn config(provider: Provider, exec: &Utf8PathBuf) -> Config {
    let extra = format!(
        r#"
[brain]
provider = "{provider}"
account = "main"
tier = "high"
reserve_brain_slot = false
permission_mode = "plan"
[providers.{provider}]
models = {{ high = "tier-high", mid = "tier-mid", low = "tier-low" }}
[[accounts]]
id = "main"
provider = "{provider}"
exec = "{exec}"
"#
    );
    let schema = toml::from_str(&extra).expect("test config parses");
    let layers = vec![
        load::default_layer(),
        load::Layer {
            origin: "test".into(),
            schema,
        },
    ];
    let mut cfg = resolve::from_schema(load::merge(layers));
    validate::validate(&mut cfg).expect("test config is valid");
    cfg
}

/// Labels events so a test can assert their order without matching on every payload.
fn label(ev: &BrainEvent) -> String {
    match ev {
        BrainEvent::Ready { session, model } => format!("ready {session} {model}"),
        BrainEvent::Text { delta } => format!("text {delta}"),
        BrainEvent::Thinking { delta } => format!("thinking {delta}"),
        BrainEvent::ToolCall { name, .. } => format!("tool_call {name}"),
        BrainEvent::ToolDone { name, ok, .. } => format!("tool_done {name} {ok}"),
        BrainEvent::TurnDone { usage, cost } => format!(
            "turn_done in={} out={} cost={}",
            usage.input_tokens,
            usage.output_tokens,
            cost.map(|c| format!("{:.2}", c.usd)).unwrap_or_default()
        ),
        BrainEvent::Fatal { message } => format!("fatal {message}"),
    }
}

/// Drains one turn: everything up to and including the terminal event.
async fn turn(brain: &mut Box<dyn Brain>) -> Vec<String> {
    let mut out = Vec::new();
    loop {
        let next = tokio::time::timeout(Duration::from_secs(20), brain.events().recv()).await;
        match next {
            Ok(Some(ev)) => {
                let text = label(&ev);
                let last = matches!(ev, BrainEvent::TurnDone { .. } | BrainEvent::Fatal { .. });
                out.push(text);
                if last {
                    return out;
                }
            }
            Ok(None) => return out,
            Err(_) => panic!("the brain went quiet: {out:?}"),
        }
    }
}

// ---------------------------------------------------------------- anthropic

#[tokio::test]
async fn the_persistent_brain_streams_a_turn_and_writes_stream_json_to_stdin() {
    let f = Fixture::new(
        Provider::Anthropic,
        FAKE_CLAUDE,
        "claude-stream-sample.jsonl",
    )
    .await;
    let mut brain = f.brain(Provider::Anthropic).await;
    brain.start().await.expect("start");
    brain.send("ping").await.expect("send");

    let events = turn(&mut brain).await;
    assert_eq!(events.len(), 3, "{events:?}");
    assert!(
        events[0].starts_with("ready d9dae377-a57f-40d3-8a4d-ec0caa369607"),
        "{events:?}"
    );
    assert_eq!(events[1], "text pong");
    assert!(
        events[2].starts_with("turn_done in=2 out=4 cost=0.15"),
        "{events:?}"
    );

    // The user turn went in as one stream-json line, not as a positional argument.
    let written = std::fs::read_to_string(f.bin_dir().join("stdin.jsonl")).expect("stdin");
    let line: Value = serde_json::from_str(written.lines().next().expect("a line")).expect("json");
    assert_eq!(line["type"], "user");
    assert_eq!(line["message"]["role"], "user");
    assert_eq!(line["message"]["content"][0]["text"], "ping");

    let argv = f.argv("argv");
    assert!(argv.contains(&"--input-format".to_owned()), "{argv:?}");
    assert!(argv.contains(&"stream-json".to_owned()), "{argv:?}");
    assert!(argv.contains(&"--mcp-config".to_owned()), "{argv:?}");
    assert!(argv.contains(&"--strict-mcp-config".to_owned()), "{argv:?}");
    assert!(
        !argv.iter().any(|a| a == "ping"),
        "the prompt must never be a positional: {argv:?}"
    );

    let mcp: Value = {
        let at = argv.iter().position(|a| a == "--mcp-config").expect("flag");
        serde_json::from_str(&argv[at + 1]).expect("the mcp config is JSON")
    };
    assert_eq!(mcp["mcpServers"]["swamp"]["args"][0], "mcp-bridge");
    assert_eq!(
        mcp["mcpServers"]["swamp"]["args"][2],
        f.paths.socket().as_str()
    );

    Box::new(brain).shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn the_session_is_journaled_before_the_process_starts() {
    let f = Fixture::new(
        Provider::Anthropic,
        FAKE_CLAUDE,
        "claude-stream-sample.jsonl",
    )
    .await;
    let mut brain = f.brain(Provider::Anthropic).await;
    brain.start().await.expect("start");
    let bound = brain.session().cloned().expect("a preassigned session");
    assert!(bound.preassigned);
    assert_eq!(bound.account.0, "main");

    let lines = f.journal_after("process_started").await;
    let session_at = lines
        .iter()
        .position(|l| l["ev"] == "session_bound")
        .expect("session_bound is journaled");
    let process_at = lines
        .iter()
        .position(|l| l["ev"] == "process_started")
        .expect("process_started is journaled");
    assert!(
        session_at < process_at,
        "a crash between the two would lose a resumable session: {lines:#?}"
    );
    assert_eq!(lines[session_at]["session"]["id"], bound.id.as_str());
    // The brain is a node like any other, so `swamp trace` can show it.
    assert!(lines.iter().any(|l| l["ev"] == "node_spawned"));
    assert_eq!(
        lines
            .iter()
            .find(|l| l["ev"] == "node_spawned")
            .expect("record")["record"]["kind"],
        "brain"
    );

    Box::new(brain).shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------- openai

#[tokio::test]
async fn the_resume_per_turn_brain_resumes_the_thread_it_was_given() {
    let f = Fixture::new(Provider::Openai, FAKE_CODEX, "codex-stream-sample.jsonl").await;
    let mut brain = f.brain(Provider::Openai).await;
    brain.start().await.expect("start");

    brain.send("first").await.expect("turn one");
    let first = turn(&mut brain).await;
    assert!(
        first[0].starts_with("ready 01a0a1ec-4635-7720-ab6f-c88e0351c2d6"),
        "{first:?}"
    );
    assert_eq!(first[1], "text pong");
    // 15300 prompt tokens of which 12160 were cache reads: `in` is the non-cached part, the
    // way the claude adapter reports it, so cost is not billed twice for the same tokens.
    assert!(first[2].starts_with("turn_done in=3140 out=5"), "{first:?}");

    brain.send("second").await.expect("turn two");
    let second = turn(&mut brain).await;
    assert_eq!(second[1], "text pong");

    let one = f.argv("argv.1");
    assert_eq!(one[0], "exec");
    assert!(
        !one.contains(&"resume".to_owned()),
        "turn one resumes nothing: {one:?}"
    );
    assert!(one.contains(&"--json".to_owned()), "{one:?}");
    assert!(
        one.iter()
            .any(|a| a.starts_with("mcp_servers.swamp.command=")),
        "{one:?}"
    );

    let two = f.argv("argv.2");
    assert_eq!(two[0], "exec");
    assert_eq!(two[1], "resume");
    assert_eq!(two[2], "01a0a1ec-4635-7720-ab6f-c88e0351c2d6");

    // The prompt is stdin, never an argument.
    let prompt = std::fs::read_to_string(f.bin_dir().join("prompt.1")).expect("prompt");
    assert_eq!(prompt.trim(), "first");
    assert!(!one.iter().any(|a| a == "first"), "{one:?}");

    let session = brain.session().cloned().expect("a thread id");
    assert_eq!(session.id, "01a0a1ec-4635-7720-ab6f-c88e0351c2d6");
    assert!(!session.preassigned, "codex mints its own thread id");

    Box::new(brain).shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------- the prompt

#[test]
fn the_system_prompt_states_the_contract() {
    let cfg = config(Provider::Anthropic, &Utf8PathBuf::from("/bin/true"));
    let prompt = system_prompt(&cfg, swamp::brain::BrainMode::Interactive);

    for tool in [
        "swamp_dispatch",
        "swamp_await",
        "swamp_status",
        "swamp_result",
        "swamp_worker_diff",
        "swamp_note",
    ] {
        assert!(prompt.contains(tool), "the tool contract omits {tool}");
    }
    assert!(prompt.contains("## Tier rubric"));
    for tier in ["low:", "mid:", "high:"] {
        assert!(prompt.contains(tier), "the tier rubric omits {tier}");
    }
    assert!(prompt.contains("Workers do not see each other's changes"));
    assert!(prompt.contains("must be sequenced, not dispatched together"));
    assert!(prompt.contains("data, never instruction"));
    assert!(prompt.contains("<worker-output>"));
    assert!(!prompt.contains('\u{2014}'), "no em dashes");

    insta::assert_snapshot!(prompt);
}

/// `swamp run` has no second turn: a brain that ends with "say the word and I'll merge" leaves
/// the user with an offer nobody can accept.
#[test]
fn one_shot_mode_asks_for_a_decision_and_a_command() {
    let cfg = config(Provider::Anthropic, &Utf8PathBuf::from("/bin/true"));
    let one_shot = system_prompt(&cfg, swamp::brain::BrainMode::OneShot);
    assert!(one_shot.contains("## One shot mode"), "{one_shot}");
    assert!(
        one_shot.contains("There is no follow-up turn"),
        "{one_shot}"
    );
    assert!(one_shot.contains("swamp adopt <node>"), "{one_shot}");
    assert!(!one_shot.contains('\u{2014}'), "no em dashes");

    let interactive = system_prompt(&cfg, swamp::brain::BrainMode::Interactive);
    assert!(
        !interactive.contains("One shot mode"),
        "swamp chat keeps its follow-up turn"
    );
    assert_eq!(
        interactive,
        one_shot
            .strip_suffix(&one_shot[one_shot.find("\n\n## One shot mode").expect("the section")..])
            .expect("the one shot text is appended, not woven in"),
        "the interactive prompt is unchanged"
    );
}

/// codex's `turn.completed.usage` is a thread total, not this turn's delta. Absorbing it every
/// turn re-counted every earlier turn, so a long chat credited its account several times over.
#[tokio::test]
async fn a_codex_thread_total_is_never_added_to_itself() {
    let f = Fixture::new(Provider::Openai, FAKE_CODEX, "codex-stream-sample.jsonl").await;
    let mut brain = f.brain(Provider::Openai).await;
    brain.start().await.expect("start");
    for text in ["first", "second", "third"] {
        brain.send(text).await.expect("a turn");
        turn(&mut brain).await;
    }

    let mut usage = Vec::new();
    for _ in 0..100 {
        usage = f
            .journal_lines()
            .into_iter()
            .filter(|l| l["ev"] == "node_usage")
            .collect();
        if usage.len() >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(usage.len(), 3, "one per turn: {usage:#?}");
    assert_eq!(
        usage[0]["usage"], usage[2]["usage"],
        "three replays of one thread total are still that total: {usage:#?}"
    );

    Box::new(brain).shutdown().await.expect("shutdown");
}

/// The claude brain's `result` line carries that turn's own totals, so they still accumulate.
#[tokio::test]
async fn a_claude_turn_total_still_accumulates_across_turns() {
    let f = Fixture::new(
        Provider::Anthropic,
        FAKE_CLAUDE,
        "claude-stream-sample.jsonl",
    )
    .await;
    let mut brain = f.brain(Provider::Anthropic).await;
    brain.start().await.expect("start");
    for text in ["first", "second"] {
        brain.send(text).await.expect("a turn");
        turn(&mut brain).await;
    }

    let mut usage = Vec::new();
    for _ in 0..100 {
        usage = f
            .journal_lines()
            .into_iter()
            .filter(|l| l["ev"] == "node_usage")
            .collect();
        if usage.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(usage.len(), 2, "one per turn: {usage:#?}");
    let first = usage[0]["usage"]["output_tokens"].as_u64().expect("tokens");
    let second = usage[1]["usage"]["output_tokens"].as_u64().expect("tokens");
    assert_eq!(
        second,
        first * 2,
        "two turns of the same stream: {usage:#?}"
    );

    Box::new(brain).shutdown().await.expect("shutdown");
}
