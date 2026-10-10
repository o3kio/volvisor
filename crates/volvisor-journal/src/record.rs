//! Versioned journal record types.
//!
//! Each frame payload is the JSON serialization of an [`Envelope`]: a
//! `record_version` discriminator plus one [`JournalRecord`]. Unknown fields
//! are rejected (`deny_unknown_fields`) so a foreign or corrupt payload is
//! treated as a torn tail on replay instead of being silently reinterpreted.
//!
//! `record_version` exists so a future format change can be detected
//! explicitly: on replay, an envelope whose version differs from
//! [`RECORD_VERSION`] is treated as an unread (torn) tail and truncated, never
//! best-effort parsed.

use serde::{Deserialize, Serialize};
use volvisor_types::OperationId;

/// Current envelope/record format version.
pub const RECORD_VERSION: u32 = 1;

/// Durable intent to execute a privileged mutation (journal-before-mutate,
/// AGENTS rule 8). Destructive provider actions may only run after this
/// record is fsynced.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    /// Operation this intent belongs to (idempotency key).
    pub operation_id: OperationId,
    /// Immutable hash of the request payload; reuse with a different hash is
    /// a conflict (Volume API v2 section 7).
    pub request_hash: [u8; 32],
    /// Operation kind (e.g. `create_volume`); descriptive, not parsed for
    /// control flow.
    pub op_kind: String,
    /// Full request payload, preserved verbatim for replay and forensics.
    pub payload: serde_json::Value,
}

/// Recorded result of an operation, journaled after the mutation completed
/// (successfully or not).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outcome {
    /// Operation this outcome belongs to.
    pub operation_id: OperationId,
    /// Whether the mutation succeeded.
    pub success: bool,
    /// Response body (or error detail) returned to the caller; replayed
    /// verbatim on idempotent retries.
    pub response: serde_json::Value,
    /// The HTTP status the original response was served with, when the
    /// operation recorded one (stage B2: the mobility routes answer
    /// `201` and `202`, and an idempotent replay must be
    /// status-compatible with the first caller's response — a replayed
    /// `PrepareNearlineHandoff` serves `201`, not `200`).
    ///
    /// `None` on outcomes recorded without a status (the pre-stage-B2
    /// shape and internal outcomes): those replay through the default
    /// mapping (`200` on success, the wire-code status on failure).
    ///
    /// Additive stage-B2 field: journals written before stage B2 decode
    /// with `None` (serde default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
}

/// Operator/administrative marker record; carries no idempotency state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    /// Free-form note; never interpreted by the journal.
    pub note: String,
}

/// One journaled record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "record_type", rename_all = "snake_case")]
pub enum JournalRecord {
    /// Intent to execute a mutation (written before the mutation).
    Intent(Intent),
    /// Result of a mutation (written after the mutation).
    Outcome(Outcome),
    /// Administrative checkpoint marker.
    Checkpoint(Checkpoint),
}

/// Versioned envelope persisted inside each frame's payload.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Envelope {
    /// Format version of this record; must equal [`RECORD_VERSION`].
    pub(crate) record_version: u32,
    /// The journaled record itself.
    pub(crate) record: JournalRecord,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intent_round_trip() {
        let record = JournalRecord::Intent(Intent {
            operation_id: OperationId::new("op-1").expect("valid id"),
            request_hash: [7; 32],
            op_kind: "create_volume".to_owned(),
            payload: serde_json::json!({"size_bytes": 1024}),
        });
        let json = serde_json::to_string(&record).expect("serialize");
        assert!(json.contains("\"record_type\":\"intent\""));
        let back: JournalRecord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, record);
    }

    #[test]
    fn outcome_round_trip() {
        let record = JournalRecord::Outcome(Outcome {
            operation_id: OperationId::new("op-2").expect("valid id"),
            success: false,
            response: serde_json::json!({"error": "boom"}),
            http_status: None,
        });
        let back: JournalRecord =
            serde_json::from_str(&serde_json::to_string(&record).expect("serialize"))
                .expect("deserialize");
        assert_eq!(back, record);

        // A stage-B2 outcome carries its HTTP status; an older journal
        // (no `http_status` key) decodes with `None` (serde default),
        // and the field is not serialized when absent.
        let with_status = JournalRecord::Outcome(Outcome {
            operation_id: OperationId::new("op-2b").expect("valid id"),
            success: true,
            response: serde_json::json!({"state": "PREPARED"}),
            http_status: Some(201),
        });
        let json = serde_json::to_string(&with_status).expect("serialize");
        assert!(json.contains("\"http_status\":201"));
        assert_eq!(
            serde_json::from_str::<JournalRecord>(&json).expect("deserialize"),
            with_status
        );
        let legacy = json.replace(",\"http_status\":201", "");
        let back: JournalRecord = serde_json::from_str(&legacy).expect("legacy decode");
        assert_eq!(
            back,
            JournalRecord::Outcome(Outcome {
                operation_id: OperationId::new("op-2b").expect("valid id"),
                success: true,
                response: serde_json::json!({"state": "PREPARED"}),
                http_status: None,
            })
        );
    }

    #[test]
    fn checkpoint_round_trip() {
        let record = JournalRecord::Checkpoint(Checkpoint {
            note: "startup replay complete".to_owned(),
        });
        let back: JournalRecord =
            serde_json::from_str(&serde_json::to_string(&record).expect("serialize"))
                .expect("deserialize");
        assert_eq!(back, record);
    }

    #[test]
    fn unknown_fields_rejected() {
        let json = r#"{"record_type":"checkpoint","note":"n","surprise":1}"#;
        assert!(serde_json::from_str::<JournalRecord>(json).is_err());
        let json = r#"{"record_version":1,"record":{"record_type":"checkpoint","note":"n"},"x":0}"#;
        assert!(serde_json::from_str::<Envelope>(json).is_err());
    }

    #[test]
    fn unknown_record_type_rejected() {
        let json = r#"{"record_type":"surprise"}"#;
        assert!(serde_json::from_str::<JournalRecord>(json).is_err());
    }
}
