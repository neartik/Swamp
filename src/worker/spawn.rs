use crate::ids::NodeId;
use crate::worker::liveness::{reap, write_pidfile};
use camino::{Utf8Path, Utf8PathBuf};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::time::{Duration, Instant};
use time::OffsetDateTime;

const REAP_POLL: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, Copy)]
pub struct Detached {
    pub pid: i32,
    pub pgid: i32,
    pub started_at: OffsetDateTime,
}

#[derive(Debug, Clone)]
pub struct NodeIo {
    pub node: NodeId,
    pub prompt: Utf8PathBuf,
    pub stdout: Utf8PathBuf,
    pub stderr: Utf8PathBuf,
    pub pidfile: Utf8PathBuf,
    pub depth: u32,
}

/// All three fds are ordinary files, never pipes: a supervisor crash loses nothing and the
/// two-pipe deadlock cannot happen. `std::process` is deliberate: its `Child` does not reap
/// on drop, so `liveness::wait_exit` can collect the real status later.
pub fn spawn_detached(
    argv: &[OsString],
    env: &[(OsString, OsString)],
    cwd: &Utf8Path,
    io: &NodeIo,
) -> anyhow::Result<Detached> {
    let program = argv
        .first()
        .ok_or_else(|| anyhow::anyhow!("empty argv for node {}", io.node))?;
    for p in [&io.stdout, &io.stderr, &io.pidfile] {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let prompt = std::fs::File::open(&io.prompt)
        .map_err(|e| anyhow::anyhow!("opening prompt {}: {e}", io.prompt))?;

    let mut cmd = std::process::Command::new(program);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .envs(env.iter().cloned())
        .env("SWAMP_DEPTH", (io.depth + 1).to_string())
        .env("SWAMP_NODE", io.node.to_string())
        .stdin(prompt)
        .stdout(open_append(&io.stdout)?)
        .stderr(open_append(&io.stderr)?)
        .process_group(0);

    let child = cmd.spawn()?;
    let pid = child.id() as i32;
    write_pidfile(&io.pidfile, pid)?;
    Ok(Detached {
        pid,
        pgid: pid,
        started_at: OffsetDateTime::now_utc(),
    })
}

fn open_append(path: &Utf8Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// Who collects the exit status of a process group member that is our own child. Two
/// concurrent `waitpid` calls on the same child steal each other's status, so exactly one
/// owner is named at every call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaper {
    /// `liveness::wait_exit` is watching this pid; `terminate` must not touch it.
    Elsewhere,
    /// Nobody else is waiting: `terminate` collects zombies itself.
    Here,
}

/// SIGTERM to -pgid, wait `grace`, then SIGKILL.
pub async fn terminate(pgid: i32, grace: Duration, reaper: Reaper) -> anyhow::Result<()> {
    let group = Pid::from_raw(pgid);
    let _ = killpg(group, Signal::SIGTERM);
    if settle(pgid, grace, reaper).await {
        return Ok(());
    }
    let _ = killpg(group, Signal::SIGKILL);
    settle(pgid, grace, reaper).await;
    Ok(())
}

/// Waits for the whole group, grandchildren included, to disappear.
async fn settle(pgid: i32, grace: Duration, reaper: Reaper) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        if reaper == Reaper::Here {
            reap(pgid);
        }
        if !group_alive(pgid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(REAP_POLL).await;
    }
}

fn group_alive(pgid: i32) -> bool {
    killpg(Pid::from_raw(pgid), None).is_ok()
}
