//! Provider conformance test kit (Volume API v2 P0 semantics).
//!
//! The kit is compiled into the library (not `cfg(test)`) so that every
//! provider crate — the fake here, the native-local LVM provider later —
//! can execute the same binding checks. Two entry points exist:
//!
//! - [`assert_conformance`]: runs every check against one provider instance
//!   and returns `Err` with the full failure list;
//! - the `provider_conformance_tests!` macro: generates `#[tokio::test]`
//!   functions, one per check, each against a freshly constructed provider.
//!
//! Covered semantics: create/inspect round trip with honest defaults,
//! idempotent create and attach (including replay response fidelity),
//! payload-conflict detection, single-writer enforcement,
//! generation fencing on attach/detach/grow/delete, detach drain
//! preconditions, grow-only resize, delete preconditions, project-scoped
//! listing, capability gating and missing-volume handling, plus the
//! MoveVolumeBackingOnline contract pins (capability-gated scope,
//! the rate-limit honored-or-refused rule, observed-target
//! validation).
//!
//! [`assert_parity`] is the cross-provider parity suite: it drives the shared
//! lifecycle (create/attach replay, attachment-id conflicts, single-writer
//! admission, volume/attachment generation fencing, detach/delete finality) and
//! compares decision classes against both the shared expected fixture and the
//! in-memory reference provider. Attach replay additionally compares the
//! response projection (generation, frontend and state), so a fabricated
//! replay cannot pass on class alone. The report/generation-fence branch is
//! capability-conditional: providers advertising
//! [`Capability::AttachmentStateReport`] must implement the positive contract,
//! and a provider without it must fail closed. `provider_conformance_tests!`
//! generates a `parity` test alongside the per-check tests.
//!
//! The kit targets the P0 `native-local` profile: fixtures build
//! `native-local` thick volumes, and post-create evidence is asserted to be
//! `PrototypeOnly`.

// This module is test-kit code shipped in the library so other crates can
// run it against their providers; fixture builders may `expect` on
// internally validated constant identifiers.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use crate::VolumeProvider;
use volvisor_types::domain::{AttachmentEvidence, EvidenceStatus, Health, VolumeClass};
use volvisor_types::request::{
    AccessModeRequest, AttachVolumeRequest, CreateVolumeRequest, DeleteVolumeRequest,
    DetachVolumeRequest, DrainProof, ErasurePolicy, GrowGuestNotification, GrowVolumeRequest,
    MoveVolumeBackingRequest, ReportAttachmentStateRequest,
};
use volvisor_types::{
    ApiError, ApiErrorCode, AttachmentId, AttachmentState, Capability, Frontend, HostId,
    MoveVolumeBackingState, OperationId, ProjectId, VolumeId, VolumeLifecycle,
};

/// Fixture size used by the kit (1 GiB, 512-aligned).
const GIB: u64 = 1 << 30;
/// Project most fixtures are created in.
const PROJECT: &str = "conformance-project";

// ---------------------------------------------------------------------------
// Fixture builders (reusable by provider crates under test)
// ---------------------------------------------------------------------------

/// A minimal valid `native-local` create request for 1-GiB-scale sizes.
#[must_use]
pub fn fixture_create_request(volume_id: &str, size_bytes: u64) -> CreateVolumeRequest {
    fixture_create_request_in_project(volume_id, size_bytes, PROJECT)
}

/// A minimal valid `native-local` create request in a specific project.
#[must_use]
pub fn fixture_create_request_in_project(
    volume_id: &str,
    size_bytes: u64,
    project_id: &str,
) -> CreateVolumeRequest {
    CreateVolumeRequest {
        api_version: volvisor_types::API_VERSION.to_owned(),
        operation_id: OperationId::new(format!("op-create-{project_id}-{volume_id}"))
            .expect("valid fixture operation id"),
        project_id: ProjectId::new(project_id).expect("valid fixture project id"),
        volume_id: VolumeId::new(volume_id).expect("valid fixture volume id"),
        volume_class: VolumeClass::NativeLocal,
        size_bytes,
        logical_block_size: None,
        provisioning: None,
        placement: None,
        local_protection: None,
        replication: None,
        migration_policy: None,
        encryption: None,
    }
}

/// A minimal valid single-writer attach request.
#[must_use]
pub fn fixture_attach_request(
    volume_id: &str,
    attachment_id: &str,
    expected_volume_generation: u64,
) -> AttachVolumeRequest {
    AttachVolumeRequest {
        api_version: volvisor_types::API_VERSION.to_owned(),
        operation_id: OperationId::new(format!("op-attach-{volume_id}-{attachment_id}"))
            .expect("valid fixture operation id"),
        vm_id: "conformance-vm".to_owned(),
        host_id: HostId::new("conformance-host").expect("valid fixture host id"),
        attachment_id: AttachmentId::new(attachment_id).expect("valid fixture attachment id"),
        expected_volume_generation,
        access_mode: AccessModeRequest::SingleWriter,
        requested_frontend: None,
        vmm_disk_id: None,
    }
}

/// A minimal valid detach request carrying a `vm_stopped` drain proof.
#[must_use]
pub fn fixture_detach_request(
    attachment_id: &str,
    expected_attachment_generation: u64,
) -> DetachVolumeRequest {
    DetachVolumeRequest {
        api_version: volvisor_types::API_VERSION.to_owned(),
        operation_id: OperationId::new(format!("op-detach-{attachment_id}"))
            .expect("valid fixture operation id"),
        expected_attachment_generation,
        vm_stopped_or_io_drained_proof: DrainProof::VmStopped,
    }
}

/// A minimal valid attachment-state report.
#[must_use]
pub fn fixture_report_request(
    attachment_id: &str,
    expected_attachment_generation: u64,
    state: AttachmentState,
    evidence: AttachmentEvidence,
) -> ReportAttachmentStateRequest {
    ReportAttachmentStateRequest {
        api_version: volvisor_types::API_VERSION.to_owned(),
        operation_id: OperationId::new(format!(
            "op-report-{attachment_id}-{expected_attachment_generation}-{state:?}-{evidence:?}"
        ))
        .expect("valid fixture operation id"),
        attachment_id: AttachmentId::new(attachment_id).expect("valid fixture attachment id"),
        expected_attachment_generation,
        state,
        evidence,
        reporter: Some("conformance-vmm".to_owned()),
        detail: None,
    }
}

/// A minimal valid grow request.
#[must_use]
pub fn fixture_grow_request(
    volume_id: &str,
    new_size_bytes: u64,
    expected_generation: u64,
) -> GrowVolumeRequest {
    GrowVolumeRequest {
        api_version: volvisor_types::API_VERSION.to_owned(),
        operation_id: OperationId::new(format!("op-grow-{volume_id}"))
            .expect("valid fixture operation id"),
        new_size_bytes,
        expected_generation,
    }
}

/// A minimal valid delete request with the `retain` erasure policy.
#[must_use]
pub fn fixture_delete_request(volume_id: &str, expected_generation: u64) -> DeleteVolumeRequest {
    DeleteVolumeRequest {
        api_version: volvisor_types::API_VERSION.to_owned(),
        operation_id: OperationId::new(format!("op-delete-{volume_id}"))
            .expect("valid fixture operation id"),
        expected_generation,
        data_erasure_policy: ErasurePolicy::Retain,
    }
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

fn require(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned())
    }
}

/// Check: create + inspect round trip.
///
/// Asserts the post-create field contract: generation 1, state `Ready`,
/// health `Unknown` (never `Healthy`) on both axes, evidence status
/// `PrototypeOnly`, provisioned = allocated bytes, no attachments.
pub async fn check_create_and_inspect_round_trip(
    provider: &dyn VolumeProvider,
) -> Result<(), String> {
    let req = fixture_create_request("cc-roundtrip", GIB);
    let created = provider
        .create_volume(&req)
        .await
        .map_err(|e| format!("create_volume: {e}"))?;
    require(created.generation == 1, "generation must be 1 after create")?;
    require(
        created.state == VolumeLifecycle::Ready,
        "state must be Ready after create",
    )?;
    require(
        created.health == Health::Unknown,
        "health must be Unknown for an unproven volume, never Healthy",
    )?;
    require(
        created.backend_health == Health::Unknown,
        "backend_health must be Unknown for an unproven backend, never Healthy",
    )?;
    require(
        created.evidence_status == EvidenceStatus::PrototypeOnly,
        "evidence_status must be PrototypeOnly in the P0 kit",
    )?;
    require(
        created.provisioned_bytes == GIB,
        "provisioned_bytes must equal the requested size",
    )?;
    require(
        created.allocated_bytes >= created.provisioned_bytes,
        "allocated_bytes must cover the provisioned size for thick volumes",
    )?;
    require(
        created.backend_class == VolumeClass::NativeLocal,
        "backend_class must round-trip the requested class",
    )?;
    require(
        created.current_writer.is_none(),
        "a fresh volume has no writer",
    )?;
    require(
        created.attachment_ids.is_empty(),
        "a fresh volume has no attachments",
    )?;
    let volume_id = created.volume_id.clone();
    require(
        volume_id == req.volume_id,
        "create must honor the caller-chosen volume_id",
    )?;
    let inspected = provider
        .inspect_volume(&volume_id)
        .await
        .map_err(|e| format!("inspect_volume: {e}"))?;
    require(
        inspected == created,
        "inspect_volume must round-trip the create response",
    )
}

/// Check: create is idempotent per volume identity and payload.
///
/// Replaying the identical create request returns the same volume and does
/// not create a duplicate.
pub async fn check_idempotent_create(provider: &dyn VolumeProvider) -> Result<(), String> {
    let req = fixture_create_request("cc-idempotent-create", GIB);
    let first = provider
        .create_volume(&req)
        .await
        .map_err(|e| format!("create_volume: {e}"))?;
    let second = provider
        .create_volume(&req)
        .await
        .map_err(|e| format!("idempotent create replay: {e}"))?;
    require(
        second == first,
        "replaying the identical create request must return the same volume",
    )?;
    let listed = provider
        .list_volumes(None)
        .await
        .map_err(|e| format!("list_volumes: {e}"))?;
    let matching = listed
        .iter()
        .filter(|vol| vol.volume_id == req.volume_id)
        .count();
    require(
        matching == 1,
        "idempotent create replay must not create a duplicate volume",
    )
}

/// Check: attach is idempotent per attachment identity and payload.
///
/// Replaying the identical attach request returns the recorded instance
/// (same attachment generation, volume generation, frontend and state),
/// leaves exactly one attachment on the volume, and does not advance the
/// inspect projection. A provider that returns `Ok` with a fabricated replay
/// projection fails here even though the decision class looks correct.
pub async fn check_idempotent_attach(provider: &dyn VolumeProvider) -> Result<(), String> {
    let volume_id = "cc-idempotent-attach";
    let create = fixture_create_request(volume_id, GIB);
    let created = provider
        .create_volume(&create)
        .await
        .map_err(|e| format!("create_volume: {e}"))?;
    let attach = fixture_attach_request(volume_id, "cc-idempotent-attach-1", created.generation);
    let first = provider
        .attach_volume(&create.volume_id, &attach)
        .await
        .map_err(|e| format!("attach_volume: {e}"))?;
    let second = provider
        .attach_volume(&create.volume_id, &attach)
        .await
        .map_err(|e| format!("idempotent attach replay: {e}"))?;
    require(
        second == first,
        "replaying the identical attach request must return the recorded instance",
    )?;

    let inspected = provider
        .inspect_volume(&create.volume_id)
        .await
        .map_err(|e| format!("inspect_volume: {e}"))?;
    require(
        inspected.attachment_ids == vec![first.attachment_id.clone()],
        "attach replay must not fabricate or duplicate an attachment id",
    )?;
    let view = inspected
        .attachments
        .iter()
        .find(|view| view.attachment_id == first.attachment_id)
        .ok_or_else(|| "inspect must expose the replayed attachment".to_owned())?;
    require(
        view.attachment_generation == first.attachment_generation,
        "inspect attachment_generation must match the replayed attach response",
    )?;
    require(
        view.frontend == first.frontend,
        "inspect frontend must match the replayed attach response",
    )?;
    require(
        view.state == first.state,
        "inspect attachment state must match the replayed attach response",
    )
}

/// Check: the same `volume_id` with a different payload is a conflict.
pub async fn check_create_payload_conflict(provider: &dyn VolumeProvider) -> Result<(), String> {
    let req = fixture_create_request("cc-create-conflict", GIB);
    provider
        .create_volume(&req)
        .await
        .map_err(|e| format!("create_volume: {e}"))?;
    let mut conflicting = fixture_create_request("cc-create-conflict", 2 * GIB);
    conflicting.operation_id =
        OperationId::new("op-create-conflicting").expect("valid fixture operation id");
    match provider.create_volume(&conflicting).await {
        Err(e) if e.code == ApiErrorCode::IdempotencyConflict => Ok(()),
        Err(e) => Err(format!(
            "create with a different payload for the same volume_id must fail with \
             IDEMPOTENCY_CONFLICT, got {e}"
        )),
        Ok(_) => Err(
            "create with a different payload for the same volume_id must not succeed".to_owned(),
        ),
    }
}

/// Check: single-writer enforcement on attach.
///
/// One writable attach succeeds and reports a host-scoped frontend handle at
/// `prepared` evidence; a second writable attach fails with
/// `WRITER_ALREADY_ACTIVE` and never fabricates another attachment.
pub async fn check_attach_single_writer(provider: &dyn VolumeProvider) -> Result<(), String> {
    let req = fixture_create_request("cc-single-writer", GIB);
    provider
        .create_volume(&req)
        .await
        .map_err(|e| format!("create_volume: {e}"))?;
    let first = provider
        .attach_volume(
            &req.volume_id,
            &fixture_attach_request("cc-single-writer", "cc-single-writer-att-1", 1),
        )
        .await
        .map_err(|e| format!("first attach: {e}"))?;
    require(
        first.attachment_generation >= 1,
        "a fresh attachment records a positive, opaque attachment generation",
    )?;
    require(
        first.volume_generation == 2,
        "attach must advance the volume generation",
    )?;
    require(
        matches!(first.frontend, Frontend::VirtioBlk { .. }),
        "attach must return a host-scoped frontend handle",
    )?;
    require(
        first.state == AttachmentState::Prepared,
        "attach evidence must start at `prepared`, never `active` unobserved",
    )?;
    let inspected = provider
        .inspect_volume(&req.volume_id)
        .await
        .map_err(|e| format!("inspect_volume: {e}"))?;
    require(
        inspected.state == VolumeLifecycle::Attached,
        "volume must be Attached after a writer attach",
    )?;
    require(
        inspected.current_writer.as_ref() == Some(&first.attachment_id),
        "the writer attachment must be recorded as current_writer",
    )?;
    let second = provider
        .attach_volume(
            &req.volume_id,
            &fixture_attach_request("cc-single-writer", "cc-single-writer-att-2", 2),
        )
        .await;
    match second {
        Err(e) if e.code == ApiErrorCode::WriterAlreadyActive => {}
        Err(e) => {
            return Err(format!(
                "a second writable attach must fail with WRITER_ALREADY_ACTIVE, got {e}"
            ));
        }
        Ok(_) => {
            return Err("a second writable attach must not succeed (single-writer)".to_owned());
        }
    }
    let inspected = provider
        .inspect_volume(&req.volume_id)
        .await
        .map_err(|e| format!("inspect_volume: {e}"))?;
    require(
        inspected.attachment_ids.len() == 1,
        "a rejected second attach must not fabricate another attachment",
    )
}

/// Check: attach validates the expected volume generation.
pub async fn check_attach_stale_generation(provider: &dyn VolumeProvider) -> Result<(), String> {
    let req = fixture_create_request("cc-attach-stale", GIB);
    provider
        .create_volume(&req)
        .await
        .map_err(|e| format!("create_volume: {e}"))?;
    let stale = fixture_attach_request("cc-attach-stale", "cc-attach-stale-att", 1_000);
    match provider.attach_volume(&req.volume_id, &stale).await {
        Err(e) if e.code == ApiErrorCode::StaleGeneration => {}
        Err(e) => {
            return Err(format!(
                "attach with a wrong expected generation must fail with STALE_GENERATION, \
                 got {e}"
            ));
        }
        Ok(_) => {
            return Err(
                "attach with a wrong expected generation must not succeed (typed conflict)"
                    .to_owned(),
            );
        }
    }
    let inspected = provider
        .inspect_volume(&req.volume_id)
        .await
        .map_err(|e| format!("inspect_volume: {e}"))?;
    require(
        inspected.state == VolumeLifecycle::Ready && inspected.attachment_ids.is_empty(),
        "a rejected attach must leave the volume untouched",
    )
}

/// Check: detach fencing.
///
/// Detach requires the correct `expected_attachment_generation` and a drain
/// proof (the `DrainProof` attestation is type-mandatory); a correct detach
/// releases the writer and returns the volume to a detached-stable state.
pub async fn check_detach_fencing(provider: &dyn VolumeProvider) -> Result<(), String> {
    let req = fixture_create_request("cc-detach", GIB);
    provider
        .create_volume(&req)
        .await
        .map_err(|e| format!("create_volume: {e}"))?;
    let attached = provider
        .attach_volume(
            &req.volume_id,
            &fixture_attach_request("cc-detach", "cc-detach-att", 1),
        )
        .await
        .map_err(|e| format!("attach: {e}"))?;
    let attachment_id = attached.attachment_id.clone();

    let wrong = fixture_detach_request("cc-detach-att", 1_000);
    match provider
        .detach_volume(&req.volume_id, &attachment_id, &wrong)
        .await
    {
        Err(e) if e.code == ApiErrorCode::StaleGeneration => {}
        Err(e) => {
            return Err(format!(
                "detach with a wrong expected attachment generation must fail with \
                 STALE_GENERATION, got {e}"
            ));
        }
        Ok(_) => {
            return Err(
                "detach with a wrong expected attachment generation must not succeed".to_owned(),
            );
        }
    }
    let still_attached = provider
        .inspect_volume(&req.volume_id)
        .await
        .map_err(|e| format!("inspect_volume: {e}"))?;
    require(
        still_attached.state == VolumeLifecycle::Attached
            && still_attached.current_writer.is_some(),
        "a rejected detach must leave the writer attached",
    )?;

    let drained = fixture_detach_request("cc-detach-att", attached.attachment_generation);
    let after = provider
        .detach_volume(&req.volume_id, &attachment_id, &drained)
        .await
        .map_err(|e| format!("detach: {e}"))?;
    require(
        after.state == VolumeLifecycle::Ready,
        "a detached volume with no attachments must be Ready",
    )?;
    require(
        after.current_writer.is_none(),
        "detach must release the writer",
    )?;
    require(
        after.attachment_ids.is_empty(),
        "detach must remove the attachment",
    )?;
    require(
        after.generation == attached.volume_generation + 1,
        "detach must advance the volume generation exactly once",
    )
}

/// Check: grow semantics.
///
/// Grow-only (a smaller or equal size fails closed), generation-fenced, and
/// the happy path advances the generation and reports the effective size.
pub async fn check_grow_semantics(provider: &dyn VolumeProvider) -> Result<(), String> {
    let req = fixture_create_request("cc-grow", GIB);
    provider
        .create_volume(&req)
        .await
        .map_err(|e| format!("create_volume: {e}"))?;

    let shrink = fixture_grow_request("cc-grow", GIB / 2, 1);
    match provider.grow_volume(&req.volume_id, &shrink).await {
        Err(e) if e.code == ApiErrorCode::UnsupportedClassOrPolicy => {}
        Err(e) => {
            return Err(format!(
                "a non-growing resize must fail with UNSUPPORTED_CLASS_OR_POLICY, got {e}"
            ));
        }
        Ok(_) => {
            return Err("shrink must not succeed (grow-only by default)".to_owned());
        }
    }

    let stale = fixture_grow_request("cc-grow", 2 * GIB, 1_000);
    match provider.grow_volume(&req.volume_id, &stale).await {
        Err(e) if e.code == ApiErrorCode::StaleGeneration => {}
        Err(e) => {
            return Err(format!(
                "grow with a wrong expected generation must fail with STALE_GENERATION, \
                 got {e}"
            ));
        }
        Ok(_) => {
            return Err(
                "grow with a wrong expected generation must not succeed (typed conflict)"
                    .to_owned(),
            );
        }
    }

    let grown = provider
        .grow_volume(&req.volume_id, &fixture_grow_request("cc-grow", 2 * GIB, 1))
        .await
        .map_err(|e| format!("grow: {e}"))?;
    require(
        grown.backing_resized,
        "a successful grow must resize the backing",
    )?;
    require(
        grown.effective_size_bytes == 2 * GIB,
        "grow must report the effective size",
    )?;
    require(
        grown.guest_notification_status == GrowGuestNotification::NotApplicable,
        "a detached volume has no frontend to notify",
    )?;
    let inspected = provider
        .inspect_volume(&req.volume_id)
        .await
        .map_err(|e| format!("inspect_volume: {e}"))?;
    require(
        inspected.generation == 2,
        "grow must advance the volume generation",
    )?;
    require(
        inspected.provisioned_bytes == 2 * GIB,
        "grow must be reflected in provisioned_bytes",
    )
}

/// Check: delete preconditions and finality.
///
/// Delete rejects an attached volume (`INVALID_STATE`), validates the
/// expected generation, succeeds once detached, and the volume is
/// `NOT_FOUND` afterwards.
pub async fn check_delete_lifecycle(provider: &dyn VolumeProvider) -> Result<(), String> {
    let req = fixture_create_request("cc-delete", GIB);
    provider
        .create_volume(&req)
        .await
        .map_err(|e| format!("create_volume: {e}"))?;
    let attached = provider
        .attach_volume(
            &req.volume_id,
            &fixture_attach_request("cc-delete", "cc-delete-att", 1),
        )
        .await
        .map_err(|e| format!("attach: {e}"))?;

    match provider
        .delete_volume(
            &req.volume_id,
            &fixture_delete_request("cc-delete", attached.volume_generation),
        )
        .await
    {
        Err(e) if e.code == ApiErrorCode::InvalidState => {}
        Err(e) => {
            return Err(format!(
                "delete on an attached volume must fail with INVALID_STATE, got {e}"
            ));
        }
        Ok(()) => {
            return Err("delete on an attached volume must not succeed".to_owned());
        }
    }

    let detached = provider
        .detach_volume(
            &req.volume_id,
            &attached.attachment_id,
            &fixture_detach_request("cc-delete-att", attached.attachment_generation),
        )
        .await
        .map_err(|e| format!("detach: {e}"))?;

    match provider
        .delete_volume(&req.volume_id, &fixture_delete_request("cc-delete", 1_000))
        .await
    {
        Err(e) if e.code == ApiErrorCode::StaleGeneration => {}
        Err(e) => {
            return Err(format!(
                "delete with a wrong expected generation must fail with STALE_GENERATION, \
                 got {e}"
            ));
        }
        Ok(()) => {
            return Err("delete with a wrong expected generation must not succeed".to_owned());
        }
    }

    provider
        .delete_volume(
            &req.volume_id,
            &fixture_delete_request("cc-delete", detached.generation),
        )
        .await
        .map_err(|e| format!("delete: {e}"))?;
    match provider.inspect_volume(&req.volume_id).await {
        Err(e) if e.code == ApiErrorCode::NotFound => Ok(()),
        Err(e) => Err(format!(
            "inspect after delete must fail with NOT_FOUND, got {e}"
        )),
        Ok(_) => Err("inspect after delete must not succeed".to_owned()),
    }
}

/// Check: list volumes filters by project and never leaks other projects.
pub async fn check_list_project_filter(provider: &dyn VolumeProvider) -> Result<(), String> {
    let in_project = fixture_create_request("cc-list-a", GIB);
    let other_project =
        fixture_create_request_in_project("cc-list-b", GIB, "conformance-project-b");
    provider
        .create_volume(&in_project)
        .await
        .map_err(|e| format!("create_volume: {e}"))?;
    provider
        .create_volume(&other_project)
        .await
        .map_err(|e| format!("create_volume: {e}"))?;

    let all = provider
        .list_volumes(None)
        .await
        .map_err(|e| format!("list_volumes: {e}"))?;
    require(
        all.iter().any(|vol| vol.volume_id == in_project.volume_id)
            && all
                .iter()
                .any(|vol| vol.volume_id == other_project.volume_id),
        "an unfiltered list must include both fixtures",
    )?;

    let project = ProjectId::new(PROJECT).expect("valid fixture project id");
    let filtered = provider
        .list_volumes(Some(&project))
        .await
        .map_err(|e| format!("list_volumes(filtered): {e}"))?;
    require(
        filtered.iter().all(|vol| vol.project_id == project),
        "a filtered list must not leak other projects' volumes",
    )?;
    require(
        filtered
            .iter()
            .any(|vol| vol.volume_id == in_project.volume_id),
        "a filtered list must include the project's own volume",
    )?;
    require(
        !filtered
            .iter()
            .any(|vol| vol.volume_id == other_project.volume_id),
        "a filtered list must exclude other projects' volumes",
    )
}

/// Check: grow is capability-gated.
///
/// A provider that does not advertise the `resize` capability must reject
/// grow with `UNSUPPORTED_CLASS_OR_POLICY` (fail-closed); a provider that
/// advertises it must actually grow.
pub async fn check_resize_capability_gate(provider: &dyn VolumeProvider) -> Result<(), String> {
    let req = fixture_create_request("cc-resize-gate", GIB);
    provider
        .create_volume(&req)
        .await
        .map_err(|e| format!("create_volume: {e}"))?;
    let grow = fixture_grow_request("cc-resize-gate", 2 * GIB, 1);
    if provider.capabilities().contains(Capability::Resize) {
        let grown = provider
            .grow_volume(&req.volume_id, &grow)
            .await
            .map_err(|e| format!("grow with the resize capability advertised: {e}"))?;
        require(
            grown.backing_resized,
            "an advertised resize capability must actually grow",
        )
    } else {
        match provider.grow_volume(&req.volume_id, &grow).await {
            Err(e) if e.code == ApiErrorCode::UnsupportedClassOrPolicy => Ok(()),
            Err(e) => Err(format!(
                "grow without the resize capability must fail with \
                 UNSUPPORTED_CLASS_OR_POLICY, got {e}"
            )),
            Ok(_) => {
                Err("grow without the resize capability must not succeed (fail-closed)".to_owned())
            }
        }
    }
}

/// Check: attach on a non-existent volume is `NOT_FOUND`.
pub async fn check_attach_missing_volume(provider: &dyn VolumeProvider) -> Result<(), String> {
    let volume_id = VolumeId::new("cc-never-created").expect("valid fixture volume id");
    let req = fixture_attach_request("cc-never-created", "cc-never-created-att", 1);
    match provider.attach_volume(&volume_id, &req).await {
        Err(e) if e.code == ApiErrorCode::NotFound => Ok(()),
        Err(e) => Err(format!(
            "attach on a missing volume must fail with NOT_FOUND, got {e}"
        )),
        Ok(_) => Err("attach on a missing volume must not succeed".to_owned()),
    }
}

/// Check: the MoveVolumeBackingOnline contract (section 4A).
///
/// Two binding rules every provider obeys regardless of scope:
///
/// - **Capability gating**: a provider that does not advertise
///   `same_vg_extent_move` refuses every move with
///   `MOVE_UNSUPPORTED_SCOPE` (the fail-closed trait default is the
///   pin — no silent degradation, and the current extents are never
///   touched by a refusal).
/// - **`max_copy_bytes_per_sec` is honored or refused**: a provider
///   that advertises the move but cannot rate-limit the copy refuses
///   a set value with `UNSUPPORTED_CLASS_OR_POLICY` **naming the
///   parameter** — never a silent ignore. (A provider that CAN honor
///   it passes by honoring it; no current one can.)
///
/// A qualified provider additionally answers a nonexistent target
/// with `NOT_FOUND` — the target is validated against the observed
/// world, never assumed.
pub async fn check_move_contract(provider: &dyn VolumeProvider) -> Result<(), String> {
    let move_req = MoveVolumeBackingRequest {
        api_version: volvisor_types::API_VERSION.to_owned(),
        operation_id: OperationId::new("op-cc-move-contract").expect("valid fixture operation id"),
        target_pool_id: "/dev/nonexistent-move-target".to_owned(),
        expected_generation: 1,
        max_copy_bytes_per_sec: None,
    };
    if !provider
        .capabilities()
        .contains(Capability::SameVgExtentMove)
    {
        // No volume is created for the unqualified path: the
        // fail-closed refusal precedes any volume lookup (and a
        // shared-instance suite must not spend provider resources on
        // it).
        let missing = VolumeId::new("cc-never-created").expect("valid fixture volume id");
        return match provider.move_volume_backing(&missing, &move_req).await {
            Err(e) if e.code == ApiErrorCode::MoveUnsupportedScope => Ok(()),
            Err(e) => Err(format!(
                "move without the same_vg_extent_move capability must fail with \
                 MOVE_UNSUPPORTED_SCOPE, got {e}"
            )),
            Ok(_) => Err(
                "move without the same_vg_extent_move capability must not succeed \
                 (fail-closed)"
                    .to_owned(),
            ),
        };
    }
    // Qualified: the target is validated against the observed world.
    let req = fixture_create_request("cc-move-contract", GIB);
    provider
        .create_volume(&req)
        .await
        .map_err(|e| format!("create_volume: {e}"))?;
    match provider
        .move_volume_backing(&req.volume_id, &move_req)
        .await
    {
        Err(e) if e.code == ApiErrorCode::NotFound => {}
        Err(e) => {
            return Err(format!(
                "a move to a nonexistent target must fail with NOT_FOUND, got {e}"
            ));
        }
        Ok(_) => {
            return Err(
                "a move to a nonexistent target must not succeed (the target is \
                 observed, never assumed)"
                    .to_owned(),
            );
        }
    }
    // Honored or refused, never ignored: the rate limit names itself.
    let mut rate_limited = move_req;
    rate_limited.max_copy_bytes_per_sec = Some(1024 * 1024);
    match provider
        .move_volume_backing(&req.volume_id, &rate_limited)
        .await
    {
        Err(e) if e.code == ApiErrorCode::UnsupportedClassOrPolicy => require(
            e.detail.contains("max_copy_bytes_per_sec"),
            "the rate-limit refusal must name the parameter",
        ),
        Err(e) => Err(format!(
            "an unhonorable max_copy_bytes_per_sec must fail with \
             UNSUPPORTED_CLASS_OR_POLICY, got {e}"
        )),
        Ok(response) => require(
            response.state != MoveVolumeBackingState::Preparing
                && response.state != MoveVolumeBackingState::Copying,
            "a rate-limited move that succeeded must have completed (a set limit \
             is never silently ignored mid-flight)",
        ),
    }
}

/// Check: consumer attachment-state reporting (contract section 3).
///
/// A provider advertising [`Capability::AttachmentStateReport`] must:
/// advance evidence monotonically (`prepared < advertised < active`),
/// keep `attachment_generation` fixed across reports, answer a
/// repeated/lower report as an idempotent no-op (`applied=false` without
/// downgrading), refuse a stale instance generation, expose the full
/// per-attachment view on inspect, answer `NOT_FOUND` after detach, and
/// record a strictly greater generation for a re-attach so a delayed
/// report carrying the retired generation is `STALE_GENERATION`.
/// A provider without the capability is skipped; its typed `NOT_FOUND` is
/// the default trait behavior, and the capability-gated path is covered by
/// the fake's `report_requires_the_capability` test.
#[allow(clippy::too_many_lines)]
pub async fn check_attachment_state_report(provider: &dyn VolumeProvider) -> Result<(), String> {
    if !provider
        .capabilities()
        .contains(Capability::AttachmentStateReport)
    {
        return Ok(());
    }
    let volume_id = "cc-report-volume";
    let attachment_id = "cc-report-attachment";
    let create = fixture_create_request(volume_id, GIB);
    provider
        .create_volume(&create)
        .await
        .map_err(|e| format!("create_volume: {e}"))?;
    let attach = fixture_attach_request(volume_id, attachment_id, 1);
    let attached = provider
        .attach_volume(&create.volume_id, &attach)
        .await
        .map_err(|e| format!("attach_volume: {e}"))?;
    require(
        attached.state == AttachmentState::Prepared,
        "attach must report the provider-recorded prepared state",
    )?;
    // ADR-0013: the attachment generation is an opaque, provider-scoped
    // value. It is not necessarily 1 on a shared provider instance; the
    // contract is that it is positive, stable across reports, and strictly
    // increasing across attachment incarnations.
    let generation = attached.attachment_generation;
    require(
        generation >= 1,
        "the attachment generation must be positive",
    )?;

    let inspected = provider
        .inspect_volume(&create.volume_id)
        .await
        .map_err(|e| format!("inspect_volume: {e}"))?;
    let view = inspected
        .attachments
        .iter()
        .find(|view| view.attachment_id == attach.attachment_id)
        .ok_or_else(|| "inspect must expose the attached instance in `attachments`".to_owned())?;
    require(
        view.state == AttachmentState::Prepared && view.state_revision == 1,
        "a fresh attachment must project prepared at revision 1",
    )?;
    require(
        view.attachment_generation == generation,
        "inspect must project the generation the attach recorded",
    )?;
    require(
        view.evidence.is_none() && view.reporter.is_none(),
        "prepared evidence must not fabricate a reporter",
    )?;

    // advertised (vmm_configured) advances the revision, not the generation.
    let advertised = provider
        .report_attachment_state(
            &create.volume_id,
            &fixture_report_request(
                attachment_id,
                generation,
                AttachmentState::Advertised,
                AttachmentEvidence::VmmConfigured,
            ),
        )
        .await
        .map_err(|e| format!("report advertised: {e}"))?;
    require(advertised.applied, "the first advertised report must apply")?;
    require(
        advertised.state == AttachmentState::Advertised && advertised.state_revision == 2,
        "advertised must project revision 2",
    )?;
    require(
        advertised.attachment_generation == generation,
        "a report must never advance attachment_generation",
    )?;
    require(
        advertised.reporter.as_deref() == Some("conformance-vmm")
            && advertised.recorded_at_ms.is_some(),
        "an applied report must return its reporter and the server receipt time",
    )?;

    // active (guest_confirmed) advances again.
    let active = provider
        .report_attachment_state(
            &create.volume_id,
            &fixture_report_request(
                attachment_id,
                generation,
                AttachmentState::Active,
                AttachmentEvidence::GuestConfirmed,
            ),
        )
        .await
        .map_err(|e| format!("report active: {e}"))?;
    require(
        active.applied && active.state == AttachmentState::Active && active.state_revision == 3,
        "active must apply at revision 3",
    )?;
    require(
        active.evidence == Some(AttachmentEvidence::GuestConfirmed)
            && active.reporter.as_deref() == Some("conformance-vmm")
            && active.recorded_at_ms.is_some(),
        "active must carry guest_confirmed evidence and observation provenance",
    )?;

    // An equal report is the same idempotent no-op as a lower one.
    let equal = provider
        .report_attachment_state(
            &create.volume_id,
            &fixture_report_request(
                attachment_id,
                generation,
                AttachmentState::Active,
                AttachmentEvidence::GuestConfirmed,
            ),
        )
        .await
        .map_err(|e| format!("report equal: {e}"))?;
    require(
        !equal.applied
            && equal.state == AttachmentState::Active
            && equal.state_revision == 3
            && equal.recorded_at_ms == active.recorded_at_ms,
        "an equal report must be a no-op preserving the applied projection",
    )?;

    // Inspect re-projects the applied evidence (a restarted consumer
    // re-observes rather than replaying reports).
    let observed = provider
        .inspect_volume(&create.volume_id)
        .await
        .map_err(|e| format!("inspect_volume after report: {e}"))?;
    let observed = observed
        .attachments
        .iter()
        .find(|view| view.attachment_id == attach.attachment_id)
        .ok_or_else(|| "inspect must still expose the reported instance".to_owned())?;
    require(
        observed.state == AttachmentState::Active
            && observed.state_revision == 3
            && observed.evidence == Some(AttachmentEvidence::GuestConfirmed)
            && observed.reporter.as_deref() == Some("conformance-vmm")
            && observed.recorded_at_ms == active.recorded_at_ms
            && observed.attachment_generation == generation,
        "inspect must project the applied evidence verbatim at the unchanged generation",
    )?;

    // A lower report is an idempotent no-op: no downgrade, no revision bump.
    let lower = provider
        .report_attachment_state(
            &create.volume_id,
            &fixture_report_request(
                attachment_id,
                generation,
                AttachmentState::Advertised,
                AttachmentEvidence::GuestConfirmed,
            ),
        )
        .await
        .map_err(|e| format!("report lower: {e}"))?;
    require(
        !lower.applied && lower.state == AttachmentState::Active && lower.state_revision == 3,
        "a lower report must be a no-op preserving the current projection",
    )?;

    // A stale instance generation is fenced with the typed conflict.
    match provider
        .report_attachment_state(
            &create.volume_id,
            &fixture_report_request(
                attachment_id,
                generation.wrapping_add(1),
                AttachmentState::Active,
                AttachmentEvidence::GuestConfirmed,
            ),
        )
        .await
    {
        Err(e) if e.code == ApiErrorCode::StaleGeneration => {}
        Err(e) => {
            return Err(format!(
                "a stale report generation must fail with STALE_GENERATION, got {e}"
            ));
        }
        Ok(_) => return Err("a stale report generation must not succeed".to_owned()),
    }

    // Detach still fences on the generation reports never moved, and a
    // report against the detached instance is NOT_FOUND for a new operation.
    provider
        .detach_volume(
            &create.volume_id,
            &attach.attachment_id,
            &fixture_detach_request(attachment_id, generation),
        )
        .await
        .map_err(|e| format!("detach at the unchanged generation: {e}"))?;
    match provider
        .report_attachment_state(
            &create.volume_id,
            &fixture_report_request(
                attachment_id,
                generation,
                AttachmentState::Active,
                AttachmentEvidence::GuestConfirmed,
            ),
        )
        .await
    {
        Err(e) if e.code == ApiErrorCode::NotFound => {}
        Err(e) => {
            return Err(format!(
                "a report after detach must fail with NOT_FOUND, got {e}"
            ));
        }
        Ok(_) => return Err("a report after detach must not succeed".to_owned()),
    }

    // A re-attach records a strictly greater generation. A delayed report
    // from the retired instance must never label the new one, even when the
    // consumer reuses the attachment id: its old generation is stale.
    let retired_generation = generation;
    let after_detach = provider
        .inspect_volume(&create.volume_id)
        .await
        .map_err(|e| format!("inspect_volume after detach: {e}"))?;
    let reattach = fixture_attach_request(volume_id, attachment_id, after_detach.generation);
    let reattached = provider
        .attach_volume(&create.volume_id, &reattach)
        .await
        .map_err(|e| format!("re-attach after detach: {e}"))?;
    require(
        reattached.attachment_generation > retired_generation,
        "a re-attach must record a strictly greater attachment generation",
    )?;
    match provider
        .report_attachment_state(
            &create.volume_id,
            &fixture_report_request(
                attachment_id,
                retired_generation,
                AttachmentState::Active,
                AttachmentEvidence::GuestConfirmed,
            ),
        )
        .await
    {
        Err(e) if e.code == ApiErrorCode::StaleGeneration => Ok(()),
        Err(e) => Err(format!(
            "a delayed report from the retired generation must fail with STALE_GENERATION, got {e}"
        )),
        Ok(_) => Err(
            "a delayed report from the retired generation must not label the new instance"
                .to_owned(),
        ),
    }
}

// ---------------------------------------------------------------------------
// Cross-provider parity driver
// ---------------------------------------------------------------------------

/// The outcome class of one parity step.
///
/// The parity suite compares *classes*, never backend-specific details
/// (device paths, health, protection axes): every provider must make the same
/// decision at each step of the shared lifecycle. `Other` keeps an unexpected
/// typed error visible instead of collapsing it to a pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParityOutcome {
    /// Create succeeded.
    Created,
    /// An idempotent create or attach replay returned the recorded result.
    Replayed,
    /// An identical attach replay returned a different projection than the
    /// recorded instance (a replay-fidelity failure, never a valid class).
    ReplayDiverged,
    /// A re-attach did not record a generation strictly greater than the
    /// retired instance (a broken attachment fence, never a valid class).
    GenerationNotAdvanced,
    /// Attach succeeded.
    Attached,
    /// Detach succeeded.
    Detached,
    /// Delete succeeded.
    Deleted,
    /// The request conflicted with recorded state (`IDEMPOTENCY_CONFLICT`).
    Conflict,
    /// A generation fence refused the request (`STALE_GENERATION`).
    Stale,
    /// Single-writer admission refused a second writer.
    WriterActive,
    /// The resource does not exist for this operation.
    NotFound,
    /// The request is invalid in the current lifecycle state.
    InvalidState,
    /// The class/policy/field is unsupported (fail-closed).
    Unsupported,
    /// A consumer report advanced the evidence rank.
    Applied(AttachmentState),
    /// A consumer report was an idempotent no-op at the current rank.
    NoOp(AttachmentState),
    /// Any other typed error code (a divergence unless both sides emit it).
    Other(ApiErrorCode),
}

impl ParityOutcome {
    fn from_error(error: &ApiError) -> Self {
        match error.code {
            ApiErrorCode::IdempotencyConflict => Self::Conflict,
            ApiErrorCode::StaleGeneration => Self::Stale,
            ApiErrorCode::WriterAlreadyActive => Self::WriterActive,
            ApiErrorCode::NotFound => Self::NotFound,
            ApiErrorCode::InvalidState => Self::InvalidState,
            ApiErrorCode::UnsupportedClassOrPolicy => Self::Unsupported,
            other => Self::Other(other),
        }
    }
}

/// One named step and the class of its observed outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParityStep {
    /// Stable step name (part of the expected fixture).
    pub name: &'static str,
    /// Observed outcome class.
    pub outcome: ParityOutcome,
}

fn outcome<T>(result: Result<T, ApiError>, success: ParityOutcome) -> ParityOutcome {
    match result {
        Ok(_) => success,
        Err(error) => ParityOutcome::from_error(&error),
    }
}

fn report_outcome(
    result: Result<volvisor_types::request::ReportAttachmentStateResponse, ApiError>,
) -> ParityOutcome {
    match result {
        Ok(response) if response.applied => ParityOutcome::Applied(response.state),
        Ok(response) => ParityOutcome::NoOp(response.state),
        Err(error) => ParityOutcome::from_error(&error),
    }
}

fn step(name: &'static str, outcome: ParityOutcome) -> ParityStep {
    ParityStep { name, outcome }
}

/// The expected universal parity sequence shared by every provider.
///
/// This is the reference fixture: attach replay, attachment-id conflicts,
/// single-writer admission, and volume/attachment generation fencing must be
/// indistinguishable across backends.
#[must_use]
pub fn expected_universal_parity() -> Vec<ParityStep> {
    use ParityOutcome::{
        Attached, Conflict, Created, Deleted, Detached, NotFound, Replayed, Stale, WriterActive,
    };
    vec![
        step("create", Created),
        step("create-replay", Replayed),
        step("create-payload-conflict", Conflict),
        step("attach-stale-before-attach", Stale),
        step("attach", Attached),
        step("attach-replay", Replayed),
        step("attach-stale-after-attach", Stale),
        step("attach-same-id-different-payload", Conflict),
        step("attach-second-writer", WriterActive),
        step("detach-stale", Stale),
        step("detach", Detached),
        step("detach-again", NotFound),
        step("delete-stale", Stale),
        step("delete", Deleted),
        step("inspect-after-delete", NotFound),
    ]
}

/// Run the universal shared-semantics sequence against `provider`.
///
/// Every provider must return [`expected_universal_parity`] (and therefore the
/// same sequence as the in-memory reference provider). The sequence never
/// advertises backend-specific behavior: no device handles, health axes,
/// capabilities, grow or consumer reports participate.
#[allow(clippy::too_many_lines)]
pub async fn run_universal_parity(provider: &dyn VolumeProvider) -> Vec<ParityStep> {
    let volume_id = "parity-universal";
    let mut steps = Vec::new();

    let create = fixture_create_request(volume_id, GIB);
    let created = provider.create_volume(&create).await;
    let volume_generation = created.as_ref().map_or(0, |volume| volume.generation);
    steps.push(step("create", outcome(created, ParityOutcome::Created)));

    steps.push(step(
        "create-replay",
        outcome(
            provider.create_volume(&create).await,
            ParityOutcome::Replayed,
        ),
    ));

    let mut conflicting = fixture_create_request(volume_id, 2 * GIB);
    conflicting.operation_id =
        OperationId::new("op-parity-create-conflict").expect("valid fixture operation id");
    steps.push(step(
        "create-payload-conflict",
        outcome(
            provider.create_volume(&conflicting).await,
            ParityOutcome::Replayed,
        ),
    ));

    let stale_before =
        fixture_attach_request(volume_id, "parity-stale-before", volume_generation + 1_000);
    steps.push(step(
        "attach-stale-before-attach",
        outcome(
            provider
                .attach_volume(&create.volume_id, &stale_before)
                .await,
            ParityOutcome::Attached,
        ),
    ));

    let attach = fixture_attach_request(volume_id, "parity-attachment", volume_generation);
    let attached = provider.attach_volume(&create.volume_id, &attach).await;
    let attachment_generation = attached
        .as_ref()
        .map_or(0, |response| response.attachment_generation);
    steps.push(step(
        "attach",
        outcome(attached.clone(), ParityOutcome::Attached),
    ));

    // A replay keeps the *original* expected volume generation: replay
    // identity wins over the (now stale) generation fence. It must also
    // return the recorded projection byte-for-byte; a class-only check would
    // let a fabricated replay (wrong state, frontend or generation) pass.
    let replayed = provider.attach_volume(&create.volume_id, &attach).await;
    let replay_outcome = match (&attached, &replayed) {
        (Ok(first), Ok(second)) if first != second => ParityOutcome::ReplayDiverged,
        _ => outcome(replayed, ParityOutcome::Replayed),
    };
    steps.push(step("attach-replay", replay_outcome));

    let stale_after = fixture_attach_request(volume_id, "parity-stale-after", volume_generation);
    steps.push(step(
        "attach-stale-after-attach",
        outcome(
            provider
                .attach_volume(&create.volume_id, &stale_after)
                .await,
            ParityOutcome::Attached,
        ),
    ));

    let mut different =
        fixture_attach_request(volume_id, "parity-attachment", volume_generation + 1);
    "parity-other-vm".clone_into(&mut different.vm_id);
    steps.push(step(
        "attach-same-id-different-payload",
        outcome(
            provider.attach_volume(&create.volume_id, &different).await,
            ParityOutcome::Replayed,
        ),
    ));

    let second_writer =
        fixture_attach_request(volume_id, "parity-second-writer", volume_generation + 1);
    steps.push(step(
        "attach-second-writer",
        outcome(
            provider
                .attach_volume(&create.volume_id, &second_writer)
                .await,
            ParityOutcome::Attached,
        ),
    ));

    let stale_detach = fixture_detach_request("parity-attachment", attachment_generation + 1);
    steps.push(step(
        "detach-stale",
        outcome(
            provider
                .detach_volume(&create.volume_id, &attach.attachment_id, &stale_detach)
                .await,
            ParityOutcome::Detached,
        ),
    ));

    let detach = fixture_detach_request("parity-attachment", attachment_generation);
    let detached = provider
        .detach_volume(&create.volume_id, &attach.attachment_id, &detach)
        .await;
    let generation_after_detach = detached
        .as_ref()
        .map_or(volume_generation + 2, |volume| volume.generation);
    steps.push(step("detach", outcome(detached, ParityOutcome::Detached)));

    steps.push(step(
        "detach-again",
        outcome(
            provider
                .detach_volume(&create.volume_id, &attach.attachment_id, &detach)
                .await,
            ParityOutcome::Detached,
        ),
    ));

    steps.push(step(
        "delete-stale",
        outcome(
            provider
                .delete_volume(
                    &create.volume_id,
                    &fixture_delete_request(volume_id, volume_generation),
                )
                .await,
            ParityOutcome::Deleted,
        ),
    ));

    steps.push(step(
        "delete",
        outcome(
            provider
                .delete_volume(
                    &create.volume_id,
                    &fixture_delete_request(volume_id, generation_after_detach),
                )
                .await,
            ParityOutcome::Deleted,
        ),
    ));

    steps.push(step(
        "inspect-after-delete",
        outcome(
            provider.inspect_volume(&create.volume_id).await,
            ParityOutcome::Created,
        ),
    ));

    steps
}

/// The expected report/attachment-generation parity sequence.
///
/// Report-capable providers apply consumer reports monotonically, keep the
/// attachment generation fixed across reports, answer repeated/lower reports
/// as no-ops, refuse a stale generation, and record a strictly greater
/// generation on re-attach, so a delayed report from the retired generation
/// is stale. Providers that do not advertise
/// [`Capability::AttachmentStateReport`] must fail closed with `NOT_FOUND`
/// instead of synthesizing evidence.
#[must_use]
pub fn expected_report_parity(report_capable: bool) -> Vec<ParityStep> {
    use ParityOutcome::{Applied, Attached, Created, Deleted, Detached, NoOp, NotFound, Stale};
    if report_capable {
        vec![
            step("create", Created),
            step("attach", Attached),
            step("report-advertised", Applied(AttachmentState::Advertised)),
            step("report-active", Applied(AttachmentState::Active)),
            step("report-equal", NoOp(AttachmentState::Active)),
            step("report-lower", NoOp(AttachmentState::Active)),
            step("report-stale", Stale),
            step("detach", Detached),
            step("report-after-detach", NotFound),
            step("reattach-new-generation", Attached),
            step("reattach-generation-advanced", Attached),
            step("report-retired-generation", Stale),
        ]
    } else {
        vec![
            step("create", Created),
            step("attach", Attached),
            step("report-without-capability", NotFound),
            step("detach", Detached),
            step("delete", Deleted),
            step("inspect-after-delete", NotFound),
        ]
    }
}

/// Run the report/attachment-generation parity sequence against `provider`.
///
/// The driver branches on the advertised capability so the same fixture
/// states the positive contract (fake/LVM) and the fail-closed contract
/// (Ceph/DRBD), without either side skipping the check.
#[allow(clippy::too_many_lines)]
pub async fn run_report_parity(provider: &dyn VolumeProvider) -> Vec<ParityStep> {
    let volume_id = "parity-reports";
    let report_capable = provider
        .capabilities()
        .contains(Capability::AttachmentStateReport);
    let mut steps = Vec::new();

    let create = fixture_create_request(volume_id, GIB);
    let created = provider.create_volume(&create).await;
    let volume_generation = created.as_ref().map_or(0, |volume| volume.generation);
    steps.push(step("create", outcome(created, ParityOutcome::Created)));

    let attach = fixture_attach_request(volume_id, "parity-report-attachment", volume_generation);
    let attached = provider.attach_volume(&create.volume_id, &attach).await;
    let attachment_generation = attached
        .as_ref()
        .map_or(0, |response| response.attachment_generation);
    // `attach` always succeeds in the fixture.
    steps.push(step("attach", outcome(attached, ParityOutcome::Attached)));

    if report_capable {
        steps.push(step(
            "report-advertised",
            report_outcome(
                provider
                    .report_attachment_state(
                        &create.volume_id,
                        &fixture_report_request(
                            "parity-report-attachment",
                            attachment_generation,
                            AttachmentState::Advertised,
                            AttachmentEvidence::VmmConfigured,
                        ),
                    )
                    .await,
            ),
        ));
        steps.push(step(
            "report-active",
            report_outcome(
                provider
                    .report_attachment_state(
                        &create.volume_id,
                        &fixture_report_request(
                            "parity-report-attachment",
                            attachment_generation,
                            AttachmentState::Active,
                            AttachmentEvidence::GuestConfirmed,
                        ),
                    )
                    .await,
            ),
        ));
        steps.push(step(
            "report-equal",
            report_outcome(
                provider
                    .report_attachment_state(
                        &create.volume_id,
                        &fixture_report_request(
                            "parity-report-attachment",
                            attachment_generation,
                            AttachmentState::Active,
                            AttachmentEvidence::GuestConfirmed,
                        ),
                    )
                    .await,
            ),
        ));
        steps.push(step(
            "report-lower",
            report_outcome(
                provider
                    .report_attachment_state(
                        &create.volume_id,
                        &fixture_report_request(
                            "parity-report-attachment",
                            attachment_generation,
                            AttachmentState::Advertised,
                            AttachmentEvidence::GuestConfirmed,
                        ),
                    )
                    .await,
            ),
        ));
        steps.push(step(
            "report-stale",
            report_outcome(
                provider
                    .report_attachment_state(
                        &create.volume_id,
                        &fixture_report_request(
                            "parity-report-attachment",
                            attachment_generation + 1,
                            AttachmentState::Active,
                            AttachmentEvidence::GuestConfirmed,
                        ),
                    )
                    .await,
            ),
        ));
    } else {
        steps.push(step(
            "report-without-capability",
            report_outcome(
                provider
                    .report_attachment_state(
                        &create.volume_id,
                        &fixture_report_request(
                            "parity-report-attachment",
                            attachment_generation,
                            AttachmentState::Advertised,
                            AttachmentEvidence::VmmConfigured,
                        ),
                    )
                    .await,
            ),
        ));
    }

    // Reports never advance the attachment generation: this detach still
    // fences on the original generation.
    let detach = fixture_detach_request("parity-report-attachment", attachment_generation);
    let detached = provider
        .detach_volume(&create.volume_id, &attach.attachment_id, &detach)
        .await;
    let generation_after_detach = detached
        .as_ref()
        .map_or(volume_generation + 2, |volume| volume.generation);
    steps.push(step("detach", outcome(detached, ParityOutcome::Detached)));

    if report_capable {
        steps.push(step(
            "report-after-detach",
            report_outcome(
                provider
                    .report_attachment_state(
                        &create.volume_id,
                        &fixture_report_request(
                            "parity-report-attachment",
                            attachment_generation,
                            AttachmentState::Active,
                            AttachmentEvidence::GuestConfirmed,
                        ),
                    )
                    .await,
            ),
        ));
        let reattach = fixture_attach_request(
            volume_id,
            "parity-report-attachment",
            generation_after_detach,
        );
        let reattached = provider.attach_volume(&create.volume_id, &reattach).await;
        let reattached_generation = reattached
            .as_ref()
            .map_or(0, |response| response.attachment_generation);
        steps.push(step(
            "reattach-new-generation",
            outcome(reattached, ParityOutcome::Attached),
        ));
        steps.push(step(
            "reattach-generation-advanced",
            if reattached_generation > attachment_generation {
                ParityOutcome::Attached
            } else {
                ParityOutcome::GenerationNotAdvanced
            },
        ));
        steps.push(step(
            "report-retired-generation",
            report_outcome(
                provider
                    .report_attachment_state(
                        &create.volume_id,
                        &fixture_report_request(
                            "parity-report-attachment",
                            attachment_generation,
                            AttachmentState::Active,
                            AttachmentEvidence::GuestConfirmed,
                        ),
                    )
                    .await,
            ),
        ));
    } else {
        steps.push(step(
            "delete",
            outcome(
                provider
                    .delete_volume(
                        &create.volume_id,
                        &fixture_delete_request(volume_id, generation_after_detach),
                    )
                    .await,
                ParityOutcome::Deleted,
            ),
        ));
        steps.push(step(
            "inspect-after-delete",
            outcome(
                provider.inspect_volume(&create.volume_id).await,
                ParityOutcome::Created,
            ),
        ));
    }

    steps
}

// ---------------------------------------------------------------------------
// Suite entry points
// ---------------------------------------------------------------------------

/// Run the cross-provider parity suite against `provider`.
///
/// Fails when the provider's decision classes diverge from either the shared
/// expected fixture or the in-memory reference provider. The
/// report/generation-fence branch is capability-conditional: capable providers
/// must match the positive contract, non-capable providers must fail closed.
///
/// # Errors
/// A description of the first divergent sequence.
pub async fn assert_parity(provider: &dyn VolumeProvider) -> Result<(), String> {
    let reference = crate::fake::FakeProvider::new();

    let reference_universal = run_universal_parity(&reference).await;
    let universal = run_universal_parity(provider).await;
    if universal != reference_universal {
        return Err(format!(
            "universal parity diverges from the reference provider:\n  reference: \
             {reference_universal:?}\n  provider:  {universal:?}"
        ));
    }
    let expected = expected_universal_parity();
    if universal != expected {
        return Err(format!(
            "universal parity does not match the shared fixture:\n  expected: {expected:?}\n  \
             provider: {universal:?}"
        ));
    }

    let report_capable = provider
        .capabilities()
        .contains(Capability::AttachmentStateReport);
    let reports = run_report_parity(provider).await;
    let expected_reports = expected_report_parity(report_capable);
    if reports != expected_reports {
        return Err(format!(
            "report/generation-fence parity does not match the shared fixture:\n  expected: \
             {expected_reports:?}\n  provider: {reports:?}"
        ));
    }
    if report_capable {
        let reference_reports = run_report_parity(&reference).await;
        if reports != reference_reports {
            return Err(format!(
                "report/generation-fence parity diverges from the reference provider:\n  reference: \
                 {reference_reports:?}\n  provider:  {reports:?}"
            ));
        }
    }
    Ok(())
}

/// Run every conformance check against `provider`.
///
/// Returns the failure messages; an empty vector means the provider passed.
/// Checks use disjoint volume identities, so a single shared instance can be
/// checked in one pass.
pub async fn run_conformance(provider: &dyn VolumeProvider) -> Vec<String> {
    let mut failures = Vec::new();
    if let Err(failure) = check_create_and_inspect_round_trip(provider).await {
        failures.push(format!("create_and_inspect_round_trip: {failure}"));
    }
    if let Err(failure) = check_idempotent_create(provider).await {
        failures.push(format!("idempotent_create: {failure}"));
    }
    if let Err(failure) = check_idempotent_attach(provider).await {
        failures.push(format!("idempotent_attach: {failure}"));
    }
    if let Err(failure) = check_create_payload_conflict(provider).await {
        failures.push(format!("create_payload_conflict: {failure}"));
    }
    if let Err(failure) = check_attach_single_writer(provider).await {
        failures.push(format!("attach_single_writer: {failure}"));
    }
    if let Err(failure) = check_attachment_state_report(provider).await {
        failures.push(format!("attachment_state_report: {failure}"));
    }
    if let Err(failure) = check_attach_stale_generation(provider).await {
        failures.push(format!("attach_stale_generation: {failure}"));
    }
    if let Err(failure) = check_detach_fencing(provider).await {
        failures.push(format!("detach_fencing: {failure}"));
    }
    if let Err(failure) = check_grow_semantics(provider).await {
        failures.push(format!("grow_semantics: {failure}"));
    }
    if let Err(failure) = check_delete_lifecycle(provider).await {
        failures.push(format!("delete_lifecycle: {failure}"));
    }
    if let Err(failure) = check_list_project_filter(provider).await {
        failures.push(format!("list_project_filter: {failure}"));
    }
    if let Err(failure) = check_resize_capability_gate(provider).await {
        failures.push(format!("resize_capability_gate: {failure}"));
    }
    if let Err(failure) = check_attach_missing_volume(provider).await {
        failures.push(format!("attach_missing_volume: {failure}"));
    }
    if let Err(failure) = check_move_contract(provider).await {
        failures.push(format!("move_contract: {failure}"));
    }
    failures
}

/// Run every conformance check; `Err` carries the full failure list.
///
/// This is test-kit code: callers decide whether to panic, unwrap or
/// soft-fail on the returned `Result`.
pub async fn assert_conformance(provider: &dyn VolumeProvider) -> Result<(), String> {
    let failures = run_conformance(provider).await;
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "provider conformance failures ({}):\n- {}",
            failures.len(),
            failures.join("\n- ")
        ))
    }
}

/// Generate `#[tokio::test]` functions running the conformance kit.
///
/// The expression must evaluate to a callable (closure or function) taking
/// no arguments and returning a fresh provider handle for every call —
/// typically `Box<dyn VolumeProvider>` or `Arc<YourProvider>` (anything
/// that dereferences to the provider). Each generated test constructs its
/// own provider instance, so tests are isolated.
///
/// The invoking crate needs `tokio` (features `macros` and `rt`) in scope,
/// usually as a dev-dependency.
///
/// # Examples
///
/// ```ignore
/// volvisor_provider::provider_conformance_tests! {
///     || Box::new(FakeProvider::new()) as Box<dyn volvisor_provider::VolumeProvider>
/// }
/// ```
#[macro_export]
macro_rules! provider_conformance_tests {
    ($make_provider:expr) => {
        mod provider_conformance {
            //! Generated by `volvisor_provider::provider_conformance_tests!`.

            use super::*;

            #[tokio::test]
            async fn create_and_inspect_round_trip() {
                let provider = $make_provider();
                let provider: &dyn $crate::VolumeProvider = &*provider;
                let outcome =
                    $crate::conformance::check_create_and_inspect_round_trip(provider).await;
                assert_eq!(outcome, Ok(()), "conformance failure");
            }

            #[tokio::test]
            async fn idempotent_create() {
                let provider = $make_provider();
                let provider: &dyn $crate::VolumeProvider = &*provider;
                let outcome = $crate::conformance::check_idempotent_create(provider).await;
                assert_eq!(outcome, Ok(()), "conformance failure");
            }

            #[tokio::test]
            async fn idempotent_attach() {
                let provider = $make_provider();
                let provider: &dyn $crate::VolumeProvider = &*provider;
                let outcome = $crate::conformance::check_idempotent_attach(provider).await;
                assert_eq!(outcome, Ok(()), "conformance failure");
            }

            #[tokio::test]
            async fn create_payload_conflict() {
                let provider = $make_provider();
                let provider: &dyn $crate::VolumeProvider = &*provider;
                let outcome = $crate::conformance::check_create_payload_conflict(provider).await;
                assert_eq!(outcome, Ok(()), "conformance failure");
            }

            #[tokio::test]
            async fn attach_single_writer() {
                let provider = $make_provider();
                let provider: &dyn $crate::VolumeProvider = &*provider;
                let outcome = $crate::conformance::check_attach_single_writer(provider).await;
                assert_eq!(outcome, Ok(()), "conformance failure");
            }

            #[tokio::test]
            async fn attach_stale_generation() {
                let provider = $make_provider();
                let provider: &dyn $crate::VolumeProvider = &*provider;
                let outcome = $crate::conformance::check_attach_stale_generation(provider).await;
                assert_eq!(outcome, Ok(()), "conformance failure");
            }

            #[tokio::test]
            async fn attachment_state_report() {
                let provider = $make_provider();
                let provider: &dyn $crate::VolumeProvider = &*provider;
                let outcome = $crate::conformance::check_attachment_state_report(provider).await;
                assert_eq!(outcome, Ok(()), "conformance failure");
            }

            #[tokio::test]
            async fn detach_fencing() {
                let provider = $make_provider();
                let provider: &dyn $crate::VolumeProvider = &*provider;
                let outcome = $crate::conformance::check_detach_fencing(provider).await;
                assert_eq!(outcome, Ok(()), "conformance failure");
            }

            #[tokio::test]
            async fn grow_semantics() {
                let provider = $make_provider();
                let provider: &dyn $crate::VolumeProvider = &*provider;
                let outcome = $crate::conformance::check_grow_semantics(provider).await;
                assert_eq!(outcome, Ok(()), "conformance failure");
            }

            #[tokio::test]
            async fn delete_lifecycle() {
                let provider = $make_provider();
                let provider: &dyn $crate::VolumeProvider = &*provider;
                let outcome = $crate::conformance::check_delete_lifecycle(provider).await;
                assert_eq!(outcome, Ok(()), "conformance failure");
            }

            #[tokio::test]
            async fn list_project_filter() {
                let provider = $make_provider();
                let provider: &dyn $crate::VolumeProvider = &*provider;
                let outcome = $crate::conformance::check_list_project_filter(provider).await;
                assert_eq!(outcome, Ok(()), "conformance failure");
            }

            #[tokio::test]
            async fn resize_capability_gate() {
                let provider = $make_provider();
                let provider: &dyn $crate::VolumeProvider = &*provider;
                let outcome = $crate::conformance::check_resize_capability_gate(provider).await;
                assert_eq!(outcome, Ok(()), "conformance failure");
            }

            #[tokio::test]
            async fn attach_missing_volume() {
                let provider = $make_provider();
                let provider: &dyn $crate::VolumeProvider = &*provider;
                let outcome = $crate::conformance::check_attach_missing_volume(provider).await;
                assert_eq!(outcome, Ok(()), "conformance failure");
            }

            #[tokio::test]
            async fn move_contract() {
                let provider = $make_provider();
                let provider: &dyn $crate::VolumeProvider = &*provider;
                let outcome = $crate::conformance::check_move_contract(provider).await;
                assert_eq!(outcome, Ok(()), "conformance failure");
            }

            #[tokio::test]
            async fn parity() {
                let provider = $make_provider();
                let provider: &dyn $crate::VolumeProvider = &*provider;
                let outcome = $crate::conformance::assert_parity(provider).await;
                assert_eq!(outcome, Ok(()), "parity failure");
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeProvider;
    use volvisor_types::CapabilitySet;

    fn make_fake() -> Box<dyn VolumeProvider> {
        Box::new(FakeProvider::new())
    }

    crate::provider_conformance_tests! { make_fake }

    /// The whole suite must also pass against a single shared instance
    /// (checks use disjoint volume identities).
    #[tokio::test]
    async fn full_suite_on_a_shared_instance() {
        let provider = FakeProvider::new();
        let provider: &dyn VolumeProvider = &provider;
        let outcome = assert_conformance(provider).await;
        assert_eq!(outcome, Ok(()));
    }

    /// A provider whose second attach call returns a fabricated projection:
    /// the decision class is still `Ok`, so only a response-fidelity check
    /// can catch it.
    struct DivergingReplay {
        inner: FakeProvider,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl VolumeProvider for DivergingReplay {
        fn name(&self) -> &str {
            self.inner.name()
        }

        fn capabilities(&self) -> CapabilitySet {
            self.inner.capabilities()
        }

        fn supported_classes(&self) -> &[VolumeClass] {
            self.inner.supported_classes()
        }

        async fn create_volume(
            &self,
            req: &CreateVolumeRequest,
        ) -> Result<volvisor_types::request::InspectVolumeResponse, ApiError> {
            self.inner.create_volume(req).await
        }

        async fn inspect_volume(
            &self,
            id: &VolumeId,
        ) -> Result<volvisor_types::request::InspectVolumeResponse, ApiError> {
            self.inner.inspect_volume(id).await
        }

        async fn list_volumes(
            &self,
            project: Option<&ProjectId>,
        ) -> Result<Vec<volvisor_types::request::InspectVolumeResponse>, ApiError> {
            self.inner.list_volumes(project).await
        }

        async fn attach_volume(
            &self,
            volume_id: &VolumeId,
            req: &AttachVolumeRequest,
        ) -> Result<volvisor_types::request::AttachVolumeResponse, ApiError> {
            let mut response = self.inner.attach_volume(volume_id, req).await?;
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                response.state = AttachmentState::Active;
            }
            Ok(response)
        }

        async fn detach_volume(
            &self,
            volume_id: &VolumeId,
            attachment_id: &AttachmentId,
            req: &DetachVolumeRequest,
        ) -> Result<volvisor_types::request::InspectVolumeResponse, ApiError> {
            self.inner
                .detach_volume(volume_id, attachment_id, req)
                .await
        }

        async fn grow_volume(
            &self,
            volume_id: &VolumeId,
            req: &GrowVolumeRequest,
        ) -> Result<volvisor_types::request::GrowVolumeResponse, ApiError> {
            self.inner.grow_volume(volume_id, req).await
        }

        async fn delete_volume(
            &self,
            volume_id: &VolumeId,
            req: &DeleteVolumeRequest,
        ) -> Result<(), ApiError> {
            self.inner.delete_volume(volume_id, req).await
        }
    }

    #[tokio::test]
    async fn idempotent_attach_rejects_a_fabricated_replay_projection() {
        let provider = DivergingReplay {
            inner: FakeProvider::new(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let failure = check_idempotent_attach(&provider)
            .await
            .expect_err("a changed replay projection must fail the check");
        assert!(
            failure.contains("recorded instance"),
            "failure names the replay fidelity violation: {failure}"
        );
    }

    #[tokio::test]
    async fn universal_parity_is_the_expected_sequence() {
        let provider = FakeProvider::new();
        assert_eq!(
            run_universal_parity(&provider).await,
            expected_universal_parity()
        );
    }

    #[tokio::test]
    async fn report_parity_is_the_expected_sequence() {
        let provider = FakeProvider::new();
        assert_eq!(
            run_report_parity(&provider).await,
            expected_report_parity(true)
        );
    }

    #[tokio::test]
    async fn a_report_incapable_fake_fails_closed_on_reports() {
        let provider = FakeProvider::new()
            .with_capabilities(CapabilitySet::of([Capability::Create, Capability::Attach]));
        assert_eq!(
            run_report_parity(&provider).await,
            expected_report_parity(false)
        );
    }

    /// A provider that does not advertise `resize` must still pass the
    /// capability-gate check by rejecting grow fail-closed.
    #[tokio::test]
    async fn resize_gate_covers_a_provider_without_resize() {
        let provider = FakeProvider::new()
            .with_capabilities(CapabilitySet::of([Capability::Create, Capability::Attach]));
        let provider: &dyn VolumeProvider = &provider;
        assert!(!provider.capabilities().contains(Capability::Resize));
        let outcome = check_resize_capability_gate(provider).await;
        assert_eq!(outcome, Ok(()));
    }
}
