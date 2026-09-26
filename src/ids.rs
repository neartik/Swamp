use serde::{Deserialize, Serialize};
use ulid::Ulid;

macro_rules! ulid_id {
    ($name:ident, $prefix:literal) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub Ulid);

        impl $name {
            pub fn new() -> Self {
                Self(Ulid::new())
            }
            /// Last 6 chars: stable, typeable, unique enough within a run.
            pub fn short(&self) -> String {
                self.0.to_string()[20..].to_ascii_lowercase()
            }
            /// True when `spec` names this id: in full, as a prefix, or by its short form.
            pub fn matches(&self, spec: &str) -> bool {
                if let Ok(exact) = spec.parse::<Self>() {
                    return exact == *self;
                }
                let needle = spec.trim_start_matches($prefix).to_ascii_lowercase();
                let full = self.0.to_string().to_ascii_lowercase();
                !needle.is_empty() && (full.starts_with(&needle) || self.short() == needle)
            }
        }
        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}{}", $prefix, self.0)
            }
        }
        impl std::str::FromStr for $name {
            type Err = ulid::DecodeError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(Ulid::from_string(s.trim_start_matches($prefix))?))
            }
        }
    };
}
ulid_id!(RunId, "run_");
ulid_id!(NodeId, "nd_");
ulid_id!(DispatchId, "dsp_");

impl DispatchId {
    /// The synthetic bucket that holds every node journaled without a dispatch (schema 1).
    pub const LEGACY: DispatchId = DispatchId(Ulid(0));
}

/// Per-run and monotonic: the n-th MCP tool call of a run, starting at 1.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct CallSeq(pub u64);

impl std::fmt::Display for CallSeq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// `claude --session-id` requires a valid UUID, so every node carries a paired UUID
/// alongside its ULID. Both are journaled; neither is derived from the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeIds {
    pub id: NodeId,
    pub session_uuid: uuid::Uuid,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn ids_render_with_a_prefix_and_parse_back() {
        let run = RunId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        assert_eq!(run.to_string(), "run_01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert_eq!(RunId::from_str(&run.to_string()).unwrap(), run);
        assert_eq!(run.short(), "9g5fav");

        let node = NodeId::from_str("nd_01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        assert_eq!(node.short(), "9g5fav");
        assert!(NodeId::from_str("not-a-ulid").is_err());
    }

    #[test]
    fn dispatch_ids_share_the_node_id_format_and_prefix_resolution() {
        let d = DispatchId::from_str("dsp_01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        assert_eq!(d.to_string(), "dsp_01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert_eq!(
            DispatchId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
            d
        );
        assert_eq!(d.short(), "9g5fav");
        for spec in ["dsp_01arz3", "01ARZ3NDEK", "9g5fav", &d.to_string()] {
            assert!(d.matches(spec), "{spec}");
        }
        assert!(!d.matches("01B"));
        assert!(!d.matches("dsp_"));
        assert_ne!(DispatchId::new(), DispatchId::LEGACY);
        assert_eq!(serde_json::to_string(&CallSeq(7)).unwrap(), "7");
    }

    #[test]
    fn ids_are_time_sortable_and_serialize_transparently() {
        let first = RunId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        let later = RunId::from_str("01ARZ3NDEMTSV4RRFFQ69G5FAV").unwrap();
        assert!(first < later, "ulids sort by their timestamp prefix");
        let json = serde_json::to_string(&first).unwrap();
        assert!(json.starts_with('"'), "{json}");
        assert_eq!(serde_json::from_str::<RunId>(&json).unwrap(), first);
    }
}
