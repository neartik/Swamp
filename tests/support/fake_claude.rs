//! A fake `claude` CLI: replays a scripted stream-json stream, records how it was called,
//! and, in brain mode, drives a real `swamp_dispatch` through the MCP bridge.
//!
//! Installed on a temp PATH under one wrapper name per subscription (`claude-main`,
//! `claude-alt`), so the name it was invoked as selects its scenario file.

#[path = "scenario.rs"]
mod scenario;

use scenario::Scenario;
use std::io::{BufRead, Read, Write};

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let name = scenario::wrapper_name(&argv);
    let dir = scenario::fake_dir();
    let script = scenario::load(&dir, &name);
    let brain = argv.iter().any(|a| a == "--mcp-config") && !script.dispatch.is_empty();

    let stdin = if brain {
        String::new()
    } else {
        scenario::read_stdin()
    };
    let n = scenario::record(&dir, &name, &argv, &stdin);
    scenario::check_argv(&argv, &script);

    if brain {
        // A brain plans and reads; it runs in the user's checkout and must not write there.
        brain_turn(&dir, &name, &argv, &script);
    } else {
        scenario::apply_edits(&script, n);
        scenario::emit(&script, n);
    }
    for line in &script.stderr {
        eprintln!("{}", scenario::subst(line, n));
    }
    std::process::exit(script.exit_code);
}

/// One turn: read the user message, call `swamp_dispatch` over the bridge, report the result
/// back as a tool_use / tool_result pair, then end the turn.
fn brain_turn(dir: &std::path::Path, name: &str, argv: &[String], s: &Scenario) {
    let session =
        scenario::flag(argv, "--session-id").unwrap_or_else(|| "fake-brain-session".to_owned());
    let model = scenario::flag(argv, "--model").unwrap_or_default();
    scenario::say(
        &serde_json::json!({
            "type": "system", "subtype": "init", "session_id": session, "model": model,
            "apiKeySource": "none",
            "mcp_servers": [{ "name": "swamp", "status": "connected" }]
        })
        .to_string(),
    );

    let turn = read_turn(dir, name);
    let tasks: Vec<serde_json::Value> = s
        .dispatch
        .iter()
        .map(|t| serde_json::json!({ "title": t.title, "prompt": t.prompt }))
        .collect();
    let args = serde_json::json!({ "tasks": tasks, "wait": true });
    scenario::say(
        &serde_json::json!({
            "type": "assistant",
            "message": { "role": "assistant", "type": "message", "model": model,
                         "content": [{ "type": "tool_use", "id": "toolu_dispatch",
                                       "name": "mcp__swamp__swamp_dispatch", "input": args }],
                         "usage": { "input_tokens": 30, "output_tokens": 9 } },
            "session_id": session
        })
        .to_string(),
    );

    let reply = call_bridge(argv, &args);
    let ok = reply.is_ok();
    let body = reply.unwrap_or_else(|e| e);
    scenario::say(
        &serde_json::json!({
            "type": "user",
            "message": { "role": "user",
                         "content": [{ "type": "tool_result", "tool_use_id": "toolu_dispatch",
                                       "is_error": !ok, "content": body }] },
            "session_id": session
        })
        .to_string(),
    );
    scenario::say(&scenario::assistant_text_line(&format!(
        "dispatched {} tasks for: {}",
        s.dispatch.len(),
        turn.trim()
    )));
    scenario::say(&scenario::result_line(
        "success",
        false,
        "the plan is done",
        0,
    ));

    // Closing fd0 is how a stream-json session ends: wait for it instead of racing the reader.
    let mut rest = String::new();
    let _ = std::io::stdin().read_to_string(&mut rest);
    for line in rest.lines().filter(|l| !l.trim().is_empty()) {
        scenario::append(&dir.join(format!("{name}.stdin.jsonl")), line);
    }
}

/// The first stream-json user turn written to fd0, recorded so a test can assert on it.
fn read_turn(dir: &std::path::Path, name: &str) -> String {
    let mut line = String::new();
    let stdin = std::io::stdin();
    let _ = stdin.lock().read_line(&mut line);
    scenario::append(&dir.join(format!("{name}.stdin.jsonl")), line.trim_end());
    serde_json::from_str::<serde_json::Value>(line.trim())
        .ok()
        .and_then(|v| {
            v.pointer("/message/content/0/text")
                .and_then(|t| t.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

/// Spawns the MCP server exactly the way the real CLI would: the command and args named by
/// `--mcp-config`, speaking JSON-RPC over its stdio.
fn call_bridge(argv: &[String], args: &serde_json::Value) -> Result<String, String> {
    let config = scenario::flag(argv, "--mcp-config").ok_or("no --mcp-config in argv")?;
    let config: serde_json::Value =
        serde_json::from_str(&config).map_err(|e| format!("unreadable --mcp-config: {e}"))?;
    let server = config
        .pointer("/mcpServers/swamp")
        .ok_or("no mcpServers.swamp entry")?;
    let command = server
        .get("command")
        .and_then(|c| c.as_str())
        .ok_or("the mcp server entry has no command")?;
    let bridge_args: Vec<String> = server
        .get("args")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .map(|v| v.as_str().unwrap_or_default().to_owned())
                .collect()
        })
        .unwrap_or_default();

    let mut child = std::process::Command::new(command)
        .args(&bridge_args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot spawn the mcp bridge {command}: {e}"))?;
    let mut to = child.stdin.take().ok_or("no bridge stdin")?;
    let mut from = std::io::BufReader::new(child.stdout.take().ok_or("no bridge stdout")?);

    let mut send = |v: serde_json::Value| -> Result<(), String> {
        let mut line = v.to_string();
        line.push('\n');
        to.write_all(line.as_bytes())
            .map_err(|e| format!("writing to the bridge: {e}"))?;
        to.flush().map_err(|e| format!("flushing the bridge: {e}"))
    };
    send(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": { "name": "fake-claude", "version": "0" } }
    }))?;
    read_reply(&mut from)?;
    send(serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))?;
    send(serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": { "name": "swamp_dispatch", "arguments": args }
    }))?;
    let reply = read_reply(&mut from)?;
    drop(to);
    let _ = child.wait();
    Ok(reply)
}

fn read_reply(from: &mut impl BufRead) -> Result<String, String> {
    let mut line = String::new();
    match from.read_line(&mut line) {
        Ok(0) => Err("the mcp bridge closed without answering".to_owned()),
        Ok(_) => Ok(line.trim().to_owned()),
        Err(e) => Err(format!("reading from the bridge: {e}")),
    }
}
