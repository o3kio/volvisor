//! Integration tests for the durable journal: crash-replay convergence,
//! torn-tail truncation, idempotency semantics and single-writer enforcement
//! (Volume API v2 contract section 7, AGENTS rules 4/8).

// Tests may use expect/unwrap for invariant assertions; production code may
// not (same policy as the library crate).
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io::Write as _;

use volvisor_journal::{IntentAppend, JOURNAL_LOG_FILE, Journal};
use volvisor_types::{ApiErrorCode, OperationId};

fn op_id(name: &str) -> OperationId {
    OperationId::new(name).expect("valid operation id")
}

fn write_n_records(dir: &std::path::Path, n: u32) {
    let mut journal = Journal::open(dir).expect("open");
    for i in 0..n {
        let id = op_id(&format!("op-{i}"));
        let append = journal
            .append_intent(
                id.clone(),
                [u8::try_from(i).expect("fits u8"); 32],
                "create_volume",
                serde_json::json!({"index": i, "size_bytes": 1024}),
            )
            .expect("append intent");
        assert!(matches!(append, IntentAppend::New));
        journal
            .append_outcome(id, true, serde_json::json!({"created": i}))
            .expect("append outcome");
    }
    journal
        .append_checkpoint("integration batch complete")
        .expect("append checkpoint");
}

#[test]
fn records_survive_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_n_records(dir.path(), 3);
    // 3 intents + 3 outcomes + 1 checkpoint.
    let journal = Journal::open(dir.path()).expect("reopen");
    assert_eq!(journal.record_count(), 7);

    let entry = journal.lookup(&op_id("op-1")).expect("registry entry");
    assert_eq!(entry.request_hash, [1; 32]);
    assert!(entry.has_outcome);
    assert_eq!(
        entry.outcome.expect("outcome").response,
        serde_json::json!({"created": 1})
    );
    assert_eq!(journal.log_path(), dir.path().join(JOURNAL_LOG_FILE));
}

#[test]
fn torn_garbage_tail_is_truncated_and_replay_is_stable() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_n_records(dir.path(), 3);
    let log_path = dir.path().join(JOURNAL_LOG_FILE);
    let valid_len = std::fs::metadata(&log_path).expect("stat").len();

    // Simulate a crash mid-append: arbitrary debris after the valid prefix.
    let mut log = std::fs::OpenOptions::new()
        .append(true)
        .open(&log_path)
        .expect("open log");
    log.write_all(&[0xA5; 37]).expect("append debris");
    drop(log);

    let journal = Journal::open(dir.path()).expect("reopen after torn tail");
    assert_eq!(journal.record_count(), 7, "only the valid prefix replays");
    assert_eq!(
        std::fs::metadata(&log_path).expect("stat").len(),
        valid_len,
        "torn tail is truncated to the valid prefix"
    );
    drop(journal);

    // Replay converges: reopening the truncated log yields the same state.
    let journal = Journal::open(dir.path()).expect("reopen truncated log");
    assert_eq!(journal.record_count(), 7);
    assert_eq!(std::fs::metadata(&log_path).expect("stat").len(), valid_len);
}

#[test]
fn truncated_last_frame_replays_only_the_valid_prefix() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_n_records(dir.path(), 3);
    let log_path = dir.path().join(JOURNAL_LOG_FILE);

    // Cut into the final frame's payload: header survives but the frame is
    // incomplete, so the CRC/length check fails.
    let full_len = std::fs::metadata(&log_path).expect("stat").len();
    let cut = full_len - 3;
    let log = std::fs::OpenOptions::new()
        .write(true)
        .open(&log_path)
        .expect("open log");
    log.set_len(cut).expect("truncate");
    drop(log);

    let journal = Journal::open(dir.path()).expect("reopen after truncated frame");
    // The checkpoint record is lost with the torn tail; the 6 operation
    // records replay.
    assert_eq!(journal.record_count(), 6);
    assert!(journal.lookup(&op_id("op-2")).is_some());
    let truncated_len = std::fs::metadata(&log_path).expect("stat").len();
    assert!(
        truncated_len < cut,
        "log is truncated back to the last valid record boundary ({truncated_len} < {cut})"
    );
    drop(journal);

    // Replay converges on the truncated prefix.
    let journal = Journal::open(dir.path()).expect("reopen truncated log");
    assert_eq!(journal.record_count(), 6);
    assert_eq!(
        std::fs::metadata(&log_path).expect("stat").len(),
        truncated_len
    );
}

#[test]
fn idempotent_replay_same_hash_returns_recorded_outcome() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = op_id("op-replay");
    let hash = [7; 32];
    let payload = serde_json::json!({"size_bytes": 512});
    let response = serde_json::json!({"volume_id": "vol-1"});

    {
        let mut journal = Journal::open(dir.path()).expect("open");
        assert!(matches!(
            journal.append_intent(id.clone(), hash, "create_volume", payload.clone()),
            Ok(IntentAppend::New)
        ));
        // Retry before the outcome: in flight, nothing new journaled.
        assert!(matches!(
            journal.append_intent(id.clone(), hash, "create_volume", payload.clone()),
            Ok(IntentAppend::AlreadyInFlight)
        ));
        journal
            .append_outcome(id.clone(), true, response.clone())
            .expect("outcome");
        // Retry after the outcome: replayed with the recorded response.
        assert!(matches!(
            journal.append_intent(id.clone(), hash, "create_volume", payload.clone()),
            Ok(IntentAppend::Replayed { success: true, response: ref body, .. }) if *body == response
        ));
    }

    // The same semantics hold after a reopen (registry derived from replay).
    let mut journal = Journal::open(dir.path()).expect("reopen");
    assert!(matches!(
        journal.append_intent(id.clone(), hash, "create_volume", payload),
        Ok(IntentAppend::Replayed { success: true, response: ref body, .. }) if *body == response
    ));

    let entry = journal.lookup(&id).expect("registry entry");
    assert_eq!(entry.request_hash, hash);
    assert!(entry.has_outcome);
}

/// A recorded *failure* outcome replays with its success flag carried
/// through, so the API layer can reconstruct the original HTTP status
/// (replays must be status-compatible, not just body-compatible).
#[test]
fn failed_outcome_replays_with_the_success_flag() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = op_id("op-failed-replay");
    let hash = [8; 32];
    let error_body = serde_json::json!({
        "code": "NO_SAFE_CAPACITY",
        "message": "no safe capacity: requested 1024 bytes, 0 remaining",
    });

    {
        let mut journal = Journal::open(dir.path()).expect("open");
        assert!(matches!(
            journal.append_intent(
                id.clone(),
                hash,
                "create_volume",
                serde_json::json!({"size_bytes": 1024}),
            ),
            Ok(IntentAppend::New)
        ));
        journal
            .append_outcome(id.clone(), false, error_body.clone())
            .expect("outcome");
        assert!(matches!(
            journal.append_intent(
                id.clone(),
                hash,
                "create_volume",
                serde_json::json!({"size_bytes": 1024}),
            ),
            Ok(IntentAppend::Replayed { success: false, response: ref body, .. }) if *body == error_body
        ));
    }

    // The flag survives a reopen (registry derived from replay).
    let mut journal = Journal::open(dir.path()).expect("reopen");
    assert!(matches!(
        journal.append_intent(
            id,
            hash,
            "create_volume",
            serde_json::json!({"size_bytes": 1024}),
        ),
        Ok(IntentAppend::Replayed { success: false, response: ref body, .. }) if *body == error_body
    ));
}

#[test]
fn same_operation_id_with_different_hash_conflicts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = op_id("op-conflict");
    let mut journal = Journal::open(dir.path()).expect("open");
    journal
        .append_intent(
            id.clone(),
            [1; 32],
            "create_volume",
            serde_json::json!({"a": 1}),
        )
        .expect("first intent");

    let err = journal
        .append_intent(
            id.clone(),
            [2; 32],
            "create_volume",
            serde_json::json!({"a": 999}),
        )
        .expect_err("different hash must conflict");
    assert_eq!(err.code, ApiErrorCode::IdempotencyConflict);
    assert_eq!(err.http_status(), 409);

    // The conflict also holds after a reopen.
    drop(journal);
    let mut journal = Journal::open(dir.path()).expect("reopen");
    let err = journal
        .append_intent(id, [3; 32], "create_volume", serde_json::json!({"a": 5}))
        .expect_err("different hash must conflict after reopen");
    assert_eq!(err.code, ApiErrorCode::IdempotencyConflict);
}

#[test]
fn outcome_less_intent_reports_already_in_flight_across_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = op_id("op-inflight");
    let hash = [5; 32];
    {
        let mut journal = Journal::open(dir.path()).expect("open");
        journal
            .append_intent(
                id.clone(),
                hash,
                "attach_volume",
                serde_json::json!({"vm": "vm-1"}),
            )
            .expect("intent");
    }
    // Crash before the outcome was journaled: on retry the operation is
    // reported in flight, never re-executed blindly.
    let mut journal = Journal::open(dir.path()).expect("reopen");
    assert!(matches!(
        journal.append_intent(id, hash, "attach_volume", serde_json::json!({"vm": "vm-1"})),
        Ok(IntentAppend::AlreadyInFlight)
    ));
}

#[test]
fn double_open_fails_on_the_second_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let first = Journal::open(dir.path()).expect("first open");

    let err = Journal::open(dir.path()).expect_err("second open must fail");
    assert_eq!(err.code, ApiErrorCode::Internal);
    assert!(err.detail.contains("already locked"), "detail: {err}");

    drop(first);
    Journal::open(dir.path()).expect("reopen after the lock is released");
}

#[test]
fn empty_and_missing_journal_directories_replay_to_zero_records() {
    let dir = tempfile::tempdir().expect("tempdir");
    let journal =
        Journal::open(dir.path().join("does-not-exist-yet")).expect("open in missing directory");
    assert_eq!(journal.record_count(), 0);
    assert!(journal.lookup(&op_id("op-none")).is_none());
}
