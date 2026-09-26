use super::{Check, Level};
use crate::config::Config;
use crate::model::core::Provider;

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
