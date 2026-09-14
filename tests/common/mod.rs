//! Shared test helpers. Fully implemented: every other test suite builds on these.
#![allow(dead_code)]

use camino::Utf8PathBuf;
use std::path::PathBuf;
use std::process::Command;

/// docs/ref/<name>, resolved against the crate root so it works from any cwd.
pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("docs")
        .join("ref")
        .join(name)
}

/// Non-empty lines with the trailing newline stripped.
pub fn fixture_lines(name: &str) -> Vec<String> {
    let path = fixture(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading fixture {}: {e}", path.display()));
    text.lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

fn git(dir: &Utf8PathBuf, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=swamp tests",
            "-c",
            "user.email=tests@swamp.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("running git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A throwaway git repo with exactly one commit. The TempDir must stay alive.
pub fn tmp_repo() -> (tempfile::TempDir, Utf8PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Canonicalized: macOS hands out /var, git reports /private/var.
    let root = std::fs::canonicalize(tmp.path()).expect("canonicalize");
    let root = Utf8PathBuf::from_path_buf(root).expect("utf8 tempdir");

    git(&root, &["init", "-q"]);
    std::fs::write(root.join("README.md"), "swamp test repo\n").expect("write README");
    git(&root, &["add", "README.md"]);
    git(&root, &["commit", "-q", "-m", "initial commit"]);
    (tmp, root)
}

/// The same repo with one uncommitted modification.
pub fn tmp_repo_dirty() -> (tempfile::TempDir, Utf8PathBuf) {
    let (tmp, root) = tmp_repo();
    std::fs::write(root.join("README.md"), "swamp test repo\ndirty\n").expect("dirty README");
    (tmp, root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixtures_are_present() {
        for name in [
            "claude-help.txt",
            "codex-help.txt",
            "codex-exec-help.txt",
            "claude-stream-sample.jsonl",
            "codex-stream-sample.jsonl",
        ] {
            assert!(fixture(name).is_file(), "missing fixture {name}");
        }
    }

    #[test]
    fn codex_sample_has_five_lines() {
        assert_eq!(fixture_lines("codex-stream-sample.jsonl").len(), 5);
        assert_eq!(fixture_lines("claude-stream-sample.jsonl").len(), 5);
    }

    #[test]
    fn tmp_repo_has_a_commit() {
        let (_tmp, root) = tmp_repo();
        let out = Command::new("git")
            .current_dir(&root)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git rev-parse");
        assert!(out.status.success(), "rev-parse failed in {root}");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim().len(), 40);
    }

    #[test]
    fn tmp_repo_dirty_is_dirty() {
        let (_tmp, root) = tmp_repo_dirty();
        let out = Command::new("git")
            .current_dir(&root)
            .args(["status", "--porcelain"])
            .output()
            .expect("git status");
        assert!(!String::from_utf8_lossy(&out.stdout).trim().is_empty());
    }
}
