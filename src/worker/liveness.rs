use crate::model::node::ExitInfo;
use camino::Utf8Path;
use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use std::future::Future;
use std::time::{Duration, Instant};

/// Writes pid plus process start ticks.
pub fn write_pidfile(path: &Utf8Path, pid: i32) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{pid} {}\n", start_time(pid).unwrap_or(0)))?;
    Ok(())
}

/// PID-reuse safe: compares the recorded process start time, not just the pid.
pub fn is_ours(path: &Utf8Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let mut parts = text.split_whitespace();
    let Some(pid) = parts.next().and_then(|p| p.parse::<i32>().ok()) else {
        return false;
    };
    let recorded = parts.next().and_then(|t| t.parse::<u64>().ok());
    match (recorded, start_time(pid)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

#[allow(clippy::manual_async_fn)]
pub fn wait_exit(pid: i32, poll: Duration) -> impl Future<Output = Option<ExitInfo>> {
    async move {
        let since = Instant::now();
        loop {
            let elapsed = || since.elapsed().as_millis() as u64;
            match waitpid(Pid::from_raw(pid), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(_, code)) => {
                    return Some(ExitInfo {
                        code: Some(code),
                        signal: None,
                        duration_ms: elapsed(),
                    });
                }
                Ok(WaitStatus::Signaled(_, sig, _)) => {
                    return Some(ExitInfo {
                        code: None,
                        signal: Some(sig as i32),
                        duration_ms: elapsed(),
                    });
                }
                Ok(_) => {}
                // Not our child, or already collected: fall back to probing the pid.
                Err(Errno::ECHILD) => {
                    if !running(pid) {
                        return Some(ExitInfo {
                            code: None,
                            signal: None,
                            duration_ms: elapsed(),
                        });
                    }
                }
                Err(_) => return None,
            }
            tokio::time::sleep(poll).await;
        }
    }
}

/// True while the pid exists, zombies included.
pub fn running(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

/// Collects whatever of the process group has died. True when nothing of ours is left to reap.
pub(crate) fn reap(pgid: i32) -> bool {
    match waitpid(Pid::from_raw(-pgid), Some(WaitPidFlag::WNOHANG)) {
        Ok(WaitStatus::StillAlive) => false,
        Ok(_) => true,
        Err(Errno::ECHILD) => true,
        Err(_) => false,
    }
}

fn start_time(pid: i32) -> Option<u64> {
    let pid = sysinfo::Pid::from_u32(u32::try_from(pid).ok()?);
    let mut sys = sysinfo::System::new();
    sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[pid]),
        true,
        sysinfo::ProcessRefreshKind::nothing(),
    );
    sys.process(pid).map(sysinfo::Process::start_time)
}
