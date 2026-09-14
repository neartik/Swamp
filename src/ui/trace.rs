#![allow(dead_code, unused_variables)]

use crate::ids::NodeId;
use crate::journal::fold::RunView;
use crate::journal::paths::RunPaths;

pub struct TraceOpts {
    pub node: Option<NodeId>,
    pub events: bool,
    pub raw: bool,
    pub stderr: bool,
    pub depth: Option<u32>,
    pub failed: bool,
    pub json: bool,
}

pub fn render(view: &RunView, o: &TraceOpts) -> String {
    todo!("WP7")
}

pub async fn follow(paths: &RunPaths, o: &TraceOpts) -> anyhow::Result<()> {
    todo!("WP7")
}
