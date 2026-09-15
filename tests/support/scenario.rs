//! The script one fake CLI invocation follows.
//!
//! Shared by `fake_claude`, `fake_codex` and the harness, so it may only use crates from
//! `[dependencies]`: the fakes are ordinary bins and cannot see dev-dependencies.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// Where the fakes look for `<wrapper-name>.json` and where they append their invocation log.
pub const FAKE_DIR: &str = "SWAMP_FAKE_DIR";

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Scenario {
    /// Lines written to stdout, verbatim.
    pub emit: Vec<String>,
    pub exit_code: i32,
    /// Pause before every emitted line, so a test can stop a worker mid-stream.
    pub delay_ms: u64,
    /// Files the fake writes into its cwd.
    pub edit: Vec<(String, String)>,
    /// Every entry must appear in argv; the fake exits 97 if one does not.
    pub expect_argv: Vec<String>,
    pub stderr: Vec<String>,
    /// What to leave in the file named by `codex exec -o`.
    pub last_message: Option<String>,
    /// Brain mode: dispatched through the MCP bridge before the turn ends.
    pub dispatch: Vec<BrainTask>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainTask {
    pub title: String,
    pub prompt: String,
}

/// One recorded call of a fake, appended to `<wrapper-name>.invocations.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invocation {
    pub argv: Vec<String>,
    pub cwd: String,
    pub stdin: String,
    pub node: Option<String>,
}

impl Invocation {
    /// The value that follows `flag` in argv, if any.
    pub fn arg(&self, flag: &str) -> Option<&str> {
        let i = self.argv.iter().position(|a| a == flag)?;
        self.argv.get(i + 1).map(String::as_str)
    }
}

impl Scenario {
    /// The recorded claude stream, replayed byte for byte.
    pub fn claude() -> Scenario {
        Scenario {
            emit: fixture_lines("claude-stream-sample.jsonl"),
            ..Scenario::default()
        }
    }

    /// The recorded codex stream, including its non-JSON first line.
    pub fn codex() -> Scenario {
        Scenario {
            emit: fixture_lines("codex-stream-sample.jsonl"),
            last_message: Some("pong".to_owned()),
            ..Scenario::default()
        }
    }

    /// Writes `content` into `path`, relative to the worker's cwd.
    pub fn edits(mut self, path: &str, content: &str) -> Scenario {
        self.edit.push((path.to_owned(), content.to_owned()));
        self
    }

    pub fn expects(mut self, arg: &str) -> Scenario {
        self.expect_argv.push(arg.to_owned());
        self
    }

    pub fn exits(mut self, code: i32) -> Scenario {
        self.exit_code = code;
        self
    }

    pub fn slow(mut self, delay_ms: u64) -> Scenario {
        self.delay_ms = delay_ms;
        self
    }

    pub fn says(mut self, line: &str) -> Scenario {
        self.stderr.push(line.to_owned());
        self
    }

    pub fn dispatches(mut self, title: &str, prompt: &str) -> Scenario {
        self.dispatch.push(BrainTask {
            title: title.to_owned(),
            prompt: prompt.to_owned(),
        });
        self
    }

    /// A subscription that has hit its five-hour window: the CLI reports the rejection in
    /// telemetry and then dies, which is what the real one does.
    pub fn claude_rate_limited(resets_at: i64) -> Scenario {
        let mut s = Scenario::claude();
        s.emit.truncate(1);
        s.emit.push(rate_limit_line("rejected", resets_at));
        s.exit_code = 1;
        s.stderr.push("Claude usage limit reached".to_owned());
        s
    }

    /// A task that failed on its own merits. Never an account problem.
    pub fn claude_task_error() -> Scenario {
        let mut s = Scenario::claude();
        s.emit.pop();
        s.emit.push(result_line(
            "error_during_execution",
            true,
            "2 tests still failing",
            0,
        ));
        s
    }

    /// `subtype: "success"` with denials: the worker did not do the work it claims.
    pub fn claude_permission_denied(denials: u32) -> Scenario {
        let tools = vec!["Edit"; denials as usize];
        Scenario::claude_denied_tools(&tools)
    }

    /// The same, with the tool names the CLI reports in `permission_denials[].tool_name`.
    pub fn claude_denied_tools(tools: &[&str]) -> Scenario {
        let mut s = Scenario::claude();
        s.emit.pop();
        s.emit
            .push(result_line_tools("success", false, "all done", tools));
        s
    }

    /// Emits a token-shaped string in assistant text, in the summary and on stderr.
    pub fn claude_leaks(secret: &str) -> Scenario {
        let mut s = Scenario::claude();
        s.emit.pop();
        s.emit.push(assistant_text_line(&format!(
            "exported ANTHROPIC_API_KEY={secret} for the test"
        )));
        s.emit.push(result_line(
            "success",
            false,
            &format!("ANTHROPIC_API_KEY={secret}"),
            0,
        ));
        s.stderr
            .push(format!("warning: ANTHROPIC_API_KEY={secret} is set"));
        s
    }
}

pub fn rate_limit_line(status: &str, resets_at: i64) -> String {
    serde_json::json!({
        "type": "rate_limit_event",
        "rate_limit_info": {
            "status": status,
            "resetsAt": resets_at,
            "rateLimitType": "five_hour",
            "unifiedWindows": {
                "five_hour": { "utilization": 1.0, "resetsAt": resets_at },
                "seven_day": { "utilization": 0.64, "resetsAt": resets_at }
            }
        },
        "session_id": "d9dae377-a57f-40d3-8a4d-ec0caa369607"
    })
    .to_string()
}

pub fn assistant_text_line(text: &str) -> String {
    serde_json::json!({
        "type": "assistant",
        "message": {
            "role": "assistant",
            "type": "message",
            "content": [{ "type": "text", "text": text }],
            "usage": { "input_tokens": 12, "output_tokens": 3 }
        },
        "session_id": "d9dae377-a57f-40d3-8a4d-ec0caa369607"
    })
    .to_string()
}

pub fn result_line(subtype: &str, is_error: bool, text: &str, denials: u32) -> String {
    let tools = vec!["Edit"; denials as usize];
    result_line_tools(subtype, is_error, text, &tools)
}

pub fn result_line_tools(subtype: &str, is_error: bool, text: &str, tools: &[&str]) -> String {
    let denials: Vec<serde_json::Value> = tools
        .iter()
        .enumerate()
        .map(|(i, tool)| serde_json::json!({ "tool_name": tool, "tool_use_id": format!("toolu_{i}") }))
        .collect();
    serde_json::json!({
        "type": "result",
        "subtype": subtype,
        "is_error": is_error,
        "result": text,
        "total_cost_usd": 0.0123,
        "num_turns": 1,
        "permission_denials": denials,
        "session_id": "d9dae377-a57f-40d3-8a4d-ec0caa369607",
        "usage": { "input_tokens": 2, "output_tokens": 4,
                   "cache_read_input_tokens": 10480, "cache_creation_input_tokens": 7472 }
    })
    .to_string()
}

/// docs/ref/<name>, resolved against the crate root so it works from any cwd.
pub fn fixture_lines(name: &str) -> Vec<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("docs")
        .join("ref")
        .join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading fixture {}: {e}", path.display()));
    text.lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

// ---------------------------------------------------------------- fake runtime
//
// Everything below runs inside `fake_claude` / `fake_codex`, never in a test.

use std::io::{Read, Write};

/// The wrapper name the fake was invoked as: one per subscription, so it selects the script.
pub fn wrapper_name(argv: &[String]) -> String {
    if let Ok(name) = std::env::var("SWAMP_FAKE_SCENARIO") {
        return name;
    }
    argv.first()
        .and_then(|a| a.rsplit('/').next())
        .unwrap_or("fake")
        .to_owned()
}

pub fn fake_dir() -> std::path::PathBuf {
    match std::env::var(FAKE_DIR) {
        Ok(dir) => std::path::PathBuf::from(dir),
        Err(_) => die(&format!("{FAKE_DIR} is not set")),
    }
}

pub fn load(dir: &std::path::Path, name: &str) -> Scenario {
    let path = dir.join(format!("{name}.json"));
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Scenario::default();
    };
    match serde_json::from_str(&text) {
        Ok(s) => s,
        Err(e) => die(&format!("unreadable scenario {}: {e}", path.display())),
    }
}

pub fn read_stdin() -> String {
    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);
    buf
}

/// Appends the call to `<name>.invocations.jsonl` and returns its 1-based ordinal.
pub fn record(dir: &std::path::Path, name: &str, argv: &[String], stdin: &str) -> u64 {
    let path = dir.join(format!("{name}.invocations.jsonl"));
    let record = Invocation {
        argv: argv.to_vec(),
        cwd: std::env::current_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_default(),
        stdin: stdin.to_owned(),
        node: std::env::var("SWAMP_NODE").ok(),
    };
    append(&path, &serde_json::to_string(&record).unwrap_or_default());
    std::fs::read_to_string(&path)
        .map(|t| t.lines().filter(|l| !l.trim().is_empty()).count() as u64)
        .unwrap_or(1)
}

pub fn append(path: &std::path::Path, line: &str) {
    let mut body = line.to_owned();
    body.push('\n');
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(body.as_bytes());
    }
}

pub fn check_argv(argv: &[String], s: &Scenario) {
    for want in &s.expect_argv {
        if !argv.iter().any(|a| a == want) {
            die(&format!("expected `{want}` in argv, got {argv:?}"));
        }
    }
}

pub fn flag(argv: &[String], name: &str) -> Option<String> {
    let i = argv.iter().position(|a| a == name)?;
    argv.get(i + 1).cloned()
}

pub fn subst(s: &str, n: u64) -> String {
    let node = std::env::var("SWAMP_NODE").unwrap_or_default();
    s.replace("{n}", &n.to_string()).replace("{node}", &node)
}

pub fn apply_edits(s: &Scenario, n: u64) {
    for (path, content) in &s.edit {
        let path = std::path::PathBuf::from(subst(path, n));
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&path, subst(content, n)) {
            die(&format!("cannot write {}: {e}", path.display()));
        }
    }
}

pub fn emit(s: &Scenario, n: u64) {
    let mut out = std::io::stdout();
    for line in &s.emit {
        if s.delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(s.delay_ms));
        }
        let _ = writeln!(out, "{}", subst(line, n));
        let _ = out.flush();
    }
}

pub fn say(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

pub fn die(message: &str) -> ! {
    eprintln!("fake: {message}");
    std::process::exit(97);
}
