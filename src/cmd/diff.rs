use crate::cli::DiffArgs;
use crate::cmd::Ctx;
use crate::model::core::FileChange;
use camino::Utf8Path;
use std::io::Write;

/// Show one worker node's diff. Git is authoritative: this is the patch that was captured.
pub async fn run(ctx: &Ctx, args: &DiffArgs) -> anyhow::Result<i32> {
    let (paths, node) = ctx.find_node(&args.node)?;
    let patch = node
        .work
        .as_ref()
        .map(|w| w.patch.clone())
        .unwrap_or_else(|| paths.patch(node.id));

    if args.name_only {
        let mut text = String::new();
        for f in &node.files {
            text.push_str(&format!("{}\n", f.path));
        }
        ctx.out(&text);
        return Ok(0);
    }
    if args.stat {
        // The journal's file list is the record; the patch is the fallback when a node
        // finished without one.
        let files: Vec<(String, u32, u32)> = if node.files.is_empty() {
            stat_of_patch(&patch)
        } else {
            node.files.iter().map(cell).collect()
        };
        if files.is_empty() {
            ctx.out(&format!("no patch recorded for {}\n", node.id.short()));
            return Ok(0);
        }
        ctx.out(&render_stat(&files));
        return Ok(0);
    }

    let bytes = std::fs::read(&patch)
        .map_err(|e| anyhow::anyhow!("no patch for node {}: {patch}: {e}", node.id.short()))?;
    let mut out = std::io::stdout().lock();
    out.write_all(&bytes)?;
    out.flush()?;
    Ok(0)
}

fn cell(f: &FileChange) -> (String, u32, u32) {
    (f.path.to_string(), f.added, f.removed)
}

/// `git diff --stat`: aligned paths, a scaled +/- bar, then the summary line.
fn render_stat(files: &[(String, u32, u32)]) -> String {
    const BAR_MAX: u32 = 40;
    let width = files.iter().map(|(p, ..)| p.len()).max().unwrap_or(0);
    let widest = files.iter().map(|(_, a, r)| a + r).max().unwrap_or(0);
    let mut out = String::new();
    let (mut ins, mut del) = (0u32, 0u32);
    for (path, added, removed) in files {
        ins += added;
        del += removed;
        let total = added + removed;
        let (plus, minus) = if widest > BAR_MAX {
            let scale = |n: u32| (n * BAR_MAX).div_ceil(widest).min(BAR_MAX);
            (scale(*added), scale(*removed))
        } else {
            (*added, *removed)
        };
        out.push_str(&format!(
            " {path:width$} | {total:>4} {}{}\n",
            "+".repeat(plus as usize),
            "-".repeat(minus as usize),
        ));
    }
    // git omits a zero clause entirely, and so does this.
    let mut summary = format!(" {} changed", plural(files.len() as u32, "file"));
    if ins > 0 {
        summary.push_str(&format!(", {}(+)", plural(ins, "insertion")));
    }
    if del > 0 {
        summary.push_str(&format!(", {}(-)", plural(del, "deletion")));
    }
    out.push_str(&summary);
    out.push('\n');
    out
}

fn plural(n: u32, word: &str) -> String {
    let s = if n == 1 { "" } else { "s" };
    format!("{n} {word}{s}")
}

/// Counts the +/- lines of a unified diff per file, so `--stat` works from the patch alone.
fn stat_of_patch(patch: &Utf8Path) -> Vec<(String, u32, u32)> {
    let Ok(text) = std::fs::read_to_string(patch) else {
        return Vec::new();
    };
    let mut out: Vec<(String, u32, u32)> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            out.push((rest.trim().to_owned(), 0, 0));
        } else if let Some(last) = out.last_mut() {
            if line.starts_with("+++") || line.starts_with("---") {
                continue;
            }
            if line.starts_with('+') {
                last.1 += 1;
            } else if line.starts_with('-') {
                last.2 += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_patch_alone_is_enough_for_a_stat() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = camino::Utf8PathBuf::from_path_buf(dir.path().join("patch.diff"))
            .expect("utf8 tempdir");
        std::fs::write(
            &path,
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n\
             @@ -1,2 +1,3 @@\n fn main() {}\n+// one\n+// two\n-// gone\n",
        )
        .expect("patch");
        let files = stat_of_patch(&path);
        assert_eq!(files, vec![("src/lib.rs".to_owned(), 2, 1)]);
        let text = render_stat(&files);
        assert!(text.contains("src/lib.rs |    3 ++-"), "{text}");
        assert!(
            text.contains("1 file changed, 2 insertions(+), 1 deletion(-)"),
            "{text}"
        );
    }

    /// git prints no `0 deletions(-)` clause, and neither does this.
    #[test]
    fn a_zero_clause_is_omitted_the_way_git_omits_it() {
        let only_added = render_stat(&[("a.rs".to_owned(), 28, 0), ("b.rs".to_owned(), 1, 0)]);
        assert!(
            only_added.contains("2 files changed, 29 insertions(+)\n"),
            "{only_added}"
        );
        assert!(!only_added.contains("deletion"), "{only_added}");

        let only_removed = render_stat(&[("a.rs".to_owned(), 0, 1)]);
        assert!(
            only_removed.contains("1 file changed, 1 deletion(-)\n"),
            "{only_removed}"
        );
        assert!(!only_removed.contains("insertion"), "{only_removed}");

        // A binary-only change counts neither, exactly like `git diff --stat`.
        let binary = render_stat(&[("logo.png".to_owned(), 0, 0)]);
        assert!(binary.contains("1 file changed\n"), "{binary}");
    }
}
