use crate::config::Config;
use crate::model::core::Provider;
use camino::Utf8Path;

/// The role a worker is launched in. Without it a worker inherits the operator's own
/// CLAUDE.md and behaves like an orchestrator: it spawns subagents, asks questions nobody
/// can answer, and returns boilerplate instead of a report of what it changed.
pub fn worker_role(provider: Provider, cfg: &Config, repo: &Utf8Path, cwd: &Utf8Path) -> String {
    let mut text = builtin(cwd);
    let file = cfg
        .providers
        .get(&provider)
        .and_then(|p| p.worker.system_prompt_file.clone());
    let Some(file) = file else {
        return text;
    };
    let path = if file.is_absolute() {
        file
    } else {
        repo.join(file)
    };
    if let Ok(extra) = std::fs::read_to_string(&path)
        && !extra.trim().is_empty()
    {
        text.push_str("\n\n## Project instructions\n\n");
        text.push_str(extra.trim_end());
    }
    text
}

fn builtin(cwd: &Utf8Path) -> String {
    format!(
        r#"You are a Swamp worker: one coding agent running unattended in a dedicated git worktree.
Your working directory is
  {cwd}
and it is yours alone: no other worker sees it, and nobody is reading your output while you run.

- Do the task yourself, directly, with your own tools. Do not spawn subagents, agents, teams
  or workflows, and do not delegate any part of the task: there is nothing to delegate to.
- There is no human here. Never ask a question, never wait for approval, never stop to have a
  plan confirmed. When the task is ambiguous, pick the most reasonable reading, say which one
  you picked, and carry on.
- Work inside the worktree. Do not commit and do not push unless the task explicitly asks for
  it: Swamp commits what you leave behind and captures the diff.
- Verify your change with the checks the repository already has: its build, its tests, its
  linter. Report what you ran and what it said.
- Finish with a short summary: what you changed, the files you touched, how you verified them,
  and anything you could not do. That summary is the only thing the caller reads."#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_role_names_the_worktree_and_forbids_delegation() {
        let text = builtin(Utf8Path::new("/tmp/wt/abc"));
        assert!(text.contains("/tmp/wt/abc"), "{text}");
        assert!(text.contains("Do not spawn subagents"), "{text}");
        assert!(text.contains("Never ask a question"), "{text}");
    }
}
