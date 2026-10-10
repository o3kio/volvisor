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
//! idempotent create, payload-conflict detection, single-writer enforcement,
//! generation fencing on attach/detach/grow/delete, detach drain
//! preconditions, grow-only resize, delete preconditions, project-scoped
//! listing, capability gating and missing-volume handling.
//!
//! The kit targets the P0 `native-local` profile: fixtures build
//! `native-local` thick volumes, and post-create evidence is asserted to be
//! `PrototypeOnly`.

// This module is test-kit code shipped in the library so other crates can
// run it against their providers; fixture builders may `expect` on
// internally validated constant identifiers.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use crate::VolumeProvider;
use volvisor_types::domain::{EvidenceStatus, Health, VolumeClass};
use volvisor_types::request::{
    AccessModeRequest, AttachVolumeRequest, CreateVolumeRequest, DeleteVolumeRequest,
    DetachVolumeRequest, DrainProof, ErasurePolicy, GrowGuestNotification, GrowVolumeRequest,
};
use volvisor_types::{
    ApiErrorCode, AttachmentId, AttachmentState, Capability, Frontend, HostId, OperationId,
    ProjectId, VolumeId, VolumeLifecycle,
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
        first.attachment_generation == 1,
        "a fresh attachment starts at generation 1",
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

// ---------------------------------------------------------------------------
// Suite entry points
// ---------------------------------------------------------------------------

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
    if let Err(failure) = check_create_payload_conflict(provider).await {
        failures.push(format!("create_payload_conflict: {failure}"));
    }
    if let Err(failure) = check_attach_single_writer(provider).await {
        failures.push(format!("attach_single_writer: {failure}"));
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
