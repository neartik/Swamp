use crate::ids::{CallSeq, DispatchId, NodeId, RunId};
use crate::model::core::{NodeState, Provider, Tier};
use crate::model::failure::Failure;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// One dispatch: who asked, and the logical id each task got before anything ran.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DispatchRecord {
    pub id: DispatchId,
    pub run: RunId,
    pub caller: NodeId,
    /// None outside an MCP tool call.
    pub call_seq: Option<CallSeq>,
    pub wait: bool,
    /// None blocks until every task settles.
    pub max_wait_s: Option<u64>,
    pub tasks: Vec<TaskRef>,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRef {
    pub logical: NodeId,
    pub title: String,
    pub tier: Tier,
    pub provider: Provider,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchState {
    Open,
    Settled,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchCounts {
    pub succeeded: u32,
    pub failed: u32,
    pub cancelled: u32,
    pub rejected: u32,
}

impl DispatchCounts {
    /// A task that has not ended counts nowhere.
    pub fn count(&mut self, s: &NodeState) {
        let n = match s {
            NodeState::Succeeded => &mut self.succeeded,
            NodeState::Failed { .. } => &mut self.failed,
            NodeState::Cancelled { .. } => &mut self.cancelled,
            NodeState::Rejected { .. } => &mut self.rejected,
            _ => return,
        };
        *n += 1;
    }
}

/// `NodeState` without its payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Queued,
    Blocked,
    Leased,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Rejected,
}

impl Phase {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Phase::Succeeded | Phase::Failed | Phase::Cancelled | Phase::Rejected
        )
    }
}

impl From<&NodeState> for Phase {
    fn from(s: &NodeState) -> Self {
        match s {
            NodeState::Queued => Phase::Queued,
            NodeState::Blocked { .. } => Phase::Blocked,
            NodeState::Leased { .. } => Phase::Leased,
            // Orphaned is derived at read time, never journaled.
            NodeState::Running { .. } | NodeState::Orphaned { .. } => Phase::Running,
            NodeState::Succeeded => Phase::Succeeded,
            NodeState::Failed { .. } => Phase::Failed,
            NodeState::Cancelled { .. } => Phase::Cancelled,
            NodeState::Rejected { .. } => Phase::Rejected,
        }
    }
}

pub fn settled_state(failure: Option<&Failure>) -> NodeState {
    match failure {
        None => NodeState::Succeeded,
        Some(Failure::Cancelled { by }) => NodeState::Cancelled { by: *by },
        Some(f) => NodeState::Failed { failure: f.clone() },
    }
}

/// `to` carries the payload, so a task that never reached an attempt still has a full state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeTransition {
    pub from: Phase,
    pub to: NodeState,
    pub why: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::core::{AccountId, CancelSource};
    use std::str::FromStr;

    #[test]
    fn a_dispatch_record_round_trips() {
        let r = DispatchRecord {
            id: DispatchId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
            run: RunId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAW").unwrap(),
            caller: NodeId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAW").unwrap(),
            call_seq: Some(CallSeq(3)),
            wait: true,
            max_wait_s: Some(600),
            tasks: vec![TaskRef {
                logical: NodeId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAX").unwrap(),
                title: "lex".into(),
                tier: Tier::Mid,
                provider: Provider::Anthropic,
            }],
            at: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<DispatchRecord>(&json).unwrap(), r);
        insta::assert_json_snapshot!(r);
    }

    #[test]
    fn phases_follow_their_states() {
        let leased = NodeState::Leased {
            account: AccountId("main".into()),
        };
        assert_eq!(Phase::from(&leased), Phase::Leased);
        assert_eq!(
            Phase::from(&NodeState::Orphaned {
                pid: 1,
                stream_offset: 0
            }),
            Phase::Running
        );
        assert!(Phase::Rejected.is_terminal());
        assert!(!Phase::Blocked.is_terminal());
        assert_eq!(settled_state(None), NodeState::Succeeded);
        assert_eq!(
            settled_state(Some(&Failure::Cancelled {
                by: CancelSource::User
            })),
            NodeState::Cancelled {
                by: CancelSource::User
            }
        );
    }
}
