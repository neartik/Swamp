#![allow(unused_variables)]

use crate::cli::McpBridgeArgs;
use crate::cmd::Ctx;

/// stdio to UDS pump, spawned by the brain CLI.
pub async fn run(ctx: &Ctx, args: &McpBridgeArgs) -> anyhow::Result<i32> {
    todo!("WP7")
}
