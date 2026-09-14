use crate::model::core::{ChangeKind, EvidenceSource, FileChange};
use crate::workspace::git::Git;
use camino::{Utf8Path, Utf8PathBuf};
use std::collections::BTreeMap;

pub struct DiffSummary {
    pub files: Vec<FileChange>,
    pub insertions: u32,
    pub deletions: u32,
    pub head: String,
    pub patch: Utf8PathBuf,
    pub empty: bool,
}

/// Git is authoritative for what a worker touched; the event stream is only a live estimate.
pub async fn collect(
    git: &Git,
    wt: &Utf8Path,
    base: &str,
    out: &Utf8Path,
) -> anyhow::Result<DiffSummary> {
    let head = git.run(wt, &["rev-parse", "HEAD"]).await?.trim().to_owned();
    let range = format!("{base}..HEAD");

    let numstat = git
        .run(wt, &["diff", "--numstat", "-z", "-M", &range])
        .await?;
    let name_status = git
        .run(wt, &["diff", "--name-status", "-z", "-M", &range])
        .await?;
    let patch = git
        .run(wt, &["diff", "--binary", "--no-color", "-M", &range])
        .await?;

    if let Some(parent) = out.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(out, patch.as_bytes()).await?;

    let counts = parse_numstat(&numstat);
    let mut files = Vec::new();
    let (mut insertions, mut deletions) = (0u32, 0u32);
    for (path, kind) in parse_name_status(&name_status) {
        let (added, removed) = counts.get(&path).copied().unwrap_or((0, 0));
        insertions += added;
        deletions += removed;
        files.push(FileChange {
            path: Utf8PathBuf::from(&path),
            kind,
            added,
            removed,
            source: EvidenceSource::Git,
        });
    }

    Ok(DiffSummary {
        empty: files.is_empty(),
        files,
        insertions,
        deletions,
        head,
        patch: out.to_owned(),
    })
}

/// `<added>\t<removed>\t<path>\0`, and for a rename `<added>\t<removed>\t\0<from>\0<to>\0`.
fn parse_numstat(raw: &str) -> BTreeMap<String, (u32, u32)> {
    let mut counts = BTreeMap::new();
    let mut fields = raw.split('\0').filter(|f| !f.is_empty()).peekable();
    while let Some(record) = fields.next() {
        let mut parts = record.splitn(3, '\t');
        let added = parts.next().unwrap_or("-");
        let removed = parts.next().unwrap_or("-");
        let path = parts.next().unwrap_or("");
        // Binary files report `-` for both counts.
        let added = added.parse().unwrap_or(0);
        let removed = removed.parse().unwrap_or(0);
        let path = if path.is_empty() {
            let _from = fields.next();
            match fields.next() {
                Some(to) => to.to_owned(),
                None => continue,
            }
        } else {
            path.to_owned()
        };
        counts.insert(path, (added, removed));
    }
    counts
}

/// `<status>\0<path>\0`, and for a rename `R<score>\0<from>\0<to>\0`.
fn parse_name_status(raw: &str) -> Vec<(String, ChangeKind)> {
    let mut out = Vec::new();
    let mut fields = raw.split('\0').filter(|f| !f.is_empty());
    while let Some(status) = fields.next() {
        let letter = status.chars().next().unwrap_or('M');
        let kind = match letter {
            'A' | 'C' => ChangeKind::Add,
            'D' => ChangeKind::Delete,
            'R' => ChangeKind::Rename,
            _ => ChangeKind::Modify,
        };
        let Some(first) = fields.next() else { break };
        // R and C carry both the source and the destination path.
        let path = match letter {
            'R' | 'C' => fields.next().unwrap_or(first),
            _ => first,
        };
        out.push((path.to_owned(), kind));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nul(fields: &[&str]) -> String {
        fields.iter().map(|f| format!("{f}\0")).collect()
    }

    #[test]
    fn numstat_parses_renames_and_binaries() {
        let raw = nul(&[
            "3\t1\tsrc/a.rs",
            "-\t-\tlogo.png",
            "10\t2\t",
            "old name.rs",
            "new name.rs",
        ]);
        let counts = parse_numstat(&raw);
        assert_eq!(counts.get("src/a.rs"), Some(&(3, 1)));
        assert_eq!(counts.get("logo.png"), Some(&(0, 0)));
        assert_eq!(counts.get("new name.rs"), Some(&(10, 2)));
    }

    #[test]
    fn name_status_classifies_every_kind() {
        let raw = nul(&[
            "M", "src/a.rs", "A", "src/b.rs", "D", "src/c.rs", "R100", "old.rs", "new.rs",
        ]);
        let parsed = parse_name_status(&raw);
        assert_eq!(
            parsed,
            vec![
                ("src/a.rs".to_owned(), ChangeKind::Modify),
                ("src/b.rs".to_owned(), ChangeKind::Add),
                ("src/c.rs".to_owned(), ChangeKind::Delete),
                ("new.rs".to_owned(), ChangeKind::Rename),
            ]
        );
    }
}
