//! Journal-before-mutate execution pipeline shared by every mutating
//! endpoint (Volume API v2 section 7; AGENTS rule 8).
//!
//! [`execute`] is the single correctness path for mutations. The ordering is
//! strict:
//!
//! 1. *(in the handler)* typed request validation — rejections happen before
//!    anything is journaled, so they leave no journal record;
//! 2. **journal lookup** — a recorded outcome for the same `operation_id` and
//!    immutable request hash is replayed byte-for-byte *without executing*;
//!    a same-hash intent without an outcome fails closed with
//!    `OPERATION_IN_DOUBT` (the operation may be in flight; it is never
//!    re-executed); a different hash for the same `operation_id` is an
//!    `IDEMPOTENCY_CONFLICT`;
//! 3. **journal intent** — durably fsynced *before* the provider mutation;
//! 4. **execute** the provider call (never while holding the journal lock),
//!    then journal the outcome — the serialized response on success, the
//!    `{"code","message"}` error body on failure — and return it. The stored
//!    success body is the exact body served to the first caller, so replays
//!    are byte-compatible. A recorded *failure* outcome is also replayed
//!    verbatim, with the status reconstructed from its recorded code.
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
use std::sync::MutexGuard;

use axum::http::StatusCode;
use axum::response::Response;
use serde::Serialize;
use serde_json::Value;
use volvisor_journal::{IntentAppend, Journal, RecordedOutcome};
use volvisor_types::request::{
    AttachVolumeRequest, CreateVolumeRequest, DeleteVolumeRequest, DetachVolumeRequest,
    GrowVolumeRequest,
};
use volvisor_types::{ApiError, ApiErrorBody, ApiErrorCode, AttachmentId, OperationId, VolumeId};

use crate::error::{json_response, status_for_wire_code, to_json_value};
use crate::state::SharedState;

/// Operation kind: create volume.
pub(crate) const OP_CREATE_VOLUME: &str = "create_volume";
/// Operation kind: attach volume.
pub(crate) const OP_ATTACH_VOLUME: &str = "attach_volume";
/// Operation kind: detach volume.
pub(crate) const OP_DETACH_VOLUME: &str = "detach_volume";
/// Operation kind: grow volume.
pub(crate) const OP_GROW_VOLUME: &str = "grow_volume";
/// Operation kind: delete volume.
pub(crate) const OP_DELETE_VOLUME: &str = "delete_volume";

/// Execute one mutating operation through the journal pipeline.
///
/// `run` performs the provider mutation; it is invoked only after the intent
/// is durable, and its result is journaled before the response is returned.
/// The guard returned by the journal lock is never held while `run`'s future
/// is polled.
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
    // (2) Resolve idempotency from the replay-derived registry.
    let recorded = {
        let journal = lock_journal(state)?;
        journal.lookup(&operation_id)
    };
    if let Some(entry) = recorded {
        if entry.request_hash != request_hash {
            return idempotency_conflict(state, op_kind, &operation_id);
        }
        return match entry.outcome {
            Some(outcome) => {
                state.metrics.record_operation(op_kind, "replayed");
                tracing::info!(
                    kind = op_kind,
                    operation_id = %operation_id,
                    "replaying recorded outcome without executing"
                );
                Ok(replay_response(&outcome)?)
            }
            None => operation_in_doubt(state, op_kind, &operation_id),
        };
    }

    // (3) Journal the intent durably BEFORE the provider mutation. Between
    // the lookup above and this append another request may have raced us;
    // append_intent re-resolves idempotency under the lock, so every outcome
    // of that race is handled below.
    let append = {
        let mut journal = lock_journal(state)?;
        journal.append_intent(operation_id.clone(), request_hash, op_kind, payload)
    }?;
    match append {
        IntentAppend::New => {}
        IntentAppend::Replayed(response) => {
            return match response {
                Some(body) => {
                    state.metrics.record_operation(op_kind, "replayed");
                    tracing::info!(
                        kind = op_kind,
                        operation_id = %operation_id,
                        "replaying concurrently recorded outcome without executing"
                    );
                    Ok(json_response(StatusCode::OK, &body))
                }
                None => operation_in_doubt(state, op_kind, &operation_id),
            };
        }
        IntentAppend::AlreadyInFlight => {
            return operation_in_doubt(state, op_kind, &operation_id);
        }
    }

    // (4) Execute the mutation. The journal guard is NOT held here.
    let result = run().await;
    match result {
        Ok(value) => {
            let body = to_json_value(&value)?;
            let journaled = {
                let mut journal = lock_journal(state)?;
                journal.append_outcome(operation_id.clone(), true, body.clone())
            };
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
            Ok(json_response(StatusCode::OK, &body))
        }
        Err(error) => {
            let body = to_json_value(&ApiErrorBody::from(error.clone()))?;
            let journaled = {
                let mut journal = lock_journal(state)?;
                journal.append_outcome(operation_id.clone(), false, body)
            };
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

/// A typed `OPERATION_IN_DOUBT` rejection (intent without outcome).
fn operation_in_doubt(
    state: &SharedState,
    op_kind: &'static str,
    operation_id: &OperationId,
) -> Result<Response, ApiError> {
    state.metrics.record_operation(op_kind, "in_doubt");
    tracing::warn!(
        kind = op_kind,
        operation_id = %operation_id,
        "journaled intent without a recorded outcome; the operation may be in \
         flight and is never re-executed"
    );
    Err(ApiError::new(
        ApiErrorCode::OperationInDoubt,
        format!(
            "operation {operation_id} has a journaled intent without a recorded \
             outcome; it may be in flight and is never re-executed"
        ),
    ))
}

/// Rebuild the response for a recorded outcome.
///
/// Successes replay as `200` with the stored body; failures replay with the
/// status reconstructed from the recorded error code and the stored body, so
/// a replay is status- and byte-compatible with the first caller's response.
fn replay_response(outcome: &RecordedOutcome) -> Result<Response, ApiError> {
    if outcome.success {
        return Ok(json_response(StatusCode::OK, &outcome.response));
    }
    let status = outcome
        .response
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
    Ok(json_response(status, &outcome.response))
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
// Journal payloads (full wire request, preserved verbatim for forensics)
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
