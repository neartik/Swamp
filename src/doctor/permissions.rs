use super::{Check, Level};
use crate::config::Config;
use crate::model::core::Provider;
use std::time::Duration;

const HELP_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) fn unsafe_args(cfg: &Config, out: &mut Vec<Check>) {
    let acked = cfg.limits.unsafe_ack == Some(true);
    for (p, provider) in &cfg.providers {
        let (_, refused) = crate::worker::adapter::gate_unsafe_args(&provider.worker.args, acked);
        if !refused.is_empty() {
            out.push(Check::new(
                format!("providers/{p}/args"),
                Level::Error,
                format!(
                    "providers.{p}.worker.args carries {} without limits.unsafe_ack = true; \
                     the configuration is refused until it is set",
                    refused.join(" ")
                ),
            ));
        }
    }
}

/// Swamp always launches with `--permission-prompts none`, and under it these modes deny
/// every Bash call that is not explicitly allowed: the worker cannot run the tests it was
/// sent to run. Allowing Bash by name is what makes them safe, and `acceptEdits` plus an
/// allowed Bash is the recommended pair, because `auto` denies the file writes as well.
const BASH_DENYING_MODES: [&str; 4] = ["acceptEdits", "plan", "manual", "dontAsk"];

/// The permission check alone, without probing.
pub fn permission_checks(cfg: &Config) -> Vec<Check> {
    let mut out = Vec::new();
    permission_modes(cfg, &mut out);
    out
}

pub(super) fn permission_modes(cfg: &Config, out: &mut Vec<Check>) {
    let Some(provider) = cfg.providers.get(&Provider::Anthropic) else {
        return;
    };
    let worker = &provider.worker;
    if denies_bash(
        worker.permission_mode.as_deref(),
        &worker.allow_tools,
        &worker.args,
    ) {
        out.push(Check::new(
            "providers/anthropic/permission_mode",
            Level::Warn,
            warning("providers.anthropic.worker", &worker.permission_mode),
        ));
    }
    if denies_bash(
        cfg.brain.permission_mode.as_deref(),
        &cfg.brain.allow_tools,
        &[],
    ) {
        out.push(Check::new(
            "brain/permission_mode",
            Level::Warn,
            warning("brain", &cfg.brain.permission_mode),
        ));
    }
}

fn warning(key: &str, mode: &Option<String>) -> String {
    let mode = mode.as_deref().unwrap_or("");
    format!(
        "{key}.permission_mode = \"{mode}\" denies every Bash call under \
         --permission-prompts none and Bash is not allowed, so it cannot run tests, a build \
         or git; add \"Bash\" to {key}.allow_tools"
    )
}

/// A mode from the list denies Bash unless the tool is allowed by name, either through
/// `allow_tools` or through a raw `--allowedTools` in `worker.args`.
fn denies_bash(mode: Option<&str>, allow: &[String], args: &[String]) -> bool {
    let mode = mode.unwrap_or("");
    if !BASH_DENYING_MODES
        .iter()
        .any(|m| m.eq_ignore_ascii_case(mode))
    {
        return false;
    }
    !allow.iter().any(|t| is_bash(t)) && !allowed_in_args(args)
}

fn is_bash(tool: &str) -> bool {
    tool == "Bash" || tool.starts_with("Bash(")
}

fn allowed_in_args(args: &[String]) -> bool {
    let Some(at) = args
        .iter()
        .position(|a| a == "--allowedTools" || a == "--allowed-tools")
    else {
        return false;
    };
    args[at + 1..]
        .iter()
        .take_while(|a| !a.starts_with("--"))
        .any(|t| is_bash(t))
}

/// `--schema`: the configured claude permission modes against the `--permission-mode`
/// choices the installed CLI lists. claude rejects a misspelt mode only once a worker starts.
pub(super) async fn mode_spelling(cfg: &Config) -> Check {
    const NAME: &str = "protocol/permission_mode";
    let modes = claude_modes(cfg);
    if modes.is_empty() {
        return Check::new(NAME, Level::Note, "no claude permission mode configured");
    }
    let Some(account) = cfg
        .accounts
        .iter()
        .find(|a| a.provider == Provider::Anthropic && which::which(&a.exec).is_ok())
    else {
        return Check::new(NAME, Level::Note, "no claude executable on PATH to ask");
    };
    let help = help_text(&account.exec, &account.env).await;
    let Some(choices) = help.as_deref().and_then(permission_mode_choices) else {
        return Check::new(
            NAME,
            Level::Note,
            format!(
                "`{} --help` lists no --permission-mode choices; spelling not checked",
                account.exec
            ),
        );
    };
    spelling(&modes, &choices, &account.exec)
}

/// Every configured claude mode with the key it came from.
fn claude_modes(cfg: &Config) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    if let Some(mode) = cfg
        .providers
        .get(&Provider::Anthropic)
        .and_then(|p| p.worker.permission_mode.clone())
        .filter(|m| !m.trim().is_empty())
    {
        out.push(("providers.anthropic.worker.permission_mode", mode));
    }
    if cfg.brain.provider.unwrap_or(Provider::Anthropic) == Provider::Anthropic
        && let Some(mode) = cfg
            .brain
            .permission_mode
            .clone()
            .filter(|m| !m.trim().is_empty())
    {
        out.push(("brain.permission_mode", mode));
    }
    out
}

pub(crate) fn spelling(modes: &[(&str, String)], choices: &[String], exec: &str) -> Check {
    const NAME: &str = "protocol/permission_mode";
    let mut wrong = Vec::new();
    for (key, mode) in modes {
        if choices.iter().any(|c| c == mode) {
            continue;
        }
        match choices.iter().find(|c| c.eq_ignore_ascii_case(mode)) {
            Some(right) => wrong.push(format!("{key} = \"{mode}\" is spelt \"{right}\"")),
            None => wrong.push(format!("{key} = \"{mode}\" is not a mode")),
        }
    }
    if wrong.is_empty() {
        let names: Vec<&str> = modes.iter().map(|(_, m)| m.as_str()).collect();
        return Check::new(
            NAME,
            Level::Ok,
            format!("{} accepted by {exec}", names.join(", ")),
        );
    }
    Check::new(
        NAME,
        Level::Error,
        format!(
            "{}; {exec} accepts {}",
            wrong.join(", "),
            choices.join(", ")
        ),
    )
}

/// The quoted `(choices: ...)` of `--permission-mode` in a claude `--help`.
pub(crate) fn permission_mode_choices(help: &str) -> Option<Vec<String>> {
    let mut lines = help
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("--permission-mode "));
    let mut text = lines.next()?.to_owned();
    for l in lines.take_while(|l| !l.trim_start().starts_with('-')) {
        text.push(' ');
        text.push_str(l.trim());
    }
    let list = text.split("(choices:").nth(1)?.split(')').next()?;
    let choices: Vec<String> = list
        .split(',')
        .map(|c| c.trim().trim_matches('"').to_owned())
        .filter(|c| !c.is_empty())
        .collect();
    (!choices.is_empty()).then_some(choices)
}

async fn help_text(exec: &str, env: &std::collections::BTreeMap<String, String>) -> Option<String> {
    let mut cmd = tokio::process::Command::new(exec);
    cmd.arg("--help").stdin(std::process::Stdio::null());
    for (k, v) in env {
        cmd.env(k, shellexpand::tilde(v).into_owned());
    }
    let out = tokio::time::timeout(HELP_TIMEOUT, cmd.output())
        .await
        .ok()?
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recorded_help() -> String {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/ref/claude-help.txt");
        std::fs::read_to_string(path).expect("docs/ref/claude-help.txt")
    }

    #[test]
    fn the_recorded_help_lists_the_permission_modes() {
        let choices = permission_mode_choices(&recorded_help()).expect("choices");
        assert_eq!(
            choices,
            [
                "acceptEdits",
                "auto",
                "bypassPermissions",
                "manual",
                "dontAsk",
                "plan"
            ]
        );
    }

    #[test]
    fn a_misspelt_mode_is_an_error_that_names_the_right_spelling() {
        let choices = permission_mode_choices(&recorded_help()).expect("choices");
        let modes = [
            ("brain.permission_mode", "acceptedits".to_owned()),
            (
                "providers.anthropic.worker.permission_mode",
                "yolo".to_owned(),
            ),
        ];
        let c = spelling(&modes, &choices, "claude");
        assert_eq!(c.level, Level::Error);
        assert!(
            c.detail.contains("is spelt \"acceptEdits\""),
            "{}",
            c.detail
        );
        assert!(c.detail.contains("\"yolo\" is not a mode"), "{}", c.detail);

        let ok = spelling(
            &[("brain.permission_mode", "plan".into())],
            &choices,
            "claude",
        );
        assert_eq!(ok.level, Level::Ok, "{}", ok.detail);
    }

    #[test]
    fn help_without_the_flag_lists_no_choices() {
        assert_eq!(
            permission_mode_choices("Usage: claude [options]\n  --help\n"),
            None
        );
    }
}
