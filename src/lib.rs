//! Swamp: parallel agent orchestration over CLI subscriptions.

pub mod brain;
pub mod cli;
pub mod cmd;
pub mod config;
pub mod dispatch;
pub mod doctor;
pub mod error;
pub mod ids;
pub mod journal;
pub mod mcp;
pub mod model;
pub mod ui;
pub mod worker;
pub mod workspace;

pub use cli::{Cli, Command};
pub use config::Config;
pub use error::{SwampError, exit_code};
pub use ids::{NodeId, NodeIds, RunId};
pub use journal::{Journal, JournalEvent, JournalHandle, JournalLine, Paths, RunPaths, RunView};
pub use model::{
    AccountId, Cost, Failure, FileChange, IsolationMode, NodeRecord, NodeResult, NodeState,
    Provider, TaskRequest, Tier, Usage, WorkerEvent, WorkspaceRef,
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
