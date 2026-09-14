use crate::config::Config;
use crate::error::SwampError;
use crate::model::result::IsolationMode;
use std::collections::BTreeSet;

/// One validation failure, named by the offending config key.
#[derive(Debug, Clone)]
pub struct Problem {
    pub key: String,
    pub detail: String,
}

/// Reports ALL problems at once; a single bad key must not hide the next one.
pub fn validate(cfg: &mut Config) -> Result<(), SwampError> {
    if cfg.workspace.isolation == Some(IsolationMode::Shared) && cfg.limits.max_parallel != Some(1)
    {
        cfg.limits.max_parallel = Some(1);
        cfg.warnings.push(
            "workspace.isolation = \"shared\" forces limits.max_parallel = 1: the real checkout \
             cannot host two workers at once"
                .into(),
        );
    }

    let problems = problems(cfg);
    if problems.is_empty() {
        return Ok(());
    }
    let rendered = problems
        .iter()
        .map(|p| format!("  {}: {}", p.key, p.detail))
        .collect::<Vec<_>>()
        .join("\n");
    Err(SwampError::ConfigInvalid(rendered))
}

pub fn problems(cfg: &Config) -> Vec<Problem> {
    let mut out = Vec::new();
    let mut push = |key: String, detail: String| out.push(Problem { key, detail });

    let mut seen = BTreeSet::new();
    for (i, a) in cfg.accounts.iter().enumerate() {
        if !seen.insert(a.id.0.clone()) {
            push(
                format!("accounts[{i}].id"),
                format!("duplicate account id `{}`", a.id.0),
            );
        }
        if a.max_concurrency == Some(0) {
            push(
                format!("accounts[{i}].max_concurrency"),
                "must be at least 1; 0 makes the account unusable".into(),
            );
        }
        if a.exec.trim().is_empty() {
            push(
                format!("accounts[{i}].exec"),
                "must name an executable on PATH".into(),
            );
        }
    }

    if let Some(id) = &cfg.brain.account {
        match cfg.accounts.iter().find(|a| &a.id == id) {
            None => push(
                "brain.account".into(),
                format!("`{}` is not a configured account", id.0),
            ),
            Some(a) => {
                let want = cfg.brain.provider;
                if let Some(want) = want
                    && a.provider != want
                {
                    push(
                        "brain.account".into(),
                        format!(
                            "`{}` belongs to provider {} but brain.provider is {want}",
                            id.0, a.provider
                        ),
                    );
                }
            }
        }
    }

    let (warn, stop) = (cfg.cooldown.quota_warn_at, cfg.cooldown.quota_stop_at);
    for (key, v) in [
        ("cooldown.quota_warn_at", warn),
        ("cooldown.quota_stop_at", stop),
    ] {
        if let Some(v) = v
            && !(0.0..=1.0).contains(&v)
        {
            push(key.into(), format!("{v} is outside 0.0 ..= 1.0"));
        }
    }
    if let (Some(w), Some(s)) = (warn, stop)
        && w >= s
    {
        push(
            "cooldown.quota_warn_at".into(),
            format!("{w} must be below cooldown.quota_stop_at ({s})"),
        );
    }

    if cfg.limits.max_parallel == Some(0) {
        push(
            "limits.max_parallel".into(),
            "must be at least 1".to_string(),
        );
    }

    for (p, fc) in &cfg.failure {
        for (field, patterns) in [
            ("rate_limit", &fc.rate_limit),
            ("auth", &fc.auth),
            ("overloaded", &fc.overloaded),
        ] {
            for (i, pat) in patterns.iter().enumerate() {
                if let Err(e) = regex::Regex::new(pat) {
                    push(
                        format!("failure.{p}.{field}[{i}]"),
                        format!("unparseable regex: {}", first_line(&e.to_string())),
                    );
                }
            }
        }
    }
    for (i, pat) in cfg.journal.redact.iter().enumerate() {
        if let Err(e) = regex::Regex::new(pat) {
            push(
                format!("journal.redact[{i}]"),
                format!("unparseable regex: {}", first_line(&e.to_string())),
            );
        }
    }

    let acked = cfg.limits.unsafe_ack == Some(true);
    for (p, pc) in &cfg.providers {
        for (field, args) in [
            ("args", &pc.worker.args),
            ("readonly_args", &pc.worker.readonly_args),
        ] {
            for (i, arg) in args.iter().enumerate() {
                if arg.starts_with("--dangerously") && !acked {
                    push(
                        format!("providers.{p}.worker.{field}[{i}]"),
                        format!("`{arg}` requires limits.unsafe_ack = true"),
                    );
                }
            }
        }
    }

    for (t, tc) in &cfg.tiers {
        if tc.provider_order.is_empty() {
            continue;
        }
        let servable = tc.provider_order.iter().any(|p| {
            cfg.providers
                .get(p)
                .is_some_and(|pc| pc.models.contains_key(t))
                || cfg
                    .accounts
                    .iter()
                    .any(|a| &a.provider == p && a.models.contains_key(t))
        });
        if !servable {
            push(
                format!("tiers.{t}.provider_order"),
                format!("no provider in {:?} maps tier {t} to a model", names(tc)),
            );
        }
    }

    if let Some(root) = &cfg.workspace.root {
        let expanded = super::resolve::expand_path(root.as_str());
        if expanded.as_str().is_empty() {
            push("workspace.root".into(), "must not be empty".into());
        } else if expanded.is_relative() && expanded.iter().next() == Some(".swamp") {
            push(
                "workspace.root".into(),
                "must live outside the repo's .swamp directory".into(),
            );
        } else {
            let repo_dot = cfg.sources.iter().find_map(|s| {
                let parent = s.parent()?;
                (parent.file_name() == Some(".swamp")).then(|| parent.to_owned())
            });
            if let Some(dot) = repo_dot
                && expanded.starts_with(&dot)
            {
                push(
                    "workspace.root".into(),
                    format!("{expanded} is inside {dot}; worktrees must live outside the repo"),
                );
            }
        }
    }

    out
}

fn names(tc: &crate::config::schema::TierCfg) -> Vec<String> {
    tc.provider_order.iter().map(|p| p.to_string()).collect()
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or(s).trim().to_owned()
}
