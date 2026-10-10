//! Journal-before-mutate execution pipeline shared by every mutating
//! endpoint (Volume API v2 section 7; AGENTS rule 8).
//!
//! [`execute`] is the single correctness path for mutations. The ordering is
//! strict:
//!
//! 1. *(in the handler)* typed request validation — rejections happen before
//!    anything is journaled, so they leave no journal record;
//! 2. **journal lookup** — a recorded **success** outcome for the same
//!    `operation_id` and immutable request hash is replayed byte-for-byte
//!    *without executing*; a recorded **failure** outcome is answered by
//!    the route's [`FailureReplay`] class (verbatim on the strict routes,
//!    re-evaluated through the caller's inspection on the re-issuable
//!    peer routes — the `grant_set` wedge fix: a failure is a fact about
//!    the attempt, not about the world, and the world may have converged
//!    past it); a same-hash intent without an outcome fails closed with
//!    `OPERATION_IN_DOUBT` (the operation may be in flight; it is never
//!    re-executed); a different hash for the same `operation_id` is an
//!    `IDEMPOTENCY_CONFLICT`;
//! 3. **journal intent** — durably fsynced *before* the provider mutation;
//! 4. **execute** the provider call (never while holding the journal lock),
//!    then journal the outcome — the serialized response on success, the
//!    `{"code","message"}` error body on failure — and return it. The stored
//!    success body is the exact body served to the first caller, so replays
//!    are byte-compatible. A recorded *failure* outcome of a strict route
//!    is also replayed verbatim, with the status reconstructed from its
//!    recorded code.
//!
//! If the outcome record of a *successful* mutation cannot be journaled
//! (journal I/O error), the failure is logged loudly: the caller still
//! receives the truthful success response, while any later replay of that
//! operation fails closed as in-doubt.
//!
//! Panic-safety is out of scope for P0: provider calls are expected not to
//! panic, and a panic between intent and outcome leaves exactly the
//! in-doubt fail-closed state described above.
//!
//! The journal mutex is a `std::sync::Mutex`; guards are acquired, used and
//! dropped inside small blocks, and **never** held across an `.await`.

use std::future::Future;
use std::sync::Arc;
use std::sync::MutexGuard;

use axum::http::StatusCode;
use axum::response::Response;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use volvisor_journal::{IntentAppend, Journal};
use volvisor_types::request::{
    AttachVolumeRequest, CreateVolumeRequest, DeleteVolumeRequest, DetachVolumeRequest,
    GrowVolumeRequest,
};
use volvisor_types::{
    ApiError, ApiErrorBody, ApiErrorCode, AttachmentId, DeviceId, MigrationId, OperationId,
    VolumeId,
};

use crate::crash::CrashPoint;
use crate::error::{json_response, status_for_wire_code, to_json_value};
use crate::state::SharedState;

/// Operation kind: create volume.
pub const OP_CREATE_VOLUME: &str = "create_volume";
/// Operation kind: attach volume.
pub const OP_ATTACH_VOLUME: &str = "attach_volume";
/// Operation kind: detach volume.
pub const OP_DETACH_VOLUME: &str = "detach_volume";
/// Operation kind: grow volume.
pub const OP_GROW_VOLUME: &str = "grow_volume";
/// Operation kind: delete volume.
pub const OP_DELETE_VOLUME: &str = "delete_volume";
/// Operation kind: claim a device for a pool (admin surface).
pub const OP_CLAIM_DEVICE: &str = "claim_device";
/// Operation kind: release a claimed device (admin surface).
pub const OP_RELEASE_DEVICE: &str = "release_device";
/// Operation kind: adopt-and-promote a nearline volume (admin surface).
pub const OP_ADOPT_VOLUME: &str = "adopt_volume";
/// Operation kind: clear a stale migration-cut marker (admin surface).
pub const OP_CLEAR_CUT_MARKER: &str = "clear_cut_marker";
/// Operation kind: `PrepareNearlineHandoff` (mobility surface).
pub const OP_MIGRATION_PREPARE: &str = "migration_prepare";
/// Operation kind: `BarrierAndTransfer` (mobility surface).
pub const OP_MIGRATION_TRANSFER: &str = "migration_transfer";
/// Operation kind: mobility abort.
pub const OP_MIGRATION_ABORT: &str = "migration_abort";
/// Operation kind: destination-side peer prepare.
pub const OP_PEER_PREPARE: &str = "peer_prepare";
/// Operation kind: destination-side peer grant + promote.
pub const OP_PEER_GRANT: &str = "peer_grant";
/// Operation kind: destination-side peer restore-vm.
pub const OP_PEER_RESTORE_VM: &str = "peer_restore_vm";
/// Operation kind: destination-side peer discard.
pub const OP_PEER_DISCARD: &str = "peer_discard";

/// Every journaled operation kind the router table exposes (P5 plan
/// §3.1's armable surface — the journal-append hook keys its armed
/// entries on exactly these). The campaign's generated kill matrix
/// (§3.2) consumes this list and must account for every entry:
/// either a matrix cell drives the kind, or the kind appears in the
/// campaign's recorded-gaps table with its reason (never a silent
/// cut).
pub const ALL_OP_KINDS: [&str; 16] = [
    OP_CREATE_VOLUME,
    OP_ATTACH_VOLUME,
    OP_DETACH_VOLUME,
    OP_GROW_VOLUME,
    OP_DELETE_VOLUME,
    OP_CLAIM_DEVICE,
    OP_RELEASE_DEVICE,
    OP_ADOPT_VOLUME,
    OP_CLEAR_CUT_MARKER,
    OP_MIGRATION_PREPARE,
    OP_MIGRATION_TRANSFER,
    OP_MIGRATION_ABORT,
    OP_PEER_PREPARE,
    OP_PEER_GRANT,
    OP_PEER_RESTORE_VM,
    OP_PEER_DISCARD,
];

// ---------------------------------------------------------------------------
// Payload redaction (SPEC-0002 section 9: no secret material at rest in the
// journal beyond what the operator already owns)
// ---------------------------------------------------------------------------

/// Object keys whose *string* values are credential references and are
/// replaced with [`REDACTED`] before any payload is journaled.
const REDACTED_KEYS: [&str; 4] = [
    "key_ref",
    "authorization_token",
    "witness_host_token",
    "peer_api_token",
];

/// Replacement value written in place of credential material.
const REDACTED: &str = "[redacted]";

/// Recursively redact credential material from a journal payload.
///
/// Walks the payload and replaces the string value of any object key named
/// `key_ref` (external secret references) or `authorization_token` (scoped
/// destructive-authorization tokens) with `[redacted]`, at any nesting
/// depth and inside arrays. Volume/attachment/device identities, sizes and
/// policies are not credentials and remain intact for forensics.
///
/// The *request hash* is computed from the typed request before redaction
/// (the hashes in `volvisor-types` deliberately exclude token material), so
/// redaction never changes idempotency behavior.
pub(crate) fn redact(payload: &mut Value) {
    match payload {
        Value::Object(map) => {
            for (key, value) in map {
                if REDACTED_KEYS.contains(&key.as_str()) && value.is_string() {
                    *value = Value::String(REDACTED.to_owned());
                } else {
                    redact(value);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact(item);
            }
        }
        _ => {}
    }
}

/// How a recorded **failure** outcome is answered on a retry (the
/// `grant_set` wedge fix, P6-A part 3; the diagnosis is
/// `docs/plans/2026-10-10-grant-set-wedge-diagnosis.md`).
///
/// Every recorded **success** replays byte-for-byte on every route — the
/// classes differ on the failure tail only.
pub(crate) enum FailureReplay {
    /// Serve the recorded failure verbatim, never re-executing — the
    /// strict routes (the volume operations and the mobility routes).
    /// Their operation ids are consumer-supplied and their refusals can
    /// carry operator judgment (a validation or policy verdict), so the
    /// recorded failure is the honest terminal answer.
    Terminal,
    /// Re-evaluate the act against the world through the caller's
    /// inspection — the re-issuable routes (the four peer acts). Their
    /// safety shape: a deterministic migration-derived operation id, a
    /// total landed-ness inspection, and an idempotent re-execution at
    /// every layer (the witness batch under its own operation id, the
    /// promote per migration, the attachment identity deterministic).
    /// Proven landed → the proven outcome is journaled (superseding the
    /// recorded failure — the registry's most-recent-outcome rule) and
    /// served; proven not landed → the act re-executes under its
    /// idempotency discipline, its new outcome superseding the failure;
    /// an inspection error surfaces typed. The recorded failure is never
    /// re-served as a terminal answer: a failure is a fact about the
    /// attempt, not about the world, and the world may have converged
    /// past it (the witness-replayed grant of the recorded wedge).
    Reissue,
}

/// Execute one mutating operation through the journal pipeline.
///
/// `run` performs the provider mutation; it is invoked only after the intent
/// is durable, and its result is journaled before the response is returned.
/// The guard returned by the journal lock is never held while `run`'s future
/// is polled.
///
/// `payload` is the full wire request; it is redacted
/// ([`redact`]) before anything is journaled, so no credential material
/// (`encryption.key_ref`, `authorization_token`) ever reaches the journal
/// file.
pub(crate) async fn execute<R, F, Fut>(
    state: &SharedState,
    op_kind: &'static str,
    operation_id: OperationId,
    request_hash: [u8; 32],
    payload: Value,
    run: F,
) -> Result<Response, ApiError>
where
    R: Serialize,
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<R, ApiError>>,
{
    // The volume operations keep the strict fail-closed in-doubt rule: an
    // intent without an outcome may be in flight right now, so it is never
    // re-executed (and never "resolved" by guessing). Their recorded
    // failures are terminal replays too (see [`FailureReplay::Terminal`]):
    // consumer-supplied operation ids, refusals that can carry operator
    // judgment. Successes replay as plain `200` with no recorded status —
    // byte-identical to the pre-stage-B2 behavior.
    let in_doubt_id = operation_id.clone();
    let in_doubt_state = Arc::clone(state);
    execute_resolvable(
        state,
        op_kind,
        operation_id,
        request_hash,
        payload,
        None,
        FailureReplay::Terminal,
        move || async move { Err(in_doubt_error(&in_doubt_state, op_kind, &in_doubt_id)) },
        run,
    )
    .await
}

/// Execute one mutating operation that answers a non-`200` success
/// status (stage B2 mobility routes): `PrepareNearlineHandoff` answers
/// `201`, `BarrierAndTransfer` answers `202`. The status is journaled
/// with the outcome so an idempotent replay is status-compatible with
/// the first caller's response, not just body-compatible. The in-doubt
/// rule is the strict one, and so is the failure rule
/// ([`FailureReplay::Terminal`]; see [`execute`]).
pub(crate) async fn execute_with_status<R, F, Fut>(
    state: &SharedState,
    op_kind: &'static str,
    operation_id: OperationId,
    request_hash: [u8; 32],
    payload: Value,
    success_status: StatusCode,
    run: F,
) -> Result<Response, ApiError>
where
    R: Serialize,
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<R, ApiError>>,
{
    let in_doubt_id = operation_id.clone();
    let in_doubt_state = Arc::clone(state);
    execute_resolvable(
        state,
        op_kind,
        operation_id,
        request_hash,
        payload,
        Some(success_status),
        FailureReplay::Terminal,
        move || async move { Err(in_doubt_error(&in_doubt_state, op_kind, &in_doubt_id)) },
        run,
    )
    .await
}

/// The general journal pipeline: [`execute`] and [`execute_with_status`]
/// (strict in-doubt, terminal failure replay) and the peer routes
/// (inspection-resolved in-doubt, re-issued failures) are all this one
/// ordering.
///
/// On an intent-without-outcome retry, the `inspect` closure decides:
/// `Ok(Some(value))` **proves** the act already landed — its result is
/// journaled as the outcome and served, closing the crash window
/// between the act and its outcome record; `Ok(None)` reports the act
/// provably did not land — the mutation is re-executed, and the act's
/// own first step re-verifies (inspection-gated re-execution, never a
/// blind one); an `Err` surfaces typed (the in-flight state could not
/// be resolved, so nothing is guessed). The strict callers pass an
/// `inspect` that always fails closed with `OPERATION_IN_DOUBT`
/// (the volume operations: a concurrent caller may be executing
/// right now, and the mobility transfer whose drive task may be
/// mid-flight).
///
/// On a recorded-**failure** retry, `failures` decides (the `grant_set`
/// wedge fix): [`FailureReplay::Terminal`] serves the recorded failure
/// verbatim; [`FailureReplay::Reissue`] runs the *same* inspection as
/// the in-flight case — proven landed → the proven outcome supersedes
/// the recorded failure (the journal's most-recent-outcome rule) and
/// is served; proven not landed → the act re-executes under its
/// idempotency discipline and its outcome supersedes the failure; an
/// `Err` surfaces typed. The recorded failure itself is never re-served
/// as a terminal answer on a re-issuable route.
// The pipeline is one ordered narrative (lookup → intent → execute)
// whose fall-through control flow does not factor further without
// scattering the ordering the module docs promise; the length is the
// honesty, not complexity.
#[allow(clippy::too_many_lines)]
pub(crate) async fn execute_resolvable<R, F, Fut, I, IFut>(
    state: &SharedState,
    op_kind: &'static str,
    operation_id: OperationId,
    request_hash: [u8; 32],
    mut payload: Value,
    success_status: Option<StatusCode>,
    failures: FailureReplay,
    inspect: I,
    run: F,
) -> Result<Response, ApiError>
where
    R: Serialize,
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<R, ApiError>>,
    I: FnOnce() -> IFut,
    IFut: Future<Output = Result<Option<R>, ApiError>>,
{
    // Redact credential material before the payload is used anywhere: the
    // journaled intent (and any forensic read of the log) must never contain
    // secret references.
    redact(&mut payload);

    // (2) Resolve idempotency from the replay-derived registry.
    let recorded = {
        let journal = lock_journal(state)?;
        journal.lookup(&operation_id)
    };
    if let Some(entry) = recorded {
        if entry.request_hash != request_hash {
            return idempotency_conflict(state, op_kind, &operation_id);
        }
        match entry.outcome {
            Some(outcome) => {
                if !outcome.success && matches!(failures, FailureReplay::Reissue) {
                    // The `grant_set` wedge fix: a recorded failure of a
                    // re-issuable act is re-issued, never re-served.
                    if let Some(reissued) = resolve_retry_by_inspection(
                        state,
                        op_kind,
                        &operation_id,
                        success_status,
                        Arrival::RecordedFailure,
                        inspect,
                    )
                    .await?
                    {
                        return Ok(reissued);
                    }
                    // Proven not landed: fall through to (4), the act
                    // re-executing under its idempotency discipline.
                } else {
                    state.metrics.record_operation(op_kind, "replayed");
                    tracing::info!(
                        kind = op_kind,
                        operation_id = %operation_id,
                        "replaying recorded outcome without executing"
                    );
                    return replay_response(
                        outcome.success,
                        outcome.http_status,
                        &outcome.response,
                    );
                }
            }
            None => {
                // In flight (or interrupted before its outcome was
                // journaled): the inspect closure resolves it.
                if let Some(response) = resolve_retry_by_inspection(
                    state,
                    op_kind,
                    &operation_id,
                    success_status,
                    Arrival::InFlight,
                    inspect,
                )
                .await?
                {
                    return Ok(response);
                }
            }
        }
    } else {
        // (3) Journal the intent durably BEFORE the provider mutation.
        // Between the lookup above and this append another request may
        // have raced us; append_intent re-resolves idempotency under
        // the lock, so every outcome of that race is handled below.
        let append = {
            let mut journal = lock_journal(state)?;
            journal.append_intent(operation_id.clone(), request_hash, op_kind, payload)
        }?;
        match append {
            IntentAppend::New => {
                // The intent is durable and no outcome exists: the
                // P5 campaign's after-intent crash point (§3.1).
                consult_crash(state, op_kind, CrashPoint::AfterIntent);
            }
            IntentAppend::Replayed {
                success,
                response,
                http_status,
            } => {
                if !success && matches!(failures, FailureReplay::Reissue) {
                    // The `grant_set` wedge fix, race flavor: the rule
                    // must not depend on winning a race — a concurrently
                    // recorded failure of a re-issuable act is re-issued
                    // exactly like the first-lookup path above.
                    if let Some(reissued) = resolve_retry_by_inspection(
                        state,
                        op_kind,
                        &operation_id,
                        success_status,
                        Arrival::RecordedFailure,
                        inspect,
                    )
                    .await?
                    {
                        return Ok(reissued);
                    }
                    // Proven not landed: fall through to (4).
                } else {
                    state.metrics.record_operation(op_kind, "replayed");
                    tracing::info!(
                        kind = op_kind,
                        operation_id = %operation_id,
                        "replaying concurrently recorded outcome without executing"
                    );
                    // Same reconstruction as the first-lookup replay above: a
                    // concurrently recorded failure must serve the recorded error
                    // status, never a 200 wrapping the error body.
                    return replay_response(success, http_status, &response);
                }
            }
            IntentAppend::AlreadyInFlight => {
                if let Some(response) = resolve_retry_by_inspection(
                    state,
                    op_kind,
                    &operation_id,
                    success_status,
                    Arrival::InFlight,
                    inspect,
                )
                .await?
                {
                    return Ok(response);
                }
            }
        }
    }

    // (4) Execute the mutation. The journal guard is NOT held here.
    // Reaching this point after an in-flight resolution means the
    // inspection proved the act had not landed — the act's own first
    // step re-verifies (never a blind re-execution).
    let result = run().await;
    finish_outcome(state, op_kind, &operation_id, result, success_status)
}

/// Journal the outcome of an executed mutation and build the reply
/// (or propagate the typed failure): the single success/failure tail
/// of [`execute_resolvable`].
fn finish_outcome<R>(
    state: &SharedState,
    op_kind: &'static str,
    operation_id: &OperationId,
    result: Result<R, ApiError>,
    success_status: Option<StatusCode>,
) -> Result<Response, ApiError>
where
    R: Serialize,
{
    match result {
        Ok(value) => {
            let body = to_json_value(&value)?;
            let status = success_status.unwrap_or(StatusCode::OK);
            // The mutation has landed and its outcome is not yet
            // durable: the campaign's before-outcome crash point.
            consult_crash(state, op_kind, CrashPoint::BeforeOutcome);
            let journaled = {
                let mut journal = lock_journal(state)?;
                journal.append_outcome_with_status(
                    operation_id.clone(),
                    true,
                    body.clone(),
                    success_status.map(|status| status.as_u16()),
                )
            };
            // The outcome is durable and the reply is not yet served:
            // the campaign's after-outcome crash point.
            consult_crash(state, op_kind, CrashPoint::AfterOutcome);
            if let Err(journal_error) = journaled {
                // The mutation DID succeed and the caller must learn the
                // truth; only the replay fidelity is compromised, and any
                // later replay fails closed as in-doubt.
                tracing::error!(
                    kind = op_kind,
                    operation_id = %operation_id,
                    error = %journal_error,
                    "mutation succeeded but its outcome could not be journaled; \
                     replays of this operation will fail closed as in-doubt"
                );
            }
            state.metrics.record_operation(op_kind, "success");
            Ok(json_response(status, &body))
        }
        Err(error) => {
            let body = to_json_value(&ApiErrorBody::from(error.clone()))?;
            // The same two boundaries on the failure tail (a killed
            // operation never records its refusal either).
            consult_crash(state, op_kind, CrashPoint::BeforeOutcome);
            let journaled = {
                let mut journal = lock_journal(state)?;
                journal.append_outcome(operation_id.clone(), false, body)
            };
            consult_crash(state, op_kind, CrashPoint::AfterOutcome);
            if let Err(journal_error) = journaled {
                tracing::error!(
                    kind = op_kind,
                    operation_id = %operation_id,
                    error = %journal_error,
                    "failed outcome could not be journaled; replays of this \
                     operation will fail closed as in-doubt"
                );
            }
            state.metrics.record_operation(op_kind, "failure");
            Err(error)
        }
    }
}

/// How the inspection resolver was reached: an intent without an
/// outcome, or a recorded failure being re-issued. The transition
/// semantics are identical — only the tracing wording differs.
#[derive(Clone, Copy)]
enum Arrival {
    /// The journal holds the intent without an outcome: the act may be
    /// in flight or may have died before journaling its outcome.
    InFlight,
    /// The journal holds a recorded *failure* outcome and the route
    /// re-issues the act ([`FailureReplay::Reissue`]): the recorded
    /// answer described a past attempt, not the world.
    RecordedFailure,
}

impl Arrival {
    /// The tracing label for this arrival.
    fn label(self) -> &'static str {
        match self {
            Arrival::InFlight => "in-flight operation",
            Arrival::RecordedFailure => "recorded failure",
        }
    }
}

/// Resolve one inspection-gated retry through the caller's inspection
/// and serve the proven outcome. Reached two ways (see [`Arrival`]):
/// an intent-without-outcome retry (the in-flight crash window), or a
/// recorded-failure retry of a re-issuable act (the `grant_set` wedge
/// fix — the callers gate on [`FailureReplay::Reissue`], so a strict
/// route's recorded failure is a terminal replay and never reaches
/// this).
///
/// `Ok(Some(response))` — the act is provably landed: the proven
/// outcome is journaled (with the caller's success status, so a later
/// replay through the recorded outcome is status-compatible) and the
/// response returned for serving, closing the in-flight crash window
/// or superseding the recorded failure (the registry's
/// most-recent-outcome rule). `Ok(None)` — the act is provably not
/// landed: the caller falls through to the execution, the act
/// re-running under its idempotency discipline with its own first
/// step re-verifying. An `Err` propagates typed (nothing is guessed);
/// in the recorded-failure arrival this is the rule that the stale
/// failure is never re-served as a terminal answer.
async fn resolve_retry_by_inspection<R, I, IFut>(
    state: &SharedState,
    op_kind: &'static str,
    operation_id: &OperationId,
    success_status: Option<StatusCode>,
    arrival: Arrival,
    inspect: I,
) -> Result<Option<Response>, ApiError>
where
    R: Serialize,
    I: FnOnce() -> IFut,
    IFut: Future<Output = Result<Option<R>, ApiError>>,
{
    match inspect().await {
        Ok(Some(value)) => {
            let body = to_json_value(&value)?;
            let journaled = {
                let mut journal = lock_journal(state)?;
                journal.append_outcome_with_status(
                    operation_id.clone(),
                    true,
                    body.clone(),
                    success_status.map(|status| status.as_u16()),
                )
            };
            if let Err(journal_error) = journaled {
                tracing::error!(
                    kind = op_kind,
                    operation_id = %operation_id,
                    error = %journal_error,
                    "the inspection proved the act landed but its outcome \
                     could not be journaled; retries will re-resolve by inspection"
                );
            }
            // The resolved outcome is durable (the campaign's
            // after-outcome point also covers the recovery path's
            // outcome writes — the re-drive of a killed operation).
            consult_crash(state, op_kind, CrashPoint::AfterOutcome);
            state.metrics.record_operation(op_kind, "resolved");
            tracing::info!(
                kind = op_kind,
                operation_id = %operation_id,
                arrival = arrival.label(),
                "resolved by inspection: the act is proven done (a recorded \
                 failure is superseded, never served past this point)"
            );
            Ok(Some(json_response(
                success_status.unwrap_or(StatusCode::OK),
                &body,
            )))
        }
        Ok(None) => {
            state.metrics.record_operation(op_kind, "re_drive");
            tracing::info!(
                kind = op_kind,
                operation_id = %operation_id,
                arrival = arrival.label(),
                "proven not landed; re-executing (the act re-verifies its \
                 own preconditions)"
            );
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Lock the journal for a short, synchronous critical section.
///
/// Poisoning (a panic while a journal operation was in flight) fails closed:
/// the durability of the registry can no longer be reasoned about, so every
/// further mutation is refused.
fn lock_journal(state: &SharedState) -> Result<MutexGuard<'_, Journal>, ApiError> {
    state.journal.lock().map_err(|_| {
        ApiError::new(
            ApiErrorCode::Internal,
            "journal mutex poisoned; refusing further mutations (fail closed)",
        )
    })
}

/// Consult the daemon's armed crash hook (P5 plan §3.1) at `point` of
/// `op_kind`. Inert in every production shape: a state built without
/// [`AppState::with_crash_hooks`](crate::AppState::with_crash_hooks)
/// carries no hooks, and an unarmed table changes nothing. When the
/// rig armed exactly this pair, the firing consumes the entry, fires
/// the registered kill switch (the supervisor's group abort) and
/// terminates this request mid-handler — the in-band equivalent of a
/// process death at the durable-write boundary (see the `crash`
/// module docs).
fn consult_crash(state: &SharedState, op_kind: &'static str, point: CrashPoint) {
    if let Some(hooks) = &state.crash {
        hooks.consult(op_kind, point);
    }
}

/// A typed `IDEMPOTENCY_CONFLICT` rejection.
fn idempotency_conflict(
    state: &SharedState,
    op_kind: &'static str,
    operation_id: &OperationId,
) -> Result<Response, ApiError> {
    state.metrics.record_operation(op_kind, "conflict");
    tracing::warn!(
        kind = op_kind,
        operation_id = %operation_id,
        "operation_id reused with a different request payload"
    );
    Err(ApiError::idempotency_conflict(operation_id))
}

/// The typed `OPERATION_IN_DOUBT` rejection (intent without outcome)
/// served to the strict callers of [`execute_resolvable`] (see its
/// module docs): a concurrent caller may be executing the operation
/// right now, so the retry fails closed instead of guessing.
fn in_doubt_error(
    state: &SharedState,
    op_kind: &'static str,
    operation_id: &OperationId,
) -> ApiError {
    state.metrics.record_operation(op_kind, "in_doubt");
    tracing::warn!(
        kind = op_kind,
        operation_id = %operation_id,
        "journaled intent without a recorded outcome; the operation may be in \
         flight and is never re-executed"
    );
    ApiError::new(
        ApiErrorCode::OperationInDoubt,
        format!(
            "operation {operation_id} has a journaled intent without a recorded \
             outcome; it may be in flight and is never re-executed. To resolve it, \
             inspect the current state of the target resource, and if you need to \
             re-attempt the operation use a new operation_id"
        ),
    )
}

/// Rebuild the HTTP response for a recorded outcome.
///
/// This is the single status-reconstruction path shared by *every* replay:
/// the first journal lookup before `append_intent`, and the race branch
/// that re-resolves idempotency inside `append_intent`. A recorded
/// `http_status` (stage B2: the mobility routes answer `201` and `202`)
/// is served verbatim; otherwise successes replay as `200` with the
/// stored body and failures replay with the status reconstructed from
/// the recorded error code, so a replay is status- and byte-compatible
/// with the first caller's response regardless of which caller executed
/// the mutation.
fn replay_response(
    success: bool,
    http_status: Option<u16>,
    response: &Value,
) -> Result<Response, ApiError> {
    if let Some(status) = http_status {
        let status = StatusCode::from_u16(status).map_err(|_| {
            ApiError::new(
                ApiErrorCode::Internal,
                "recorded outcome carries an invalid HTTP status",
            )
        })?;
        return Ok(json_response(status, response));
    }
    if success {
        return Ok(json_response(StatusCode::OK, response));
    }
    let status = response
        .get("code")
        .and_then(Value::as_str)
        .and_then(status_for_wire_code)
        .ok_or_else(|| {
            ApiError::new(
                ApiErrorCode::Internal,
                "recorded failure outcome carries no recognizable error code",
            )
        })?;
    let status = StatusCode::from_u16(status).map_err(|_| {
        ApiError::new(
            ApiErrorCode::Internal,
            "recorded failure outcome carries an invalid HTTP status",
        )
    })?;
    Ok(json_response(status, response))
}

// ---------------------------------------------------------------------------
// Immutable request hashes
// ---------------------------------------------------------------------------
//
// The canonical request hashes in `volvisor-types` cover the request *body*
// (SHA-256 over a domain-separated serialization). For attach, grow and
// delete, the operation's target volume arrives via the URL path, and for
// detach the body additionally carries the `attachment_id`, which the typed
// `DetachVolumeRequest` does not include. Folding those target identities
// into the hashed `api_version` domain segment extends the SHA-256 domain to
// the *full* immutable wire request (path + body): two requests sharing an
// `operation_id` but targeting different objects hash differently and fail
// closed with `IDEMPOTENCY_CONFLICT` instead of replaying a foreign
// response. Both `api_version` (already validated as
// `volvisor.volume.v2`) and the ID charset exclude `#`, so the fold is
// unambiguous.

/// Clone a request with the target identities folded into the hashed
/// `api_version` segment (the field is validated as `volvisor.volume.v2`
/// before any hashing, so only the hash domain is affected).
fn folded_domain(api_version: &str, targets: &[&str]) -> String {
    let mut domain = format!("{api_version}#target");
    for target in targets {
        domain.push('#');
        domain.push_str(target);
    }
    domain
}

/// Immutable request hash for CreateVolume (the body carries the volume id).
pub(crate) fn create_hash(req: &CreateVolumeRequest) -> [u8; 32] {
    req.request_hash()
}

/// Immutable request hash for AttachVolume (path target folded in).
pub(crate) fn attach_hash(req: &AttachVolumeRequest, volume_id: &VolumeId) -> [u8; 32] {
    let mut hashed = req.clone();
    hashed.api_version = folded_domain(&req.api_version, &[volume_id.as_str()]);
    hashed.request_hash()
}

/// Immutable request hash for DetachVolume (path volume and body attachment
/// id folded in).
pub(crate) fn detach_hash(
    req: &DetachVolumeRequest,
    volume_id: &VolumeId,
    attachment_id: &AttachmentId,
) -> [u8; 32] {
    let mut hashed = req.clone();
    hashed.api_version = folded_domain(
        &req.api_version,
        &[volume_id.as_str(), attachment_id.as_str()],
    );
    hashed.request_hash()
}

/// Immutable request hash for GrowVolume (path target folded in).
pub(crate) fn grow_hash(req: &GrowVolumeRequest, volume_id: &VolumeId) -> [u8; 32] {
    let mut hashed = req.clone();
    hashed.api_version = folded_domain(&req.api_version, &[volume_id.as_str()]);
    hashed.request_hash()
}

/// Immutable request hash for DeleteVolume (path target folded in).
pub(crate) fn delete_hash(req: &DeleteVolumeRequest, volume_id: &VolumeId) -> [u8; 32] {
    let mut hashed = req.clone();
    hashed.api_version = folded_domain(&req.api_version, &[volume_id.as_str()]);
    hashed.request_hash()
}

// ---------------------------------------------------------------------------
// Mobility and peer-route journal identities (stage B2)
// ---------------------------------------------------------------------------
//
// The mobility routes and the internal peer routes journal through the
// same pipeline, but their `operation_id`s are **derived**, not
// consumer-supplied: the mobility request names a `migration_id`, and
// every act of that migration (consumer-facing or peer) must replay
// the recorded outcome across a daemon restart without anyone having
// remembered a random id. The derivation mirrors
// `volvisor_handoff::batch_operation_id`'s discipline —
// domain-separated SHA-256 over the migration identity and a route
// tag, rendered `mig-api-{tag}-{16 hex}` — and the request hash folds
// the same tag into its domain so two acts of one migration can never
// collide in the journal (a `prepare` and a `transfer` of the same
// migration are different operations even though both key on the
// migration id).

/// Derive the deterministic journal operation id for one mobility or
/// peer-route act of one migration: `mig-api-{tag}-{16 hex}` over a
/// domain-separated SHA-256 of the migration id and the route tag.
///
/// `tag` must be non-empty and use the identity charset (the fixed
/// route tags are; a caller-supplied tag is validated by
/// [`OperationId::new`] on the rendered result regardless).
///
/// # Errors
/// `INTERNAL` only if the derived string failed identity validation
/// (unreachable for the fixed route tags).
pub(crate) fn mobility_operation_id(
    migration_id: &MigrationId,
    tag: &str,
) -> Result<OperationId, ApiError> {
    let mut hasher = Sha256::new();
    hasher.update(b"volvisor.api.mobility.op.v1:");
    hasher.update(migration_id.as_str().as_bytes());
    hasher.update(b":");
    hasher.update(tag.as_bytes());
    let digest = hasher.finalize();
    let raw = format!("mig-api-{tag}-{}", hex16(&digest));
    OperationId::new(raw).map_err(|error| {
        ApiError::new(
            ApiErrorCode::Internal,
            format!("derived mobility operation id failed validation: {error}"),
        )
    })
}

/// The immutable request hash of one mobility or peer-route act: the
/// canonical JSON serialization of the (typed) request body, folded
/// with the route tag into one SHA-256 domain. `serde_json`'s `Value`
/// map ordering is deterministic for a given document, so the same
/// typed request always hashes identically — and a different body
/// under the same derived operation id fails closed with
/// `IDEMPOTENCY_CONFLICT`, exactly like the volume operations.
pub(crate) fn mobility_request_hash(tag: &str, body: &Value) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"volvisor.api.mobility.hash.v1:");
    hasher.update(tag.as_bytes());
    hasher.update(b":");
    // The canonical serialization of the typed request (deterministic
    // key order for a given document; `unwrap_or_default` mirrors the
    // typed requests' own hash discipline — serialization of these
    // shapes cannot fail in practice).
    hasher.update(serde_json::to_vec(body).unwrap_or_default());
    hasher.finalize().into()
}

/// The first 16 hex characters of a digest (the house
/// `volvisor-handoff` `hex_prefix` discipline, without `format!`).
fn hex16(digest: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

// ---------------------------------------------------------------------------
// Journal payloads (full wire request, credential material redacted by
// `execute` before anything is written; preserved otherwise for forensics)
// ---------------------------------------------------------------------------

/// Journal payload for CreateVolume.
pub(crate) fn create_payload(req: &CreateVolumeRequest) -> Result<Value, ApiError> {
    to_json_value(req)
}

/// Journal payload for a path-scoped operation (attach/grow/delete).
pub(crate) fn path_payload(volume_id: &VolumeId, body: &impl Serialize) -> Result<Value, ApiError> {
    let volume = to_json_value(volume_id)?;
    let request = to_json_value(body)?;
    Ok(serde_json::json!({ "volume_id": volume, "request": request }))
}

/// Journal payload for DetachVolume.
pub(crate) fn detach_payload(
    volume_id: &VolumeId,
    attachment_id: &AttachmentId,
    body: &DetachVolumeRequest,
) -> Result<Value, ApiError> {
    let volume = to_json_value(volume_id)?;
    let attachment = to_json_value(attachment_id)?;
    let request = to_json_value(body)?;
    Ok(serde_json::json!({
        "volume_id": volume,
        "attachment_id": attachment,
        "request": request,
    }))
}

/// Journal payload for an admin-surface operation (claim/release): the
/// target device identity plus the request body. The request hash is taken
/// from the typed request's `request_hash(&device_id)` (which excludes the
/// token), so the redaction performed by [`execute`] on this payload never
/// changes idempotency behavior — and a replay after token rotation still
/// resolves to the recorded outcome, by design.
pub(crate) fn admin_payload(
    device_id: &DeviceId,
    body: &impl Serialize,
) -> Result<Value, ApiError> {
    let device = to_json_value(device_id)?;
    let request = to_json_value(body)?;
    Ok(serde_json::json!({ "device_id": device, "request": request }))
}

/// Journal payload for a volume-scoped admin operation (adopt): the
/// target volume identity plus the (redacted) request body.
pub(crate) fn volume_payload(
    volume_id: &VolumeId,
    body: &impl Serialize,
) -> Result<Value, ApiError> {
    let volume = to_json_value(volume_id)?;
    let request = to_json_value(body)?;
    Ok(serde_json::json!({ "volume_id": volume, "request": request }))
}
