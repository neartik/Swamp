use crate::ids::NodeId;
use crate::model::core::{AccountId, Cost, FileChange, Provider, Tier, Usage};
use crate::model::failure::Failure;
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct TaskRequest {
    pub title: String,
    pub prompt: String,
    #[serde(default)]
    pub tier: Option<Tier>,
    #[serde(default)]
    pub provider: Option<Provider>,
    #[serde(default)]
    pub isolation: Option<IsolationMode>,
    #[serde(default)]
    pub account: Option<AccountId>,
    /// Parsed and rejected in v1 with a clear message. The seam for DAG dispatch.
    #[serde(default)]
    pub deps: Vec<NodeId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum IsolationMode {
    Worktree,
    Shared,
    // CLI spells this `readonly`; the wire format keeps kebab-case.
    #[value(name = "readonly")]
    ReadOnly,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeResult {
    pub node: NodeId,
    pub title: String,
    pub ok: bool,
    pub state: &'static str,
    pub tier: Tier,
    pub provider: Provider,
    pub account: Option<AccountId>,
    pub model: Option<String>,
    pub attempts: u32,
    /// Truncated to limits.max_result_bytes and wrapped by the MCP layer in an explicit
    /// untrusted-content envelope. Worker output is data, never instruction.
    pub summary: Option<String>,
    pub files: Vec<FileChange>,
    pub branch: Option<String>,
    pub patch: Option<Utf8PathBuf>,
    pub insertions: u32,
    pub deletions: u32,
    pub usage: Usage,
    pub cost: Option<Cost>,
    pub duration_ms: u64,
    pub failure: Option<Failure>,
    pub permission_denials: u32,
}
