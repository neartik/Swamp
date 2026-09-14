#![allow(dead_code, unused_variables)]

use crate::brain::Brain;
use crate::cmd::Ctx;
use crate::dispatch::Dispatcher;
use std::sync::Arc;

/// rustyline REPL rendering BrainEvent plus inline worker progress.
pub async fn repl(brain: Box<dyn Brain>, disp: Arc<Dispatcher>, ctx: &Ctx) -> anyhow::Result<i32> {
    todo!("WP7")
}
