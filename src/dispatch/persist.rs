use crate::dispatch::account::{AccountState, WindowKey};
use crate::model::core::AccountId;
use anyhow::Context;
use camino::Utf8Path;
use fs4::fs_std::FileExt;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;

pub type StateMap = BTreeMap<AccountId, AccountState>;

/// Cross-run, cross-repo, fs4-locked, temp-write-and-rename.
pub fn load_state(path: &Utf8Path) -> anyhow::Result<StateMap> {
    if !path.is_file() {
        return Ok(StateMap::new());
    }
    let _guard = lock(path)?;
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    if text.trim().is_empty() {
        return Ok(StateMap::new());
    }
    serde_json::from_str(&text).with_context(|| format!("parsing {path}"))
}

/// Read-modify-write under the lock. Another repo's run may have learned a cooldown since we
/// loaded, and overwriting the whole file with our snapshot would erase it.
pub fn merge_state(path: &Utf8Path, mine: &StateMap) -> anyhow::Result<StateMap> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{path} has no parent directory"))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {dir}"))?;
    let _guard = lock(path)?;
    let mut merged = read_unlocked(path).unwrap_or_default();
    for (id, ours) in mine {
        let theirs = match merged.get(id) {
            Some(theirs) if theirs.updated_at > ours.updated_at => continue,
            other => other.cloned(),
        };
        let mut entry = ours.clone();
        if let Some(theirs) = &theirs {
            entry.lifetime_nodes = theirs.lifetime_nodes.max(ours.lifetime_nodes);
            entry.lifetime_tokens.take_max(&theirs.lifetime_tokens);
            // Cost accumulates per process from whatever the file held at startup, so our
            // own total does not include what the other process has spent since.
            entry.lifetime_cost_usd = entry.lifetime_cost_usd.max(theirs.lifetime_cost_usd);
            // A rolled window starts at zero: only the SAME window may keep the other
            // process's count, or a reset window comes straight back from the file.
            if same_window(theirs.window_key.as_ref(), ours.window_key.as_ref()) {
                entry.window_tokens.take_max(&theirs.window_tokens);
            }
        }
        // inflight is this process's runtime state and means nothing to anyone else.
        entry.inflight = 0;
        merged.insert(id.clone(), entry);
    }
    write_locked(path, dir, &merged)?;
    Ok(merged)
}

/// Two processes reading one codex window derive reset instants that differ by the age of
/// each reading, so the keys are compared by window and not byte for byte.
fn same_window(a: Option<&WindowKey>, b: Option<&WindowKey>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.same_window(b),
        (None, None) => true,
        _ => false,
    }
}

pub fn save_state(path: &Utf8Path, s: &StateMap) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{path} has no parent directory"))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {dir}"))?;
    let _guard = lock(path)?;
    write_locked(path, dir, s)
}

fn read_unlocked(path: &Utf8Path) -> anyhow::Result<StateMap> {
    if !path.is_file() {
        return Ok(StateMap::new());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    if text.trim().is_empty() {
        return Ok(StateMap::new());
    }
    serde_json::from_str(&text).with_context(|| format!("parsing {path}"))
}

fn write_locked(path: &Utf8Path, dir: &Utf8Path, s: &StateMap) -> anyhow::Result<()> {
    let mut tmp = tempfile::NamedTempFile::new_in(dir.as_std_path())
        .with_context(|| format!("creating a temp file in {dir}"))?;
    let body = serde_json::to_vec_pretty(s)?;
    tmp.write_all(&body)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path.as_std_path())
        .map_err(|e| anyhow::anyhow!("renaming into {path}: {}", e.error))?;
    Ok(())
}

/// A sibling lock file, not the state file itself: the lock must outlive the rename.
fn lock(path: &Utf8Path) -> anyhow::Result<File> {
    let lock_path = path.with_extension("lock");
    if let Some(dir) = lock_path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {dir}"))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening {lock_path}"))?;
    FileExt::lock_exclusive(&file).with_context(|| format!("locking {lock_path}"))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::account::Health;
    use camino::Utf8PathBuf;

    fn tmp_dir() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().join("accounts.json")).expect("utf8");
        (dir, path)
    }

    #[test]
    fn a_missing_file_is_an_empty_pool_not_an_error() {
        let (_dir, path) = tmp_dir();
        assert!(load_state(&path).unwrap().is_empty());
    }

    #[test]
    fn state_round_trips_through_the_file() {
        let (_dir, path) = tmp_dir();
        let mut s = StateMap::new();
        s.insert(
            AccountId("main".into()),
            AccountState {
                health: Health::Cooling,
                cooldown_until: Some(
                    time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
                ),
                consecutive_infra_failures: 2,
                lifetime_nodes: 7,
                lifetime_cost_usd: 1.25,
                ..Default::default()
            },
        );
        save_state(&path, &s).unwrap();
        let back = load_state(&path).unwrap();
        assert_eq!(back.len(), 1);
        let got = &back[&AccountId("main".into())];
        assert_eq!(got.health, Health::Cooling);
        assert_eq!(got.consecutive_infra_failures, 2);
        assert_eq!(got.lifetime_nodes, 7);
    }

    /// Repo A learns a 5h cooldown; repo B, which loaded before that, writes its own snapshot.
    /// A whole-file overwrite used to erase the cooldown and route straight back into the limit.
    #[test]
    fn a_merge_never_erases_another_process_entry() {
        let (_dir, path) = tmp_dir();
        let now = time::OffsetDateTime::now_utc();
        let mut a = StateMap::new();
        a.insert(
            AccountId("main".into()),
            AccountState {
                health: Health::Cooling,
                cooldown_until: Some(now + std::time::Duration::from_secs(5 * 3600)),
                updated_at: Some(now),
                ..Default::default()
            },
        );
        merge_state(&path, &a).unwrap();

        // B's view of `main` predates the cooldown; it only means to record its own account.
        let mut b = StateMap::new();
        b.insert(
            AccountId("main".into()),
            AccountState {
                updated_at: Some(now - std::time::Duration::from_secs(60)),
                ..Default::default()
            },
        );
        b.insert(
            AccountId("alt".into()),
            AccountState {
                lifetime_nodes: 3,
                updated_at: Some(now),
                ..Default::default()
            },
        );
        let merged = merge_state(&path, &b).unwrap();

        assert_eq!(merged[&AccountId("main".into())].health, Health::Cooling);
        assert!(merged[&AccountId("main".into())].cooldown_until.is_some());
        assert_eq!(merged[&AccountId("alt".into())].lifetime_nodes, 3);
        assert_eq!(load_state(&path).unwrap().len(), 2);
    }

    #[test]
    fn interleaved_writers_never_leave_a_torn_file() {
        let (_dir, path) = tmp_dir();
        std::thread::scope(|scope| {
            for w in 0..4u64 {
                let path = path.clone();
                scope.spawn(move || {
                    for i in 0..25u64 {
                        let mut s = StateMap::new();
                        for a in 0..8 {
                            s.insert(
                                AccountId(format!("account-{a}")),
                                AccountState {
                                    lifetime_nodes: w * 100 + i,
                                    ..Default::default()
                                },
                            );
                        }
                        save_state(&path, &s).unwrap();
                        assert_eq!(load_state(&path).unwrap().len(), 8);
                    }
                });
            }
        });
        assert_eq!(load_state(&path).unwrap().len(), 8);
    }
}
