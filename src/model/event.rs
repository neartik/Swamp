use crate::model::core::{ChangeKind, FinalSummary, RateLimitSnapshot, Usage};
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

/// Provider-neutral. Every adapter normalizes into this; the journal, the TUI and the brain
/// only ever see this shape. Raw provider lines live untouched in nodes/<id>/stream.jsonl.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "e", rename_all = "snake_case")]
pub enum WorkerEvent {
    SessionStarted {
        session: String,
        model: Option<String>,
        auth_hint: Option<String>,
    },
    AssistantText {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        summary: String,
    },
    ToolResult {
        id: String,
        ok: bool,
        summary: String,
        /// The result body, flattened to text and truncated. Additive: older journals have none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    FileChanged {
        path: Utf8PathBuf,
        kind: ChangeKind,
    },
    Usage(Usage),
    RateLimit(RateLimitSnapshot),
    Final(FinalSummary),
    /// Never dropped and never fatal: a CLI schema change degrades instead of breaking.
    Unknown {
        raw: RawLine,
    },
}

/// `Box<RawValue>` with structural equality, so `WorkerEvent` can keep `PartialEq`.
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct RawLine(pub Box<RawValue>);

impl PartialEq for RawLine {
    fn eq(&self, other: &Self) -> bool {
        self.0.get() == other.0.get()
    }
}

/// `RawValue` cannot be read back through the buffered deserializer that an internally tagged
/// enum uses, so the line is rebuilt from a `Value` instead of borrowed.
impl<'de> Deserialize<'de> for RawLine {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(d)?;
        let text = serde_json::to_string(&value).map_err(D::Error::custom)?;
        Ok(RawLine(
            RawValue::from_string(text).map_err(D::Error::custom)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::core::{Cost, CostBasis, LimitScope, LimitStatus, LimitWindow};

    fn every_variant() -> Vec<WorkerEvent> {
        vec![
            WorkerEvent::SessionStarted {
                session: "0f0f".into(),
                model: Some("model-a".into()),
                auth_hint: Some("subscription".into()),
            },
            WorkerEvent::AssistantText {
                text: "hello".into(),
            },
            WorkerEvent::Thinking { text: "hmm".into() },
            WorkerEvent::ToolCall {
                id: "t1".into(),
                name: "Edit".into(),
                summary: "src/lib.rs".into(),
            },
            WorkerEvent::ToolResult {
                id: "t1".into(),
                ok: true,
                summary: "applied".into(),
                detail: None,
            },
            WorkerEvent::FileChanged {
                path: "src/lib.rs".into(),
                kind: ChangeKind::Modify,
            },
            WorkerEvent::Usage(Usage {
                input_tokens: 10,
                cached_input_tokens: 2,
                cache_write_tokens: 1,
                output_tokens: 4,
                reasoning_tokens: 0,
            }),
            WorkerEvent::RateLimit(RateLimitSnapshot {
                status: LimitStatus::Warning,
                windows: vec![LimitWindow {
                    scope: LimitScope::SevenDay,
                    utilization: 0.64,
                    resets_at: None,
                    ..Default::default()
                }],
                resets_at: None,
                ..Default::default()
            }),
            WorkerEvent::Final(FinalSummary {
                ok: true,
                subtype: "success".into(),
                text: Some("done".into()),
                usage: Usage::default(),
                cost: Some(Cost {
                    usd: 0.5,
                    basis: CostBasis::Reported,
                }),
                api_error_status: None,
                num_turns: 3,
                permission_denials: 0,
                denied_tools: Vec::new(),
                ..Default::default()
            }),
            WorkerEvent::Unknown {
                raw: RawLine(RawValue::from_string(r#"{"type":"future_thing"}"#.into()).unwrap()),
            },
        ]
    }

    #[test]
    fn events_round_trip() {
        let all = every_variant();
        for e in &all {
            let json = serde_json::to_string(e).unwrap();
            assert_eq!(&serde_json::from_str::<WorkerEvent>(&json).unwrap(), e);
        }
        insta::assert_json_snapshot!(all);
    }

    #[test]
    fn unknown_lines_compare_structurally() {
        let a = RawLine(RawValue::from_string(r#"{"a":1}"#.into()).unwrap());
        let b = RawLine(RawValue::from_string(r#"{"a":1}"#.into()).unwrap());
        let c = RawLine(RawValue::from_string(r#"{"a":2}"#.into()).unwrap());
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
