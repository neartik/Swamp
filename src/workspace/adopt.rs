use crate::model::node::WorkResultRef;
use crate::workspace::git::{Git, GitOutput};
use camino::Utf8PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum MergeStrategy {
    #[default]
    Apply,
    Merge,
    CherryPick,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdoptResult {
    Clean { commit: Option<String> },
    Conflicted { paths: Vec<Utf8PathBuf> },
    Rejected { reason: String },
}

pub async fn adopt(
    git: &Git,
    work: &WorkResultRef,
    strategy: MergeStrategy,
    into: Option<&str>,
    force: bool,
    dry_run: bool,
) -> anyhow::Result<AdoptResult> {
    if work.empty {
        return Ok(rejected("the node produced no changes"));
    }
    let root = git.root.clone();
    let current = git
        .run(&root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await?
        .trim()
        .to_owned();

    if !git.is_clean().await? && !force {
        return Ok(rejected(format!(
            "working tree {root} is dirty; commit, stash, or pass --force"
        )));
    }

    let target = into.unwrap_or(&current).to_owned();
    if target != current {
        if dry_run {
            return Ok(rejected(format!(
                "dry run cannot switch from `{current}` to `{target}`; check it out first"
            )));
        }
        let out = git.output(&root, &["switch", &target]).await?;
        if !out.ok {
            return Ok(rejected(format!(
                "cannot switch to `{target}`: {}",
                out.stderr.trim().replace('\n', "; ")
            )));
        }
    }

    match strategy {
        MergeStrategy::Apply => apply(git, work, dry_run).await,
        MergeStrategy::Merge => merge(git, work, &target, dry_run).await,
        MergeStrategy::CherryPick => cherry_pick(git, work, dry_run).await,
    }
}

async fn apply(git: &Git, work: &WorkResultRef, dry_run: bool) -> anyhow::Result<AdoptResult> {
    let root = git.root.clone();
    let patch = work.patch.as_str();
    if !work.patch.is_file() {
        return Ok(rejected(format!("patch {patch} is missing")));
    }
    let check = git.output(&root, &["apply", "--check", patch]).await?;
    if !check.ok {
        return Ok(AdoptResult::Conflicted {
            paths: apply_conflicts(&check.stderr),
        });
    }
    if dry_run {
        return Ok(AdoptResult::Clean { commit: None });
    }
    let out = git.output(&root, &["apply", patch]).await?;
    if !out.ok {
        return Ok(AdoptResult::Conflicted {
            paths: apply_conflicts(&out.stderr),
        });
    }
    // `apply` leaves the change in the tree for the user to review and commit.
    Ok(AdoptResult::Clean { commit: None })
}

async fn merge(
    git: &Git,
    work: &WorkResultRef,
    target: &str,
    dry_run: bool,
) -> anyhow::Result<AdoptResult> {
    let root = git.root.clone();
    if dry_run {
        let out = git
            .output(
                &root,
                &[
                    "merge-tree",
                    "--write-tree",
                    "--name-only",
                    target,
                    &work.branch,
                ],
            )
            .await?;
        return Ok(merge_tree_verdict(&out));
    }
    let out = git
        .output(&root, &["merge", "--no-edit", "--no-ff", &work.branch])
        .await?;
    if out.ok {
        return Ok(AdoptResult::Clean {
            commit: Some(git.head().await?),
        });
    }
    // The merge is left in progress on purpose: the user resolves or `git merge --abort`s it.
    Ok(AdoptResult::Conflicted {
        paths: unmerged(git).await?,
    })
}

async fn cherry_pick(
    git: &Git,
    work: &WorkResultRef,
    dry_run: bool,
) -> anyhow::Result<AdoptResult> {
    let root = git.root.clone();
    if dry_run {
        let merge_base = format!("--merge-base={}^", work.head);
        let out = git
            .output(
                &root,
                &[
                    "merge-tree",
                    "--write-tree",
                    "--name-only",
                    &merge_base,
                    "HEAD",
                    &work.head,
                ],
            )
            .await?;
        return Ok(merge_tree_verdict(&out));
    }
    let out = git.output(&root, &["cherry-pick", &work.head]).await?;
    if out.ok {
        return Ok(AdoptResult::Clean {
            commit: Some(git.head().await?),
        });
    }
    let paths = unmerged(git).await?;
    if paths.is_empty() {
        return Ok(rejected(format!(
            "cherry-pick of {} failed: {}",
            work.head,
            out.stderr.trim().replace('\n', "; ")
        )));
    }
    Ok(AdoptResult::Conflicted { paths })
}

async fn unmerged(git: &Git) -> anyhow::Result<Vec<Utf8PathBuf>> {
    let root = git.root.clone();
    let out = git
        .run(&root, &["diff", "--name-only", "--diff-filter=U", "-z"])
        .await?;
    Ok(out
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(Utf8PathBuf::from)
        .collect())
}

fn rejected(reason: impl Into<String>) -> AdoptResult {
    AdoptResult::Rejected {
        reason: reason.into(),
    }
}

/// `error: patch failed: <path>:<line>` and `error: <path>: patch does not apply`.
fn apply_conflicts(stderr: &str) -> Vec<Utf8PathBuf> {
    let mut paths: Vec<Utf8PathBuf> = Vec::new();
    for line in stderr.lines() {
        let candidate = if let Some(rest) = line.strip_prefix("error: patch failed: ") {
            rest.rsplit_once(':').map_or(rest, |(p, _)| p)
        } else if let Some(rest) = line.strip_prefix("error: ") {
            match rest.split_once(": patch does not apply") {
                Some((p, _)) => p,
                None => continue,
            }
        } else {
            continue;
        };
        let candidate = Utf8PathBuf::from(candidate);
        if !paths.contains(&candidate) {
            paths.push(candidate);
        }
    }
    paths
}

/// `merge-tree` exits 1 on a conflict; anything else past 0 is git refusing the command, such
/// as a git older than 2.40 that has no `--merge-base`, and is never reported as a conflict.
fn merge_tree_verdict(out: &GitOutput) -> AdoptResult {
    match out.code {
        Some(0) => AdoptResult::Clean { commit: None },
        Some(1) => AdoptResult::Conflicted {
            paths: merge_tree_conflicts(&out.stdout),
        },
        _ => rejected(format!(
            "git merge-tree could not predict the result (a dry run needs git 2.40 or newer): {}",
            out.stderr.trim().replace('\n', "; ")
        )),
    }
}

/// `merge-tree --write-tree --name-only` prints the tree oid, then the conflicted paths.
fn merge_tree_conflicts(stdout: &str) -> Vec<Utf8PathBuf> {
    stdout
        .lines()
        .skip(1)
        .take_while(|l| !l.trim().is_empty())
        .map(Utf8PathBuf::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_errors_name_the_failing_files() {
        let stderr = "error: patch failed: src/lib.rs:12\n\
                      error: src/lib.rs: patch does not apply\n\
                      error: patch failed: a b.txt:1\n";
        assert_eq!(
            apply_conflicts(stderr),
            vec![
                Utf8PathBuf::from("src/lib.rs"),
                Utf8PathBuf::from("a b.txt")
            ]
        );
    }

    #[test]
    fn merge_tree_output_yields_conflicted_paths() {
        let stdout = "4b825dc642cb6eb9a060e54bf8d69288fbee4904\nsrc/lib.rs\nREADME.md\n\nAuto-merging src/lib.rs\n";
        assert_eq!(
            merge_tree_conflicts(stdout),
            vec![
                Utf8PathBuf::from("src/lib.rs"),
                Utf8PathBuf::from("README.md")
            ]
        );
    }

    #[test]
    fn a_merge_tree_that_git_refuses_is_not_a_conflict() {
        let out = |code: i32, stdout: &str, stderr: &str| GitOutput {
            ok: code == 0,
            code: Some(code),
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
        };
        assert_eq!(
            merge_tree_verdict(&out(0, "4b82\n", "")),
            AdoptResult::Clean { commit: None }
        );
        assert_eq!(
            merge_tree_verdict(&out(1, "4b82\nREADME.md\n", "")),
            AdoptResult::Conflicted {
                paths: vec![Utf8PathBuf::from("README.md")]
            }
        );
        let AdoptResult::Rejected { reason } =
            merge_tree_verdict(&out(129, "", "error: unknown option `merge-base=x'"))
        else {
            panic!("an unknown option is not a conflict");
        };
        assert!(reason.contains("git 2.40"), "{reason}");
    }
}
