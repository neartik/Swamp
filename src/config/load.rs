use crate::config::schema::Schema;
use crate::error::SwampError;
use camino::{Utf8Path, Utf8PathBuf};

/// One configuration layer plus where it came from, for `config show --effective`.
#[derive(Debug, Clone)]
pub struct Layer {
    pub origin: String,
    pub schema: Schema,
}

/// Built-in defaults, the lowest layer. No model id and no account lives here: those come
/// from the config file only.
pub const DEFAULTS_TOML: &str = r#"
version = 1

[limits]
max_nodes_per_run = 32
max_depth = 2
worker_timeout = "25m"
brain_turn_timeout = "15m"
grace_period = "5s"
max_prompt_bytes = 200000
max_result_bytes = 8000
unsafe_ack = false

[brain]
transport = "cli"
provider = "anthropic"
tier = "high"
reserve_brain_slot = true
permission_mode = "acceptEdits"
include_partial_messages = true
system_prompt_file = ".swamp/brain.md"

[dispatch]
policy = "least-loaded"
max_attempts = 3
cross_provider_failover = false
default_provider = "anthropic"
default_tier = "mid"
quota_max_age = "60s"
near_exhaustion_penalty = 2.0

  [dispatch.weights]
  util = 0.50
  load = 0.30
  share = 0.15
  weight = 0.05
  idle = 0.02

[providers.openai]
quota_source = "auto"
estimated_window = "7d"
estimated_window_tokens = 0

[cooldown]
min = "60s"
max = "6h"
default = "15m"
breaker_threshold = 3
quota_warn_at = 0.90
quota_stop_at = 0.98

[workspace]
isolation = "worktree"
root = "~/.swamp/worktrees"
base = "HEAD"
branch_prefix = "swamp"
include_dirty = false
require_clean = true
commit_on_success = true
commit_template = "swamp({tier}): {title}\n\nnode: {node}\nrun: {run}"
keep_on_failure = true
post_create_timeout = "5m"

[journal]
fsync = "barrier"
max_line_bytes = 8388608
keep_runs = 200
keep_runs_for = "30d"

[ui]
refresh_hz = 20
tree_width = 46
show_thinking = false
tail_lines = 200
"#;

/// Env vars are `SWAMP_<SECTION>__<KEY>`; anything else (SWAMP_LOG, SWAMP_DEPTH) is not config.
const ENV_SECTIONS: &[&str] = &[
    "version",
    "limits",
    "brain",
    "dispatch",
    "cooldown",
    "workspace",
    "journal",
    "providers",
    "accounts",
    "tiers",
    "failure",
    "pricing",
    "ui",
    "profiles",
];

pub fn default_layer() -> Layer {
    Layer {
        origin: "defaults".into(),
        schema: toml::from_str(DEFAULTS_TOML).expect("built-in defaults parse"),
    }
}

pub fn user_config_path() -> Option<camino::Utf8PathBuf> {
    if let Ok(dir) = std::env::var("SWAMP_CONFIG_DIR") {
        return Some(Utf8PathBuf::from(dir).join("config.toml"));
    }
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        return Some(Utf8PathBuf::from(dir).join("swamp").join("config.toml"));
    }
    let home = std::env::var("HOME").ok()?;
    Some(
        Utf8PathBuf::from(home)
            .join(".config")
            .join("swamp")
            .join("config.toml"),
    )
}

pub fn repo_config_path(repo: &Utf8Path) -> camino::Utf8PathBuf {
    repo.join(".swamp").join("config.toml")
}

pub fn read_layer(path: &Utf8Path) -> Result<Layer, SwampError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| SwampError::ConfigInvalid(format!("{path}: {e}")))?;
    let schema: Schema = toml::from_str(&text)
        .map_err(|e| SwampError::ConfigInvalid(format!("{path}: {e}{}", removed_key_hint(&e))))?;
    Ok(Layer {
        origin: path.to_string(),
        schema,
    })
}

/// Keys WP-A deleted, with the reason and the replacement, so a config written for an older
/// Swamp gets a redirect instead of a bare "unknown field".
const REMOVED_KEYS: &[&str] = &[
    "max_parallel",
    "max_parallel_dispatch",
    "max_high_tier_concurrent",
    "run_budget_usd",
    "node_budget_usd",
];

fn removed_key_hint(e: &toml::de::Error) -> String {
    let msg = e.to_string();
    REMOVED_KEYS
        .iter()
        .find(|k| msg.contains(&format!("unknown field `{k}`")))
        .map(|k| {
            format!(
                "\n`{k}` was removed: Swamp enforces no global parallelism cap and no budget \
                 cap; set accounts[].max_concurrency per account for a concurrency ceiling."
            )
        })
        .unwrap_or_default()
}

pub fn env_layer() -> Result<Layer, SwampError> {
    let mut table = toml::Table::new();
    let mut vars: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| k.starts_with("SWAMP_"))
        .collect();
    vars.sort();

    for (key, raw) in vars {
        let lowered = key["SWAMP_".len()..].to_ascii_lowercase();
        let path: Vec<String> = lowered.split("__").map(str::to_owned).collect();
        if !ENV_SECTIONS.contains(&path[0].as_str()) {
            continue;
        }
        let mut one = toml::Table::new();
        set_path(&mut one, &path, parse_scalar(&raw));
        toml::Value::Table(one.clone())
            .try_into::<Schema>()
            .map_err(|e| SwampError::ConfigInvalid(format!("{key}: {e}{}", removed_key_hint(&e))))?;
        set_path(&mut table, &path, parse_scalar(&raw));
    }

    let schema = toml::Value::Table(table)
        .try_into()
        .map_err(|e| SwampError::ConfigInvalid(format!("SWAMP_* environment: {e}")))?;
    Ok(Layer {
        origin: "SWAMP_* environment".into(),
        schema,
    })
}

/// Later layers win. Tables merge key by key; an empty list never clears a populated one.
pub fn merge(layers: Vec<Layer>) -> Schema {
    let mut acc = toml::Value::Table(toml::Table::new());
    for layer in &layers {
        let value = toml::Value::try_from(&layer.schema).expect("config layer serializes");
        merge_value(&mut acc, value);
    }
    acc.try_into().expect("merged config layers deserialize")
}

pub fn apply_profile(s: &mut Schema, profile: &str) -> Result<(), SwampError> {
    let entries = s.profiles.get(profile).cloned().ok_or_else(|| {
        let known: Vec<&str> = s.profiles.keys().map(String::as_str).collect();
        SwampError::ConfigInvalid(format!(
            "  profiles.{profile}: no such profile (known: {})",
            if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            }
        ))
    })?;

    let mut value = toml::Value::try_from(&*s).expect("config serializes");
    let table = value.as_table_mut().expect("config is a table");
    for (dotted, v) in entries {
        let path: Vec<String> = dotted.split('.').map(str::to_owned).collect();
        set_path(table, &path, v);
    }
    *s = value
        .try_into()
        .map_err(|e| SwampError::ConfigInvalid(format!("  profiles.{profile}: {e}")))?;
    Ok(())
}

fn parse_scalar(raw: &str) -> toml::Value {
    toml::from_str::<toml::Table>(&format!("v = {raw}"))
        .ok()
        .and_then(|t| t.get("v").cloned())
        .unwrap_or_else(|| toml::Value::String(raw.to_owned()))
}

fn set_path(table: &mut toml::Table, path: &[String], value: toml::Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut cursor = table;
    for segment in parents {
        let entry = cursor
            .entry(segment.clone())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !entry.is_table() {
            *entry = toml::Value::Table(toml::Table::new());
        }
        cursor = entry.as_table_mut().expect("replaced with a table above");
    }
    cursor.insert(last.clone(), value);
}

fn merge_value(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(existing) => merge_value(existing, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (_, toml::Value::Array(o)) if o.is_empty() => {}
        (b, o) => *b = o,
    }
}
