//! HTTP endpoint handlers.
//!
//! Mutating handlers are thin: validate the typed request, derive the
//! immutable request hash and journal payload, then delegate to
//! [`crate::ops::execute`], which owns the journal-before-mutate ordering.
//! Read-only handlers call the provider directly (no journal interaction).
//!
//! Logging follows SPEC-0002 section 9: operation kind, `operation_id` and
//! target identities only — never payloads (which may carry secret
//! references), never the admin token and never the admin-surface
//! authorization token.

// axum handlers consume their extractors by value; clippy's pass-by-value
// heuristics do not apply to the handler boundary.
#![allow(clippy::needless_pass_by_value)]

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use volvisor_handoff::{MobilityRequest, migration_not_enabled};
use volvisor_types::domain::VolumeClass;
use volvisor_types::request::{
    AdoptVolumeRequest, AttachVolumeRequest, CreateVolumeRequest, DeleteVolumeRequest,
    DetachVolumeRequest, DrainProof, GrowVolumeRequest, GrowVolumeResponse, ListVolumesResponse,
    MoveVolumeBackingRequest,
};
use volvisor_types::{
    ApiError, CapabilitySet, ClaimDeviceRequest, DeviceId, FencingProof, MigrationId, ProjectId,
    ReleaseDeviceRequest, VolumeId,
};

use crate::error::{ApiErrorReply, json_response, text_response, to_json_value};
use crate::extract::{RequireAdmin, ValidJson};
use crate::ops;
use crate::state::SharedState;

/// `POST /v2/volumes` — CreateVolume.
pub(crate) async fn create_volume(
    State(state): State<SharedState>,
    _admin: RequireAdmin,
    ValidJson(req): ValidJson<CreateVolumeRequest>,
) -> Result<Response, ApiErrorReply> {
    req.validate()?;
    tracing::info!(
        kind = ops::OP_CREATE_VOLUME,
        operation_id = %req.operation_id,
        volume_id = %req.volume_id,
        "accepting create_volume"
    );
    let payload = ops::create_payload(&req)?;
    let provider = state.provider.clone();
    ops::execute(
        &state,
        ops::OP_CREATE_VOLUME,
        req.operation_id.clone(),
        ops::create_hash(&req),
        payload,
        move || async move { provider.create_volume(&req).await },
    )
    .await
    .map_err(ApiErrorReply::from)
}

/// `GET /v2/volumes` — ListVolumes, optionally scoped to one project.
///
/// Unknown query parameters are ignored (a `serde_urlencoded` limitation);
/// an invalid `project_id` value is rejected with `INVALID_REQUEST`.
pub(crate) async fn list_volumes(
    State(state): State<SharedState>,
    query: Result<Query<ListVolumesQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, ApiErrorReply> {
    let query = query
        .map_err(|rejection| {
            ApiError::invalid_request(format!("invalid query string: {rejection}"))
        })?
        .0;
    let volumes = state
        .provider
        .list_volumes(query.project_id.as_ref())
        .await?;
    let body = to_json_value(&ListVolumesResponse { volumes })?;
    Ok(json_response(StatusCode::OK, &body))
}

/// `GET /v2/volumes/{volume_id}` — InspectVolume.
pub(crate) async fn inspect_volume(
    State(state): State<SharedState>,
    Path(volume_id): Path<String>,
) -> Result<Response, ApiErrorReply> {
    let volume_id = parse_volume_id(&volume_id)?;
    let inspected = state.provider.inspect_volume(&volume_id).await?;
    let body = to_json_value(&inspected)?;
    Ok(json_response(StatusCode::OK, &body))
}

/// `POST /v2/volumes/{volume_id}/attach` — AttachVolume.
pub(crate) async fn attach_volume(
    State(state): State<SharedState>,
    _admin: RequireAdmin,
    Path(volume_id): Path<String>,
    ValidJson(req): ValidJson<AttachVolumeRequest>,
) -> Result<Response, ApiErrorReply> {
    req.validate()?;
    let volume_id = parse_volume_id(&volume_id)?;
    tracing::info!(
        kind = ops::OP_ATTACH_VOLUME,
        operation_id = %req.operation_id,
        volume_id = %volume_id,
        attachment_id = %req.attachment_id,
        "accepting attach_volume"
    );
    let payload = ops::path_payload(&volume_id, &req)?;
    let provider = state.provider.clone();
    ops::execute(
        &state,
        ops::OP_ATTACH_VOLUME,
        req.operation_id.clone(),
        ops::attach_hash(&req, &volume_id),
        payload,
        move || async move { provider.attach_volume(&volume_id, &req).await },
    )
    .await
    .map_err(ApiErrorReply::from)
}

/// `POST /v2/volumes/{volume_id}/detach` — DetachVolume.
///
/// The wire body carries the `attachment_id` alongside the typed
/// [`DetachVolumeRequest`] fields (the typed request deliberately omits it:
/// the provider trait passes the attachment identity separately).
pub(crate) async fn detach_volume(
    State(state): State<SharedState>,
    _admin: RequireAdmin,
    Path(volume_id): Path<String>,
    ValidJson(wire): ValidJson<DetachRequestWire>,
) -> Result<Response, ApiErrorReply> {
    let volume_id = parse_volume_id(&volume_id)?;
    let attachment_id = wire.attachment_id.clone();
    let req = wire.into_typed_request();
    req.validate()?;
    tracing::info!(
        kind = ops::OP_DETACH_VOLUME,
        operation_id = %req.operation_id,
        volume_id = %volume_id,
        attachment_id = %attachment_id,
        "accepting detach_volume"
    );
    let payload = ops::detach_payload(&volume_id, &attachment_id, &req)?;
    let provider = state.provider.clone();
    ops::execute(
        &state,
        ops::OP_DETACH_VOLUME,
        req.operation_id.clone(),
        ops::detach_hash(&req, &volume_id, &attachment_id),
        payload,
        move || async move {
            provider
                .detach_volume(&volume_id, &attachment_id, &req)
                .await
        },
    )
    .await
    .map_err(ApiErrorReply::from)
}

/// `POST /v2/volumes/{volume_id}/grow` — GrowVolume.
///
/// Only the state-independent envelope (api version, sector alignment) is
/// validated here, before the journal: rejections leave no journal record
/// and the `operation_id` stays reusable — the same pre-journal-rejection
/// invariant every other mutating endpoint upholds. The grow-only check
/// needs the current size; validating it against a read taken *before* the
/// journal lookup would break replay-safety (a replayed grow after later
/// grows or after deletion would be rejected as a shrink/404 instead of
/// replaying the recorded outcome). The provider therefore re-validates
/// grow-only authoritatively under its own lock; a rejection is then
/// journaled and replays byte-compatibly.
pub(crate) async fn grow_volume(
    State(state): State<SharedState>,
    _admin: RequireAdmin,
    Path(volume_id): Path<String>,
    ValidJson(req): ValidJson<GrowVolumeRequest>,
) -> Result<Response, ApiErrorReply> {
    let volume_id = parse_volume_id(&volume_id)?;
    req.validate_envelope()?;
    tracing::info!(
        kind = ops::OP_GROW_VOLUME,
        operation_id = %req.operation_id,
        volume_id = %volume_id,
        "accepting grow_volume"
    );
    let payload = ops::path_payload(&volume_id, &req)?;
    let provider = state.provider.clone();
    // P6-B (ADR-0006 first slice part 1): when the daemon wired a
    // grow-notification engine, the provider's response is composed
    // with the real `guest_notification_status` INSIDE this closure —
    // after the provider resized the backing, before the journal
    // records the outcome — so the recorded outcome carries the
    // notified/retry_required/not_applicable the client saw and
    // replays byte-compatibly. The notification is deliberately
    // infallible at this seam: the grow already succeeded, and the
    // contract §4A's partial-failure rule makes every
    // notification-side refusal a recorded `retry_required`, never
    // the grow's failure.
    let notifier = state.grow_notifier.clone();
    ops::execute(
        &state,
        ops::OP_GROW_VOLUME,
        req.operation_id.clone(),
        ops::grow_hash(&req, &volume_id),
        payload,
        move || async move {
            let response = provider.grow_volume(&volume_id, &req).await?;
            let guest_notification_status = match &notifier {
                Some(notifier) => notifier.notify_grow(&volume_id, response.effective_size_bytes),
                // No engine wired (providers without a VMM
                // integration): the provider's placeholder status is
                // the honest answer.
                None => response.guest_notification_status,
            };
            Ok(GrowVolumeResponse {
                guest_notification_status,
                ..response
            })
        },
    )
    .await
    .map_err(ApiErrorReply::from)
}

/// `POST /v2/volumes/{volume_id}/move-backing` — MoveVolumeBackingOnline
/// (contract section 4A).
///
/// Terminal-class volume op: consumer-supplied `operation_id`, the
/// journaled outcome replays byte-for-byte. The state-independent
/// envelope validates before the journal (rejections leave no record
/// and the `operation_id` stays reusable); every scope/capacity/
/// state check lives in the provider under its lock, where a typed
/// refusal is journaled and replays byte-compatibly.
///
/// The response reports the state the move reached **inside this
/// call's supervision window**: `COMPLETE` (verified — the source
/// freed only then), `COPYING` (the window expired with the move
/// progressing; a fresh `operation_id` re-attaches to the same move
/// and the daemon's retry reconcile completes it independently), or
/// `IN_DOUBT` (an unverified outcome with the source intact). A
/// `COPYING` outcome is a truthful observation of that call's
/// window, not a claim that the move finished.
pub(crate) async fn move_volume_backing(
    State(state): State<SharedState>,
    _admin: RequireAdmin,
    Path(volume_id): Path<String>,
    ValidJson(req): ValidJson<MoveVolumeBackingRequest>,
) -> Result<Response, ApiErrorReply> {
    let volume_id = parse_volume_id(&volume_id)?;
    req.validate_envelope()?;
    tracing::info!(
        kind = ops::OP_MOVE_VOLUME_BACKING,
        operation_id = %req.operation_id,
        volume_id = %volume_id,
        target_pool_id = %req.target_pool_id,
        "accepting move_volume_backing"
    );
    let payload = ops::path_payload(&volume_id, &req)?;
    let provider = state.provider.clone();
    ops::execute(
        &state,
        ops::OP_MOVE_VOLUME_BACKING,
        req.operation_id.clone(),
        ops::move_hash(&req, &volume_id),
        payload,
        move || async move { provider.move_volume_backing(&volume_id, &req).await },
    )
    .await
    .map_err(ApiErrorReply::from)
}

/// `DELETE /v2/volumes/{volume_id}` — DeleteVolume.
pub(crate) async fn delete_volume(
    State(state): State<SharedState>,
    _admin: RequireAdmin,
    Path(volume_id): Path<String>,
    ValidJson(req): ValidJson<DeleteVolumeRequest>,
) -> Result<Response, ApiErrorReply> {
    req.validate()?;
    let volume_id = parse_volume_id(&volume_id)?;
    tracing::info!(
        kind = ops::OP_DELETE_VOLUME,
        operation_id = %req.operation_id,
        volume_id = %volume_id,
        "accepting delete_volume"
    );
    let payload = ops::path_payload(&volume_id, &req)?;
    let provider = state.provider.clone();
    ops::execute(
        &state,
        ops::OP_DELETE_VOLUME,
        req.operation_id.clone(),
        ops::delete_hash(&req, &volume_id),
        payload,
        move || async move {
            provider.delete_volume(&volume_id, &req).await?;
            Ok(DeleteVolumeAck {
                deleted: volume_id.clone(),
            })
        },
    )
    .await
    .map_err(ApiErrorReply::from)
}

/// `GET /v2/capabilities` — provider name, capability set and served classes.
pub(crate) async fn capabilities(
    State(state): State<SharedState>,
) -> Result<Response, ApiErrorReply> {
    let body = to_json_value(&CapabilitiesResponse {
        provider: state.provider.name().to_owned(),
        capabilities: state.provider.capabilities(),
        supported_classes: state.provider.supported_classes().to_vec(),
    })?;
    Ok(json_response(StatusCode::OK, &body))
}

// ---------------------------------------------------------------------------
// Admin surface (device enrollment) — SPEC-0002 section 3
// ---------------------------------------------------------------------------

/// The typed `NOT_FOUND` rejection served when the configured provider does
/// not implement an admin surface.
fn admin_surface_unavailable() -> ApiError {
    ApiError::not_found("admin surface not available for this provider")
}

/// The typed 404 for the adopt route on providers without an adoption
/// surface (every non-witness-managed class).
fn adoption_surface_unavailable() -> ApiError {
    ApiError::not_found("adoption surface not available for this provider")
}

/// `GET /v2/admin/devices` — read-only device discovery.
///
/// Privileged (admin token required even though it is a read): the device
/// inventory is host-level information. No journaling: the operation is
/// read-only and never mutates anything.
pub(crate) async fn admin_list_devices(
    State(state): State<SharedState>,
    _admin: RequireAdmin,
) -> Result<Response, ApiErrorReply> {
    let admin = state
        .admin
        .clone()
        .ok_or_else(|| ApiErrorReply(admin_surface_unavailable()))?;
    let devices = admin.discover_devices().await?;
    let body = to_json_value(&devices)?;
    Ok(json_response(StatusCode::OK, &body))
}

/// `POST /v2/admin/devices/{device_id}/claim` — claim a device for a pool.
///
/// Routed through the same journal pipeline as every other mutation: the
/// (redacted) intent is durable *before* the destructive provider action.
/// The request hash comes from [`ClaimDeviceRequest::request_hash`] (device
/// id folded in, token excluded), so a replay after token rotation still
/// returns the recorded outcome — by design.
pub(crate) async fn claim_device(
    State(state): State<SharedState>,
    _admin: RequireAdmin,
    Path(device_id): Path<String>,
    ValidJson(req): ValidJson<ClaimDeviceRequest>,
) -> Result<Response, ApiErrorReply> {
    // Route-level availability check first: a provider without an admin
    // surface answers 404 for any request shape.
    let admin = state
        .admin
        .clone()
        .ok_or_else(|| ApiErrorReply(admin_surface_unavailable()))?;
    req.validate()?;
    let device_id = parse_device_id(&device_id)?;
    tracing::info!(
        kind = ops::OP_CLAIM_DEVICE,
        operation_id = %req.operation_id,
        device_id = %device_id,
        "accepting claim_device"
    );
    let payload = ops::admin_payload(&device_id, &req)?;
    ops::execute(
        &state,
        ops::OP_CLAIM_DEVICE,
        req.operation_id.clone(),
        req.request_hash(&device_id),
        payload,
        move || async move { admin.claim_device(&device_id, &req).await },
    )
    .await
    .map_err(ApiErrorReply::from)
}

/// `POST /v2/admin/devices/{device_id}/release` — release a claimed device.
///
/// Journaled exactly like claim (see [`claim_device`]); refuses while
/// volumes still reside on the device's pool.
pub(crate) async fn release_device(
    State(state): State<SharedState>,
    _admin: RequireAdmin,
    Path(device_id): Path<String>,
    ValidJson(req): ValidJson<ReleaseDeviceRequest>,
) -> Result<Response, ApiErrorReply> {
    // Route-level availability check first (see `claim_device`).
    let admin = state
        .admin
        .clone()
        .ok_or_else(|| ApiErrorReply(admin_surface_unavailable()))?;
    req.validate()?;
    let device_id = parse_device_id(&device_id)?;
    tracing::info!(
        kind = ops::OP_RELEASE_DEVICE,
        operation_id = %req.operation_id,
        device_id = %device_id,
        "accepting release_device"
    );
    let payload = ops::admin_payload(&device_id, &req)?;
    ops::execute(
        &state,
        ops::OP_RELEASE_DEVICE,
        req.operation_id.clone(),
        req.request_hash(&device_id),
        payload,
        move || async move {
            admin.release_device(&device_id, &req).await?;
            Ok(ReleaseDeviceAck {
                released: device_id.clone(),
            })
        },
    )
    .await
    .map_err(ApiErrorReply::from)
}

/// `POST /v2/admin/nearline/{volume_id}/adopt` — the adopt-and-promote
/// admin operation (P4a plan §5/§6).
///
/// Routed through the journal pipeline like every privileged mutation:
/// the (redacted) intent is durable before the provider call, and the
/// volume id plus `allow_loss` are folded into the request hash — a
/// replay with a different `allow_loss` for the same operation id is an
/// `IDEMPOTENCY_CONFLICT`, never a silent second attempt. The
/// classification-based refusals (`unsafe`, unauthorized
/// `possible_loss`) are *successful* journaled responses with
/// `volume: null`, so they replay exactly like promotions.
pub(crate) async fn adopt_volume(
    State(state): State<SharedState>,
    _admin: RequireAdmin,
    Path(volume_id): Path<String>,
    ValidJson(req): ValidJson<AdoptVolumeRequest>,
) -> Result<Response, ApiErrorReply> {
    // Route-level availability check first (see `claim_device`).
    let adoption = state
        .adoption
        .clone()
        .ok_or_else(|| ApiErrorReply(adoption_surface_unavailable()))?;
    req.validate()?;
    let volume_id = parse_volume_id(&volume_id)?;
    tracing::info!(
        kind = ops::OP_ADOPT_VOLUME,
        operation_id = %req.operation_id,
        volume_id = %volume_id,
        allow_loss = req.allow_loss,
        "accepting adopt_volume"
    );
    let payload = ops::volume_payload(&volume_id, &req)?;
    ops::execute(
        &state,
        ops::OP_ADOPT_VOLUME,
        req.operation_id.clone(),
        req.request_hash(&volume_id),
        payload,
        move || async move { adoption.adopt_volume(&volume_id, req.allow_loss).await },
    )
    .await
    .map_err(ApiErrorReply::from)
}

/// `POST /v2/admin/nearline/{volume_id}/clear-cut-marker` — the
/// operator-driven residue cleanup of an interrupted handoff (P4b plan
/// §6): clear a stale migration-cut marker once this host provably no
/// longer writes the volume (it is Secondary here, or the caller
/// supplies a fencing proof the witness corroborates).
///
/// Routed through the journal pipeline like every privileged mutation
/// (rule 8): the volume id and the (optional) fencing proof are folded
/// into the request hash, so a replay with a different proof under the
/// same operation id is an `IDEMPOTENCY_CONFLICT`, never a silent
/// second clearing. The surface's refusals (a Primary/writer without a
/// corroborated proof, no marker present) are typed errors and journal
/// as failures, replaying verbatim — fail-closed, exactly like the
/// adopt refusals.
pub(crate) async fn clear_cut_marker(
    State(state): State<SharedState>,
    _admin: RequireAdmin,
    Path(volume_id): Path<String>,
    ValidJson(req): ValidJson<ClearCutMarkerRequest>,
) -> Result<Response, ApiErrorReply> {
    // Route-level availability check first (see `claim_device`).
    let handoff = state
        .handoff
        .clone()
        .ok_or_else(|| ApiErrorReply(handoff_surface_unavailable()))?;
    volvisor_types::validate_api_version(&req.api_version)?;
    let volume_id = parse_volume_id(&volume_id)?;
    tracing::info!(
        kind = ops::OP_CLEAR_CUT_MARKER,
        operation_id = %req.operation_id,
        volume_id = %volume_id,
        "accepting clear_cut_marker"
    );
    let payload = ops::volume_payload(&volume_id, &req)?;
    let hash = ops::mobility_request_hash("clear-cut-marker", &payload);
    let proof = req.fencing_proof;
    ops::execute(
        &state,
        ops::OP_CLEAR_CUT_MARKER,
        req.operation_id.clone(),
        hash,
        payload,
        move || async move { handoff.clear_cut_marker(&volume_id, proof.as_ref()).await },
    )
    .await
    .map_err(ApiErrorReply::from)
}

// ---------------------------------------------------------------------------
// Mobility surface (P4b plan §6, stage B2) — consumer routes
// ---------------------------------------------------------------------------

/// The typed 404 for the check-mobility route on providers without a
/// handoff surface (every class without a coordinated-handoff engine).
fn handoff_surface_unavailable() -> ApiError {
    ApiError::not_found("handoff surface not available for this provider")
}

/// `POST /v2/vms/{vm_id}/check-mobility` — `CheckVmStorageMobility`:
/// the VM-wide eligibility answer across every attached volume this
/// provider holds (rule 6), read from the provider's handoff surface.
///
/// Read-only: no suspension, no witness mutation, no journal record —
/// there is nothing privileged to replay. Admin token required (the
/// eligibility answer names a consumer's whole writable set).
pub(crate) async fn check_mobility(
    State(state): State<SharedState>,
    _admin: RequireAdmin,
    Path(vm_id): Path<String>,
    ValidJson(req): ValidJson<CheckMobilityRequest>,
) -> Result<Response, ApiErrorReply> {
    let handoff = state
        .handoff
        .clone()
        .ok_or_else(|| ApiErrorReply(handoff_surface_unavailable()))?;
    if req.target_host.as_str().is_empty() {
        return Err(ApiErrorReply(ApiError::invalid_request(
            "target_host must not be empty",
        )));
    }
    let report = handoff.handoff_eligibility(&vm_id).await?;
    let body = to_json_value(&report)?;
    Ok(json_response(StatusCode::OK, &body))
}

/// `POST /v2/migrations` — `PrepareNearlineHandoff`: verify the
/// participant set and the destination, then persist the `PREPARED`
/// migration record. Answers `201`; idempotent by `migration_id`
/// (identical content re-serves the record, different content is the
/// typed conflict). Journaled through the shared pipeline with a
/// derived operation id (`mig-api-prepare-{16hex}`), so a retry after
/// a lost response replays the recorded outcome byte-for-byte.
pub(crate) async fn prepare_migration(
    State(state): State<SharedState>,
    _admin: RequireAdmin,
    ValidJson(req): ValidJson<MobilityRequest>,
) -> Result<Response, ApiErrorReply> {
    let migration = state
        .migration
        .clone()
        .ok_or_else(|| ApiErrorReply(migration_not_enabled()))?;
    req.validate()?;
    let operation_id = ops::mobility_operation_id(&req.migration_id, "prepare")?;
    let payload = to_json_value(&req)?;
    let hash = ops::mobility_request_hash("prepare", &payload);
    tracing::info!(
        kind = ops::OP_MIGRATION_PREPARE,
        operation_id = %operation_id,
        migration_id = %req.migration_id,
        vm_id = %req.vm_id,
        "accepting prepare_nearline_handoff"
    );
    ops::execute_with_status(
        &state,
        ops::OP_MIGRATION_PREPARE,
        operation_id,
        hash,
        payload,
        StatusCode::CREATED,
        move || async move { migration.prepare(req).await },
    )
    .await
    .map_err(ApiErrorReply::from)
}

/// `POST /v2/migrations/{migration_id}/transfer` —
/// `BarrierAndTransfer`: record the consumer's proof as
/// **corroboration** (plan §6: recorded, never trusted — volvisor
/// performs and verifies its own pause and its own suspension proof)
/// and start the long-running drive. Answers `202` with the
/// observation at drive start; progress is read through
/// `GET /v2/migrations/{migration_id}`.
///
/// Journaled with the strict in-doubt rule: the drive task may be
/// mid-flight, so an intent-without-outcome retry fails closed with
/// `OPERATION_IN_DOUBT` (never a second drive).
pub(crate) async fn transfer_migration(
    State(state): State<SharedState>,
    _admin: RequireAdmin,
    Path(migration_id): Path<String>,
    ValidJson(req): ValidJson<TransferRequestWire>,
) -> Result<Response, ApiErrorReply> {
    let migration = state
        .migration
        .clone()
        .ok_or_else(|| ApiErrorReply(migration_not_enabled()))?;
    let migration_id = parse_migration_id(&migration_id)?;
    let operation_id = ops::mobility_operation_id(&migration_id, "transfer")?;
    let payload = to_json_value(&TransferJournalBody {
        migration_id: migration_id.clone(),
        vm_paused_and_io_drained_proof: req.vm_paused_and_io_drained_proof.clone(),
    })?;
    let hash = ops::mobility_request_hash("transfer", &payload);
    tracing::info!(
        kind = ops::OP_MIGRATION_TRANSFER,
        operation_id = %operation_id,
        migration_id = %migration_id,
        "accepting barrier_and_transfer"
    );
    let proof = req.vm_paused_and_io_drained_proof;
    ops::execute_with_status(
        &state,
        ops::OP_MIGRATION_TRANSFER,
        operation_id,
        hash,
        payload,
        StatusCode::ACCEPTED,
        move || async move { migration.transfer(&migration_id, proof).await },
    )
    .await
    .map_err(ApiErrorReply::from)
}

/// `GET /v2/migrations/{migration_id}` — `ObserveHandoff`: the durable
/// observation (state, append-only trace, participants, in-doubt
/// detail), or the typed `NOT_FOUND` for an unknown migration.
/// Read-only: no journal record.
pub(crate) async fn observe_migration(
    State(state): State<SharedState>,
    Path(migration_id): Path<String>,
) -> Result<Response, ApiErrorReply> {
    let migration = state
        .migration
        .clone()
        .ok_or_else(|| ApiErrorReply(migration_not_enabled()))?;
    let migration_id = parse_migration_id(&migration_id)?;
    let Some(summary) = migration.observe(&migration_id)? else {
        return Err(ApiErrorReply(ApiError::not_found(format!(
            "no migration {migration_id} on this daemon"
        ))));
    };
    let body = to_json_value(&summary)?;
    Ok(json_response(StatusCode::OK, &body))
}

/// `POST /v2/migrations/{migration_id}/abort` — the pre-cut abort: the
/// G5-ordered rollback voids every recorded barrier before anything is
/// resumed, failing closed into terminal `IN_DOUBT` when the void
/// cannot be confirmed. The coordinator refuses every cut-or-later
/// state typed (there is no abort handler past the point of no
/// return). Journaled like every mobility mutation.
pub(crate) async fn abort_migration(
    State(state): State<SharedState>,
    _admin: RequireAdmin,
    Path(migration_id): Path<String>,
) -> Result<Response, ApiErrorReply> {
    let migration = state
        .migration
        .clone()
        .ok_or_else(|| ApiErrorReply(migration_not_enabled()))?;
    let migration_id = parse_migration_id(&migration_id)?;
    let operation_id = ops::mobility_operation_id(&migration_id, "abort")?;
    let payload = serde_json::json!({ "migration_id": migration_id });
    let hash = ops::mobility_request_hash("abort", &payload);
    tracing::info!(
        kind = ops::OP_MIGRATION_ABORT,
        operation_id = %operation_id,
        migration_id = %migration_id,
        "accepting migration abort"
    );
    ops::execute(
        &state,
        ops::OP_MIGRATION_ABORT,
        operation_id,
        hash,
        payload,
        move || async move { migration.abort(&migration_id).await },
    )
    .await
    .map_err(ApiErrorReply::from)
}

/// Parse and validate the `{migration_id}` path segment.
fn parse_migration_id(raw: &str) -> Result<MigrationId, ApiError> {
    MigrationId::try_from(raw.to_owned())
        .map_err(|err| ApiError::invalid_request(format!("invalid migration_id in path: {err}")))
}

/// `GET /healthz` — daemon liveness only.
///
/// This says nothing about volume health: a volume's health lives on its
/// inspect response (`health`/`backend_health`, honestly `Unknown` until
/// proven). Liveness and volume health are never conflated.
pub(crate) async fn healthz() -> Response {
    json_response(StatusCode::OK, &serde_json::json!({ "status": "ok" }))
}

/// `GET /metrics` — Prometheus text exposition.
pub(crate) async fn metrics(State(state): State<SharedState>) -> Response {
    text_response(StatusCode::OK, state.metrics.render())
}

/// Fallback for unmatched routes: the contract error shape with `NOT_FOUND`.
pub(crate) async fn not_found() -> Response {
    crate::error::error_response(&ApiError::not_found("unknown route"))
}

/// Parse and validate the `{volume_id}` path segment into a [`VolumeId`].
fn parse_volume_id(raw: &str) -> Result<VolumeId, ApiError> {
    VolumeId::try_from(raw)
        .map_err(|err| ApiError::invalid_request(format!("invalid volume_id in path: {err}")))
}

/// Parse and validate the `{device_id}` path segment into a [`DeviceId`].
fn parse_device_id(raw: &str) -> Result<DeviceId, ApiError> {
    DeviceId::try_from(raw)
        .map_err(|err| ApiError::invalid_request(format!("invalid device_id in path: {err}")))
}

/// Query parameters of `GET /v2/volumes`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ListVolumesQuery {
    /// Restrict the listing to this project.
    project_id: Option<ProjectId>,
}

/// Wire body of the detach endpoint: the typed detach request plus the
/// `attachment_id` it targets.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DetachRequestWire {
    /// Must equal `volvisor.volume.v2`.
    api_version: String,
    /// Idempotency key.
    operation_id: volvisor_types::OperationId,
    /// The attachment to detach.
    attachment_id: volvisor_types::AttachmentId,
    /// Expected current attachment generation (typed conflict when stale).
    expected_attachment_generation: u64,
    /// Proof that writes cannot be in flight.
    vm_stopped_or_io_drained_proof: DrainProof,
}

impl DetachRequestWire {
    /// Convert into the typed request passed to the provider.
    fn into_typed_request(self) -> DetachVolumeRequest {
        DetachVolumeRequest {
            api_version: self.api_version,
            operation_id: self.operation_id,
            expected_attachment_generation: self.expected_attachment_generation,
            vm_stopped_or_io_drained_proof: self.vm_stopped_or_io_drained_proof,
        }
    }
}

/// DELETE response body: an explicit acknowledgment naming the deleted
/// volume (DELETE has no natural body; the journal stores exactly these
/// bytes so replays are byte-compatible).
#[derive(Debug, Serialize)]
struct DeleteVolumeAck {
    /// The deleted volume identity.
    deleted: VolumeId,
}

/// Admin release response body: an explicit acknowledgment naming the
/// released device (the provider returns `()`; the journal stores exactly
/// these bytes so replays are byte-compatible).
#[derive(Debug, Serialize)]
struct ReleaseDeviceAck {
    /// The released device identity.
    released: DeviceId,
}

/// `GET /v2/capabilities` response body.
#[derive(Debug, Serialize)]
struct CapabilitiesResponse {
    /// Provider implementation name (diagnostics only, never a secret).
    provider: String,
    /// Advertised capability set.
    capabilities: CapabilitySet,
    /// Volume classes served by this provider instance.
    supported_classes: Vec<VolumeClass>,
}

/// `POST /v2/admin/nearline/{volume_id}/clear-cut-marker` request body.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClearCutMarkerRequest {
    /// Must equal `volvisor.volume.v2`.
    api_version: String,
    /// Idempotency key.
    operation_id: volvisor_types::OperationId,
    /// The witness's durable retirement statement that authorizes
    /// clearing the marker while this host still holds the volume
    /// Primary (the surface corroborates it against the witness);
    /// `None` is sufficient only when the volume is Secondary here —
    /// the surface refuses a live writer typed, never on the
    /// caller's say-so.
    fencing_proof: Option<FencingProof>,
}

/// `POST /v2/vms/{vm_id}/check-mobility` request body.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckMobilityRequest {
    /// The candidate destination host (diagnostics on the report; the
    /// eligibility answer itself is target-independent in v1).
    target_host: volvisor_types::HostId,
}

/// `POST /v2/migrations/{migration_id}/transfer` request body: the
/// consumer's pause/drain proof, recorded verbatim as corroboration
/// (plan §6 — recorded, never trusted; see [`transfer_migration`]).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TransferRequestWire {
    /// The consumer's proof that the VM is paused and its I/O drained.
    vm_paused_and_io_drained_proof: serde_json::Value,
}

/// The journal payload of `BarrierAndTransfer`: the migration identity
/// (folded into the immutable request hash together with the route
/// tag) plus the verbatim corroboration proof.
#[derive(Clone, Debug, Serialize)]
struct TransferJournalBody {
    /// The migration being transferred.
    migration_id: MigrationId,
    /// The consumer's proof, recorded verbatim.
    vm_paused_and_io_drained_proof: serde_json::Value,
}
