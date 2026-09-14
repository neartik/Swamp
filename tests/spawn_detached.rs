//! WP3: the detached launch model. Workers outlive the supervisor, and the follower resumes
//! from a byte offset without duplicating or losing an event.

mod common;

use camino::{Utf8Path, Utf8PathBuf};
use regex::RegexSet;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use swamp::config::FailurePatterns;
use swamp::ids::{NodeId, NodeIds, RunId};
use swamp::journal::paths::RunPaths;
use swamp::journal::raw::{RawSink, Redactor};
use swamp::model::core::{NodeKind, Provider, Tier};
use swamp::model::event::WorkerEvent;
use swamp::model::failure::Failure;
use swamp::model::result::IsolationMode;
use swamp::worker::liveness::{is_ours, running, wait_exit, write_pidfile};
use swamp::worker::spawn::{spawn_detached, terminate};
use swamp::worker::{
    ExecReq, LaunchSpec, NodeIo, ParseState, SessionPlan, adapter_for, execute, follow::follow,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const POLL: Duration = Duration::from_millis(20);

fn text_line(text: &str) -> String {
    format!(r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{text}"}}]}}}}"#)
}

struct Node {
    _tmp: tempfile::TempDir,
    dir: Utf8PathBuf,
    io: NodeIo,
}

fn node(script: &str) -> Node {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = Utf8PathBuf::from_path_buf(std::fs::canonicalize(tmp.path()).expect("canonicalize"))
        .expect("utf8 tempdir");
    let id = NodeId::new();
    std::fs::write(dir.join("prompt.md"), "ping\n").unwrap();
    let path = dir.join("worker.sh");
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let io = NodeIo {
        node: id,
        prompt: dir.join("prompt.md"),
        stdout: dir.join("stream.jsonl"),
        stderr: dir.join("stderr.log"),
        pidfile: dir.join("worker.pid"),
        depth: 0,
    };
    Node { _tmp: tmp, dir, io }
}

/// These tests feed the follower lines that always parse, so the noise sink is never reached.
async fn sink(io: &NodeIo) -> RawSink {
    let dir = io.stdout.parent().expect("node dir").to_owned();
    let paths = RunPaths {
        run: RunId::new(),
        dir,
    };
    RawSink::open(
        &paths,
        io.node,
        Arc::new(Redactor::new(&[]).expect("redactor")),
    )
    .await
    .expect("raw sink")
}

fn patterns() -> FailurePatterns {
    FailurePatterns {
        rate_limit: RegexSet::empty(),
        auth: RegexSet::empty(),
        overloaded: RegexSet::empty(),
        sources: BTreeMap::new(),
    }
}

fn spec(n: &Node) -> LaunchSpec {
    LaunchSpec {
        node: NodeIds {
            id: n.io.node,
            session_uuid: uuid::Uuid::from_u128(1),
        },
        provider: Provider::Anthropic,
        exec: n.dir.join("worker.sh").into_string(),
        env: BTreeMap::new(),
        model: "tier-mid-model".into(),
        tier: Tier::Mid,
        cwd: n.dir.clone(),
        isolation: IsolationMode::Worktree,
        session: SessionPlan::New { preassigned: None },
        kind: NodeKind::Worker,
        permission_mode: "acceptEdits".into(),
        sandbox: "workspace-write".into(),
        budget_usd: None,
        append_system_prompt: None,
        allow_tools: Vec::new(),
        deny_tools: Vec::new(),
        mcp: None,
        last_message_path: n.dir.join("last-message.txt"),
        extra_args: Vec::new(),
        attempt: 1,
    }
}

/// Follows until the pid is gone, which is exactly what the executor does.
async fn follow_to_exit(
    io: &NodeIo,
    pid: Option<i32>,
    from: u64,
) -> (u64, Vec<(WorkerEvent, u64)>) {
    let alive_flag = Arc::new(AtomicBool::new(pid.is_some()));
    if let Some(pid) = pid {
        let flag = alive_flag.clone();
        tokio::spawn(async move {
            wait_exit(pid, POLL).await;
            flag.store(false, Ordering::SeqCst);
        });
    }
    let alive: Arc<dyn Fn() -> bool + Send + Sync> = {
        let flag = alive_flag.clone();
        Arc::new(move || flag.load(Ordering::SeqCst))
    };
    let (tx, mut rx) = mpsc::channel(64);
    let collector = tokio::spawn(async move {
        let mut out = Vec::new();
        while let Some((_, ev, off)) = rx.recv().await {
            out.push((ev, off));
        }
        out
    });
    let mut st = ParseState::default();
    let mut sink = sink(io).await;
    let offset = follow(
        io.node,
        &io.stdout,
        from,
        adapter_for(Provider::Anthropic),
        &mut st,
        &mut sink,
        tx,
        alive,
    )
    .await
    .expect("follow");
    (offset, collector.await.expect("collector"))
}

fn wait_for(mut done: impl FnMut() -> bool, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(POLL);
    }
    done()
}

fn file_len(p: &Utf8Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

#[tokio::test]
async fn stdout_lands_in_a_file_and_the_follower_reads_it_across_a_pause() {
    let n = node(&format!(
        "#!/bin/sh\nprintf '%s\\n' '{}'\nsleep 0.4\nprintf '%s\\n' '{}'\n",
        text_line("one"),
        text_line("two")
    ));
    let argv = vec![std::ffi::OsString::from(n.dir.join("worker.sh").as_str())];
    let d = spawn_detached(&argv, &[], &n.dir, &n.io).expect("spawn");

    let (offset, events) = follow_to_exit(&n.io, Some(d.pid), 0).await;
    let texts: Vec<String> = events
        .iter()
        .filter_map(|(e, _)| match e {
            WorkerEvent::AssistantText { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["one", "two"]);
    assert_eq!(offset, file_len(&n.io.stdout), "offset must equal the file");
    assert!(is_ours(&n.io.pidfile) || !running(d.pid));
}

#[tokio::test]
async fn the_child_keeps_writing_after_the_follower_is_dropped() {
    let n = node(&format!(
        "#!/bin/sh\nfor i in 1 2 3 4 5 6 7 8; do printf '%s\\n' '{}'; sleep 0.1; done\n",
        text_line("tick")
    ));
    let argv = vec![std::ffi::OsString::from(n.dir.join("worker.sh").as_str())];
    let d = spawn_detached(&argv, &[], &n.dir, &n.io).expect("spawn");

    let stdout = n.io.stdout.clone();
    let follower = {
        let io = n.io.clone();
        tokio::spawn(async move { follow_to_exit(&io, Some(d.pid), 0).await })
    };
    assert!(wait_for(|| file_len(&stdout) > 0, Duration::from_secs(5)));
    follower.abort();
    let at_abort = file_len(&stdout);

    // The whole point of the detached model: losing the supervisor loses nothing.
    assert!(
        wait_for(|| file_len(&stdout) > at_abort, Duration::from_secs(5)),
        "the child stopped writing when its follower went away"
    );
    let _ = terminate(d.pgid, Duration::from_millis(500)).await;
}

#[tokio::test]
async fn a_restart_resumes_at_the_journaled_offset_without_duplicates() {
    let n = node("#!/bin/sh\nexit 0\n");
    let body = format!(
        "{}\n{}\n{}\n",
        text_line("one"),
        text_line("two"),
        text_line("three")
    );
    std::fs::write(&n.io.stdout, &body).unwrap();

    let (full_offset, all) = follow_to_exit(&n.io, None, 0).await;
    assert_eq!(all.len(), 3);
    assert_eq!(full_offset, body.len() as u64);

    let resume_at = all[0].1;
    let (offset, rest) = follow_to_exit(&n.io, None, resume_at).await;
    assert_eq!(offset, full_offset);
    let texts: Vec<&WorkerEvent> = rest.iter().map(|(e, _)| e).collect();
    assert_eq!(texts.len(), 2, "nothing lost, nothing replayed");
    assert_eq!(texts[0], &all[1].0);
    assert_eq!(texts[1], &all[2].0);
}

#[tokio::test]
async fn a_half_written_trailing_line_is_never_parsed_and_never_hangs() {
    let n = node("#!/bin/sh\nexit 0\n");
    let whole = text_line("one");
    std::fs::write(&n.io.stdout, format!("{whole}\n{{\"type\":\"assis")).unwrap();

    let (offset, events) = follow_to_exit(&n.io, None, 0).await;
    assert_eq!(events.len(), 1);
    assert_eq!(
        offset,
        whole.len() as u64 + 1,
        "resume from the last whole line"
    );
}

#[tokio::test]
async fn terminate_takes_the_whole_process_group_including_grandchildren() {
    let n = node(
        "#!/bin/sh\nsh -c 'echo $$ > grandchild.pid; while true; do sleep 1; done' &\n\
         while true; do sleep 1; done\n",
    );
    let argv = vec![std::ffi::OsString::from(n.dir.join("worker.sh").as_str())];
    let d = spawn_detached(&argv, &[], &n.dir, &n.io).expect("spawn");

    let gpid_file = n.dir.join("grandchild.pid");
    assert!(
        wait_for(|| file_len(&gpid_file) > 0, Duration::from_secs(5)),
        "the grandchild never started"
    );
    let gpid: i32 = std::fs::read_to_string(&gpid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(running(gpid));

    terminate(d.pgid, Duration::from_secs(2)).await.unwrap();
    assert!(!running(d.pid), "the worker survived terminate");
    assert!(!running(gpid), "the grandchild survived terminate");
}

#[tokio::test]
async fn a_pidfile_is_not_ours_when_the_pid_was_recycled() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    let mine = dir.join("mine.pid");
    let me = std::process::id() as i32;

    write_pidfile(&mine, me).unwrap();
    assert!(is_ours(&mine));

    let recycled = dir.join("recycled.pid");
    std::fs::write(&recycled, format!("{me} 1\n")).unwrap();
    assert!(
        !is_ours(&recycled),
        "a live pid with a different start time is a different process"
    );
    assert!(!is_ours(&dir.join("missing.pid")));
}

#[tokio::test]
async fn a_worker_that_never_exits_is_timed_out_and_its_group_is_gone() {
    let n = node(&format!(
        "#!/bin/sh\nprintf '%s\\n' '{}'\nwhile true; do sleep 1; done\n",
        text_line("working")
    ));
    let s = spec(&n);
    let p = patterns();
    let mut sink = sink(&n.io).await;
    let outcome = execute(ExecReq {
        adapter: adapter_for(Provider::Anthropic),
        spec: &s,
        io: &n.io,
        patterns: &p,
        sink: &mut sink,
        journal: None,
        resume: None,
        timeout: Duration::from_secs(1),
        grace: Duration::from_millis(300),
        cancel: CancellationToken::new(),
    })
    .await
    .expect("execute");

    assert_eq!(outcome.failure, Some(Failure::Timeout { after_s: 1 }));
    assert!(
        outcome.stream_offset > 0,
        "the partial stream was still read"
    );
    let pid: i32 = std::fs::read_to_string(&n.io.pidfile)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(!running(pid), "the process group outlived the timeout");
}
