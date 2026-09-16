use crate::config::Config;
use crate::error::SwampError;
use std::collections::BTreeSet;

/// One validation failure, named by the offending config key.
#[derive(Debug, Clone)]
pub struct Problem {
    pub key: String,
    pub detail: String,
}

/// Reports ALL problems at once; a single bad key must not hide the next one.
pub fn validate(cfg: &mut Config) -> Result<(), SwampError> {
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

    for (key, d) in [
        ("cooldown.min", cfg.cooldown.min),
        ("cooldown.max", cfg.cooldown.max),
        ("cooldown.default", cfg.cooldown.default),
    ] {
        if let Some(d) = d
            && d > crate::config::MAX_COOLDOWN
        {
            push(
                key.into(),
                format!(
                    "{}d is past the {}d ceiling; a timer that long parks the account for good",
                    d.as_secs() / 86_400,
                    crate::config::MAX_COOLDOWN.as_secs() / 86_400
                ),
            );
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

    if let Some(w) = &cfg.dispatch.weights {
        for (key, v) in [
            ("util", w.util),
            ("load", w.load),
            ("share", w.share),
            ("weight", w.weight),
            ("idle", w.idle),
        ] {
            if let Some(v) = v
                && !(v.is_finite() && v >= 0.0)
            {
                push(
                    format!("dispatch.weights.{key}"),
                    format!("{v} must be finite and non-negative"),
                );
            }
        }
    }
    if let Some(age) = cfg.dispatch.quota_max_age
        && age < std::time::Duration::from_secs(10)
    {
        push(
            "dispatch.quota_max_age".into(),
            "must be at least 10s".to_string(),
        );
    }
    if cfg
        .dispatch
        .near_exhaustion_penalty
        .is_some_and(|p| !(p.is_finite() && p >= 0.0))
    {
        push(
            "dispatch.near_exhaustion_penalty".into(),
            "must be finite and non-negative".to_string(),
        );
    }
    if let Some(fsync) = cfg.journal.fsync.as_deref()
        && let Err(e) = fsync.parse::<crate::journal::writer::FsyncPolicy>()
    {
        push("journal.fsync".into(), format!("{e}"));
    }
    for (p, pc) in &cfg.providers {
        if let Some(src) = pc.quota_source.as_deref()
            && !matches!(src, "auto" | "rollout" | "app-server" | "none")
        {
            push(
                format!("providers.{p}.quota_source"),
                format!("`{src}` is not one of auto, rollout, app-server, none"),
            );
        }
        if pc.estimated_window.is_some() != pc.estimated_window_tokens.is_some() {
            push(
                format!("providers.{p}.estimated_window"),
                "estimated_window and estimated_window_tokens must be set together".to_string(),
            );
        }
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

    // An explicitly empty value is worse than an absent one: the adapter cannot tell them
    // apart at the argv, and the vendor CLI rejects `--permission-mode ''`.
    if cfg
        .brain
        .permission_mode
        .as_deref()
        .is_some_and(str::is_empty)
    {
        push("brain.permission_mode".into(), "must not be empty".into());
    }
    for (p, pc) in &cfg.providers {
        for (key, value) in [
            ("permission_mode", &pc.worker.permission_mode),
            ("sandbox", &pc.worker.sandbox),
        ] {
            if value.as_deref().is_some_and(str::is_empty) {
                push(
                    format!("providers.{p}.worker.{key}"),
                    "must not be empty; remove the key to leave it to the CLI".into(),
                );
            }
        }
        if let Some(a) = &pc.adapter
            && a != builtin_adapter(*p)
        {
            push(
                format!("providers.{p}.adapter"),
                format!(
                    "`{a}` is not built in; {p} is served by `{}`",
                    builtin_adapter(*p)
                ),
            );
        }
        for (t, model) in &pc.models {
            if model.starts_with('<') && model.ends_with('>') {
                push(
                    format!("providers.{p}.models.{t}"),
                    format!("`{model}` is the template placeholder; fill in a real model id"),
                );
            }
        }
    }
    for (i, a) in cfg.accounts.iter().enumerate() {
        for (t, model) in &a.models {
            if model.starts_with('<') && model.ends_with('>') {
                push(
                    format!("accounts[{i}].models.{t}"),
                    format!("`{model}` is the template placeholder; fill in a real model id"),
                );
            }
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

fn builtin_adapter(p: crate::model::core::Provider) -> &'static str {
    match p {
        crate::model::core::Provider::Anthropic => "claude-cli",
        crate::model::core::Provider::Openai => "codex-cli",
    }
}

fn names(tc: &crate::config::schema::TierCfg) -> Vec<String> {
    tc.provider_order.iter().map(|p| p.to_string()).collect()
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or(s).trim().to_owned()
}
