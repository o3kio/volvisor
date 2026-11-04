//! The journal: single-writer open, crash replay, idempotency registry and
//! fsynced appends.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
#[cfg(feature = "test-faults")]
use std::io::Read;
use std::io::{BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[cfg(feature = "test-faults")]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use volvisor_types::schema::ensure_supported_schema_version;
use volvisor_types::{ApiError, ApiErrorCode, OperationId};

use crate::frame;
use crate::io_err;
use crate::record::{
    Envelope, Intent, JournalRecord, Outcome, RECORD_VERSION, Snapshot, SnapshotOperation,
    SnapshotOutcome,
};

/// File name of the append-only journal log inside the journal directory.
pub const JOURNAL_LOG_FILE: &str = "journal.log";

/// File name of the single-writer `flock` target inside the journal directory.
pub const JOURNAL_LOCK_FILE: &str = "journal.lock";

/// Default bound on terminal operations retained in memory and in the
/// compacted snapshot (in-flight intents are never evicted).
pub const DEFAULT_MAX_RETAINED_OPERATIONS: usize = 65_536;

/// Default bound on the exact 64-bit tombstone ring written by compaction.
pub const DEFAULT_MAX_TOMBSTONES: usize = 65_536;

/// Default record-count threshold that triggers compaction.
pub const DEFAULT_MAX_LOG_RECORDS: u64 = 500_000;

/// Default byte threshold that triggers compaction (64 MiB).
pub const DEFAULT_MAX_LOG_BYTES: u64 = 64 * 1024 * 1024;

/// Minimum number of new records after a successful compaction before the
/// record trigger may fire again.
///
/// Without this floor a legal-but-tiny `max_log_records` (for example `2`)
/// rewrites the whole retained snapshot on every append: the post-compaction
/// log holds one record, so it immediately re-crosses the threshold. The
/// floor caps the write amplification at one snapshot per
/// `MIN_COMPACTION_RECORD_GROWTH` records.
const MIN_COMPACTION_RECORD_GROWTH: u64 = 64;

/// Minimum number of new log bytes after a successful compaction before the
/// byte trigger may fire again (1 MiB).
///
/// A configured `max_log_bytes` below the compacted snapshot's own size can
/// never be "gotten under"; without this floor it would rewrite the snapshot
/// on every append.
const MIN_COMPACTION_BYTE_GROWTH: u64 = 1024 * 1024;

/// Maximum exponent of the failed-compaction retry backoff.
///
/// A failed compaction is retried after `2^streak` new records or
/// `2^streak` MiB of new log bytes, capped at this exponent (64 records /
/// 64 MiB). The first failure therefore still retries promptly (after one
/// record, which preserves the "unblock and append again" recovery path),
/// while a persistently failing compaction is attempted only O(log n) times
/// over n appends instead of once per append.
const MAX_COMPACTION_RETRY_SHIFT: u32 = 6;

/// Journal retention/compaction policy (issue #64).
///
/// Appends are still fsynced one record at a time. When a threshold is
/// crossed, the journal rewrites its log as a single [`Snapshot`] frame
/// atomically (tmp file, fsync, rename, directory fsync), retaining the
/// newest `max_retained_operations` terminal operations and every in-flight
/// intent, and recording exact 64-bit fingerprints for recently evicted
/// terminal operations.
///
/// # Retention contract
///
/// A retried operation whose entry was evicted **within the tombstone
/// window** is refused with `OPERATION_EXPIRED` (fail closed). An operation
/// evicted longer ago than the newest `max_tombstones` evictions is
/// indistinguishable from an unknown operation and will be executed again.
/// Operators must therefore keep the window (in operations) larger than the
/// longest client retry horizon; see `docs/schema-compatibility.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalRetention {
    /// Terminal operations retained in the registry/snapshot. In-flight
    /// intents are always retained regardless of this bound.
    pub max_retained_operations: usize,
    /// Newest evicted-operation fingerprints retained by the tombstone ring.
    pub max_tombstones: usize,
    /// Compact after this many valid records in the current log.
    pub max_log_records: u64,
    /// Compact after the log reaches this many bytes, or `None` to disable
    /// the byte trigger.
    pub max_log_bytes: Option<u64>,
}

impl Default for JournalRetention {
    fn default() -> Self {
        Self {
            max_retained_operations: DEFAULT_MAX_RETAINED_OPERATIONS,
            max_tombstones: DEFAULT_MAX_TOMBSTONES,
            max_log_records: DEFAULT_MAX_LOG_RECORDS,
            max_log_bytes: Some(DEFAULT_MAX_LOG_BYTES),
        }
    }
}

impl JournalRetention {
    /// Validate the policy. Zero-valued bounds are rejected: a zero
    /// retention bound would silently evict every terminal operation and a
    /// zero tombstone ring would silently re-execute retries.
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.max_retained_operations == 0 {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                "journal retention max_retained_operations must be at least 1",
            ));
        }
        if self.max_tombstones == 0 {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                "journal retention max_tombstones must be at least 1 \
                 (a zero ring would silently re-execute evicted operations)",
            ));
        }
        if self.max_log_records < 2 {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                "journal retention max_log_records must be at least 2 \
                 (a one-record threshold rewrites the snapshot on every append)",
            ));
        }
        if self.max_log_bytes == Some(0) {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                "journal retention max_log_bytes must be at least 1 when set",
            ));
        }
        Ok(())
    }
}

/// Growth required before a failed compaction may be retried.
#[derive(Clone, Copy, Debug)]
struct CompactionRetry {
    /// Retry once the log again holds at least this many records.
    at_records: u64,
    /// Retry once the log again reaches at least this many bytes.
    at_bytes: u64,
}

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
    /// Terminal operations in terminal-order (oldest first), for bounded
    /// eviction at compaction.
    terminal_order: VecDeque<OperationId>,
    /// Fingerprints of recently evicted terminal operations (oldest first),
    /// paired with an O(1) membership index. Each fingerprint appears at most
    /// once; see [`TombstoneRing`].
    tombstones: TombstoneRing,
    /// Total operations evicted by compaction (diagnostic).
    evicted_operations: u64,
    next_sequence: u64,
    record_count: u64,
    /// Current log size in bytes, tracked without a stat per append.
    log_bytes: u64,
    /// Set when a write/fsync failed in a way that could not be rolled
    /// back: the log may now end in a partial frame, so accepting another
    /// append would write a valid frame after a torn tail that replay
    /// would never reach. The journal must be reopened.
    poisoned: Option<String>,
    retention: JournalRetention,
    /// Whether this journal may compact/evict. Witness journals
    /// (`open_with_records`) opt out: they fold the complete record history.
    compaction_enabled: bool,
    /// Number of successful compactions (diagnostic).
    compactions: u64,
    /// Number of failed compaction attempts (diagnostic; a failed attempt
    /// never fails the already-fsynced append and is retried after bounded
    /// new growth).
    compaction_failures: u64,
    /// Last compaction failure detail, if any (diagnostic).
    last_compaction_error: Option<String>,
    /// Consecutive failed compaction attempts since the last success. Drives
    /// the exponential retry backoff (`2^streak`, capped by
    /// [`MAX_COMPACTION_RETRY_SHIFT`]); reset to zero on success.
    compaction_failure_streak: u32,
    /// Growth gate for retrying a failed compaction, or `None` when no
    /// failure is pending. A persistently failing compaction is not attempted
    /// on every append; the log must grow by the recorded amount first.
    compaction_retry: Option<CompactionRetry>,
    /// Record count immediately after the last applied compaction. Used with
    /// [`MIN_COMPACTION_RECORD_GROWTH`] to prevent a below-snapshot record
    /// threshold from compacting on every append.
    last_compaction_records: u64,
    /// Log size in bytes immediately after the last applied compaction.
    last_compaction_bytes: u64,
    /// A compaction rename succeeded but its directory fsync failed. Every
    /// later append must make the rename durable before it is acknowledged;
    /// failing to do so could lose acknowledged records if the directory
    /// entry reverts to the old log after a crash.
    dir_sync_pending: bool,
    /// Operations pinned by an executing mutation pipeline. A pinned
    /// operation is never evicted by compaction, even if terminal: a retry
    /// may be re-driving it and about to append a replacement outcome, and
    /// evicting it between those steps would refuse that outcome after the
    /// mutation already landed. In-memory only; no pin survives a restart
    /// (an interrupted request leaves a durable in-flight or terminal
    /// record, which retention treats normally).
    pinned_operations: HashMap<OperationId, usize>,
    /// Test-fault injection (feature `test-faults`): appends fail once this
    /// countdown reaches zero. `u64::MAX` (default) never fires.
    #[cfg(feature = "test-faults")]
    fail_after_remaining: AtomicU64,
    /// Test-fault injection (feature `test-faults`): the append writes a
    /// short (partial) frame once this countdown reaches zero, exercising
    /// the mid-write rollback path. The fault fires once and then disarms.
    #[cfg(feature = "test-faults")]
    fail_write_after_remaining: AtomicU64,
}

struct Replayed {
    operations: HashMap<OperationId, OperationState>,
    terminal_order: VecDeque<OperationId>,
    /// Ordered eviction ring plus its O(1) membership index, replayed from a
    /// snapshot and extended by bounded replay evictions.
    tombstones: TombstoneRing,
    /// The subset of [`Replayed::tombstones`] that came from a durable
    /// compaction [`Snapshot`].
    ///
    /// Replay itself also evicts terminal operations when the configured
    /// retention bound is smaller than the historical log window. Those
    /// provisional fingerprints must not make a later, legal record for the
    /// same operation a fatal inconsistency: the raw intent/outcome is still
    /// in this log. Only a *snapshot* tombstone is a durable
    /// retention decision, so only that kind is a hard refusal.
    snapshot_tombstones: HashSet<u64>,
    evicted_operations: u64,
    next_sequence: u64,
    record_count: u64,
    records: Vec<JournalRecord>,
    /// Whether bounded replay itself evicted terminal operations, so the
    /// caller must persist a snapshot before serving.
    evicted_during_replay: bool,
}

/// Version-only probe for a frame payload.
///
/// Deliberately tolerates unknown fields: a record written by a newer build
/// must be recognized by its version *before* the strict decode rejects the
/// fields this build does not know, so the operator sees the precise
/// unsupported-version refusal instead of a generic parse error.
#[derive(Deserialize)]
struct RecordVersionProbe {
    record_version: u32,
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
        Self::open_with_retention(dir, JournalRetention::default())
    }

    /// Open (or create) the journal with an explicit retention/compaction
    /// policy (issue #64). See [`JournalRetention`].
    ///
    /// Replays the log with bounded memory: frames are read one at a time,
    /// terminal operations beyond the retention bound are evicted into the
    /// tombstone ring as replay proceeds, and a compaction snapshot is
    /// written before the journal is served if the resulting log crosses a
    /// policy threshold. In-flight intents are never evicted.
    pub fn open_with_retention(
        dir: impl AsRef<Path>,
        retention: JournalRetention,
    ) -> Result<Self, ApiError> {
        retention.validate()?;
        Self::open_inner(dir, false, retention, true).map(|(journal, _)| journal)
    }

    /// Open like [`Journal::open`], additionally returning every valid
    /// record in journal order (the same record set replay derived state
    /// from, torn tails excluded).
    ///
    /// For consumers that derive their own state by folding the log (the
    /// witness registry): they get the one-time snapshot at open and track
    /// subsequent appends themselves, so the journal keeps no per-record
    /// retention for regular users.
    ///
    /// # Compaction opt-out
    ///
    /// A history-folding consumer cannot derive authority state from a
    /// compacted snapshot. This open therefore **disables** eviction and
    /// compaction, and refuses a log that already begins with a [`Snapshot`]
    /// record with a typed error rather than silently folding a prefix.
    pub fn open_with_records(
        dir: impl AsRef<Path>,
    ) -> Result<(Self, Vec<JournalRecord>), ApiError> {
        Self::open_inner(dir, true, JournalRetention::default(), false)
    }

    /// Shared open path; `collect_records` retains the replayed records and
    /// `compaction_enabled` controls whether this journal may evict/compact.
    fn open_inner(
        dir: impl AsRef<Path>,
        collect_records: bool,
        retention: JournalRetention,
        compaction_enabled: bool,
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

        let replayed = replay(
            &mut log,
            &log_path,
            collect_records,
            &retention,
            compaction_enabled,
        )?;
        let log_bytes = log
            .metadata()
            .map_err(io_err("failed to stat journal log after replay"))?
            .len();
        let evicted_during_replay = replayed.evicted_during_replay;

        let mut journal = Self {
            lock_file,
            log,
            log_path,
            operations: replayed.operations,
            terminal_order: replayed.terminal_order,
            tombstones: replayed.tombstones,
            evicted_operations: replayed.evicted_operations,
            next_sequence: replayed.next_sequence,
            record_count: replayed.record_count,
            log_bytes,
            poisoned: None,
            retention,
            compaction_enabled,
            compactions: 0,
            compaction_failures: 0,
            last_compaction_error: None,
            compaction_failure_streak: 0,
            compaction_retry: None,
            last_compaction_records: 0,
            last_compaction_bytes: 0,
            dir_sync_pending: false,
            pinned_operations: HashMap::new(),
            #[cfg(feature = "test-faults")]
            fail_after_remaining: AtomicU64::new(u64::MAX),
            #[cfg(feature = "test-faults")]
            fail_write_after_remaining: AtomicU64::new(u64::MAX),
        };
        // Bounded replay may itself have crossed the retention bound (or the
        // existing log may already be over a threshold). Compact before
        // serving so the first post-restart append starts from a bounded
        // snapshot. A failure here is tolerated: the replayed in-memory
        // state is correct, and the next append retries.
        if compaction_enabled && (journal.should_compact() || evicted_during_replay) {
            let _ = journal.compact();
        }
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

    /// Test-only fault injection (feature `test-faults`): let the next `n`
    /// appends succeed, then make one append write a short (partial) frame
    /// and fail, exercising the mid-write rollback. The fault disarms after
    /// firing so the caller's retry executes normally.
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn inject_write_failures_after(&self, n: u64) {
        self.fail_write_after_remaining.store(n, Ordering::SeqCst);
    }

    /// Test-only journal rollback (feature `test-faults`; P5 plan
    /// §5.4's witness-divergence injection): simulate a LOST journal
    /// write by truncating the log to before the last intent record —
    /// the dropped mutation's intent, its outcome and any checkpoint
    /// after it disappear, so the next open's replay derives the
    /// pre-mutation state while callers that already received the
    /// dropped mutation's response hold the newer view. Enforces the
    /// same single-writer discipline as [`Journal::open`] (the
    /// exclusive non-blocking `flock` proves no live writer holds the
    /// directory), reads the log with the replay decoder, truncates
    /// and fsyncs. **Never reachable from any production path**: no
    /// route, config or input takes a journal directory to roll back;
    /// only a test rig that constructed (and stopped) the witness owns
    /// the directory. Used by dependent crates' test harnesses to
    /// exercise the fail-closed lease/epoch checks over a stale
    /// authority view.
    ///
    /// # Errors
    /// Typed `INTERNAL` when the directory cannot be locked (a live
    /// writer holds it), the log cannot be read, or the truncation or
    /// its fsync fails.
    ///
    /// # Returns
    /// `Ok(Some(op_kind))` with the dropped intent's operation kind
    /// when a mutation was rolled back; `Ok(None)` when the log holds
    /// no intent record (nothing to drop — the rollback is a no-op).
    #[cfg(feature = "test-faults")]
    #[doc(hidden)]
    pub fn rollback_last_mutation(dir: impl AsRef<Path>) -> Result<Option<String>, ApiError> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir).map_err(io_err("failed to create journal directory"))?;
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
        let mut log = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&log_path)
            .map_err(io_err("failed to open journal log for rollback"))?;
        log.seek(SeekFrom::Start(0))
            .map_err(io_err("failed to seek journal log for rollback"))?;
        let mut data = Vec::new();
        log.read_to_end(&mut data)
            .map_err(io_err("failed to read journal log for rollback"))?;

        // Walk the same frame chain replay walks, remembering the byte
        // offset of every INTENT record; the rollback point is the
        // last one (a mutation is its intent plus everything after).
        let mut offset = 0usize;
        let mut sequence = 1u64;
        let mut last_intent: Option<(usize, String)> = None;
        while let Some(raw) = frame::decode_at(&data, offset, sequence) {
            let envelope = serde_json::from_slice::<Envelope>(raw.payload)
                .ok()
                .filter(|envelope| envelope.record_version == RECORD_VERSION);
            if let Some(envelope) = envelope {
                if let JournalRecord::Intent(intent) = envelope.record {
                    last_intent = Some((offset, intent.op_kind.clone()));
                }
            }
            sequence = sequence.saturating_add(1);
            offset = raw.end;
        }
        let Some((rollback_offset, op_kind)) = last_intent else {
            return Ok(None);
        };
        log.set_len(rollback_offset as u64)
            .map_err(io_err("failed to truncate journal log for rollback"))?;
        log.sync_all()
            .map_err(io_err("failed to fsync journal log after rollback"))?;
        Ok(Some(op_kind))
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
        // The operation was compacted away within the retained tombstone
        // window: it may have executed. Re-executing is forbidden, and an
        // unknown-again answer would invite exactly that, so fail closed.
        if self.is_tombstoned(&operation_id) {
            return Err(ApiError::operation_expired(&operation_id));
        }

        self.append(JournalRecord::Intent(Intent {
            operation_id: operation_id.clone(),
            request_hash,
            op_kind: op_kind.to_owned(),
            payload,
        }))?;
        let entry = self.operations.entry(operation_id).or_default();
        entry.request_hash = Some(request_hash);
        self.maybe_compact();
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
        // Never resurrect a compacted operation through an outcome-only
        // record: its intent is gone, so the pairing cannot be verified.
        if !self.operations.contains_key(&operation_id) && self.is_tombstoned(&operation_id) {
            return Err(ApiError::operation_expired(&operation_id));
        }
        self.append(JournalRecord::Outcome(Outcome {
            operation_id: operation_id.clone(),
            success,
            response: response.clone(),
            http_status,
        }))?;
        let entry = self.operations.entry(operation_id.clone()).or_default();
        let was_terminal = entry.outcome.is_some();
        entry.outcome = Some(RecordedOutcome {
            success,
            response,
            http_status,
        });
        if !was_terminal {
            self.terminal_order.push_back(operation_id);
        }
        self.maybe_compact();
        Ok(())
    }

    /// Append a checkpoint marker and fsync it. Carries no idempotency
    /// state; useful for operator annotations and journal smoke tests.
    pub fn append_checkpoint(&mut self, note: &str) -> Result<(), ApiError> {
        self.append(JournalRecord::Checkpoint(crate::record::Checkpoint {
            note: note.to_owned(),
        }))?;
        self.maybe_compact();
        Ok(())
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

    /// The active retention/compaction policy.
    #[must_use]
    pub fn retention(&self) -> JournalRetention {
        self.retention
    }

    /// Current log size in bytes (as tracked by appends and compactions).
    #[must_use]
    pub fn log_bytes(&self) -> u64 {
        self.log_bytes
    }

    /// Number of successful compactions since this journal was opened.
    #[must_use]
    pub fn compactions(&self) -> u64 {
        self.compactions
    }

    /// Number of failed compaction attempts since this journal was opened.
    ///
    /// A failed compaction never fails an already-fsynced append; the next
    /// append retries. A persistently non-zero value is an operational
    /// signal that the log is not being bounded.
    #[must_use]
    pub fn compaction_failures(&self) -> u64 {
        self.compaction_failures
    }

    /// The last compaction failure detail, if any (never contains secrets).
    #[must_use]
    pub fn last_compaction_error(&self) -> Option<&str> {
        self.last_compaction_error.as_deref()
    }

    /// Number of operations currently retained in the idempotency registry
    /// (terminal plus in-flight).
    #[must_use]
    pub fn retained_operations(&self) -> usize {
        self.operations.len()
    }

    /// Number of exact tombstone fingerprints currently retained.
    #[must_use]
    pub fn tombstone_count(&self) -> usize {
        self.tombstones.len()
    }

    /// Whether `operation_id` was evicted within the retained tombstone
    /// window. A collision (different id, same 64-bit fingerprint) also
    /// answers `true`; the caller must fail closed either way.
    ///
    /// This is an O(1) membership test against the tombstone ring's index, not
    /// a scan of the ring (which may hold `max_tombstones` entries).
    #[must_use]
    pub fn is_tombstoned(&self, operation_id: &OperationId) -> bool {
        self.tombstones.contains(fingerprint(operation_id))
    }

    /// Pin `operation_id` against compaction eviction (idempotency re-drive
    /// safety). Pins are counted; every call must be paired with
    /// [`Journal::unpin_operation`]. A pin for an unknown or already-evicted
    /// operation is harmless: compaction cannot evict what it does not hold.
    pub fn pin_operation(&mut self, operation_id: &OperationId) {
        *self
            .pinned_operations
            .entry(operation_id.clone())
            .or_insert(0) += 1;
    }

    /// Release one [`Journal::pin_operation`] pin.
    pub fn unpin_operation(&mut self, operation_id: &OperationId) {
        if let Some(count) = self.pinned_operations.get_mut(operation_id) {
            *count -= 1;
            if *count == 0 {
                self.pinned_operations.remove(operation_id);
            }
        }
    }

    /// Number of distinct operations currently pinned against eviction.
    #[must_use]
    pub fn pinned_operations(&self) -> usize {
        self.pinned_operations.len()
    }

    /// Whether the retention policy asks for a compaction now.
    fn should_compact(&self) -> bool {
        if !self.compaction_enabled {
            return false;
        }
        // A failed compaction must not be retried on every append: the same
        // threshold remains crossed (and `compactions == 0` still bypasses
        // the post-success growth floor), so without this gate an ENOSPC or
        // unopenable compaction target would re-clone and re-serialize the
        // whole registry under the writer lock after every record. Wait for
        // bounded new growth first; a transient failure therefore recovers
        // automatically, and the first attempt after the threshold was
        // crossed is still immediate (no retry is pending yet).
        if let Some(retry) = self.compaction_retry {
            if self.record_count < retry.at_records && self.log_bytes < retry.at_bytes {
                return false;
            }
        }
        // Before the first compaction, honor the configured thresholds
        // exactly: a pre-existing oversized log must compact immediately.
        // Afterwards require meaningful growth, so a threshold at or below
        // the compacted snapshot's own size cannot rewrite it on every
        // append (see `MIN_COMPACTION_*_GROWTH`).
        let records_since = self
            .record_count
            .saturating_sub(self.last_compaction_records);
        let bytes_since = self.log_bytes.saturating_sub(self.last_compaction_bytes);
        let record_floor = if self.compactions == 0 {
            self.retention.max_log_records
        } else {
            self.retention
                .max_log_records
                .max(MIN_COMPACTION_RECORD_GROWTH)
        };
        let record_trigger =
            self.record_count >= self.retention.max_log_records && records_since >= record_floor;
        let byte_trigger = self.retention.max_log_bytes.is_some_and(|max| {
            let floor = if self.compactions == 0 {
                max
            } else {
                max.max(MIN_COMPACTION_BYTE_GROWTH)
            };
            self.log_bytes >= max && bytes_since >= floor
        });
        // Hysteresis: retain `max_retained_operations` but only rewrite
        // when terminal state has grown to twice the bound, so a single
        // threshold crossing does not compact on every append. The growth
        // floor keeps a tiny configured bound (for example `1`) from
        // rewriting the whole snapshot on every terminal append; the
        // registry may then overshoot the bound by at most the floor
        // between compactions, and replay still evicts to the bound.
        let terminal_due = self.terminal_order.len()
            >= self.retention.max_retained_operations.saturating_mul(2)
            && (self.compactions == 0 || records_since >= MIN_COMPACTION_RECORD_GROWTH);
        record_trigger || byte_trigger || terminal_due
    }

    /// Compact after an append, best-effort. The append is already durable;
    /// a compaction failure must not turn it into a reported failure.
    fn maybe_compact(&mut self) {
        if self.should_compact() {
            let _ = self.compact();
        }
    }

    /// Rewrite the log as one [`Snapshot`] frame, atomically.
    ///
    /// The new file is written to `journal.log.compact`, fsynced, renamed
    /// over `journal.log`, and the directory is fsynced. The already-open
    /// temporary file handle becomes the append handle, so a rename cannot
    /// leave the journal writing to an unlinked inode. Until the rename
    /// succeeds, the old log and in-memory state are untouched; after it,
    /// the new snapshot fully describes the retained state, so a crash at
    /// any instant leaves either the old complete log or the new complete
    /// snapshot.
    fn compact(&mut self) -> Result<(), ApiError> {
        let (plan, payload) = match self.plan_compaction_within_frame_budget() {
            Ok(plan) => plan,
            Err(err) => return Err(self.record_compaction_failure(err.detail)),
        };
        // A compacted log always restarts its frame sequence at 1.
        let encoded = match frame::encode(1, &payload) {
            Ok(encoded) => encoded,
            Err(err) => return Err(self.record_compaction_failure(err.detail)),
        };

        let tmp_path = self.log_path.with_file_name("journal.log.compact");
        // `O_APPEND | O_TRUNC` is rejected by std, and this handle is the
        // only writer: it starts at end-of-file after the snapshot bytes.
        // Every later append therefore writes at the current cursor, and a
        // failed append's rollback must seek back to the frame boundary (see
        // `roll_back_failed_append`).
        let mut tmp_opts = OpenOptions::new();
        tmp_opts.read(true).write(true).create(true).truncate(true);
        #[cfg(unix)]
        tmp_opts.mode(0o600);
        let mut tmp = tmp_opts.open(&tmp_path).map_err(|err| {
            self.record_compaction_failure(format!(
                "failed to create compaction file {}: {err}",
                tmp_path.display()
            ))
        })?;
        if let Err(err) = tmp.write_all(&encoded).and_then(|()| tmp.sync_all()) {
            drop(tmp);
            let _ = std::fs::remove_file(&tmp_path);
            return Err(self.record_compaction_failure(format!(
                "failed to write/fsync compaction file {}: {err}",
                tmp_path.display()
            )));
        }
        if let Err(err) = std::fs::rename(&tmp_path, &self.log_path) {
            drop(tmp);
            let _ = std::fs::remove_file(&tmp_path);
            return Err(self.record_compaction_failure(format!(
                "failed to rename compaction file over {}: {err}",
                self.log_path.display()
            )));
        }
        // The rename is visible in-process; the new handle must replace the
        // old (now-unlinked) one before any later append.
        let dir_sync = self.sync_journal_dir();
        self.log = tmp;
        self.apply_compaction_plan(plan, encoded.len() as u64);
        if let Err(err) = dir_sync {
            // The snapshot is the live file, but its directory entry may not
            // survive a crash. Fail closed: every later append retries the
            // directory fsync before it is acknowledged.
            self.dir_sync_pending = true;
            return Err(self.record_compaction_failure(format!(
                "compaction was applied but fsyncing the journal directory failed: {}",
                err.detail
            )));
        }
        self.compactions = self.compactions.saturating_add(1);
        // A successful attempt ends the failure streak, so a later unrelated
        // failure starts its backoff from the prompt-retry step again.
        self.compaction_failure_streak = 0;
        self.compaction_retry = None;
        Ok(())
    }

    /// Apply the in-memory half of a compaction after its rename.
    fn apply_compaction_plan(&mut self, plan: CompactionPlan, log_bytes: u64) {
        let CompactionPlan {
            operations,
            terminal_order,
            tombstones,
            evicted,
            ..
        } = plan;
        self.operations = operations;
        self.terminal_order = terminal_order;
        self.tombstones = tombstones;
        self.evicted_operations = self.evicted_operations.saturating_add(evicted);
        self.next_sequence = 2;
        self.record_count = 1;
        self.log_bytes = log_bytes;
        self.last_compaction_records = self.record_count;
        self.last_compaction_bytes = self.log_bytes;
    }

    /// Fsync the directory containing the log, making a rename durable.
    fn sync_journal_dir(&self) -> Result<(), ApiError> {
        let dir = self.log_path.parent().ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "journal log {} has no parent directory to fsync",
                    self.log_path.display()
                ),
            )
        })?;
        File::open(dir)
            .and_then(|handle| handle.sync_all())
            .map_err(io_err("failed to fsync journal directory after compaction"))
    }

    /// Record a compaction failure, arm the bounded retry backoff and return
    /// the matching typed error.
    fn record_compaction_failure(&mut self, detail: impl Into<String>) -> ApiError {
        let detail = detail.into();
        self.compaction_failures = self.compaction_failures.saturating_add(1);
        self.last_compaction_error = Some(detail.clone());
        // The first failure retries after one record (so unblocking the
        // target and performing the next append still recovers promptly), the
        // next after two, then four, ... capped at 64 records or 64 MiB.
        // Measuring growth from the current counters also survives a failed
        // attempt that applied its snapshot before a directory-fsync failure
        // reset those counters.
        let shift = self
            .compaction_failure_streak
            .min(MAX_COMPACTION_RETRY_SHIFT);
        let record_growth = 1u64 << shift;
        let byte_growth = MIN_COMPACTION_BYTE_GROWTH.saturating_mul(record_growth);
        self.compaction_failure_streak = self.compaction_failure_streak.saturating_add(1);
        self.compaction_retry = Some(CompactionRetry {
            at_records: self.record_count.saturating_add(record_growth),
            at_bytes: self.log_bytes.saturating_add(byte_growth),
        });
        tracing::warn!(
            compaction_failures = self.compaction_failures,
            retry_at_records = self.compaction_retry.map(|retry| retry.at_records),
            retry_at_bytes = self.compaction_retry.map(|retry| retry.at_bytes),
            detail = %detail,
            "journal compaction failed; the log is not being bounded"
        );
        ApiError::new(
            ApiErrorCode::Internal,
            format!("journal compaction failed: {detail}"),
        )
    }

    /// Compute a compaction plan whose encoded snapshot fits one frame.
    ///
    /// Retention bounds are upper bounds, not guarantees: a single frame is
    /// the durability unit, so the planner evicts the oldest unpinned
    /// terminal operations (recording their fingerprints as tombstones) and
    /// then trims the tombstone ring until the snapshot fits. In-flight
    /// intents and pinned operations are irreducible; when they alone exceed
    /// the frame budget the planner fails closed rather than writing a
    /// snapshot that can never be framed.
    fn plan_compaction_within_frame_budget(&self) -> Result<(CompactionPlan, Vec<u8>), ApiError> {
        let mut keep_operations = self.retention.max_retained_operations;
        let mut keep_tombstones = self.retention.max_tombstones;
        // Each successful iteration evicts at least one operation or trims at
        // least one tombstone; the ceiling is defensive only.
        for _ in 0..64 {
            let plan = self.plan_compaction(keep_operations, keep_tombstones);
            let payload = encode_snapshot(&plan.snapshot)?;
            if payload.len() <= frame::MAX_PAYLOAD_LEN {
                return Ok((plan, payload));
            }
            let excess = payload.len() - frame::MAX_PAYLOAD_LEN;
            let pinned = &self.pinned_operations;
            // Evict oldest-first among unpinned terminal operations. This is
            // the safest lever: the evicted ids become tombstones, so a retry
            // still fails closed with OPERATION_EXPIRED.
            let mut freed = 0usize;
            let mut evict = 0usize;
            let tombstones_have_room = plan.tombstones.len() < keep_tombstones;
            for operation in plan.snapshot.operations.iter().filter(|operation| {
                operation.outcome.is_some() && !pinned.contains_key(&operation.operation_id)
            }) {
                let size = serde_json::to_vec(operation).map_or(0, |bytes| bytes.len());
                // Removing the entry also removes a separator; a new
                // tombstone is at most 21 bytes (20 decimal digits + comma)
                // unless the ring is already full (then it displaces one).
                freed = freed.saturating_add(size.saturating_add(1));
                if tombstones_have_room {
                    freed = freed.saturating_sub(21);
                }
                evict = evict.saturating_add(1);
                if freed >= excess {
                    break;
                }
            }
            if evict > 0 {
                keep_operations = plan.terminal_order.len().saturating_sub(evict);
                continue;
            }
            if plan.tombstones.is_empty() {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "journal compaction snapshot cannot fit the {} byte frame limit: \
                         {} in-flight/pinned operations and {} tombstones remain after \
                         evicting every evictable terminal operation; refusing to write \
                         a snapshot that can never be framed",
                        frame::MAX_PAYLOAD_LEN,
                        plan.snapshot.operations.len(),
                        plan.tombstones.len()
                    ),
                ));
            }
            // The irreducible entries still leave tombstone bytes over the
            // budget. Trim the oldest fingerprints; the retention window is
            // correspondingly shorter (documented in the retention contract).
            let mut freed = 0usize;
            let mut trim = 0usize;
            for value in &plan.snapshot.tombstones {
                freed = freed.saturating_add(value.to_string().len().saturating_add(1));
                trim = trim.saturating_add(1);
                if freed >= excess {
                    break;
                }
            }
            let trim = trim.max(1).min(plan.tombstones.len());
            keep_tombstones = plan.tombstones.len().saturating_sub(trim);
        }
        Err(ApiError::new(
            ApiErrorCode::Internal,
            "journal compaction did not converge within the frame payload budget",
        ))
    }

    /// Compute the snapshot and the retained in-memory state without
    /// mutating anything; the caller applies it only after the rename.
    ///
    /// `keep_operations` and `keep_tombstones` are the effective retention
    /// bounds for this attempt (the configured values, possibly reduced to
    /// fit one frame).
    fn plan_compaction(&self, keep_operations: usize, keep_tombstones: usize) -> CompactionPlan {
        let mut operations = self.operations.clone();
        // Terminal operations in terminal order, then any terminal entry a
        // corrupt/legacy state did not order (sorted for determinism). The
        // `HashSet` keeps this O(n log n): the retention bound is tens of
        // thousands of entries, and the previous linear `contains` scan was
        // quadratic under the writer lock.
        let mut terminal: VecDeque<OperationId> = VecDeque::new();
        let mut seen: HashSet<OperationId> = HashSet::new();
        for id in &self.terminal_order {
            if operations
                .get(id)
                .is_some_and(|state| state.outcome.is_some())
                && seen.insert(id.clone())
            {
                terminal.push_back(id.clone());
            }
        }
        let mut unordered: Vec<OperationId> = operations
            .iter()
            .filter(|(id, state)| state.outcome.is_some() && !seen.contains(*id))
            .map(|(id, _)| id.clone())
            .collect();
        unordered.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        terminal.extend(unordered);

        let mut tombstones = self.tombstones.clone();
        tombstones.trim(keep_tombstones);
        let mut evicted = 0u64;
        let mut over = terminal.len().saturating_sub(keep_operations);
        if over > 0 {
            // Evict oldest-first among unpinned entries. A pinned operation is
            // one an executing request is re-driving; it must stay available
            // until that request journals its replacement outcome. If every
            // remaining entry is pinned, the bound is exceeded only until
            // those requests finish (bounded by the concurrency cap).
            let mut kept = VecDeque::with_capacity(terminal.len());
            while let Some(id) = terminal.pop_front() {
                if over > 0 && !self.pinned_operations.contains_key(&id) {
                    operations.remove(&id);
                    tombstones.push(fingerprint(&id), keep_tombstones);
                    evicted = evicted.saturating_add(1);
                    over -= 1;
                } else {
                    kept.push_back(id);
                }
            }
            terminal = kept;
        }

        let mut snapshot_operations = Vec::with_capacity(operations.len());
        for id in &terminal {
            if let Some(state) = operations.get(id) {
                snapshot_operations.push(snapshot_operation(id, state));
            }
        }
        let mut in_flight: Vec<OperationId> = operations
            .iter()
            .filter(|(_, state)| state.outcome.is_none())
            .map(|(id, _)| id.clone())
            .collect();
        // Deterministic snapshot bytes across restarts (HashMap iteration is
        // not stable). Order is not semantically meaningful for in-flight
        // entries; every one is retained.
        in_flight.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        for id in &in_flight {
            if let Some(state) = operations.get(id) {
                snapshot_operations.push(snapshot_operation(id, state));
            }
        }

        CompactionPlan {
            snapshot: Snapshot {
                operations: snapshot_operations,
                tombstones: tombstones.iter().copied().collect(),
                evicted_operations: self.evicted_operations.saturating_add(evicted),
            },
            operations,
            terminal_order: terminal,
            tombstones,
            evicted,
        }
    }

    /// Serialize, frame, write and fsync one record. Acknowledged only
    /// after the data reaches stable storage.
    fn append(&mut self, record: JournalRecord) -> Result<(), ApiError> {
        if let Some(detail) = &self.poisoned {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "journal {} cannot append after an unrecoverable write \
                     failure ({detail}); reopen it before retrying",
                    self.log_path.display()
                ),
            ));
        }
        // A previous compaction renamed the log but could not fsync the
        // directory. Make the rename durable before accepting (and
        // acknowledging) any new record; otherwise a crash could revert to
        // the old log and lose post-compaction records that were fsynced to
        // the new inode.
        if self.dir_sync_pending {
            self.sync_journal_dir()?;
            self.dir_sync_pending = false;
        }
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
        let frame_len = frame.len() as u64;
        let start_bytes = self.log_bytes;
        #[cfg(feature = "test-faults")]
        let write_result = {
            let remaining = self.fail_write_after_remaining.load(Ordering::SeqCst);
            if remaining == u64::MAX {
                self.log.write_all(&frame)
            } else if remaining == 0 {
                self.fail_write_after_remaining
                    .store(u64::MAX, Ordering::SeqCst);
                let cut = frame.len().min(16);
                let mut result = self.log.write_all(&frame[..cut]);
                if result.is_ok() {
                    result = Err(std::io::Error::other(
                        "injected journal write failure (test-faults)",
                    ));
                }
                result
            } else {
                self.fail_write_after_remaining
                    .store(remaining - 1, Ordering::SeqCst);
                self.log.write_all(&frame)
            }
        };
        #[cfg(not(feature = "test-faults"))]
        let write_result = self.log.write_all(&frame);
        if let Err(error) = write_result {
            // A partial frame at the tail would make replay stop before any
            // later (valid) append. Roll it back; if even that fails, the
            // journal is unusable until reopened.
            if !self.roll_back_failed_append(start_bytes) {
                let detail = format!("failed to write journal record: {error}");
                self.poisoned = Some(detail.clone());
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!("{detail}; the partial frame could not be truncated"),
                ));
            }
            return Err(io_err("failed to write journal record")(error));
        }
        // The bytes are in the file even if the fsync below fails. Advance
        // the sequence and byte counters before the fsync so a later append
        // can never reuse a sequence number (replay would treat the
        // surviving frame as a torn tail and discard everything after it).
        let start_sequence = self.next_sequence;
        let start_records = self.record_count;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.record_count = self.record_count.saturating_add(1);
        self.log_bytes = self.log_bytes.saturating_add(frame_len);
        if let Err(error) = self.log.sync_all() {
            // The record was written but not durably acknowledged. Truncate
            // it back when possible so in-memory state and the log agree
            // and the caller's retry executes from a clean frame boundary;
            // if the rollback fails the counters above already match the
            // bytes left on disk.
            if !self.roll_back_failed_append(start_bytes) {
                let detail = format!("failed to fsync journal record: {error}");
                self.poisoned = Some(detail.clone());
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!("{detail}; the unacknowledged frame could not be truncated"),
                ));
            }
            // The record is gone, so its counters must go with it.
            self.next_sequence = start_sequence;
            self.record_count = start_records;
            self.log_bytes = start_bytes;
            return Err(io_err("failed to fsync journal record")(error));
        }
        Ok(())
    }

    /// Truncate the log back to `start` (the last acknowledged frame
    /// boundary) after a write/fsync failure. Returns whether the rollback
    /// (including its own fsync) completed; on failure [`Journal::log_bytes`]
    /// is refreshed from the file so it never under-counts bytes that remain
    /// on disk.
    ///
    /// The seek is required, not cosmetic: the handle adopted after a
    /// compaction is not `O_APPEND`, so `set_len` alone leaves the cursor
    /// past the truncated EOF and the next write would create a zero-filled
    /// hole that replay mistakes for a torn tail.
    fn roll_back_failed_append(&mut self, start: u64) -> bool {
        let rolled_back = self.log.set_len(start).is_ok()
            && self.log.seek(SeekFrom::Start(start)).is_ok()
            && self.log.sync_all().is_ok();
        self.log_bytes = if rolled_back {
            start
        } else {
            self.log
                .metadata()
                .map_or(self.log_bytes.max(start), |metadata| metadata.len())
        };
        rolled_back
    }
}

/// A computed compaction result, applied only after the snapshot rename.
struct CompactionPlan {
    snapshot: Snapshot,
    operations: HashMap<OperationId, OperationState>,
    terminal_order: VecDeque<OperationId>,
    tombstones: TombstoneRing,
    evicted: u64,
}

/// Stable 64-bit fingerprint of an operation id (first eight SHA-256 bytes,
/// little-endian). Persisted in snapshots; changing this function is a
/// journal schema change.
fn fingerprint(operation_id: &OperationId) -> u64 {
    let digest = Sha256::digest(operation_id.as_str().as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_le_bytes(bytes)
}

/// Bounded, oldest-first ring of evicted-operation fingerprints plus an O(1)
/// membership index.
///
/// The ring (`order`) is what compaction serializes, so its order is the
/// durable retention window. The index mirrors the ring so
/// [`Journal::is_tombstoned`] never scans up to `max_tombstones` entries on a
/// first-seen append or on every replayed record. Each fingerprint appears at
/// most once: re-inserting an existing fingerprint refreshes it to the newest
/// position, because the newest eviction is what the window must retain. That
/// keeps both structures consistent when an operation is re-admitted after a
/// provisional replay eviction and later evicted again.
#[derive(Clone, Debug, Default)]
struct TombstoneRing {
    /// Fingerprints in eviction order, oldest first; the serialized order.
    order: VecDeque<u64>,
    /// Exactly the values in `order` (set membership is O(1)).
    index: HashSet<u64>,
}

impl TombstoneRing {
    /// Build a ring from oldest-first fingerprints, applying `capacity` with
    /// the same refresh-on-duplicate semantics as [`TombstoneRing::push`].
    fn from_oldest_first(values: impl IntoIterator<Item = u64>, capacity: usize) -> Self {
        let mut ring = Self::default();
        for value in values {
            ring.push(value, capacity);
        }
        ring
    }

    fn len(&self) -> usize {
        self.order.len()
    }

    fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    fn contains(&self, value: u64) -> bool {
        self.index.contains(&value)
    }

    fn iter(&self) -> impl Iterator<Item = &u64> {
        self.order.iter()
    }

    /// Insert/fingerprint-refresh at the newest position, then trim to
    /// `capacity`. A zero capacity retains nothing.
    fn push(&mut self, value: u64, capacity: usize) {
        if capacity == 0 {
            return;
        }
        if self.index.contains(&value) {
            if let Some(position) = self.order.iter().position(|stored| *stored == value) {
                self.order.remove(position);
            }
        }
        self.order.push_back(value);
        self.index.insert(value);
        self.trim(capacity);
    }

    /// Drop a fingerprint from both structures (runtime re-admission after a
    /// provisional replay-window eviction). Returns whether it was present.
    fn remove(&mut self, value: u64) -> bool {
        if !self.index.remove(&value) {
            return false;
        }
        if let Some(position) = self.order.iter().position(|stored| *stored == value) {
            self.order.remove(position);
        }
        true
    }

    /// Keep only the newest `capacity` fingerprints.
    fn trim(&mut self, capacity: usize) {
        while self.order.len() > capacity {
            if let Some(value) = self.order.pop_front() {
                self.index.remove(&value);
            }
        }
    }
}

fn snapshot_operation(operation_id: &OperationId, state: &OperationState) -> SnapshotOperation {
    SnapshotOperation {
        operation_id: operation_id.clone(),
        request_hash: state.request_hash,
        outcome: state.outcome.as_ref().map(|outcome| SnapshotOutcome {
            success: outcome.success,
            response: outcome.response.clone(),
            http_status: outcome.http_status,
        }),
    }
}

/// Serialize a compaction snapshot as one record envelope (one frame
/// payload). The caller checks it against [`frame::MAX_PAYLOAD_LEN`].
fn encode_snapshot(snapshot: &Snapshot) -> Result<Vec<u8>, ApiError> {
    let envelope = Envelope {
        record_version: RECORD_VERSION,
        record: JournalRecord::Snapshot(snapshot.clone()),
    };
    serde_json::to_vec(&envelope).map_err(|err| {
        ApiError::new(
            ApiErrorCode::Internal,
            format!("failed to serialize journal compaction snapshot: {err}"),
        )
    })
}

impl Drop for Journal {
    fn drop(&mut self) {
        // Best-effort explicit release; closing the fd releases the flock
        // regardless, so errors are deliberately ignored here.
        let _ = rustix::fs::flock(&self.lock_file, rustix::fs::FlockOperation::Unlock);
    }
}

/// Replay the log file, rebuilding the idempotency registry and truncating
/// a structurally torn tail.
///
/// Frames are streamed one at a time (issue #64): the log is never buffered
/// whole. With `compaction_enabled`, terminal operations beyond the retention
/// bound are evicted into the tombstone ring as replay proceeds, so restart
/// memory is bounded too; `evicted_during_replay` tells the caller to write a
/// snapshot before serving.
///
/// A frame that decodes structurally but whose payload is undecodable, has
/// unknown fields, or declares an unsupported record version is **not** a
/// torn tail: replay fails typed and leaves the log byte-for-byte intact.
fn replay(
    log: &mut File,
    log_path: &Path,
    collect_records: bool,
    retention: &JournalRetention,
    compaction_enabled: bool,
) -> Result<Replayed, ApiError> {
    log.seek(SeekFrom::Start(0))
        .map_err(io_err("failed to seek journal log for replay"))?;
    let data_len = log
        .metadata()
        .map_err(io_err("failed to stat journal log for replay"))?
        .len();
    let mut reader = BufReader::new(
        log.try_clone()
            .map_err(io_err("failed to clone journal log for replay"))?,
    );

    let mut replayed = Replayed {
        operations: HashMap::new(),
        terminal_order: VecDeque::new(),
        tombstones: TombstoneRing::default(),
        snapshot_tombstones: HashSet::new(),
        evicted_operations: 0,
        next_sequence: 1,
        record_count: 0,
        records: Vec::new(),
        evicted_during_replay: false,
    };
    let mut offset = 0u64;
    while let Some(raw) = frame::read_frame(&mut reader, replayed.next_sequence)? {
        let probe: RecordVersionProbe = serde_json::from_slice(&raw.payload).map_err(|err| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "journal log {} has a complete CRC-valid frame at sequence {} whose \
                         payload has no decodable record_version: {err}; refusing to \
                         truncate durable records",
                    log_path.display(),
                    raw.sequence
                ),
            )
        })?;
        ensure_supported_schema_version(
            "journal record",
            log_path,
            probe.record_version,
            RECORD_VERSION,
        )?;
        let envelope = serde_json::from_slice::<Envelope>(&raw.payload).map_err(|err| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "journal log {} has a complete CRC-valid frame at sequence {} whose \
                     payload does not decode at record_version {}: {err}; refusing to \
                     truncate durable records (unknown fields are rejected by policy)",
                    log_path.display(),
                    raw.sequence,
                    RECORD_VERSION
                ),
            )
        })?;
        if collect_records && matches!(envelope.record, JournalRecord::Snapshot(_)) {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "journal log {} is compacted (it contains a snapshot record), but this \
                     consumer folds the complete record history and cannot derive state \
                     from a truncated prefix; refusing to open it",
                    log_path.display()
                ),
            ));
        }
        if collect_records {
            replayed.records.push(envelope.record.clone());
        }
        apply_record(
            &mut replayed,
            envelope.record,
            log_path,
            retention,
            compaction_enabled,
        )?;
        replayed.next_sequence = raw.sequence.saturating_add(1);
        replayed.record_count = replayed.record_count.saturating_add(1);
        offset = offset.saturating_add(raw.frame_len as u64);
    }
    drop(reader);

    if offset < data_len {
        // Torn tail after a crash: expected, never an error. Truncate back
        // to the last valid record so future appends start at a clean
        // boundary, and make the truncation durable.
        log.set_len(offset)
            .map_err(io_err("failed to truncate torn journal tail"))?;
        log.sync_all()
            .map_err(io_err("failed to fsync journal after tail truncation"))?;
    }
    Ok(replayed)
}

/// Fold one replayed record into the registry state.
fn apply_record(
    replayed: &mut Replayed,
    record: JournalRecord,
    log_path: &Path,
    retention: &JournalRetention,
    compaction_enabled: bool,
) -> Result<(), ApiError> {
    match record {
        JournalRecord::Intent(intent) => {
            if !replayed.operations.contains_key(&intent.operation_id) {
                let fp = fingerprint(&intent.operation_id);
                if replayed.tombstones.contains(fp) {
                    if replayed.snapshot_tombstones.contains(&fp) {
                        return Err(ApiError::new(
                            ApiErrorCode::Internal,
                            format!(
                                "journal log {} has an intent for operation {} after a \
                                 durable compaction snapshot evicted it; the log is \
                                 inconsistent and cannot be replayed safely",
                                log_path.display(),
                                intent.operation_id
                            ),
                        ));
                    }
                    // A provisional replay-window eviction, not a durable
                    // snapshot decision: this later intent is still inside
                    // the log, so re-admit it and drop the provisional
                    // fingerprint. The 64-bit fingerprint is an
                    // accepted-risk index: a collision can also drop a
                    // different provisional entry, which then becomes
                    // indistinguishable from a new operation and may be
                    // executed again. Durable snapshot tombstones are never
                    // cleared by this path (a collision that hits one is a
                    // fail-closed refusal).
                    replayed.tombstones.remove(fp);
                }
            }
            let entry = replayed.operations.entry(intent.operation_id).or_default();
            if entry.request_hash.is_none() {
                entry.request_hash = Some(intent.request_hash);
            }
        }
        JournalRecord::Outcome(outcome) => {
            if !replayed.operations.contains_key(&outcome.operation_id) {
                let fp = fingerprint(&outcome.operation_id);
                if replayed.tombstones.contains(fp) {
                    if replayed.snapshot_tombstones.contains(&fp) {
                        return Err(ApiError::new(
                            ApiErrorCode::Internal,
                            format!(
                                "journal log {} has an outcome for operation {} after a \
                                 durable compaction snapshot evicted it; refusing to \
                                 resurrect it",
                                log_path.display(),
                                outcome.operation_id
                            ),
                        ));
                    }
                    // The operation was only evicted by bounded replay, and
                    // this later outcome is still inside the same log: a
                    // re-drive is legal (the API journals a replacement
                    // outcome when it supersedes a recorded failure). Re-admit
                    // it as an outcome-only entry; its intent is no longer in
                    // the registry, so `lookup` returns `None` and any later
                    // intent fails closed as an idempotency conflict rather
                    // than re-executing the mutation. The 64-bit fingerprint
                    // is an accepted-risk index: a collision can also drop a
                    // different provisional entry, which may then be admitted
                    // again and re-executed. Durable snapshot tombstones are
                    // never cleared.
                    replayed.tombstones.remove(fp);
                }
            }
            let entry = replayed
                .operations
                .entry(outcome.operation_id.clone())
                .or_default();
            let was_terminal = entry.outcome.is_some();
            entry.outcome = Some(RecordedOutcome {
                success: outcome.success,
                response: outcome.response,
                http_status: outcome.http_status,
            });
            if !was_terminal {
                replayed.terminal_order.push_back(outcome.operation_id);
            }
        }
        JournalRecord::Snapshot(snapshot) => {
            if replayed.record_count != 0 {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "journal log {} contains a compaction snapshot after other \
                         records; a snapshot must be the first and only base record",
                        log_path.display()
                    ),
                ));
            }
            apply_snapshot(replayed, snapshot, log_path, retention)?;
        }
        JournalRecord::Checkpoint(_) => {}
    }

    if compaction_enabled {
        // Bound replay memory: evict the oldest terminal operations as they
        // become eligible. The tombstone ring is trimmed to its configured
        // capacity as it grows; anything older than the ring is the documented
        // beyond-window case, so holding it would only inflate restart
        // memory. The snapshot written before serving keeps the same newest
        // fingerprints.
        while replayed.terminal_order.len() > retention.max_retained_operations {
            let Some(id) = replayed.terminal_order.pop_front() else {
                break;
            };
            replayed.operations.remove(&id);
            replayed
                .tombstones
                .push(fingerprint(&id), retention.max_tombstones);
            replayed.evicted_operations = replayed.evicted_operations.saturating_add(1);
            replayed.evicted_during_replay = true;
        }
    }
    Ok(())
}

/// Install a compaction snapshot as the replay base.
fn apply_snapshot(
    replayed: &mut Replayed,
    snapshot: Snapshot,
    log_path: &Path,
    retention: &JournalRetention,
) -> Result<(), ApiError> {
    replayed.operations.clear();
    replayed.terminal_order.clear();
    // Build the order + index together, keeping the newest fingerprints if
    // the configured ring shrank and deduplicating any legacy snapshot that
    // stored the same fingerprint twice.
    replayed.tombstones =
        TombstoneRing::from_oldest_first(snapshot.tombstones, retention.max_tombstones);
    replayed.snapshot_tombstones = replayed.tombstones.iter().copied().collect();
    replayed.evicted_operations = snapshot.evicted_operations;

    for operation in snapshot.operations {
        if replayed.operations.contains_key(&operation.operation_id) {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "journal log {} has a compaction snapshot with duplicate operation \
                     {}; refusing to replay it",
                    log_path.display(),
                    operation.operation_id
                ),
            ));
        }
        let terminal = operation.outcome.is_some();
        replayed.operations.insert(
            operation.operation_id.clone(),
            OperationState {
                request_hash: operation.request_hash,
                outcome: operation.outcome.map(|outcome| RecordedOutcome {
                    success: outcome.success,
                    response: outcome.response,
                    http_status: outcome.http_status,
                }),
            },
        );
        if terminal {
            replayed.terminal_order.push_back(operation.operation_id);
        }
    }
    Ok(())
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
    fn unknown_record_version_refuses_open_and_preserves_the_log() {
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

        let err = Journal::open(dir.path()).expect_err("future version must refuse open");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(err.is_unsupported_schema_version(), "{err}");
        assert_eq!(
            std::fs::metadata(&log_path).expect("stat").len(),
            u64::try_from(frame.len()).expect("frame length"),
            "a complete future-version frame is never truncated away"
        );
    }

    // -----------------------------------------------------------------
    // Golden historical fixture: a record_version 1 journal written before
    // `http_status` existed. There is no N-2 format (record_version has
    // been 1 since the journal was introduced); see
    // docs/schema-compatibility.md.
    // -----------------------------------------------------------------

    const N1_FIXTURE: &[u8] = include_bytes!("../../../tests/fixtures/schema/journal-v1-n1.log");

    fn write_journal_fixture(dir: &tempfile::TempDir) -> PathBuf {
        let path = dir.path().join(JOURNAL_LOG_FILE);
        std::fs::write(&path, N1_FIXTURE).expect("write fixture");
        path
    }

    #[test]
    fn n_minus_one_fixture_replays_and_defaults_absent_http_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_journal_fixture(&dir);
        let mut journal = Journal::open(dir.path()).expect("fixture opens");
        assert_eq!(journal.record_count(), 3, "intent + outcome + checkpoint");

        let id = op_id("op-upgrade-journal-n1");
        let entry = journal.lookup(&id).expect("registry entry");
        assert_eq!(entry.request_hash, [7; 32]);
        let outcome = entry.outcome.expect("outcome record");
        assert!(outcome.success);
        assert_eq!(outcome.http_status, None, "pre-http-status outcome");
        assert_eq!(
            outcome.response,
            serde_json::json!({"volume_id": "vol-upgrade-journal", "generation": 1})
        );

        // Idempotent replay returns the recorded outcome and appends nothing.
        let replayed = journal
            .append_intent(
                id.clone(),
                [7; 32],
                "create_volume",
                serde_json::json!({
                    "volume_id": "vol-upgrade-journal",
                    "size_bytes": 1_048_576
                }),
            )
            .expect("replay");
        assert_eq!(
            replayed,
            IntentAppend::Replayed {
                success: true,
                response: serde_json::json!({
                    "volume_id": "vol-upgrade-journal",
                    "generation": 1
                }),
                http_status: None,
            }
        );
        assert_eq!(journal.record_count(), 3, "replay writes no record");

        // The log remains appendable after replay, and the upgrade is inert
        // (the replay is idempotent across a second crash/restart too).
        journal
            .append_checkpoint("after historical replay")
            .expect("append after replay");
        drop(journal);
        let journal = Journal::open(dir.path()).expect("reopen");
        assert_eq!(journal.record_count(), 4);
        assert_eq!(
            journal.lookup(&id).expect("entry after reopen").outcome,
            Some(RecordedOutcome {
                success: true,
                response: serde_json::json!({
                    "volume_id": "vol-upgrade-journal",
                    "generation": 1
                }),
                http_status: None,
            })
        );
    }

    #[test]
    fn complete_frame_with_unknown_field_refuses_open_and_preserves_the_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log_path = dir.path().join(JOURNAL_LOG_FILE);
        // A supported record_version carrying a field this build does not
        // know: silent field loss is forbidden, so open refuses.
        let payload = br#"{"record_version":1,"record":{"record_type":"checkpoint","note":"n","surprise":true}}"#;
        let frame = frame::encode(1, payload).expect("encode");
        std::fs::write(&log_path, &frame).expect("write");

        let err = Journal::open(dir.path()).expect_err("unknown field must refuse open");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(!err.is_unsupported_schema_version(), "{err}");
        assert!(err.detail.contains("unknown fields are rejected"), "{err}");
        assert_eq!(
            std::fs::metadata(&log_path).expect("stat").len(),
            u64::try_from(frame.len()).expect("frame length"),
            "a complete unknown-field frame is never truncated away"
        );
    }

    #[test]
    fn future_version_with_unknown_fields_reports_the_version_not_a_parse_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log_path = dir.path().join(JOURNAL_LOG_FILE);
        let payload = br#"{"record_version":3,"future_field":true,"record":{"record_type":"checkpoint","note":"n"}}"#;
        let frame = frame::encode(1, payload).expect("encode");
        std::fs::write(&log_path, &frame).expect("write");

        let err = Journal::open(dir.path()).expect_err("future version must refuse open");
        assert!(err.is_unsupported_schema_version(), "{err}");
        assert!(err.detail.contains("schema_version 3"), "{err}");
        assert_eq!(
            std::fs::metadata(&log_path).expect("stat").len(),
            u64::try_from(frame.len()).expect("frame length")
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

    #[test]
    fn a_rolled_back_append_after_compaction_resumes_at_the_frame_boundary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut journal =
            Journal::open_with_retention(dir.path(), tight_retention()).expect("open");
        let first = op_id("op-before-rollback");
        let target = op_id("op-after-rollback");
        complete(&mut journal, &first, [1; 32], serde_json::json!({}));

        // Compaction adopts a non-O_APPEND handle. Simulate the write half
        // of a failing append: the cursor advances past the frame boundary.
        journal.compact().expect("compact");
        let start = journal.log_bytes();
        journal
            .log
            .write_all(b"partial frame bytes")
            .expect("simulate a partial write");
        assert!(journal.roll_back_failed_append(start));

        // The retry must land at `start`, not after a zero-filled hole.
        complete(&mut journal, &target, [7; 32], serde_json::json!({"v": 1}));
        assert_eq!(
            journal.log_bytes(),
            journal.log.metadata().expect("log metadata").len(),
            "the tracked size must match the file after a rollback"
        );

        drop(journal);
        let journal = Journal::open_with_retention(dir.path(), tight_retention()).expect("reopen");
        assert!(
            journal.lookup(&target).is_some(),
            "the retried record must replay after a rolled-back append"
        );
    }

    #[cfg(feature = "test-faults")]
    #[test]
    fn an_injected_mid_write_failure_rolls_back_and_the_retry_replays() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut journal =
            Journal::open_with_retention(dir.path(), tight_retention()).expect("open");
        complete(
            &mut journal,
            &op_id("op-before-injected-write"),
            [1; 32],
            serde_json::json!({}),
        );
        journal.compact().expect("compact");
        journal.inject_write_failures_after(0);

        let target = op_id("op-injected-write-retry");
        let error = journal
            .append_intent(
                target.clone(),
                [7; 32],
                "create_volume",
                serde_json::json!({}),
            )
            .expect_err("the partial write fails");
        assert_eq!(error.code, ApiErrorCode::Internal);
        assert!(error.detail.contains("injected journal write failure"));

        complete(
            &mut journal,
            &target,
            [7; 32],
            serde_json::json!({"ok": true}),
        );
        assert_eq!(
            journal.log_bytes(),
            journal.log.metadata().expect("log metadata").len(),
            "the rollback and retry leave no hole"
        );

        drop(journal);
        let journal = Journal::open_with_retention(dir.path(), tight_retention()).expect("reopen");
        let entry = journal.lookup(&target).expect("retried record replayed");
        assert!(entry.outcome.expect("outcome").success);
    }

    // -----------------------------------------------------------------
    // Issue #64: bounded retention, compaction, tombstones and the
    // OPERATION_EXPIRED fail-closed contract.
    // -----------------------------------------------------------------

    fn tight_retention() -> JournalRetention {
        JournalRetention {
            max_retained_operations: 1,
            max_tombstones: 8,
            max_log_records: 1_000,
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
    fn compaction_bounds_the_log_and_expires_evicted_operations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = op_id("op-old-terminal");
        let second = op_id("op-new-terminal");
        let in_flight = op_id("op-in-flight");
        let mut journal =
            Journal::open_with_retention(dir.path(), tight_retention()).expect("open");

        complete(&mut journal, &first, [1; 32], serde_json::json!({"v": 1}));
        journal
            .append_intent(
                in_flight.clone(),
                [3; 32],
                "attach_volume",
                serde_json::json!({}),
            )
            .expect("in-flight intent");
        complete(&mut journal, &second, [2; 32], serde_json::json!({"v": 2}));

        assert!(
            journal.compactions() >= 1,
            "the retention bound must trigger a compaction (failures={}, last={:?}, terminal-retained={})",
            journal.compaction_failures(),
            journal.last_compaction_error(),
            journal.retained_operations()
        );
        assert!(
            journal.log_bytes() < 4_096,
            "compaction bounds the on-disk log: {} bytes",
            journal.log_bytes()
        );
        assert!(
            journal.record_count() <= 4,
            "the compacted log starts from one snapshot frame: {}",
            journal.record_count()
        );

        // The old terminal operation is gone from the registry but its
        // tombstone refuses the retry with the typed fail-closed code.
        assert!(journal.lookup(&first).is_none());
        let err = journal
            .append_intent(
                first.clone(),
                [1; 32],
                "create_volume",
                serde_json::json!({}),
            )
            .expect_err("an evicted operation must never re-execute");
        assert_eq!(err.code, ApiErrorCode::OperationExpired);
        assert_eq!(err.http_status(), 410);

        // The newest terminal operation replays its outcome; the in-flight
        // intent is retained and still reports in-doubt, not expired.
        let entry = journal.lookup(&second).expect("newest retained");
        assert_eq!(
            entry.outcome.expect("outcome").response,
            serde_json::json!({"v": 2})
        );
        let pending = journal.lookup(&in_flight).expect("in-flight retained");
        assert!(!pending.has_outcome);
        assert!(matches!(
            journal
                .append_intent(
                    in_flight.clone(),
                    [3; 32],
                    "attach_volume",
                    serde_json::json!({}),
                )
                .expect("in-flight replay"),
            IntentAppend::AlreadyInFlight
        ));

        drop(journal);
        // The snapshot is the durable post-compaction state: reopen and
        // repeat the same guarantees.
        let mut journal =
            Journal::open_with_retention(dir.path(), tight_retention()).expect("reopen");
        let err = journal
            .append_intent(first, [1; 32], "create_volume", serde_json::json!({}))
            .expect_err("tombstone survives the snapshot");
        assert_eq!(err.code, ApiErrorCode::OperationExpired);
        assert_eq!(
            journal
                .lookup(&second)
                .expect("retained outcome")
                .outcome
                .expect("outcome")
                .response,
            serde_json::json!({"v": 2})
        );
        assert!(
            !journal
                .lookup(&in_flight)
                .expect("retained in-flight")
                .has_outcome
        );
    }

    #[test]
    fn the_tombstone_window_is_bounded_and_its_overflow_is_the_documented_contract() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ids: Vec<OperationId> = (1..=4).map(|n| op_id(&format!("op-{n}"))).collect();
        let mut journal = Journal::open_with_retention(
            dir.path(),
            JournalRetention {
                max_retained_operations: 1,
                max_tombstones: 2,
                max_log_records: 1_000,
                max_log_bytes: None,
            },
        )
        .expect("open");
        for (index, id) in ids.iter().enumerate() {
            complete(
                &mut journal,
                id,
                [u8::try_from(index + 1).expect("small index"); 32],
                serde_json::json!({"n": index}),
            );
        }
        // The retention hysteresis has a growth floor, so force the
        // compaction this test is about rather than relying on a
        // pathological one-record cadence.
        journal.compact().expect("compact");

        // Four terminal operations, retaining one, with a two-entry ring:
        // the two newest evictions are detectable and the oldest is not.
        assert_eq!(journal.tombstone_count(), 2);
        assert!(!journal.is_tombstoned(&ids[0]));
        assert!(journal.is_tombstoned(&ids[1]));
        assert!(journal.is_tombstoned(&ids[2]));
        assert!(journal.lookup(&ids[3]).is_some());

        // Documented (and dangerous) beyond-window behavior: the oldest
        // operation is indistinguishable from a new one. Operators must
        // size the window above the client retry horizon.
        assert!(matches!(
            journal
                .append_intent(
                    ids[0].clone(),
                    [1; 32],
                    "create_volume",
                    serde_json::json!({}),
                )
                .expect("beyond-window retry is treated as new"),
            IntentAppend::New
        ));
    }

    #[test]
    fn tombstone_ring_deduplicates_duplicate_insertions_and_refreshes_their_position() {
        let mut ring = TombstoneRing::default();
        ring.push(7, 3);
        ring.push(8, 3);
        // A duplicate occupies exactly one slot and becomes the newest entry:
        // the ring's order and its index must agree after every operation.
        ring.push(7, 3);
        assert_eq!(ring.len(), 2, "a duplicate must not consume two slots");
        assert_eq!(ring.iter().copied().collect::<Vec<_>>(), vec![8, 7]);
        assert!(ring.contains(7));
        assert!(ring.contains(8));

        ring.push(9, 3);
        assert_eq!(ring.iter().copied().collect::<Vec<_>>(), vec![8, 7, 9]);
        // Inserting the oldest entry again refreshes it, so the current
        // oldest (8) is the one evicted by the next new fingerprint.
        ring.push(7, 3);
        assert_eq!(ring.iter().copied().collect::<Vec<_>>(), vec![8, 9, 7]);
        ring.push(10, 3);
        assert_eq!(ring.iter().copied().collect::<Vec<_>>(), vec![9, 7, 10]);
        assert!(!ring.contains(8));
        assert!(ring.contains(9));
        assert!(ring.contains(7));
        assert!(ring.contains(10));
        let unique: HashSet<u64> = ring.iter().copied().collect();
        assert_eq!(unique.len(), ring.len(), "the ring stays duplicate-free");
    }

    #[test]
    fn tombstone_ring_reinsertion_after_eviction_restores_only_the_newest_window() {
        let mut ring = TombstoneRing::default();
        ring.push(1, 2);
        ring.push(2, 2);
        ring.push(3, 2);
        assert_eq!(ring.iter().copied().collect::<Vec<_>>(), vec![2, 3]);
        assert!(!ring.contains(1));

        ring.push(1, 2);
        assert_eq!(ring.iter().copied().collect::<Vec<_>>(), vec![3, 1]);
        assert!(!ring.contains(2));
        assert!(ring.contains(1));
        assert!(ring.contains(3));

        // A trim below the current length drops from the oldest side and
        // removes exactly the evicted values from the index too.
        ring.trim(1);
        assert_eq!(ring.iter().copied().collect::<Vec<_>>(), vec![1]);
        assert!(!ring.contains(3));
        assert!(ring.contains(1));
        assert!(ring.remove(1));
        assert!(!ring.remove(1));
        assert!(ring.is_empty());
        assert_eq!(ring.index.len(), 0);
    }

    #[test]
    fn tombstone_membership_is_consistent_across_a_snapshot_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ids: Vec<OperationId> = (0..6)
            .map(|n| op_id(&format!("op-roundtrip-{n}")))
            .collect();
        let retention = JournalRetention {
            max_retained_operations: 1,
            max_tombstones: 8,
            max_log_records: 1_000,
            max_log_bytes: None,
        };
        let mut journal = Journal::open_with_retention(dir.path(), retention).expect("open");
        for (index, id) in ids.iter().enumerate() {
            complete(
                &mut journal,
                id,
                [u8::try_from(index + 1).expect("small"); 32],
                serde_json::json!({"n": index}),
            );
        }
        journal.compact().expect("compact");

        let before: Vec<bool> = ids.iter().map(|id| journal.is_tombstoned(id)).collect();
        assert_eq!(
            before,
            vec![true, true, true, true, true, false],
            "only the newest terminal operation survives the window"
        );
        assert_eq!(
            journal.tombstone_count(),
            before.iter().filter(|present| **present).count(),
            "is_tombstoned and the ring size agree"
        );
        let serialized: Vec<u64> = journal.tombstones.iter().copied().collect();
        let unique: HashSet<u64> = serialized.iter().copied().collect();
        assert_eq!(
            serialized.len(),
            unique.len(),
            "the snapshot ring is unique"
        );

        drop(journal);
        let reopened =
            Journal::open_with_retention(dir.path(), retention).expect("snapshot reopen");
        let after: Vec<bool> = ids.iter().map(|id| reopened.is_tombstoned(id)).collect();
        assert_eq!(after, before, "snapshot restore rebuilds the index exactly");
        assert_eq!(
            reopened.tombstones.iter().copied().collect::<Vec<_>>(),
            serialized,
            "the oldest-first snapshot order round-trips unchanged"
        );
        assert_eq!(
            reopened.tombstone_count(),
            unique.len(),
            "a restart does not introduce duplicate ring entries"
        );
        assert_eq!(
            reopened.tombstones.index, unique,
            "the membership index is rebuilt from the snapshot ring"
        );
    }

    #[test]
    fn a_legacy_snapshot_with_duplicate_tombstones_restores_a_unique_ring() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = op_id("op-legacy-tombstone-a");
        let b = op_id("op-legacy-tombstone-b");
        let c = op_id("op-legacy-tombstone-c");
        let absent = op_id("op-legacy-tombstone-absent");
        let (fa, fb, fc) = (fingerprint(&a), fingerprint(&b), fingerprint(&c));
        // A snapshot from a legacy/older producer with the same fingerprint
        // twice. The duplicate is the newest entry, so restore must refresh it
        // to the newest position rather than retaining two copies.
        let snapshot = Envelope {
            record_version: RECORD_VERSION,
            record: JournalRecord::Snapshot(Snapshot {
                operations: Vec::new(),
                tombstones: vec![fa, fb, fc, fb],
                evicted_operations: 3,
            }),
        };
        let payload = serde_json::to_vec(&snapshot).expect("serialize snapshot");
        std::fs::write(
            dir.path().join(JOURNAL_LOG_FILE),
            frame::encode(1, &payload).expect("encode snapshot"),
        )
        .expect("write snapshot log");

        let retention = JournalRetention {
            max_retained_operations: 8,
            max_tombstones: 8,
            max_log_records: 1_000_000,
            max_log_bytes: None,
        };
        let journal =
            Journal::open_with_retention(dir.path(), retention).expect("snapshot reopens");

        let order: Vec<u64> = journal.tombstones.iter().copied().collect();
        assert_eq!(
            order,
            vec![fa, fc, fb],
            "the duplicate's newest occurrence wins"
        );
        assert_eq!(journal.tombstone_count(), 3);
        let unique: HashSet<u64> = order.iter().copied().collect();
        assert_eq!(order.len(), unique.len(), "restore deduplicated the ring");
        assert_eq!(
            journal.tombstones.index, unique,
            "the index equals the serialized order set"
        );
        for id in [&a, &b, &c] {
            assert!(
                journal.is_tombstoned(id),
                "a remaining fingerprint must not be lost: {}",
                id.as_str()
            );
        }
        assert!(!journal.is_tombstoned(&absent));
    }

    #[test]
    fn is_tombstoned_matches_a_brute_force_reference_over_a_deterministic_sequence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut journal = Journal::open(dir.path()).expect("open");
        let ids: Vec<OperationId> = (0..64)
            .map(|index| op_id(&format!("op-random-tombstone-{index:02}")))
            .collect();
        // A deterministic LCG; the test must not depend on HashMap order or
        // any randomness source.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut reference: VecDeque<u64> = VecDeque::new();
        let mut capacity = 17usize;
        let id_count = u64::try_from(ids.len()).expect("small id set");

        for step in 0..2_000u64 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let id = &ids[usize::try_from(state % id_count).expect("fits usize")];
            match (state >> 32) % 5 {
                0 | 1 => {
                    let fp = fingerprint(id);
                    if let Some(position) = reference.iter().position(|stored| *stored == fp) {
                        reference.remove(position);
                    }
                    reference.push_back(fp);
                    while reference.len() > capacity {
                        reference.pop_front();
                    }
                    journal.tombstones.push(fp, capacity);
                }
                2 => {
                    let fp = fingerprint(id);
                    if let Some(position) = reference.iter().position(|stored| *stored == fp) {
                        reference.remove(position);
                    }
                    journal.tombstones.remove(fp);
                }
                3 => {
                    capacity = usize::try_from((state >> 16) % 23).expect("fits usize") + 1;
                    while reference.len() > capacity {
                        reference.pop_front();
                    }
                    journal.tombstones.trim(capacity);
                }
                _ => {
                    // Re-inserting a value already present exercises the
                    // duplicate-refresh path (the reference applies the same
                    // newest-position semantics).
                    let fp = fingerprint(id);
                    if let Some(position) = reference.iter().position(|stored| *stored == fp) {
                        reference.remove(position);
                    }
                    reference.push_back(fp);
                    while reference.len() > capacity {
                        reference.pop_front();
                    }
                    journal.tombstones.push(fp, capacity);
                }
            }

            let actual: Vec<u64> = journal.tombstones.iter().copied().collect();
            let expected: Vec<u64> = reference.iter().copied().collect();
            assert_eq!(actual, expected, "ring order diverged at step {step}");
            assert_eq!(
                journal.tombstones.index.len(),
                expected.len(),
                "index size diverged at step {step}"
            );
            for candidate in &ids {
                let expected_member = reference
                    .iter()
                    .any(|stored| *stored == fingerprint(candidate));
                assert_eq!(
                    journal.is_tombstoned(candidate),
                    expected_member,
                    "is_tombstoned diverged at step {step} for {}",
                    candidate.as_str()
                );
            }
        }
    }

    #[test]
    fn bounded_replay_evicts_and_compacts_before_serving() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ids: Vec<OperationId> = (1..=6).map(|n| op_id(&format!("op-replay-{n}"))).collect();
        {
            let mut journal = Journal::open(dir.path()).expect("open with defaults");
            for (index, id) in ids.iter().enumerate() {
                complete(
                    &mut journal,
                    id,
                    [u8::try_from(index + 1).expect("small index"); 32],
                    serde_json::json!({"n": index}),
                );
            }
            assert_eq!(journal.compactions(), 0, "defaults do not compact here");
        }

        // Reopen under a tight bound: replay itself must evict, then write
        // a snapshot before serving.
        let journal =
            Journal::open_with_retention(dir.path(), tight_retention()).expect("tight reopen");
        assert!(journal.compactions() >= 1);
        assert!(journal.retained_operations() <= 2);
        assert!(journal.lookup(&ids[5]).is_some());
        assert!(journal.is_tombstoned(&ids[0]));
        drop(journal);

        // The second reopen sees the snapshot, not the historical prefix.
        let journal =
            Journal::open_with_retention(dir.path(), tight_retention()).expect("snapshot reopen");
        assert_eq!(journal.record_count(), 1, "only the snapshot base remains");
        assert!(journal.lookup(&ids[5]).is_some());
    }

    #[test]
    fn a_history_folding_consumer_refuses_a_compacted_journal() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let mut journal =
                Journal::open_with_retention(dir.path(), tight_retention()).expect("open");
            complete(
                &mut journal,
                &op_id("op-one"),
                [1; 32],
                serde_json::json!({}),
            );
            complete(
                &mut journal,
                &op_id("op-two"),
                [2; 32],
                serde_json::json!({}),
            );
            assert!(journal.compactions() >= 1);
        }
        let err = Journal::open_with_records(dir.path())
            .expect_err("a witness must refuse a compacted journal");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(err.detail.contains("compacted"), "{err}");
        assert!(err.detail.contains("complete record history"), "{err}");
    }

    #[test]
    fn a_partial_compaction_file_from_a_crash_is_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = op_id("op-survivor");
        {
            let mut journal = Journal::open(dir.path()).expect("open");
            complete(&mut journal, &id, [7; 32], serde_json::json!({"ok": true}));
        }
        // Simulate a crash between writing the new snapshot and renaming it:
        // a complete-looking but unrenamed compaction file.
        let fake = Envelope {
            record_version: RECORD_VERSION,
            record: JournalRecord::Snapshot(Snapshot {
                operations: vec![SnapshotOperation {
                    operation_id: op_id("op-not-durable"),
                    request_hash: Some([9; 32]),
                    outcome: Some(SnapshotOutcome {
                        success: true,
                        response: serde_json::json!({"fabricated": true}),
                        http_status: None,
                    }),
                }],
                tombstones: Vec::new(),
                evicted_operations: 99,
            }),
        };
        let payload = serde_json::to_vec(&fake).expect("serialize");
        std::fs::write(
            dir.path().join("journal.log.compact"),
            frame::encode(1, &payload).expect("encode"),
        )
        .expect("write debris");

        let journal = Journal::open(dir.path()).expect("old log still opens");
        assert!(
            journal.lookup(&op_id("op-not-durable")).is_none(),
            "an unrenamed compaction file must never be served"
        );
        assert_eq!(
            journal
                .lookup(&id)
                .expect("the durable old log replays")
                .outcome
                .expect("outcome")
                .response,
            serde_json::json!({"ok": true})
        );
    }

    #[test]
    fn a_snapshot_after_other_records_refuses_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log_path = dir.path().join(JOURNAL_LOG_FILE);
        let checkpoint = Envelope {
            record_version: RECORD_VERSION,
            record: JournalRecord::Checkpoint(crate::record::Checkpoint {
                note: "before".to_owned(),
            }),
        };
        let snapshot = Envelope {
            record_version: RECORD_VERSION,
            record: JournalRecord::Snapshot(Snapshot {
                operations: Vec::new(),
                tombstones: Vec::new(),
                evicted_operations: 0,
            }),
        };
        let mut bytes = frame::encode(
            1,
            &serde_json::to_vec(&checkpoint).expect("serialize checkpoint"),
        )
        .expect("encode checkpoint");
        bytes.extend_from_slice(
            &frame::encode(
                2,
                &serde_json::to_vec(&snapshot).expect("serialize snapshot"),
            )
            .expect("encode snapshot"),
        );
        std::fs::write(&log_path, &bytes).expect("write log");

        let err = Journal::open(dir.path()).expect_err("misplaced snapshot must refuse");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(err.detail.contains("must be the first"), "{err}");
        assert_eq!(
            std::fs::metadata(&log_path).expect("stat").len(),
            u64::try_from(bytes.len()).expect("length"),
            "a complete invalid record set is never truncated"
        );
    }

    #[test]
    fn a_failed_compaction_never_fails_the_durable_append_and_is_retried() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut journal =
            Journal::open_with_retention(dir.path(), tight_retention()).expect("open");
        // Make the fixed compaction target path unopenable (a directory).
        std::fs::create_dir(dir.path().join("journal.log.compact")).expect("block compaction");

        complete(
            &mut journal,
            &op_id("op-one"),
            [1; 32],
            serde_json::json!({}),
        );
        complete(
            &mut journal,
            &op_id("op-two"),
            [2; 32],
            serde_json::json!({}),
        );
        assert!(
            journal.compaction_failures() >= 1,
            "the compaction failure is recorded"
        );
        assert_eq!(journal.compactions(), 0);
        assert!(journal.last_compaction_error().is_some());
        // The append was already fsynced and stays replayable.
        assert!(journal.lookup(&op_id("op-two")).is_some());

        std::fs::remove_dir(dir.path().join("journal.log.compact")).expect("unblock");
        complete(
            &mut journal,
            &op_id("op-three"),
            [3; 32],
            serde_json::json!({}),
        );
        assert!(journal.compactions() >= 1, "the next attempt succeeds");
    }

    #[test]
    fn a_persistently_failing_compaction_backs_off_and_recovers_after_growth() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut journal =
            Journal::open_with_retention(dir.path(), tight_retention()).expect("open");
        std::fs::create_dir(dir.path().join("journal.log.compact")).expect("block compaction");

        // The first threshold crossing is attempted immediately: one failure,
        // not a silent delay until the backoff has elapsed.
        complete(
            &mut journal,
            &op_id("op-backoff-one"),
            [1; 32],
            serde_json::json!({}),
        );
        complete(
            &mut journal,
            &op_id("op-backoff-two"),
            [2; 32],
            serde_json::json!({}),
        );
        assert_eq!(journal.compaction_failures(), 1);
        assert_eq!(journal.compactions(), 0);

        // Keep the target blocked for many completed operations (512
        // appends). The retry gate must turn a would-be attempt per append
        // into O(log n) attempts: 1, 2, 4, ... 64 records of growth.
        for index in 0..256 {
            complete(
                &mut journal,
                &op_id(&format!("op-backoff-blocked-{index:03}")),
                [u8::try_from(index % 251).expect("small"); 32],
                serde_json::json!({}),
            );
        }
        let failures = journal.compaction_failures();
        assert!(
            failures > 1,
            "a persistent failure must still be retried automatically"
        );
        assert!(
            failures <= 20,
            "512 appends must cause far fewer than one attempt each: {failures}"
        );
        assert!(
            failures * 10 < 512,
            "the attempt count must be bounded well below one per append: {failures}"
        );
        assert!(journal.last_compaction_error().is_some());
        assert_eq!(journal.compactions(), 0);
        // No failed compaction may fail or hide the already-durable append.
        assert!(journal.lookup(&op_id("op-backoff-blocked-255")).is_some());

        // Unblock without restarting. The capped backoff is at most 64
        // records, so this growth crosses the pending retry threshold and the
        // scheduled compaction succeeds.
        std::fs::remove_dir(dir.path().join("journal.log.compact")).expect("unblock");
        for index in 0..64 {
            complete(
                &mut journal,
                &op_id(&format!("op-backoff-recovered-{index:03}")),
                [u8::try_from(index % 251).expect("small"); 32],
                serde_json::json!({}),
            );
        }
        assert!(
            journal.compactions() >= 1,
            "compaction must recover without a restart (failures={failures}, last={:?})",
            journal.last_compaction_error()
        );
        assert_eq!(
            journal.compaction_failures(),
            failures,
            "a recovered compaction adds no new failure"
        );
    }

    #[test]
    fn a_pinned_operation_is_never_evicted_while_its_retry_is_executing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut journal =
            Journal::open_with_retention(dir.path(), tight_retention()).expect("open");
        let a = op_id("op-pinned");
        let b = op_id("op-b");
        let c = op_id("op-c");
        let d = op_id("op-d");

        complete(&mut journal, &a, [1; 32], serde_json::json!({}));
        journal.pin_operation(&a);
        assert_eq!(journal.pinned_operations(), 1);

        // Two further terminal operations would ordinarily evict `a`, but
        // the pin keeps it available for the re-drive's replacement outcome.
        complete(&mut journal, &b, [2; 32], serde_json::json!({}));
        complete(&mut journal, &c, [3; 32], serde_json::json!({}));
        journal.compact().expect("compact with the pin held");
        assert!(journal.compactions() >= 1);
        assert!(
            journal.lookup(&a).is_some(),
            "a pinned operation must survive compaction"
        );
        assert!(!journal.is_tombstoned(&a));

        // Once the pipeline releases the pin, the next compaction may evict
        // it and the tombstone makes the refusal fail closed.
        journal.unpin_operation(&a);
        assert_eq!(journal.pinned_operations(), 0);
        complete(&mut journal, &d, [4; 32], serde_json::json!({}));
        journal.compact().expect("compact after unpin");
        assert!(journal.is_tombstoned(&a));
        let err = journal
            .append_intent(a, [1; 32], "create_volume", serde_json::json!({}))
            .expect_err("an unpinned evicted operation is expired");
        assert_eq!(err.code, ApiErrorCode::OperationExpired);
    }

    #[test]
    fn a_pin_interleaved_with_compaction_preserves_the_replacement_outcome_across_reopen() {
        // Deterministic interleaving of the pin/compaction race: a re-drive
        // pins an already-terminal operation, compaction runs between the
        // pin and the replacement outcome, and the outcome is journaled
        // while the pin still holds. The snapshot must carry the *latest*
        // outcome, and only an unpinned later compaction may expire it.
        let dir = tempfile::tempdir().expect("tempdir");
        let a = op_id("op-race-a");
        let b = op_id("op-race-b");
        let c = op_id("op-race-c");
        let mut journal =
            Journal::open_with_retention(dir.path(), tight_retention()).expect("open");

        complete(&mut journal, &a, [1; 32], serde_json::json!({"v": 1}));
        journal.pin_operation(&a);

        // B crosses the retention bound while A is pinned. Compaction may
        // evict anything unpinned, but A stays available for the re-drive.
        complete(&mut journal, &b, [2; 32], serde_json::json!({}));
        journal.compact().expect("compact while A is pinned");
        assert!(journal.compactions() >= 1, "the retention trigger fired");
        assert!(journal.lookup(&a).is_some(), "the pinned A survived");
        assert!(!journal.is_tombstoned(&a));

        // The replacement outcome lands while the pin still holds; the
        // compaction it may trigger must not turn A into a tombstone.
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

        // Once the pipeline releases the pin, the next compaction evicts A
        // and both retry paths fail closed with the expiry code.
        journal.unpin_operation(&a);
        complete(&mut journal, &c, [3; 32], serde_json::json!({}));
        journal.compact().expect("compact after unpin");
        assert!(journal.is_tombstoned(&a), "the unpinned A was evicted");
        let intent_error = journal
            .append_intent(a.clone(), [1; 32], "create_volume", serde_json::json!({}))
            .expect_err("an expired intent must never re-execute");
        assert_eq!(intent_error.code, ApiErrorCode::OperationExpired);
        let outcome_error = journal
            .append_outcome(a.clone(), true, serde_json::json!({}))
            .expect_err("an expired outcome must never resurrect the operation");
        assert_eq!(outcome_error.code, ApiErrorCode::OperationExpired);

        drop(journal);
        // The durable snapshot is the interleaving's result: A is expired
        // and the surviving terminal operations keep their outcomes.
        let mut reopened =
            Journal::open_with_retention(dir.path(), tight_retention()).expect("reopen");
        assert!(reopened.is_tombstoned(&a));
        assert_eq!(
            reopened
                .append_outcome(a, true, serde_json::json!({}))
                .expect_err("the tombstone survives the snapshot")
                .code,
            ApiErrorCode::OperationExpired
        );
        assert!(
            reopened.lookup(&c).is_some(),
            "the newest terminal survives"
        );
    }

    #[test]
    fn a_redriven_outcome_after_a_replay_window_eviction_reopens_instead_of_wedging() {
        // The live journal only compacts at `2 * max_retained_operations`,
        // so a legal log can hold more terminal operations than replay's
        // tighter in-memory bound. An API re-drive can then append a
        // replacement outcome for an operation replay evicted. That is not
        // the same as a durable snapshot tombstone: the raw intent/outcome
        // pair is still in this log, so replay must re-admit the operation
        // (outcome-only, hence fail-closed for any later intent) instead of
        // making the daemon permanently unopenable.
        let dir = tempfile::tempdir().expect("tempdir");
        let retention = JournalRetention {
            max_retained_operations: 4,
            max_tombstones: 16,
            max_log_records: 1_000_000,
            max_log_bytes: None,
        };
        let target = op_id("op-replay-redrive");
        {
            let mut journal = Journal::open_with_retention(dir.path(), retention).expect("open");
            journal
                .append_intent(
                    target.clone(),
                    [7; 32],
                    "prepare_nearline_handoff",
                    serde_json::json!({}),
                )
                .expect("intent");
            journal
                .append_outcome(
                    target.clone(),
                    false,
                    serde_json::json!({"code": "INTERNAL"}),
                )
                .expect("recorded failure");
            for index in 0..4 {
                complete(
                    &mut journal,
                    &op_id(&format!("op-replay-other-{index}")),
                    [u8::try_from(index + 1).expect("small"); 32],
                    serde_json::json!({}),
                );
            }
            assert_eq!(
                journal.compactions(),
                0,
                "the live hysteresis bound has not fired yet"
            );
            journal
                .append_outcome(target.clone(), true, serde_json::json!({"ok": true}))
                .expect("replacement outcome");
        }

        let mut journal = Journal::open_with_retention(dir.path(), retention)
            .expect("a legal re-driven outcome must not wedge startup");
        assert!(!journal.is_tombstoned(&target));
        assert_eq!(
            journal.retained_operations(),
            4,
            "replay still bounds the registry"
        );
        // The intent is no longer paired with a retained hash, so a retry
        // fails closed as a conflict instead of re-executing the mutation.
        let error = journal
            .append_intent(
                target,
                [7; 32],
                "prepare_nearline_handoff",
                serde_json::json!({}),
            )
            .expect_err("an outcome-only re-admission has no verified hash");
        assert_eq!(error.code, ApiErrorCode::IdempotencyConflict);
    }

    #[test]
    fn a_post_snapshot_record_for_a_snapshot_tombstone_is_still_refused() {
        // A fingerprint from a durable snapshot is a real retention
        // decision. A record after that snapshot for the evicted id is an
        // inconsistent log and must stay a hard refusal, unlike a
        // replay-window eviction.
        let dir = tempfile::tempdir().expect("tempdir");
        let evicted = op_id("op-durable-evicted");
        {
            let mut journal =
                Journal::open_with_retention(dir.path(), tight_retention()).expect("open");
            complete(&mut journal, &evicted, [1; 32], serde_json::json!({}));
            complete(
                &mut journal,
                &op_id("op-durable-later"),
                [2; 32],
                serde_json::json!({}),
            );
            assert!(journal.compactions() >= 1);
            assert!(journal.is_tombstoned(&evicted));
        }

        let payload = serde_json::to_vec(&Envelope {
            record_version: RECORD_VERSION,
            record: JournalRecord::Outcome(Outcome {
                operation_id: evicted,
                success: true,
                response: serde_json::json!({"resurrected": true}),
                http_status: None,
            }),
        })
        .expect("serialize outcome");
        let encoded = frame::encode(2, &payload).expect("encode outcome");
        let mut log = OpenOptions::new()
            .append(true)
            .open(dir.path().join(JOURNAL_LOG_FILE))
            .expect("open log for raw append");
        log.write_all(&encoded).expect("raw append");
        log.sync_all().expect("fsync raw append");

        let error = Journal::open_with_retention(dir.path(), tight_retention())
            .expect_err("a post-snapshot record for an evicted id is inconsistent");
        assert_eq!(error.code, ApiErrorCode::Internal);
        assert!(
            error.detail.contains("durable compaction snapshot"),
            "the refusal names the durable source: {error}"
        );
    }

    #[test]
    fn irreducible_in_flight_operations_fail_closed_without_poisoning_the_log() {
        // In-flight intents are never evicted. If they alone exceed the
        // one-frame durability unit, compaction must fail closed before any
        // file I/O: the already-fsynced log stays intact, is not poisoned,
        // and later appends keep succeeding.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut journal = Journal::open(dir.path()).expect("open");
        // Populate the in-memory registry directly: this test is about the
        // frame planner, not fsync throughput. Each entry serializes to
        // roughly 60 bytes, so this is comfortably over 8 MiB.
        for index in 0..200_000u64 {
            let id = op_id(&format!("op-inflight-{index:08}"));
            journal.operations.insert(
                id,
                OperationState {
                    request_hash: Some([0; 32]),
                    outcome: None,
                },
            );
        }

        let error = journal
            .compact()
            .expect_err("irreducible in-flight state must fail closed");
        assert_eq!(error.code, ApiErrorCode::Internal);
        assert!(
            error.detail.contains("cannot fit"),
            "the refusal names the one-frame budget: {error}"
        );
        assert!(
            journal.compaction_failures() >= 1,
            "the failed attempt is visible"
        );
        assert_eq!(journal.compactions(), 0);
        assert!(journal.last_compaction_error().is_some());

        // The durable log was untouched and the journal is not poisoned:
        // appends continue.
        let id = op_id("op-after-frame-failure");
        journal
            .append_intent(id.clone(), [7; 32], "create_volume", serde_json::json!({}))
            .expect("a failed compaction must not fail later appends");
        drop(journal);
        let reopened = Journal::open(dir.path()).expect("the log reopens");
        assert!(reopened.lookup(&id).is_some());
    }

    #[test]
    fn compaction_evicts_to_fit_one_frame_instead_of_failing_forever() {
        // Default-sized retention is arithmetically larger than one 8 MiB
        // frame once response bodies are realistic. Compaction must evict
        // oldest terminal operations (recording tombstones) until the
        // snapshot fits, not fail forever and leave the log unbounded.
        let dir = tempfile::tempdir().expect("tempdir");
        let body = "x".repeat(128 * 1024);
        let retention = JournalRetention {
            max_retained_operations: 64,
            max_tombstones: 1_024,
            max_log_records: 100_000_000,
            max_log_bytes: None,
        };
        let mut journal = Journal::open_with_retention(dir.path(), retention).expect("open");
        let ids: Vec<OperationId> = (0..128)
            .map(|index| op_id(&format!("op-frame-{index:03}")))
            .collect();
        for (index, id) in ids.iter().enumerate() {
            complete(
                &mut journal,
                id,
                [u8::try_from(index % 251).expect("small"); 32],
                serde_json::json!({ "body": body }),
            );
        }

        assert_eq!(
            journal.compaction_failures(),
            0,
            "the frame budget must be honored by eviction, not by failing: {:?}",
            journal.last_compaction_error()
        );
        assert!(
            journal.compactions() >= 1,
            "a snapshot larger than one frame must still compact"
        );
        assert!(
            journal.log_bytes() <= frame::MAX_PAYLOAD_LEN as u64 + 1_024 * 1_024,
            "the compacted log is bounded by one snapshot frame plus appends: {}",
            journal.log_bytes()
        );
        assert!(
            journal.retained_operations() <= 64,
            "retention is an upper bound: {} retained",
            journal.retained_operations()
        );
        // The oldest terminal operations were evicted and are refused with
        // the fail-closed expiry code, not silently re-executed.
        assert!(journal.lookup(&ids[0]).is_none());
        assert!(journal.is_tombstoned(&ids[0]));
        // The newest operation and its large body survive the compaction.
        let newest = journal.lookup(&ids[127]).expect("newest retained");
        assert_eq!(
            newest.outcome.expect("outcome").response,
            serde_json::json!({ "body": body })
        );

        drop(journal);
        let reopened =
            Journal::open_with_retention(dir.path(), retention).expect("snapshot reopens");
        assert_eq!(
            reopened
                .lookup(&ids[127])
                .expect("newest survives the snapshot")
                .outcome
                .expect("outcome")
                .response,
            serde_json::json!({ "body": body })
        );
        assert!(reopened.is_tombstoned(&ids[0]));
    }

    #[test]
    fn a_tiny_record_threshold_does_not_compact_on_every_append() {
        // `max_log_records = 2` is legal, but after a compaction the log
        // holds one record, so a naive threshold rewrites the whole retained
        // set on every append (quadratic write amplification). The growth
        // floor must keep the compaction count proportional to records / 64.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut journal = Journal::open_with_retention(
            dir.path(),
            JournalRetention {
                max_retained_operations: 10_000,
                max_tombstones: 10_000,
                max_log_records: 2,
                max_log_bytes: None,
            },
        )
        .expect("open");
        for index in 0..300 {
            complete(
                &mut journal,
                &op_id(&format!("op-growth-{index:03}")),
                [u8::try_from(index % 251).expect("small"); 32],
                serde_json::json!({}),
            );
        }
        assert!(journal.compactions() >= 1, "the trigger still fires");
        assert!(
            journal.compactions() <= 12,
            "a tiny threshold must not rewrite the snapshot on every append: {}",
            journal.compactions()
        );
    }

    #[test]
    fn a_tiny_byte_threshold_compacts_once_then_requires_growth() {
        // A configured byte threshold below the snapshot's own size cannot
        // be "gotten under". Compaction must fire once, then wait for the
        // documented 1 MiB growth floor instead of rewriting forever.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut journal = Journal::open_with_retention(
            dir.path(),
            JournalRetention {
                max_retained_operations: 10_000,
                max_tombstones: 10_000,
                max_log_records: 1_000_000,
                max_log_bytes: Some(1),
            },
        )
        .expect("open");
        for index in 0..50 {
            complete(
                &mut journal,
                &op_id(&format!("op-byte-{index:03}")),
                [u8::try_from(index % 251).expect("small"); 32],
                serde_json::json!({}),
            );
        }
        assert_eq!(
            journal.compactions(),
            1,
            "the byte trigger fires once, then the growth floor holds it"
        );

        // More than the 1 MiB floor of new bytes crosses the trigger again.
        complete(
            &mut journal,
            &op_id("op-byte-large"),
            [9; 32],
            serde_json::json!({"blob": "x".repeat(1024 * 1024 + 4096)}),
        );
        assert!(
            journal.compactions() >= 2,
            "new bytes beyond the floor must trigger a compaction"
        );
    }

    #[test]
    fn an_append_fails_closed_until_a_pending_compaction_dir_sync_succeeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut journal = Journal::open(dir.path()).expect("open");
        // Simulate a compaction rename whose directory fsync failed: every
        // later append must retry the sync before it can be acknowledged.
        journal.dir_sync_pending = true;
        let real_path = journal.log_path.clone();
        journal.log_path = real_path.with_file_name("missing/journal.log");
        let error = journal
            .append_checkpoint("blocked")
            .expect_err("a failed directory fsync must fail the append closed");
        assert_eq!(error.code, ApiErrorCode::Internal);
        assert!(journal.dir_sync_pending, "the retry obligation is retained");
        assert_eq!(
            journal.record_count(),
            0,
            "the refused append was not written or acknowledged"
        );

        journal.log_path = real_path;
        journal
            .append_checkpoint("now durable")
            .expect("a successful directory fsync clears the obligation");
        assert!(!journal.dir_sync_pending);
        assert_eq!(journal.record_count(), 1);
    }

    const V2_N1_FIXTURE: &[u8] = include_bytes!("../../../tests/fixtures/schema/journal-v2-n1.log");

    #[test]
    fn journal_v1_is_the_n_minus_one_fixture_and_upgrades_on_write() {
        assert_eq!(crate::record::LEGACY_RECORD_VERSION_N1, 1);
        let dir = tempfile::tempdir().expect("tempdir");
        let log_path = dir.path().join(JOURNAL_LOG_FILE);
        std::fs::write(&log_path, V2_N1_FIXTURE).expect("write fixture");

        let mut journal = Journal::open(dir.path()).expect("v1 fixture opens under v2");
        assert_eq!(journal.record_count(), 3);
        let id = op_id("op-upgrade-journal-n1");
        assert!(journal.lookup(&id).expect("v1 registry entry").has_outcome);
        journal
            .append_checkpoint("after v1 replay")
            .expect("append v2 record");
        drop(journal);

        let text = std::fs::read(&log_path).expect("read log");
        let text = String::from_utf8_lossy(&text);
        assert!(
            text.contains("\"record_version\":2"),
            "a new append stamps the current record version: {text}"
        );
        let journal = Journal::open(dir.path()).expect("mixed v1/v2 log replays");
        assert_eq!(journal.record_count(), 4);
        assert!(journal.lookup(&id).expect("entry survives").has_outcome);
    }
}
