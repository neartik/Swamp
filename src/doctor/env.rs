use super::{Check, Level};
use crate::config::Config;
use crate::journal::paths::Paths;
use camino::{Utf8Path, Utf8PathBuf};

pub(super) async fn environment(paths: &Paths, out: &mut Vec<Check>) {
    match tokio::process::Command::new("git")
        .arg("--version")
        .output()
        .await
    {
        Ok(o) if o.status.success() => out.push(Check::new(
            "environment/git",
            Level::Ok,
            String::from_utf8_lossy(&o.stdout).trim().to_owned(),
        )),
        _ => out.push(Check::new(
            "environment/git",
            Level::Error,
            "git is not on PATH; worktree isolation needs it",
        )),
    }

    let head = run_git(&paths.repo, &["rev-parse", "--short", "HEAD"]).await;
    let dirty = run_git(&paths.repo, &["status", "--porcelain"]).await;
    match (head, dirty) {
        (Some(head), Some(status)) => out.push(Check::new(
            "environment/repo",
            Level::Ok,
            format!(
                "{} HEAD {head}, {}",
                paths.repo,
                if status.trim().is_empty() {
                    "clean"
                } else {
                    "dirty"
                }
            ),
        )),
        _ => out.push(Check::new(
            "environment/repo",
            Level::Error,
            format!("{} is not a usable git repository", paths.repo),
        )),
    }

    out.push(match writable(&paths.dot_swamp) {
        Ok(()) => {
            let excluded = git_excludes_swamp(&paths.repo);
            if excluded {
                Check::new(
                    "environment/.swamp",
                    Level::Ok,
                    format!("{} writable, listed in .git/info/exclude", paths.dot_swamp),
                )
            } else {
                Check::new(
                    "environment/.swamp",
                    Level::Warn,
                    format!(
                        "{} is not listed in .git/info/exclude; run any swamp command from the \
                         repo root to add it",
                        paths.dot_swamp
                    ),
                )
            }
        }
        Err(e) => Check::new(
            "environment/.swamp",
            Level::Error,
            format!("{} is not writable: {e}", paths.dot_swamp),
        ),
    });

    out.push(match writable(&paths.home_swamp) {
        Ok(()) => Check::new(
            "environment/state",
            Level::Ok,
            format!("{} writable", paths.home_swamp),
        ),
        Err(e) => Check::new(
            "environment/state",
            Level::Error,
            format!("{} is not writable: {e}", paths.home_swamp),
        ),
    });

    out.push(match std::env::current_exe() {
        Ok(p) => Check::new(
            "environment/swamp",
            Level::Ok,
            format!(
                "{} (the MCP bridge is spawned by absolute path)",
                p.display()
            ),
        ),
        Err(e) => Check::new(
            "environment/swamp",
            Level::Error,
            format!("cannot resolve the swamp executable: {e}"),
        ),
    });

    out.push(match std::env::var("SWAMP_DEPTH") {
        Err(_) => Check::new("environment/depth", Level::Ok, "not inside a worker"),
        Ok(d) => Check::new(
            "environment/depth",
            Level::Warn,
            format!("SWAMP_DEPTH={d}: this shell is inside a worker; nested dispatch is capped"),
        ),
    });
}

pub(super) fn config_sources(cfg: &Config, out: &mut Vec<Check>) {
    let level = if cfg.sources.is_empty() {
        Level::Note
    } else {
        Level::Ok
    };
    // SWAMP_CONFIG_DIR and XDG_CONFIG_HOME move the user layer: without the resolved path
    // here, a file written to ~/.config/swamp is simply never mentioned again.
    let detail = if cfg.sources.is_empty() {
        match crate::config::load::user_config_path() {
            Some(p) => format!(
                "built-in defaults only; no config file was found (looked for {p} and <repo>/.swamp/config.toml)"
            ),
            None => "built-in defaults only; no config file was found".to_owned(),
        }
    } else {
        cfg.sources
            .iter()
            .map(Utf8PathBuf::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    out.push(Check::new("config/sources", level, detail));
    for w in &cfg.warnings {
        out.push(Check::new("config/warning", Level::Warn, w.clone()));
    }
}

async fn run_git(dir: &Utf8Path, args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .await
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn writable(dir: &Utf8Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let probe = dir.join(".swamp-doctor-probe");
    std::fs::write(&probe, b"ok")?;
    std::fs::remove_file(&probe)
}

fn git_excludes_swamp(repo: &Utf8Path) -> bool {
    let exclude = repo.join(".git").join("info").join("exclude");
    let Ok(text) = std::fs::read_to_string(exclude) else {
        return false;
    };
    text.lines()
        .any(|l| matches!(l.trim(), "/.swamp/" | "/.swamp" | ".swamp/" | ".swamp"))
}
