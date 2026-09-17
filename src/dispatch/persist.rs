use crate::dispatch::account::{AccountState, WindowKey};
use crate::model::core::{AccountId, CostBasis};
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
    read_state(path)
}

fn read_state(path: &Utf8Path) -> anyhow::Result<StateMap> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    if text.trim().is_empty() {
        return Ok(StateMap::new());
    }
    serde_json::from_str(&text).with_context(|| format!("parsing {path}"))
}

/// `load_state` takes the lock exclusively and blocks; a reader polling every second would
/// then stall the dispatcher trying to rename a new state file into place. Shared and
/// non-blocking instead: `Ok(None)` means the file was contended, so keep the numbers you
/// already had rather than waiting for it.
pub fn try_load_state(path: &Utf8Path) -> anyhow::Result<Option<StateMap>> {
    if !path.is_file() {
        return Ok(Some(StateMap::new()));
    }
    let Ok(file) = open_lock_file(path) else {
        // A read-only directory still holds a readable state file, and the lock is advisory.
        return read_state(path).map(Some);
    };
    if !FileExt::try_lock_shared(&file)? {
        return Ok(None);
    }
    let state = read_state(path);
    drop(file);
    state.map(Some)
}

/// Read-modify-write under the lock. Another repo's run may have learned a cooldown since we
/// loaded, and overwriting the whole file with our snapshot would erase it.
pub fn merge_state(path: &Utf8Path, mine: &StateMap) -> anyhow::Result<StateMap> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{path} has no parent directory"))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {dir}"))?;
    let _guard = lock(path)?;
    // Never `unwrap_or_default`: writing our own map over a file we failed to parse would
    // erase every account this repo does not configure.
    let mut merged = read_unlocked(path)?;
    for (id, ours) in mine {
        let theirs = merged.get(id).cloned();
        // The newer entry wins the last-writer-wins fields; the counters are still
        // reconciled both ways, because the two processes count independently.
        let newer_is_theirs = theirs
            .as_ref()
            .is_some_and(|t| t.updated_at > ours.updated_at);
        let mut entry = match (&theirs, newer_is_theirs) {
            (Some(theirs), true) => theirs.clone(),
            _ => ours.clone(),
        };
        if let Some(theirs) = &theirs {
            entry.lifetime_nodes = theirs.lifetime_nodes.max(ours.lifetime_nodes);
            entry.lifetime_tokens.take_max(&theirs.lifetime_tokens);
            entry.lifetime_tokens.take_max(&ours.lifetime_tokens);
            // Cost accumulates per process from whatever the file held at startup, so our
            // own total does not include what the other process has spent since.
            entry.lifetime_cost_usd = theirs.lifetime_cost_usd.max(ours.lifetime_cost_usd);
            entry.lifetime_cost_basis =
                coarsest_basis(theirs.lifetime_cost_basis, ours.lifetime_cost_basis);
            // A rolled window starts at zero: only the SAME window may keep the other
            // process's count, or a reset window comes straight back from the file.
            if same_window(theirs.window_key.as_ref(), ours.window_key.as_ref()) {
                entry.window_tokens.take_max(&theirs.window_tokens);
                entry.window_tokens.take_max(&ours.window_tokens);
            }
        }
        // inflight is this process's runtime state and means nothing to anyone else.
        entry.inflight = 0;
        merged.insert(id.clone(), entry);
    }
    write_locked(path, dir, &merged)?;
    Ok(merged)
}

/// One estimated fold anywhere makes the merged total an estimate.
fn coarsest_basis(a: Option<CostBasis>, b: Option<CostBasis>) -> Option<CostBasis> {
    match (a, b) {
        (Some(CostBasis::Estimated), _) | (_, Some(CostBasis::Estimated)) => {
            Some(CostBasis::Estimated)
        }
        (x, y) => x.or(y),
    }
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

/// Read-modify-write of a few named entries, under the lock. A caller that refreshed one
/// field group must not write back the rest of an entry it read minutes ago: `f` edits the
/// file's own entry, so a cooldown another process learned meanwhile survives.
pub fn update_state(
    path: &Utf8Path,
    ids: &[AccountId],
    mut f: impl FnMut(&AccountId, &mut AccountState),
) -> anyhow::Result<StateMap> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{path} has no parent directory"))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {dir}"))?;
    let _guard = lock(path)?;
    let mut merged = read_unlocked(path)?;
    for id in ids {
        f(id, merged.entry(id.clone()).or_default());
    }
    write_locked(path, dir, &merged)?;
    Ok(merged)
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
    let file = open_lock_file(path)?;
    FileExt::lock_exclusive(&file)
        .with_context(|| format!("locking {}", path.with_extension("lock")))?;
    Ok(file)
}

fn open_lock_file(path: &Utf8Path) -> anyhow::Result<File> {
    let lock_path = path.with_extension("lock");
    if let Some(dir) = lock_path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {dir}"))?;
    }
    OpenOptions::new()
        .create(true)
        .read(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening {lock_path}"))
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

    /// `swamp usage --probe` refreshes quota while a supervisor cools the same account.
    /// Writing back the whole entry it loaded before probing used to erase that cooldown.
    #[test]
    fn an_update_keeps_the_fields_it_did_not_touch() {
        let (_dir, path) = tmp_dir();
        let now = time::OffsetDateTime::now_utc();
        let mut supervisor = StateMap::new();
        supervisor.insert(
            AccountId("main".into()),
            AccountState {
                health: Health::Cooling,
                cooldown_until: Some(now + std::time::Duration::from_secs(900)),
                updated_at: Some(now),
                ..Default::default()
            },
        );
        save_state(&path, &supervisor).unwrap();

        let ids = [AccountId("main".into())];
        let merged = update_state(&path, &ids, |_, entry| {
            entry.lifetime_nodes = 9;
            entry.updated_at = Some(now + std::time::Duration::from_secs(3));
        })
        .unwrap();

        let got = &merged[&AccountId("main".into())];
        assert_eq!(got.health, Health::Cooling);
        assert!(got.cooldown_until.is_some());
        assert_eq!(got.lifetime_nodes, 9);
        assert_eq!(
            load_state(&path).unwrap()[&AccountId("main".into())].health,
            Health::Cooling
        );
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
