//! # volvisor-journal
//!
//! Durable append-only intent journal with a replay-derived idempotency
//! registry for the Volvisor control plane (Volume API v2 contract section 7,
//! AGENTS rules 4 and 8).
//!
//! ## Durability model
//!
//! - Records live in `<dir>/journal.log` as length-prefixed, CRC-checked
//!   frames with a monotonic sequence number (see the [`frame`] module
//!   docs for the exact layout).
//! - Every acknowledged append writes the frame bytes and calls
//!   `fsync` on the log file before returning; the journal directory is
//!   fsynced once at creation time so the log's directory entry is durable.
//! - A partially written or torn trailing record (truncated length, bad CRC,
//!   bad magic, broken sequence, undecodable payload) is detected on replay
//!   and the log is truncated back to the last valid record. Torn tails are
//!   the expected outcome of power loss mid-append and are never an error.
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

#![deny(unsafe_code)]
// Tests may use expect/unwrap for invariant assertions; production code may not.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod frame;
mod journal;
mod record;

pub use journal::{
    IntentAppend, JOURNAL_LOCK_FILE, JOURNAL_LOG_FILE, Journal, RecordedOutcome, RegistryEntry,
};
pub use record::{Checkpoint, Intent, JournalRecord, Outcome, RECORD_VERSION};

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
