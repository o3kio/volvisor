//! The journal: single-writer open, crash replay, idempotency registry and
//! fsynced appends.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[cfg(feature = "test-faults")]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use volvisor_types::{ApiError, ApiErrorCode, OperationId};

use crate::frame;
use crate::io_err;
use crate::record::{Envelope, Intent, JournalRecord, Outcome, RECORD_VERSION};

/// File name of the append-only journal log inside the journal directory.
pub const JOURNAL_LOG_FILE: &str = "journal.log";

/// File name of the single-writer `flock` target inside the journal directory.
pub const JOURNAL_LOCK_FILE: &str = "journal.lock";

/// A recorded operation outcome, served to idempotent replays.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedOutcome {
    /// Whether the mutation succeeded.
    pub success: bool,
    /// Response body (or error detail) recorded when the operation finished.
    pub response: serde_json::Value,
    /// The HTTP status the original response was served with, when the
    /// outcome recorded one (stage B2: `201`/`202` mobility routes); a
    /// replay serves this status instead of the default mapping.
    pub http_status: Option<u16>,
}

/// Idempotency-registry view of one operation, derived from replay.
#[derive(Clone, Debug, PartialEq)]
pub struct RegistryEntry {
    /// Request hash recorded with the operation's intent.
    pub request_hash: [u8; 32],
    /// Whether an outcome record exists for the operation.
    pub has_outcome: bool,
    /// The recorded outcome, when one exists.
    pub outcome: Option<RecordedOutcome>,
}

/// Result of appending an intent to the journal.
#[derive(Clone, Debug, PartialEq)]
pub enum IntentAppend {
    /// Fresh intent, durably recorded; the caller may execute the mutation.
    New,
    /// Replay of an already-recorded intent with the same request hash,
    /// carrying the recorded outcome.
    ///
    /// `success` lets the caller reconstruct the HTTP status of the
    /// original response (successes replay as `200` unless the outcome
    /// recorded its own [`http_status`](IntentAppend::Replayed::http_status),
    /// failures as the recorded wire code's status), keeping replays
    /// status- and byte-compatible with the first caller's response.
    /// An earlier revision carried `Option<Value>` for outcome kinds
    /// "without a replayable body"; no such kind exists (every recorded
    /// outcome carries a JSON body), so the never-`None` `Option` is
    /// collapsed into this unconditional shape.
    Replayed {
        /// Whether the recorded mutation succeeded.
        success: bool,
        /// The recorded response body, replayed verbatim.
        response: serde_json::Value,
        /// The recorded HTTP status, when the outcome carried one
        /// (stage-B2 mobility routes); `None` replays through the
        /// default status mapping.
        http_status: Option<u16>,
    },
    /// The intent is durably recorded but has no outcome yet: the operation
    /// is in flight (or was interrupted before its outcome was journaled).
    AlreadyInFlight,
}

/// Internal registry state for one operation.
///
/// `request_hash` is `None` only for an operation known exclusively through
/// an outcome record (no intent). That cannot be produced by the documented
/// journal-before-mutate ordering; if a hand-crafted journal contains one,
/// the registry fails closed on any later intent for that operation.
#[derive(Clone, Debug, Default)]
struct OperationState {
    request_hash: Option<[u8; 32]>,
    outcome: Option<RecordedOutcome>,
}

/// The durable intent journal for one journal directory.
///
/// Holds the exclusive single-writer lock for its lifetime; dropping the
/// value releases it. All appends are fsynced before they return.
#[derive(Debug)]
pub struct Journal {
    lock_file: File,
    log: File,
    log_path: PathBuf,
    operations: HashMap<OperationId, OperationState>,
    next_sequence: u64,
    record_count: u64,
    /// Test-fault injection (feature `test-faults`): appends fail once this
    /// countdown reaches zero. `u64::MAX` (default) never fires.
    #[cfg(feature = "test-faults")]
    fail_after_remaining: AtomicU64,
}

struct Replayed {
    operations: HashMap<OperationId, OperationState>,
    next_sequence: u64,
    record_count: u64,
    records: Vec<JournalRecord>,
}

impl Journal {
    /// Open (or create) the journal in `dir`, enforcing single-writer
    /// ownership and replaying the log.
    ///
    /// Fails with a typed internal error when the directory cannot be
    /// created, the lock cannot be acquired (another writer holds it), or
    /// any file operation fails. A torn trailing record is *not* an error:
    /// the log is truncated to the last valid record.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, ApiError> {
        Self::open_inner(dir, false).map(|(journal, _)| journal)
    }

    /// Open like [`Journal::open`], additionally returning every valid
    /// record in journal order (the same record set replay derived state
    /// from, torn tails excluded).
    ///
    /// For consumers that derive their own state by folding the log (the
    /// witness registry): they get the one-time snapshot at open and track
    /// subsequent appends themselves, so the journal keeps no per-record
    /// retention for regular users.
    pub fn open_with_records(
        dir: impl AsRef<Path>,
    ) -> Result<(Self, Vec<JournalRecord>), ApiError> {
        Self::open_inner(dir, true)
    }

    /// Shared open path; `collect_records` retains the replayed records.
    fn open_inner(
        dir: impl AsRef<Path>,
        collect_records: bool,
    ) -> Result<(Self, Vec<JournalRecord>), ApiError> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir).map_err(io_err("failed to create journal directory"))?;

        // Single-writer enforcement: exclusive, non-blocking flock held for
        // the lifetime of this struct. A second open fails fast.
        let lock_path = dir.join(JOURNAL_LOCK_FILE);
        let mut lock_opts = OpenOptions::new();
        lock_opts
            .read(true)
            .write(true)
            .create(true)
            .truncate(false);
        #[cfg(unix)]
        lock_opts.mode(0o600);
        let lock_file = lock_opts.open(&lock_path).map_err(io_err(&format!(
            "failed to open journal lock file {}",
            lock_path.display()
        )))?;
        rustix::fs::flock(
            &lock_file,
            rustix::fs::FlockOperation::NonBlockingLockExclusive,
        )
        .map_err(|err| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "journal directory {} is already locked by another writer \
                     (single-writer enforcement): {err}",
                    dir.display()
                ),
            )
        })?;

        let log_path = dir.join(JOURNAL_LOG_FILE);
        let log_created = !log_path.exists();
        let mut log_opts = OpenOptions::new();
        log_opts.read(true).append(true).create(true);
        #[cfg(unix)]
        log_opts.mode(0o600);
        let mut log = log_opts.open(&log_path).map_err(io_err(&format!(
            "failed to open journal log {}",
            log_path.display()
        )))?;

        if log_created {
            // Make the new log file's directory entry durable before any
            // record is acknowledged.
            File::open(dir)
                .and_then(|dir_handle| dir_handle.sync_all())
                .map_err(io_err(
                    "failed to fsync journal directory after log creation",
                ))?;
        }

        let replayed = replay(&mut log, collect_records)?;

        let journal = Self {
            lock_file,
            log,
            log_path,
            operations: replayed.operations,
            next_sequence: replayed.next_sequence,
            record_count: replayed.record_count,
            #[cfg(feature = "test-faults")]
            fail_after_remaining: AtomicU64::new(u64::MAX),
        };
        Ok((journal, replayed.records))
    }

    /// Test-only fault injection (feature `test-faults`): let the next `n`
    /// appends succeed, then fail every subsequent append with a typed
    /// `INTERNAL` error. Used to exercise the API layer's
    /// outcome-could-not-be-journaled branches honestly.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn inject_append_failures_after(&self, n: u64) {
        self.fail_after_remaining.store(n, Ordering::SeqCst);
    }

    /// Look up an operation in the replay-derived idempotency registry.
    ///
    /// Returns `None` for unknown operations and for operations known only
    /// through an outcome record (no intent, hence no request hash to
    /// compare — see the private `OperationState` docs).
    #[must_use]
    pub fn lookup(&self, operation_id: &OperationId) -> Option<RegistryEntry> {
        let state = self.operations.get(operation_id)?;
        let request_hash = state.request_hash?;
        Some(RegistryEntry {
            request_hash,
            has_outcome: state.outcome.is_some(),
            outcome: state.outcome.clone(),
        })
    }

    /// Append an intent record, resolving idempotency first.
    ///
    /// - unknown operation: journals and fsyncs the intent, returns
    ///   [`IntentAppend::New`];
    /// - known operation, same request hash: returns
    ///   [`IntentAppend::Replayed`] with the recorded outcome (success flag
    ///   and response body) when one exists, otherwise
    ///   [`IntentAppend::AlreadyInFlight`] (nothing is written);
    /// - known operation, different request hash (or hash unverifiable):
    ///   `ApiError::idempotency_conflict`, fail closed.
    pub fn append_intent(
        &mut self,
        operation_id: OperationId,
        request_hash: [u8; 32],
        op_kind: &str,
        payload: serde_json::Value,
    ) -> Result<IntentAppend, ApiError> {
        if let Some(state) = self.operations.get(&operation_id) {
            return match state.request_hash {
                Some(recorded) if recorded == request_hash => Ok(match &state.outcome {
                    Some(outcome) => IntentAppend::Replayed {
                        success: outcome.success,
                        response: outcome.response.clone(),
                        http_status: outcome.http_status,
                    },
                    None => IntentAppend::AlreadyInFlight,
                }),
                // Different request hash, or an outcome-only entry whose
                // request hash is unknown: both fail closed.
                _ => Err(ApiError::idempotency_conflict(&operation_id)),
            };
        }

        self.append(JournalRecord::Intent(Intent {
            operation_id: operation_id.clone(),
            request_hash,
            op_kind: op_kind.to_owned(),
            payload,
        }))?;
        let entry = self.operations.entry(operation_id).or_default();
        entry.request_hash = Some(request_hash);
        Ok(IntentAppend::New)
    }

    /// Append an outcome record for `operation_id` and fsync it.
    ///
    /// The most recent outcome wins; an outcome for an operation without a
    /// journaled intent is recorded (registry stays consistent on replay)
    /// but the operation fails closed if an intent for it is ever appended.
    pub fn append_outcome(
        &mut self,
        operation_id: OperationId,
        success: bool,
        response: serde_json::Value,
    ) -> Result<(), ApiError> {
        self.append_outcome_with_status(operation_id, success, response, None)
    }

    /// Append an outcome record carrying the HTTP status the response was
    /// served with (stage B2: the mobility routes answer `201` and `202`,
    /// and an idempotent replay must serve the same status, not the
    /// default `200`-on-success mapping). Otherwise identical to
    /// [`Journal::append_outcome`].
    pub fn append_outcome_with_status(
        &mut self,
        operation_id: OperationId,
        success: bool,
        response: serde_json::Value,
        http_status: Option<u16>,
    ) -> Result<(), ApiError> {
        self.append(JournalRecord::Outcome(Outcome {
            operation_id: operation_id.clone(),
            success,
            response: response.clone(),
            http_status,
        }))?;
        let entry = self.operations.entry(operation_id).or_default();
        entry.outcome = Some(RecordedOutcome {
            success,
            response,
            http_status,
        });
        Ok(())
    }

    /// Append a checkpoint marker and fsync it. Carries no idempotency
    /// state; useful for operator annotations and journal smoke tests.
    pub fn append_checkpoint(&mut self, note: &str) -> Result<(), ApiError> {
        self.append(JournalRecord::Checkpoint(crate::record::Checkpoint {
            note: note.to_owned(),
        }))
    }

    /// Number of valid records in the journal (replayed plus appended).
    #[must_use]
    pub fn record_count(&self) -> u64 {
        self.record_count
    }

    /// Path of the journal log file (exposed for tests and diagnostics).
    #[must_use]
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    /// Serialize, frame, write and fsync one record. Acknowledged only
    /// after the data reaches stable storage.
    fn append(&mut self, record: JournalRecord) -> Result<(), ApiError> {
        #[cfg(feature = "test-faults")]
        {
            let remaining = self.fail_after_remaining.load(Ordering::SeqCst);
            if remaining == 0 {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    "injected journal append failure (test-faults)",
                ));
            }
            if remaining != u64::MAX {
                self.fail_after_remaining
                    .store(remaining - 1, Ordering::SeqCst);
            }
        }
        let envelope = Envelope {
            record_version: RECORD_VERSION,
            record,
        };
        let payload = serde_json::to_vec(&envelope).map_err(|err| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!("failed to serialize journal record: {err}"),
            )
        })?;
        let frame = frame::encode(self.next_sequence, &payload)?;
        self.log
            .write_all(&frame)
            .map_err(io_err("failed to write journal record"))?;
        self.log
            .sync_all()
            .map_err(io_err("failed to fsync journal record"))?;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.record_count = self.record_count.saturating_add(1);
        Ok(())
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        // Best-effort explicit release; closing the fd releases the flock
        // regardless, so errors are deliberately ignored here.
        let _ = rustix::fs::flock(&self.lock_file, rustix::fs::FlockOperation::Unlock);
    }
}

/// Replay the log file, rebuilding the idempotency registry and truncating
/// any torn tail.
fn replay(log: &mut File, collect_records: bool) -> Result<Replayed, ApiError> {
    log.seek(SeekFrom::Start(0))
        .map_err(io_err("failed to seek journal log for replay"))?;
    let mut data = Vec::new();
    log.read_to_end(&mut data)
        .map_err(io_err("failed to read journal log for replay"))?;

    let mut replayed = Replayed {
        operations: HashMap::new(),
        next_sequence: 1,
        record_count: 0,
        records: Vec::new(),
    };
    let mut offset = 0usize;
    while let Some(raw) = frame::decode_at(&data, offset, replayed.next_sequence) {
        let envelope = serde_json::from_slice::<Envelope>(raw.payload)
            .ok()
            .filter(|envelope| envelope.record_version == RECORD_VERSION);
        let Some(envelope) = envelope else {
            // Undecodable payload or unknown record version: torn/corrupt
            // tail. Stop replay at this frame boundary.
            break;
        };
        if collect_records {
            replayed.records.push(envelope.record.clone());
        }
        apply_record(&mut replayed, envelope.record);
        replayed.next_sequence = raw.sequence + 1;
        replayed.record_count += 1;
        offset = raw.end;
    }

    if offset < data.len() {
        // Torn tail after a crash: expected, never an error. Truncate back
        // to the last valid record so future appends start at a clean
        // boundary, and make the truncation durable.
        log.set_len(u64::try_from(offset).map_err(|_| {
            ApiError::new(
                ApiErrorCode::Internal,
                "journal valid prefix length does not fit in u64",
            )
        })?)
        .map_err(io_err("failed to truncate torn journal tail"))?;
        log.sync_all()
            .map_err(io_err("failed to fsync journal after tail truncation"))?;
    }
    Ok(replayed)
}

/// Fold one replayed record into the registry state.
fn apply_record(replayed: &mut Replayed, record: JournalRecord) {
    match record {
        JournalRecord::Intent(intent) => {
            let entry = replayed.operations.entry(intent.operation_id).or_default();
            if entry.request_hash.is_none() {
                entry.request_hash = Some(intent.request_hash);
            }
        }
        JournalRecord::Outcome(outcome) => {
            let entry = replayed.operations.entry(outcome.operation_id).or_default();
            entry.outcome = Some(RecordedOutcome {
                success: outcome.success,
                response: outcome.response,
                http_status: outcome.http_status,
            });
        }
        JournalRecord::Checkpoint(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use volvisor_types::OperationId;

    fn op_id(name: &str) -> OperationId {
        OperationId::new(name).expect("valid operation id")
    }

    #[test]
    fn open_creates_missing_directory_and_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal_dir = dir.path().join("nested").join("journal");
        let journal = Journal::open(&journal_dir).expect("open");
        assert!(journal_dir.join(JOURNAL_LOG_FILE).is_file());
        assert!(journal_dir.join(JOURNAL_LOCK_FILE).is_file());
        assert_eq!(journal.record_count(), 0);
    }

    #[test]
    fn second_open_fails_while_lock_held() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = Journal::open(dir.path()).expect("first open");
        let err = Journal::open(dir.path()).expect_err("second open must fail");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(err.detail.contains("already locked"), "detail: {err}");
        drop(first);
        Journal::open(dir.path()).expect("reopen after lock release");
    }

    #[test]
    fn torn_tail_bad_crc_is_truncated_on_replay() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log_path = dir.path().join(JOURNAL_LOG_FILE);
        let mut journal = Journal::open(dir.path()).expect("open");

        let first = op_id("op-1");
        journal
            .append_intent(
                first.clone(),
                [1; 32],
                "create_volume",
                serde_json::json!({"n": 1}),
            )
            .expect("intent 1");
        journal
            .append_outcome(first, true, serde_json::json!({"ok": true}))
            .expect("outcome 1");
        let valid_len = std::fs::metadata(&log_path).expect("stat").len();

        journal
            .append_intent(
                op_id("op-2"),
                [2; 32],
                "create_volume",
                serde_json::json!({"n": 2}),
            )
            .expect("intent 2");
        drop(journal);

        // Corrupt the last byte of the final frame: CRC fails on replay.
        let mut bytes = std::fs::read(&log_path).expect("read log");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        std::fs::write(&log_path, &bytes).expect("write log");

        let journal = Journal::open(dir.path()).expect("reopen after torn tail");
        assert_eq!(journal.record_count(), 2, "only the valid prefix replays");
        assert_eq!(
            std::fs::metadata(&log_path).expect("stat").len(),
            valid_len,
            "log is truncated back to the valid prefix"
        );
        assert!(journal.lookup(&op_id("op-2")).is_none());
    }

    #[test]
    fn appends_after_tail_truncation_continue_cleanly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log_path = dir.path().join(JOURNAL_LOG_FILE);
        {
            let mut journal = Journal::open(dir.path()).expect("open");
            journal
                .append_intent(
                    op_id("op-1"),
                    [1; 32],
                    "create_volume",
                    serde_json::json!({}),
                )
                .expect("intent");
            drop(journal);
        }
        // Torn tail: trailing garbage after the valid frame.
        {
            use std::io::Write as _;
            let mut log = OpenOptions::new()
                .append(true)
                .open(&log_path)
                .expect("open log");
            log.write_all(b"debris from a crash mid-append")
                .expect("garbage");
        }
        {
            let mut journal = Journal::open(dir.path()).expect("reopen");
            assert_eq!(journal.record_count(), 1);
            journal
                .append_checkpoint("after recovery")
                .expect("checkpoint after truncation");
            assert_eq!(journal.record_count(), 2);
        }
        let journal = Journal::open(dir.path()).expect("second reopen");
        assert_eq!(journal.record_count(), 2, "post-truncation appends survive");
    }

    #[test]
    fn unknown_record_version_is_treated_as_torn_tail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log_path = dir.path().join(JOURNAL_LOG_FILE);
        let envelope = Envelope {
            record_version: RECORD_VERSION + 1,
            record: JournalRecord::Checkpoint(crate::record::Checkpoint {
                note: "from the future".to_owned(),
            }),
        };
        let payload = serde_json::to_vec(&envelope).expect("serialize");
        let frame = frame::encode(1, &payload).expect("encode");
        std::fs::write(&log_path, &frame).expect("write");

        let journal = Journal::open(dir.path()).expect("open");
        assert_eq!(journal.record_count(), 0);
        assert_eq!(
            std::fs::metadata(&log_path).expect("stat").len(),
            0,
            "unreadable-version record is truncated away"
        );
    }

    #[test]
    fn outcome_without_intent_fails_closed_on_later_intent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut journal = Journal::open(dir.path()).expect("open");
        let orphan = op_id("op-orphan");
        journal
            .append_outcome(orphan.clone(), true, serde_json::json!({"ok": true}))
            .expect("outcome");
        // No intent was journaled: the request hash is unknown, so lookup
        // reports nothing and a later intent must fail closed.
        assert!(journal.lookup(&orphan).is_none());
        let err = journal
            .append_intent(orphan, [9; 32], "create_volume", serde_json::json!({}))
            .expect_err("intent for outcome-only operation must fail");
        assert_eq!(err.code, ApiErrorCode::IdempotencyConflict);
    }

    #[test]
    fn registry_entry_shape_after_replay() {
        let dir = tempfile::tempdir().expect("tempdir");
        let done = op_id("op-done");
        let pending = op_id("op-pending");
        {
            let mut journal = Journal::open(dir.path()).expect("open");
            journal
                .append_intent(
                    done.clone(),
                    [3; 32],
                    "create_volume",
                    serde_json::json!({"n": 1}),
                )
                .expect("intent done");
            journal
                .append_outcome(done.clone(), true, serde_json::json!({"volume": "v"}))
                .expect("outcome done");
            journal
                .append_intent(
                    pending.clone(),
                    [4; 32],
                    "delete_volume",
                    serde_json::json!({"n": 2}),
                )
                .expect("intent pending");
        }
        let journal = Journal::open(dir.path()).expect("reopen");

        let done_entry = journal.lookup(&done).expect("done entry");
        assert_eq!(done_entry.request_hash, [3; 32]);
        assert!(done_entry.has_outcome);
        let outcome = done_entry.outcome.expect("outcome");
        assert!(outcome.success);
        assert_eq!(outcome.response, serde_json::json!({"volume": "v"}));

        let pending_entry = journal.lookup(&pending).expect("pending entry");
        assert_eq!(pending_entry.request_hash, [4; 32]);
        assert!(!pending_entry.has_outcome);
        assert!(pending_entry.outcome.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn journal_files_are_owner_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let mut journal = Journal::open(dir.path()).expect("open");
        let op = OperationId::new("op-perms").expect("op");
        journal
            .append_intent(op, [1; 32], "create_volume", serde_json::json!({}))
            .expect("append");

        let mode = |path: std::path::PathBuf| {
            std::fs::metadata(path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(
            mode(dir.path().join(JOURNAL_LOG_FILE)),
            0o600,
            "journal log must not be readable by other local users"
        );
        assert_eq!(
            mode(dir.path().join(JOURNAL_LOCK_FILE)),
            0o600,
            "journal lock must not be readable by other local users"
        );
    }

    #[test]
    fn outcome_with_status_replays_status_compatible() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = op_id("op-status-replay");
        let hash = [11; 32];
        let body = serde_json::json!({"state": "PREPARED"});
        {
            let mut journal = Journal::open(dir.path()).expect("open");
            journal
                .append_intent(
                    id.clone(),
                    hash,
                    "prepare_nearline_handoff",
                    serde_json::json!({"migration_id": "mig-1"}),
                )
                .expect("intent");
            journal
                .append_outcome_with_status(id.clone(), true, body.clone(), Some(201))
                .expect("outcome with status");
            // Same-process replay: the recorded status is carried.
            assert!(matches!(
                journal.append_intent(
                    id.clone(),
                    hash,
                    "prepare_nearline_handoff",
                    serde_json::json!({"migration_id": "mig-1"}),
                ),
                Ok(IntentAppend::Replayed {
                    success: true,
                    http_status: Some(201),
                    ..
                })
            ));
        }
        // The status survives a reopen (registry derived from replay).
        let journal = Journal::open(dir.path()).expect("reopen");
        let entry = journal.lookup(&id).expect("registry entry");
        let outcome = entry.outcome.expect("outcome");
        assert!(outcome.success);
        assert_eq!(outcome.http_status, Some(201));
        assert_eq!(outcome.response, body);
    }

    #[cfg(feature = "test-faults")]
    #[test]
    fn injected_append_failures_fail_typed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut journal = Journal::open(dir.path()).expect("open");
        journal.inject_append_failures_after(1);

        let ok = OperationId::new("op-ok").expect("op");
        journal
            .append_intent(ok, [1; 32], "create_volume", serde_json::json!({}))
            .expect("first append succeeds");

        let blocked = OperationId::new("op-blocked").expect("op");
        let err = journal
            .append_intent(blocked, [2; 32], "create_volume", serde_json::json!({}))
            .expect_err("second append fails");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(err.detail.contains("injected"));
    }
}
