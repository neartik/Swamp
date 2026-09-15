use crate::cli::McpBridgeArgs;
use crate::cmd::Ctx;

/// stdio to UDS pump, spawned by the brain CLI. No protocol logic lives here.
pub async fn run(ctx: &Ctx, args: &McpBridgeArgs) -> anyhow::Result<i32> {
    let _ = ctx;
    crate::mcp::run_bridge(&args.socket).await?;
    Ok(0)
}
