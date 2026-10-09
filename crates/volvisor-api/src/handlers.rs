//! HTTP endpoint handlers.
//!
//! Mutating handlers are thin: validate the typed request, derive the
//! immutable request hash and journal payload, then delegate to
//! [`crate::ops::execute`], which owns the journal-before-mutate ordering.
//! Read-only handlers call the provider directly (no journal interaction).
//!
//! Logging follows SPEC-0002 section 9: operation kind, `operation_id` and
//! target identities only — never payloads (which may carry secret
//! references) and never the admin token.

// axum handlers consume their extractors by value; clippy's pass-by-value
// heuristics do not apply to the handler boundary.
#![allow(clippy::needless_pass_by_value)]

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use volvisor_types::domain::VolumeClass;
use volvisor_types::request::{
    AttachVolumeRequest, CreateVolumeRequest, DeleteVolumeRequest, DetachVolumeRequest, DrainProof,
    GrowVolumeRequest, ListVolumesResponse,
};
use volvisor_types::{ApiError, CapabilitySet, ProjectId, VolumeId};

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
/// The grow-only check needs the current size, so a read-only inspect
/// precedes the journal; the provider re-validates under its own lock, so a
/// concurrent change between the two cannot slip through.
pub(crate) async fn grow_volume(
    State(state): State<SharedState>,
    _admin: RequireAdmin,
    Path(volume_id): Path<String>,
    ValidJson(req): ValidJson<GrowVolumeRequest>,
) -> Result<Response, ApiErrorReply> {
    let volume_id = parse_volume_id(&volume_id)?;
    let current = state.provider.inspect_volume(&volume_id).await?;
    req.validate(current.provisioned_bytes)?;
    tracing::info!(
        kind = ops::OP_GROW_VOLUME,
        operation_id = %req.operation_id,
        volume_id = %volume_id,
        "accepting grow_volume"
    );
    let payload = ops::path_payload(&volume_id, &req)?;
    let provider = state.provider.clone();
    ops::execute(
        &state,
        ops::OP_GROW_VOLUME,
        req.operation_id.clone(),
        ops::grow_hash(&req, &volume_id),
        payload,
        move || async move { provider.grow_volume(&volume_id, &req).await },
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
