use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{REVIEW_SCHEMA_VERSION, ReviewId};

/// One versioned event in the review NDJSON stream.
///
/// `event` is namespaced (`review.started`, `review.finding`, ...) while
/// `data` carries that event's typed payload serialized as JSON. Keeping the
/// stable envelope here means the CLI, editor extension, and future hosted
/// service can share framing without coupling this domain crate to any one
/// transport.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewEventEnvelope {
    pub schema_version: String,
    pub review_id: ReviewId,
    pub sequence: u64,
    pub event: String,
    pub data: Value,
}

impl ReviewEventEnvelope {
    pub const STARTED: &'static str = "review.started";
    pub const TARGET_COLLECTED: &'static str = "review.target_collected";
    pub const CONTEXT_BUILT: &'static str = "review.context_built";
    pub const FINDING: &'static str = "review.finding";
    pub const COMPLETED: &'static str = "review.completed";
    pub const FAILED: &'static str = "review.failed";

    /// Build an envelope from an already-serialized payload.
    ///
    /// Sequence allocation belongs to the orchestrator because it is the
    /// only layer that knows the order in which concurrent work became
    /// visible. This type preserves that order; it does not guess one.
    pub fn new(review_id: ReviewId, sequence: u64, event: impl Into<String>, data: Value) -> Self {
        Self {
            schema_version: REVIEW_SCHEMA_VERSION.to_string(),
            review_id,
            sequence,
            event: event.into(),
            data,
        }
    }

    /// Serialize a typed event payload into the common envelope.
    pub fn from_serializable<T: Serialize>(
        review_id: ReviewId,
        sequence: u64,
        event: impl Into<String>,
        data: &T,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self::new(
            review_id,
            sequence,
            event,
            serde_json::to_value(data)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Started<'a> {
        target: &'a str,
    }

    #[test]
    fn typed_payload_uses_the_versioned_stable_envelope() {
        let event = ReviewEventEnvelope::from_serializable(
            ReviewId("review_123".into()),
            7,
            ReviewEventEnvelope::STARTED,
            &Started {
                target: "working_tree",
            },
        )
        .unwrap();

        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["schema_version"], REVIEW_SCHEMA_VERSION);
        assert_eq!(value["review_id"], "review_123");
        assert_eq!(value["sequence"], 7);
        assert_eq!(value["event"], "review.started");
        assert_eq!(value["data"]["target"], "working_tree");
    }

    #[test]
    fn envelope_round_trips_without_losing_event_data() {
        let event = ReviewEventEnvelope::new(
            ReviewId("review_abc".into()),
            3,
            ReviewEventEnvelope::FINDING,
            serde_json::json!({"finding_id": "HM-001", "confidence": 0.91}),
        );
        let encoded = serde_json::to_string(&event).unwrap();
        let decoded: ReviewEventEnvelope = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, event);
    }

    #[test]
    fn serialized_event_is_safe_for_one_line_ndjson() {
        let event = ReviewEventEnvelope::new(
            ReviewId("review_line".into()),
            1,
            ReviewEventEnvelope::FAILED,
            serde_json::json!({"message": "first line\nsecond line"}),
        );
        let encoded = serde_json::to_string(&event).unwrap();
        assert_eq!(encoded.lines().count(), 1);
        assert!(encoded.contains("first line\\nsecond line"));
    }
}
