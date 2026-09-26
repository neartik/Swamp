use crate::journal::paths::Paths;
use camino::{Utf8Path, Utf8PathBuf};

/// What one `--reap` removed, counted per place so the report can name both.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Reaped {
    /// Sockets, pidfiles and worktrees belonging to runs of this repo.
    pub runs: u32,
    /// Sockets left in the machine-wide socket directory by any repo.
    pub sockets: u32,
}

/// Removes stale worktrees, sockets and pidfiles.
pub async fn reap(paths: &Paths) -> anyhow::Result<Reaped> {
    let mut removed = 0u32;
    for run in paths.list_runs().unwrap_or_default() {
        let rp = paths.run_paths(run);
        let view = match crate::journal::fold::RunView::load(&rp.dir, false) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let live = view.nodes.values().any(|n| {
            matches!(n.state, crate::model::core::NodeState::Running { .. }) && rp.is_live(n.id)
        });
        if live {
            continue;
        }
        for node in view.nodes.keys() {
            let pidfile = rp.pidfile(*node);
            if pidfile.is_file() && !crate::worker::liveness::is_ours(&pidfile) {
                std::fs::remove_file(&pidfile).ok();
                removed += 1;
            }
        }
        let socket = rp.socket();
        if socket.exists() {
            std::fs::remove_file(&socket).ok();
            removed += 1;
        }
    }
    if let Ok(git) = crate::workspace::git::Git::discover(&paths.repo).await {
        removed += crate::workspace::worktree::prune(&git).await.unwrap_or(0);
    }
    Ok(Reaped {
        runs: removed,
        sockets: reap_sockets(&paths.sock_dir()),
    })
}

/// A run whose repository is gone leaves its socket behind here, so the directory is swept
/// on its own: anything that still accepts a connection is live and is never touched.
fn reap_sockets(dir: &Utf8Path) -> u32 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0u32;
    for entry in entries.flatten() {
        let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) else {
            continue;
        };
        if path.extension() != Some("sock") || !is_dead_socket(&path) {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

fn is_dead_socket(path: &Utf8Path) -> bool {
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => false,
        Err(e) => matches!(
            e.kind(),
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
        ),
    }
}
