use super::{Check, Level};
use crate::config::Config;
use crate::journal::paths::Paths;
use camino::{Utf8Path, Utf8PathBuf};

/// Directories a fresh worktree will not inherit unless `workspace.link` names them.
const HEAVY_DIRS: [&str; 5] = ["target", "node_modules", ".venv", "vendor", "build"];
const HEAVY_BYTES: u64 = 1 << 30;

pub(super) fn workspace(cfg: &Config, paths: &Paths, out: &mut Vec<Check>) {
    if !cfg.workspace.link.is_empty() {
        return;
    }
    for name in HEAVY_DIRS {
        let dir = paths.repo.join(name);
        if !dir.is_dir() {
            continue;
        }
        let bytes = dir_size(&dir, 20_000);
        if bytes >= HEAVY_BYTES {
            out.push(Check::new(
                "workspace/link",
                Level::Warn,
                format!(
                    "[workspace] link is empty but ./{name} is {:.1} GiB; fresh worktrees will \
                     rebuild from scratch. Consider link = [\"{name}\"]",
                    bytes as f64 / (1u64 << 30) as f64
                ),
            ));
        }
    }
}

/// Bounded on purpose: doctor must stay fast on a repo with a huge build directory.
fn dir_size(dir: &Utf8Path, budget: usize) -> u64 {
    let mut stack = vec![dir.to_path_buf()];
    let mut bytes = 0u64;
    let mut seen = 0usize;
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > budget {
                return bytes;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                if let Ok(p) = Utf8PathBuf::from_path_buf(entry.path()) {
                    stack.push(p);
                }
            } else if meta.is_file() {
                bytes += meta.len();
            }
        }
    }
    bytes
}
