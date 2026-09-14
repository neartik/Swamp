#![allow(dead_code, unused_variables)]

use crate::model::core::Provider;
use crate::model::failure::Failure;
use crate::worker::adapter::{
    BrainTransport, Capability, ExitContext, LaunchSpec, ParseOutput, ParseState, ProviderAdapter,
};
use std::ffi::OsString;

#[derive(Debug, Default, Clone, Copy)]
pub struct CodexAdapter;

impl ProviderAdapter for CodexAdapter {
    fn provider(&self) -> Provider {
        todo!("WP3")
    }
    fn supports(&self, cap: Capability) -> bool {
        todo!("WP3")
    }
    fn build_argv(&self, spec: &LaunchSpec) -> anyhow::Result<Vec<OsString>> {
        todo!("WP3")
    }
    fn env(&self, spec: &LaunchSpec) -> Vec<(OsString, OsString)> {
        todo!("WP3")
    }
    fn parse_line(&self, line: &str, st: &mut ParseState) -> ParseOutput {
        todo!("WP3")
    }
    fn classify(&self, cx: &ExitContext<'_>) -> Option<Failure> {
        todo!("WP3")
    }
    fn brain_transport(&self) -> BrainTransport {
        todo!("WP3")
    }
}
