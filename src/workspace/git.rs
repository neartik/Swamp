use crate::error::SwampError;
use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use std::process::Stdio;

/// Thin async wrapper over the `git` CLI. Porcelain only, -z everywhere.
#[derive(Debug, Clone)]
pub struct Git {
    pub root: Utf8PathBuf,
}

/// A completed `git` invocation, including the failing ones: merge and apply report
/// conflicts through a non-zero exit plus stdout, and that is data, not an error.
#[derive(Debug, Clone)]
pub struct GitOutput {
    pub ok: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Git {
    pub async fn discover(cwd: &Utf8Path) -> Result<Git, SwampError> {
        let probe = Git {
            root: cwd.to_owned(),
        };
        let out = probe
            .output(cwd, &["rev-parse", "--show-toplevel"])
            .await
            .map_err(|_| SwampError::NotAGitRepo(cwd.to_owned()))?;
        if !out.ok {
            return Err(SwampError::NotAGitRepo(cwd.to_owned()));
        }
        let root = out.stdout.trim();
        if root.is_empty() {
            return Err(SwampError::NotAGitRepo(cwd.to_owned()));
        }
        Ok(Git {
            root: Utf8PathBuf::from(root),
        })
    }

    pub async fn output(&self, cwd: &Utf8Path, args: &[&str]) -> anyhow::Result<GitOutput> {
        let out = tokio::process::Command::new("git")
            .current_dir(cwd)
            .args([
                "-c",
                "core.quotepath=false",
                "-c",
                "advice.detachedHead=false",
            ])
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .output()
            .await
            .with_context(|| format!("running `git {}` in {cwd}", args.join(" ")))?;
        Ok(GitOutput {
            ok: out.status.success(),
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    pub async fn run(&self, cwd: &Utf8Path, args: &[&str]) -> anyhow::Result<String> {
        let out = self.output(cwd, args).await?;
        if !out.ok {
            anyhow::bail!(
                "git {} failed in {cwd} (exit {}): {}",
                args.join(" "),
                out.code.unwrap_or(-1),
                out.stderr.trim()
            );
        }
        Ok(out.stdout)
    }

    pub async fn head(&self) -> anyhow::Result<String> {
        let out = self.run(&self.root, &["rev-parse", "HEAD"]).await?;
        Ok(out.trim().to_owned())
    }

    pub async fn is_clean(&self) -> anyhow::Result<bool> {
        self.is_clean_at(&self.root).await
    }

    /// Same check inside any worktree of this repo.
    pub async fn is_clean_at(&self, cwd: &Utf8Path) -> anyhow::Result<bool> {
        let out = self.run(cwd, &["status", "--porcelain", "-z"]).await?;
        Ok(out.trim_matches('\0').is_empty())
    }

    /// The `--include-dirty` base.
    pub async fn stash_create(&self) -> anyhow::Result<Option<String>> {
        let out = self.run(&self.root, &["stash", "create"]).await?;
        let sha = out.trim();
        Ok((!sha.is_empty()).then(|| sha.to_owned()))
    }

    pub async fn version(&self) -> anyhow::Result<(u32, u32)> {
        let out = self.run(&self.root, &["--version"]).await?;
        let digits = out
            .split_whitespace()
            .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))
            .context("git --version printed no version number")?;
        let mut parts = digits.split('.');
        let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        Ok((major, minor))
    }
}
