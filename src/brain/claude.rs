#![allow(dead_code, unused_variables)]

use crate::brain::{Brain, BrainEvent};
use crate::model::core::SessionHandle;
use async_trait::async_trait;
use tokio::sync::mpsc;

/// Persistent stream-json session over stdin/stdout.
pub struct ClaudeBrain {
    pub rx: mpsc::Receiver<BrainEvent>,
}

#[async_trait]
impl Brain for ClaudeBrain {
    async fn start(&mut self) -> anyhow::Result<()> {
        todo!("WP6")
    }
    async fn send(&mut self, text: &str) -> anyhow::Result<()> {
        todo!("WP6")
    }
    fn events(&mut self) -> &mut mpsc::Receiver<BrainEvent> {
        todo!("WP6")
    }
    async fn interrupt(&mut self) -> anyhow::Result<()> {
        todo!("WP6")
    }
    async fn shutdown(self: Box<Self>) -> anyhow::Result<()> {
        todo!("WP6")
    }
    fn session(&self) -> Option<&SessionHandle> {
        todo!("WP6")
    }
}
