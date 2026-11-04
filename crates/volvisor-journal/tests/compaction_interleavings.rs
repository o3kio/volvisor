//! Deterministic compaction-interleaving regressions for the durable journal
//! (issue #49).
//!
//! These tests drive the *scheduled* compaction path (retention thresholds)
//! rather than calling the private `compact` helper. Threads and sleeps would
//! make the interleaving nondeterministic; the sequence of appends below fixes
//! the order explicitly, so the same durable facts must hold on every run.
//!
//! The in-module pin/compaction test covers an explicit compaction while a
//! re-drive pin is held. The tests here cover the additional boundaries:
//! compaction fired by the retention scheduler during a re-drive, and
//! compaction fired between an intent and its outcome.

// Tests may use expect/unwrap for invariant assertions; production code may
// not (same policy as the library crate).
#![allow(clippy::expect_used, clippy::unwrap_used)]

use volvisor_journal::{IntentAppend, Journal, JournalRetention};
use volvisor_types::{ApiErrorCode, OperationId};

fn op_id(name: &str) -> OperationId {
    OperationId::new(name).expect("valid operation id")
}

/// Retain two terminal operations and compact as soon as four are pending.
/// The record-count trigger is deliberately far away so only the retention
/// hysteresis schedules compaction.
fn two_terminal_retention() -> JournalRetention {
    JournalRetention {
        max_retained_operations: 2,
        max_tombstones: 128,
        max_log_records: 1_000_000,
        max_log_bytes: None,
    }
}

fn complete(journal: &mut Journal, id: &OperationId, hash: [u8; 32], body: serde_json::Value) {
    journal
        .append_intent(id.clone(), hash, "create_volume", serde_json::json!({}))
        .expect("intent");
    journal
        .append_outcome(id.clone(), true, body)
        .expect("outcome");
}

#[test]
fn scheduled_compaction_during_a_redrive_pin_preserves_the_latest_outcome_across_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let retention = two_terminal_retention();
    let a = op_id("op-scheduled-pin-a");
    let mut journal = Journal::open_with_retention(dir.path(), retention).expect("open");

    complete(&mut journal, &a, [1; 32], serde_json::json!({"v": 1}));

    // The API re-drive observes the recorded outcome and pins A while it
    // prepares the replacement. No compaction has fired yet.
    assert!(matches!(
        journal
            .append_intent(a.clone(), [1; 32], "create_volume", serde_json::json!({}))
            .expect("re-drive"),
        IntentAppend::Replayed { success: true, .. }
    ));
    journal.pin_operation(&a);
    assert_eq!(journal.compactions(), 0);

    // Three more terminal operations reach the retention hysteresis while A
    // is pinned. The scheduler compacts; A must not be evicted.
    complete(
        &mut journal,
        &op_id("op-scheduled-pin-b"),
        [2; 32],
        serde_json::json!({}),
    );
    complete(
        &mut journal,
        &op_id("op-scheduled-pin-c"),
        [3; 32],
        serde_json::json!({}),
    );
    complete(
        &mut journal,
        &op_id("op-scheduled-pin-d"),
        [4; 32],
        serde_json::json!({}),
    );
    assert!(
        journal.compactions() >= 1,
        "the retention scheduler must compact (failures={}, last={:?})",
        journal.compaction_failures(),
        journal.last_compaction_error()
    );
    assert!(journal.lookup(&a).is_some(), "the pinned re-drive survived");
    assert!(!journal.is_tombstoned(&a));

    // The replacement outcome lands while the pin still holds and must be the
    // durable value, not the replayed v1 outcome.
    journal
        .append_outcome(a.clone(), true, serde_json::json!({"v": 2}))
        .expect("replacement outcome");
    assert_eq!(
        journal
            .lookup(&a)
            .expect("pinned A retained")
            .outcome
            .expect("outcome")
            .response,
        serde_json::json!({"v": 2})
    );
    assert!(!journal.is_tombstoned(&a));

    // Release the pin. The scheduler needs the documented growth floor (64
    // records) before the next compaction; 32 terminal pairs cross it.
    journal.unpin_operation(&a);
    for index in 0..32 {
        complete(
            &mut journal,
            &op_id(&format!("op-scheduled-pin-later-{index:02}")),
            [u8::try_from(index + 10).expect("fits"); 32],
            serde_json::json!({}),
        );
    }
    assert!(
        journal.compactions() >= 2,
        "the unpinned A must be eligible for a later scheduled compaction"
    );
    assert!(
        journal.is_tombstoned(&a),
        "A is now beyond the retained window"
    );
    assert!(journal.lookup(&a).is_none());

    // Both retry paths fail closed across the compacted boundary.
    let intent_error = journal
        .append_intent(a.clone(), [1; 32], "create_volume", serde_json::json!({}))
        .expect_err("an expired intent must never re-execute");
    assert_eq!(intent_error.code, ApiErrorCode::OperationExpired);
    let outcome_error = journal
        .append_outcome(a.clone(), true, serde_json::json!({}))
        .expect_err("an expired outcome must never resurrect the operation");
    assert_eq!(outcome_error.code, ApiErrorCode::OperationExpired);

    drop(journal);
    // The scheduled interleaving's result is durable: the tombstone (and the
    // newest retained outcomes) survive the snapshot reopen.
    let mut reopened = Journal::open_with_retention(dir.path(), retention).expect("reopen");
    assert!(reopened.is_tombstoned(&a));
    assert_eq!(
        reopened
            .append_intent(a, [1; 32], "create_volume", serde_json::json!({}))
            .expect_err("the snapshot tombstone still refuses the retry")
            .code,
        ApiErrorCode::OperationExpired
    );
    assert!(
        reopened
            .lookup(&op_id("op-scheduled-pin-later-31"))
            .expect("newest terminal survives the snapshot")
            .has_outcome
    );
}

#[test]
fn scheduled_compaction_between_an_intent_and_outcome_keeps_the_operation_in_flight_across_reopen()
{
    let dir = tempfile::tempdir().expect("tempdir");
    let retention = two_terminal_retention();
    let in_flight = op_id("op-scheduled-in-flight");
    let hash = [9; 32];
    let mut journal = Journal::open_with_retention(dir.path(), retention).expect("open");

    // The intent is durable, but the mutation's outcome does not exist yet.
    journal
        .append_intent(
            in_flight.clone(),
            hash,
            "attach_volume",
            serde_json::json!({"vm": "vm-1"}),
        )
        .expect("in-flight intent");
    assert_eq!(journal.compactions(), 0);

    // Four other terminal operations fire the retention scheduler between the
    // in-flight intent and its (not yet journaled) outcome.
    for index in 0..4 {
        complete(
            &mut journal,
            &op_id(&format!("op-scheduled-in-flight-other-{index}")),
            [u8::try_from(index + 1).expect("fits"); 32],
            serde_json::json!({"index": index}),
        );
    }
    assert!(
        journal.compactions() >= 1,
        "the retention scheduler must compact (failures={}, last={:?})",
        journal.compaction_failures(),
        journal.last_compaction_error()
    );

    // In-flight intents are irreducible: compaction retains the operation and
    // a retry stays in doubt instead of re-executing or expiring.
    let entry = journal.lookup(&in_flight).expect("in-flight retained");
    assert!(!entry.has_outcome);
    assert!(!journal.is_tombstoned(&in_flight));
    assert!(matches!(
        journal
            .append_intent(
                in_flight.clone(),
                hash,
                "attach_volume",
                serde_json::json!({"vm": "vm-1"}),
            )
            .expect("in-flight retry"),
        IntentAppend::AlreadyInFlight
    ));

    drop(journal);
    // The snapshot preserves the same fact: still in flight, never expired and
    // never silently re-executed.
    let mut reopened = Journal::open_with_retention(dir.path(), retention).expect("reopen");
    let entry = reopened
        .lookup(&in_flight)
        .expect("in-flight survives reopen");
    assert!(!entry.has_outcome);
    assert!(!reopened.is_tombstoned(&in_flight));
    assert!(matches!(
        reopened
            .append_intent(
                in_flight,
                hash,
                "attach_volume",
                serde_json::json!({"vm": "vm-1"}),
            )
            .expect("in-flight retry after reopen"),
        IntentAppend::AlreadyInFlight
    ));
    assert!(
        reopened
            .lookup(&op_id("op-scheduled-in-flight-other-3"))
            .expect("newest terminal survives the snapshot")
            .has_outcome
    );
}
