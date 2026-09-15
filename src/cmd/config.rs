use crate::cli::{ConfigArgs, ConfigCmd};
use crate::cmd::Ctx;
use crate::config::{Config, load};

/// A starter file. Model ids belong in config, so the template only names the keys.
const TEMPLATE: &str = r#"version = 1

[limits]
max_parallel = 4
worker_timeout = "25m"

[dispatch]
policy = "least-loaded"
default_provider = "anthropic"
default_tier = "mid"

# Model ids live only here. Uncomment and fill these in for your plan: until you do,
# `swamp doctor` reports the tiers as unmapped rather than letting a placeholder through.
[providers.anthropic]
# models = { high = "opus", mid = "sonnet", low = "haiku" }

# One entry per subscription. `exec` is a wrapper on PATH that sets the CLI's config dir;
# swamp never touches credentials.
[[accounts]]
id = "main"
provider = "anthropic"
exec = "claude-main"
max_concurrency = 2
"#;

/// Inspect and validate the effective configuration.
pub async fn run(ctx: &Ctx, args: &ConfigArgs) -> anyhow::Result<i32> {
    match &args.command {
        ConfigCmd::Show { effective } => {
            let mut text = String::new();
            if *effective {
                text.push_str("# effective configuration, lowest priority first:\n");
                text.push_str("#   built-in defaults\n");
                for s in &ctx.cfg.sources {
                    text.push_str(&format!("#   {s}\n"));
                }
                text.push_str("#   SWAMP_* environment, then --config and CLI flags\n\n");
            }
            text.push_str(&ctx.cfg.effective_toml());
            ctx.out(&text);
            Ok(0)
        }
        ConfigCmd::Path => {
            let user = load::user_config_path();
            let repo = load::repo_config_path(&ctx.paths.repo);
            let mut text = String::new();
            if let Some(user) = user {
                text.push_str(&format!("user  {user}{}\n", mark(user.is_file())));
            }
            text.push_str(&format!("repo  {repo}{}\n", mark(repo.is_file())));
            ctx.out(&text);
            Ok(0)
        }
        ConfigCmd::Validate => {
            // Reload from scratch: the in-memory config is already known good.
            let cfg = Config::load(&ctx.paths.repo, None, None)?;
            let mut text = format!("ok: {} layers\n", cfg.sources.len() + 1);
            for w in &cfg.warnings {
                text.push_str(&format!("warning: {w}\n"));
            }
            ctx.out(&text);
            Ok(0)
        }
        ConfigCmd::Init => {
            let path = load::repo_config_path(&ctx.paths.repo);
            if path.is_file() {
                println!("{path} already exists");
                return Ok(1);
            }
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, TEMPLATE)?;
            println!("wrote {path}");
            Ok(0)
        }
    }
}

fn mark(exists: bool) -> &'static str {
    if exists { "" } else { "  (missing)" }
}
