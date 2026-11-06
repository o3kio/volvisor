//! # volvisor-journal
//!
//! Durable append-only intent journal with a replay-derived idempotency
//! registry for the Volvisor control plane (Volume API v2 contract section 7,
//! AGENTS rules 4 and 8).
//!
//! ## Durability model
//!
//! - Records live in `<dir>/journal.log` as length-prefixed, CRC-checked
//!   frames with a monotonic sequence number (see the private `frame` module
//!   docs for the exact layout).
//! - Every acknowledged append writes the frame bytes and calls
//!   `fsync` on the log file before returning; the journal directory is
//!   fsynced once at creation time so the log's directory entry is durable.
//! - A partially written or torn trailing record (truncated length, bad CRC,
//!   bad magic, broken sequence) is detected on replay and the log is
//!   truncated back to the last valid record. Torn tails are the expected
//!   outcome of power loss mid-append and are never an error.
//! - A **complete, CRC-valid frame** whose payload is undecodable, carries an
//!   unknown field, or declares a `record_version` this build does not
//!   support is not a torn tail: replay refuses with a typed `INTERNAL`
//!   error and leaves the log untouched. Truncating it would silently
//!   discard durable state written by a newer build (see
//!   `docs/schema-compatibility.md`).
//!
//! ## Single-writer enforcement
//!
//! Opening a journal takes an exclusive non-blocking `flock` on
//! `<dir>/journal.lock`, held for the lifetime of the [`Journal`] value. A
//! second `Journal::open` on the same directory fails fast with a typed
//! `ApiError` instead of allowing concurrent writers.
//!
//! ## Idempotency registry (contract section 7)
//!
//! The registry is derived from replay, never stored separately:
//!
//! - the same `operation_id` with the same request hash is a replay: the
//!   recorded outcome response is returned when present, otherwise the
//!   operation is reported as in flight;
//! - the same `operation_id` with a *different* request hash is a typed
//!   idempotency conflict (`ApiError::idempotency_conflict`) — fail closed,
//!   never silently accepted.
//!
//! ## Bounded retention and compaction (record_version 2)
//!
//! The registry is bounded ([`JournalRetention`]). When a record-count, byte,
//! or retention threshold is crossed, the journal rewrites its log as one
//! [`Snapshot`] frame (tmp file, fsync, rename, directory fsync) after the
//! triggering append is already durable. The snapshot retains the newest
//! terminal operations, every in-flight intent, and a bounded ring of exact
//! 64-bit fingerprints of recently evicted operations.
//!
//! A snapshot must fit one 8 MiB frame, so the configured bounds are upper
//! bounds: compaction evicts the oldest unpinned terminal operations and then
//! shortens the tombstone ring until the encoded snapshot fits. In-flight and
//! pinned operations are never evicted; if they alone exceed the frame limit,
//! compaction fails closed and a later append retries after bounded new
//! growth. After a successful compaction the record and byte triggers require
//! meaningful new growth before firing again, so a threshold below the
//! snapshot's own size cannot rewrite it on every append. A persistently
//! failing compaction uses the same idea: the first failure retries after one
//! record (so unblocking the target recovers promptly), then after 2, 4, ...
//! records or MiB, capped at 64, so it is never attempted on every append.
//!
//! A retry of an operation evicted within the tombstone window returns
//! `OPERATION_EXPIRED` (HTTP 410) instead of re-executing a possibly
//! completed mutation. The window is an ordered ring with a matching O(1)
//! membership index, so first-seen appends and replay do not scan it. An
//! operation evicted beyond the ring is indistinguishable from a new one;
//! size the ring above the client retry horizon. An operation currently
//! executing is pinned against eviction so a re-drive cannot lose its
//! replacement outcome.
//!
//! [`Journal::open_with_records`] (the witness registry) folds the complete
//! record history and therefore refuses a compacted journal instead of
//! deriving authority state from a truncated prefix.

#![deny(unsafe_code)]
// Tests may use expect/unwrap for invariant assertions; production code may not.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod frame;
mod journal;
mod record;

pub use journal::{
    DEFAULT_HARD_CEILING_BYTES, DEFAULT_MAX_LOG_BYTES, DEFAULT_MAX_LOG_RECORDS,
    DEFAULT_MAX_RETAINED_OPERATIONS, DEFAULT_MAX_TOMBSTONES, IntentAppend, JOURNAL_LOCK_FILE,
    JOURNAL_LOG_FILE, Journal, JournalRetention, RecordedOutcome, RegistryEntry,
};
pub use record::{
    Checkpoint, Intent, JournalRecord, LEGACY_RECORD_VERSION_N1, Outcome, RECORD_VERSION, Snapshot,
    SnapshotOperation, SnapshotOutcome,
};

/// Fuzz-only frame-chain walker (`#[doc(hidden)]`, no production
/// caller): decode frames exactly as replay does and report how many
/// frames were valid and how many bytes they consumed. The fuzz target
/// feeds arbitrary bytes; the helper must never panic, must never loop,
/// and `offset` must always be within `data`.
#[doc(hidden)]
#[must_use]
pub fn fuzz_decode_frame_chain(data: &[u8]) -> (usize, usize) {
    let mut frames = 0usize;
    let mut offset = 0usize;
    let mut sequence = 1u64;
    while let Some(raw) = frame::decode_at(data, offset, sequence) {
        if raw.end <= offset {
            break;
        }
        frames += 1;
        offset = raw.end;
        sequence = sequence.wrapping_add(1);
    }
    (frames, offset)
}

/// Fuzz-only walker over the **production streaming decoder**
/// [`frame::read_frame`] (`#[doc(hidden)]`, no production caller).
///
/// [`fuzz_decode_frame_chain`] exercises the test-only `decode_at`
/// implementation; replay reads incrementally through `read_frame`, a
/// separate implementation with its own bounds and CRC checks. Fuzzing
/// both keeps the stream path covered. The helper must never panic or
/// loop and the returned offset must stay within `data`.
#[doc(hidden)]
#[must_use]
pub fn fuzz_read_frame_chain(data: &[u8]) -> (usize, usize) {
    let mut reader = std::io::Cursor::new(data);
    let mut frames = 0usize;
    let mut offset = 0usize;
    let mut sequence = 1u64;
    // `Ok(None)` (clean EOF or a torn tail) and `Err` (a real I/O failure,
    // impossible over a `Cursor`) both stop replay at the last boundary.
    while let Ok(Some(raw)) = frame::read_frame(&mut reader, sequence) {
        if raw.frame_len == 0 {
            break;
        }
        frames += 1;
        offset = offset.saturating_add(raw.frame_len);
        sequence = sequence.wrapping_add(1);
    }
    (frames, offset)
}

/// Fuzz-only record-envelope decoder (`#[doc(hidden)]`, no production
/// caller): the same version probe then strict `Envelope` decode replay
/// uses (unknown fields rejected, unsupported versions refused). This
/// does **not** weaken the decoder; it only exposes it to the fuzz
/// target and the seed-corpus regression test.
#[doc(hidden)]
pub fn fuzz_decode_record_envelope(payload: &[u8]) -> Result<(), String> {
    #[derive(serde::Deserialize)]
    struct RecordVersionProbe {
        record_version: u32,
    }
    let probe: RecordVersionProbe =
        serde_json::from_slice(payload).map_err(|error| error.to_string())?;
    // Delegate the acceptance window to the same production helper replay
    // calls, so the fuzz target cannot silently drift from replay (the
    // current version and the N-1 legacy version; 0 and newer refused).
    if volvisor_types::schema::ensure_supported_schema_version(
        "journal record",
        std::path::Path::new("<fuzz input>"),
        probe.record_version,
        RECORD_VERSION,
    )
    .is_err()
    {
        return Err(format!(
            "unsupported record_version {}",
            probe.record_version
        ));
    }
    serde_json::from_slice::<record::Envelope>(payload)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Map an `std::io::Error` into a typed internal `ApiError` with context.
///
/// A helper instead of a blanket `From` impl: journal call sites always pair
/// the failure with the operation being attempted, which keeps the resulting
/// detail string actionable.
pub(crate) fn io_err(context: &str) -> impl Fn(std::io::Error) -> volvisor_types::ApiError + '_ {
    move |err| {
        volvisor_types::ApiError::new(
            volvisor_types::ApiErrorCode::Internal,
            format!("{context}: {err}"),
        )
    }
}
