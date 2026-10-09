//! Deterministic in-memory [`crate::VolumeProvider`] for tests.
//!
//! [`FakeProvider`] is thread-safe (a single tokio mutex serializes every
//! mutation) and honors the full [`crate::VolumeProvider`] semantic contract:
//! generation fencing, single-writer attachments, fail-closed policy
//! negotiation, idempotent create, host-scoped handles, honest `Unknown`
//! health, grow-only resize and quarantine-instead-of-adopt on foreign
//! state. It performs no real I/O: every support claim it makes is
//! `PrototypeOnly` evidence.

use std::collections::BTreeMap;

use async_trait::async_trait;
use volvisor_types::domain::{
    AccessMode, EffectiveProtection, EvidenceStatus, FailureDomain, Frontend, Generation, Health,
    LocalProtectionAxis, Provisioning, RemoteProtectionAxis, Volume, VolumeClass,
};
use volvisor_types::request::{
    AccessModeRequest, AttachVolumeRequest, AttachVolumeResponse, CreateVolumeRequest,
    DeleteVolumeRequest, DetachVolumeRequest, GrowGuestNotification, GrowVolumeRequest,
    GrowVolumeResponse, InspectVolumeResponse, LocalProtectionModeRequest,
};
use volvisor_types::{
    ApiError, ApiErrorCode, Attachment, AttachmentId, AttachmentState, Capability, CapabilitySet,
    PoolId, ProjectId, VolumeId, VolumeLifecycle, validate_api_version,
};

/// Host identity the fake places every volume on.
const FAKE_HOST: &str = "fake-host-1";
/// Pool identity backing every fake volume.
const FAKE_POOL: &str = "fake-pool-1";
/// Default physical capacity advertised by the fake (1 TiB).
const DEFAULT_CAPACITY_BYTES: u64 = 1 << 40;
/// Default logical block size when a create request omits one.
const DEFAULT_BLOCK_SIZE: u32 = 4096;

/// Mutable fake state, guarded by the provider mutex.
#[derive(Default, Debug)]
struct FakeState {
    volumes: BTreeMap<VolumeId, Volume>,
    attachments: BTreeMap<AttachmentId, Attachment>,
    /// Canonical creation payload per volume id (idempotent-create check).
    creation_payloads: BTreeMap<VolumeId, String>,
    /// Fault injected by [`FakeProvider::set_next_failure`].
    next_failure: Option<ApiError>,
    /// Whether the fake admin device is claimed (admin-surface tests).
    admin_device_claimed: bool,
}

/// Deterministic, thread-safe, fault-injectable in-memory provider.
///
/// # Idempotency model (documented choice)
///
/// Provider-level create idempotency is keyed by `volume_id`: an identical
/// creation payload (with the `operation_id` normalized out, since
/// `operation_id` replay is the journal layer's responsibility per Volume
/// API v2 section 7) returns the existing volume's *current* state; a
/// different payload for the same `volume_id` fails with
/// `IDEMPOTENCY_CONFLICT` instead of silently creating a duplicate or
/// mutating the existing volume.
///
/// Attach idempotency is keyed by `attachment_id`: replaying an attach with
/// the same volume, VM, host and access mode returns the recorded
/// attachment and never fabricates a second one (Volume API v2 section 3,
/// "a crash must not fabricate a second attachment"); the same
/// `attachment_id` with a different payload is an `IDEMPOTENCY_CONFLICT`.
#[derive(Debug)]
pub struct FakeProvider {
    name: String,
    host_id: String,
    capabilities: CapabilitySet,
    supported_classes: Vec<VolumeClass>,
    capacity_bytes: u64,
    state: tokio::sync::Mutex<FakeState>,
}

impl FakeProvider {
    /// A default fake: name `fake`, the `native_local_p0` capability set,
    /// serving [`VolumeClass::NativeLocal`] with 1 TiB of capacity.
    #[must_use]
    pub fn new() -> Self {
        Self {
            name: "fake".to_owned(),
            host_id: FAKE_HOST.to_owned(),
            capabilities: CapabilitySet::native_local_p0(),
            supported_classes: vec![VolumeClass::NativeLocal],
            capacity_bytes: DEFAULT_CAPACITY_BYTES,
            state: tokio::sync::Mutex::new(FakeState::default()),
        }
    }

    /// Override the advertised capability set (capability-negotiation tests).
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: CapabilitySet) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Override the served volume classes (fail-closed class negotiation
    /// tests).
    #[must_use]
    pub fn with_supported_classes(mut self, classes: &[VolumeClass]) -> Self {
        self.supported_classes = classes.to_vec();
        self
    }

    /// Override the total physical capacity in bytes (`NO_SAFE_CAPACITY`
    /// tests).
    #[must_use]
    pub fn with_capacity_bytes(mut self, capacity_bytes: u64) -> Self {
        self.capacity_bytes = capacity_bytes;
        self
    }

    /// Fault injection: the next mutation attempt fails with `failure`
    /// before any state is applied, then the fault clears.
    ///
    /// This models an in-flight or crashed mutation for crash-replay and
    /// journal-ordering tests: state is observably unchanged after the
    /// injected failure, and the retried mutation (with a fresh expected
    /// generation) succeeds normally.
    pub async fn set_next_failure(&self, failure: Option<ApiError>) {
        self.state.lock().await.next_failure = failure;
    }

    /// Admin-surface state: whether the fake device is claimed.
    pub(crate) async fn admin_device_claimed(&self) -> bool {
        self.state.lock().await.admin_device_claimed
    }

    /// Admin-surface state: set the fake device claim flag.
    pub(crate) async fn set_admin_device_claimed(&self, claimed: bool) {
        self.state.lock().await.admin_device_claimed = claimed;
    }

    /// Test hook: force a volume into an arbitrary lifecycle state,
    /// bypassing the state machine.
    ///
    /// This exists so tests can reach states that the synchronous fake
    /// cannot produce through the public API (`Detaching`, `Deleting`,
    /// `Quarantined`, ...). Real providers never expose an equivalent; the
    /// fake's own mutation paths still validate every transition.
    pub async fn force_state(
        &self,
        volume_id: &VolumeId,
        state: VolumeLifecycle,
    ) -> Result<(), ApiError> {
        self.state
            .lock()
            .await
            .volumes
            .get_mut(volume_id)
            .map_or_else(
                || Err(ApiError::not_found(format!("volume {volume_id} not found"))),
                |vol| {
                    vol.state = state;
                    Ok(())
                },
            )
    }

    /// Number of stored attachments (asserting "no fabricated second
    /// attachment" in crash-replay tests).
    pub async fn attachment_count(&self) -> usize {
        self.state.lock().await.attachments.len()
    }

    /// Run a mutation under the state lock with fault injection applied
    /// first (before any validation or state change).
    async fn mutate<T>(
        &self,
        apply: impl FnOnce(&mut FakeState) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        let mut state = self.state.lock().await;
        if let Some(failure) = state.next_failure.take() {
            return Err(failure);
        }
        apply(&mut state)
    }
}

impl Default for FakeProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl crate::VolumeProvider for FakeProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> CapabilitySet {
        self.capabilities.clone()
    }

    fn supported_classes(&self) -> &[VolumeClass] {
        &self.supported_classes
    }

    async fn create_volume(
        &self,
        req: &CreateVolumeRequest,
    ) -> Result<InspectVolumeResponse, ApiError> {
        self.mutate(|state| apply_create(self, state, req)).await
    }

    async fn inspect_volume(&self, id: &VolumeId) -> Result<InspectVolumeResponse, ApiError> {
        let state = self.state.lock().await;
        state
            .volumes
            .get(id)
            .map_or_else(|| Err(not_found(id)), |vol| Ok(inspect_response(vol)))
    }

    async fn list_volumes(
        &self,
        project: Option<&ProjectId>,
    ) -> Result<Vec<InspectVolumeResponse>, ApiError> {
        let state = self.state.lock().await;
        Ok(state
            .volumes
            .values()
            .filter(|vol| project.is_none_or(|p| &vol.project_id == p))
            .map(inspect_response)
            .collect())
    }

    async fn attach_volume(
        &self,
        volume_id: &VolumeId,
        req: &AttachVolumeRequest,
    ) -> Result<AttachVolumeResponse, ApiError> {
        self.mutate(|state| apply_attach(self, state, volume_id, req))
            .await
    }

    async fn detach_volume(
        &self,
        volume_id: &VolumeId,
        attachment_id: &AttachmentId,
        req: &DetachVolumeRequest,
    ) -> Result<InspectVolumeResponse, ApiError> {
        self.mutate(|state| apply_detach(state, volume_id, attachment_id, req))
            .await
    }

    async fn grow_volume(
        &self,
        volume_id: &VolumeId,
        req: &GrowVolumeRequest,
    ) -> Result<GrowVolumeResponse, ApiError> {
        self.mutate(|state| apply_grow(self, state, volume_id, req))
            .await
    }

    async fn delete_volume(
        &self,
        volume_id: &VolumeId,
        req: &DeleteVolumeRequest,
    ) -> Result<(), ApiError> {
        self.mutate(|state| apply_delete(state, volume_id, req))
            .await
    }
}

// ---------------------------------------------------------------------------
// Mutation implementations (pure functions over the locked state)
// ---------------------------------------------------------------------------

fn apply_create(
    provider: &FakeProvider,
    state: &mut FakeState,
    req: &CreateVolumeRequest,
) -> Result<InspectVolumeResponse, ApiError> {
    req.validate()?;
    if !provider.capabilities.contains(Capability::Create) {
        return Err(unsupported("the create capability is not advertised"));
    }
    if !provider.supported_classes.contains(&req.volume_class) {
        return Err(unsupported(format!(
            "volume class {:?} is not served by this provider",
            req.volume_class
        )));
    }
    check_class_capability(provider, req)?;
    check_policies(provider, req)?;
    check_placement(provider, req)?;

    // Provider-level create idempotency: same volume_id + same payload
    // (operation_id normalized out) replays; a different payload conflicts.
    let payload_key = create_payload_key(req)?;
    if let Some(existing) = state.volumes.get(&req.volume_id) {
        if state
            .creation_payloads
            .get(&req.volume_id)
            .is_some_and(|key| *key == payload_key)
        {
            return Ok(inspect_response(existing));
        }
        return Err(ApiError::idempotency_conflict(&req.volume_id));
    }

    let remaining = provider.capacity_bytes.saturating_sub(used_bytes(state));
    if req.size_bytes > remaining {
        return Err(ApiError::new(
            ApiErrorCode::NoSafeCapacity,
            format!(
                "no safe capacity: requested {} bytes, {remaining} remaining",
                req.size_bytes
            ),
        ));
    }

    let effective_local = match req.local_protection.as_ref().map(|lp| lp.mode) {
        None | Some(LocalProtectionModeRequest::None) => LocalProtectionAxis::None,
        Some(LocalProtectionModeRequest::Mirror) => LocalProtectionAxis::Mirror,
        Some(LocalProtectionModeRequest::ProviderSpecific) => LocalProtectionAxis::ProviderSpecific,
    };
    let effective_remote = if req.volume_class == VolumeClass::NearlineReplicated {
        // Policy modes async/semi-sync/sync all sit on the asynchronous-peer
        // axis (a possible-RPO remote copy; never conflated with local legs).
        RemoteProtectionAxis::AsynchronousPeer
    } else {
        RemoteProtectionAxis::None
    };
    let pool_ref = PoolId::new(FAKE_POOL)
        .map_err(|e| ApiError::new(ApiErrorCode::Internal, format!("{e}")))?;
    let volume = Volume {
        id: req.volume_id.clone(),
        project_id: req.project_id.clone(),
        class: req.volume_class,
        pool_ref,
        size_bytes: req.size_bytes,
        provisioning: Provisioning::Thick,
        block_size: req.logical_block_size.unwrap_or(DEFAULT_BLOCK_SIZE),
        generation: Generation(1),
        state: VolumeLifecycle::Ready,
        effective_protection: EffectiveProtection {
            local: effective_local,
            remote: effective_remote,
        },
        failure_domain: FailureDomain::Host,
        health: Health::Unknown,
        attachment_ids: Vec::new(),
        current_writer: None,
        data_epoch: 0,
        // Provider-internal, never exposed through InspectVolumeResponse.
        backend_private_ref: Some(format!("fake-backend-ref:{}", req.volume_id)),
        evidence_status: EvidenceStatus::PrototypeOnly,
    };
    state
        .creation_payloads
        .insert(req.volume_id.clone(), payload_key);
    state.volumes.insert(req.volume_id.clone(), volume.clone());
    Ok(inspect_response(&volume))
}

fn apply_attach(
    provider: &FakeProvider,
    state: &mut FakeState,
    volume_id: &VolumeId,
    req: &AttachVolumeRequest,
) -> Result<AttachVolumeResponse, ApiError> {
    req.validate()?;
    if !provider.capabilities.contains(Capability::Attach) {
        return Err(unsupported("the attach capability is not advertised"));
    }
    if let Some(frontend) = &req.requested_frontend {
        if frontend != "virtio-blk" {
            return Err(unsupported(format!(
                "requested frontend {frontend:?}: only virtio-blk exists"
            )));
        }
    }

    // Idempotent replay by attachment identity: a crash-replayed request
    // must never fabricate a second attachment (Volume API v2 section 3).
    if let Some(existing) = state.attachments.get(&req.attachment_id) {
        let same_payload = existing.volume_id == *volume_id
            && existing.vm_id == req.vm_id
            && existing.host_id == req.host_id
            && existing.access_mode == requested_mode(req.access_mode);
        if same_payload {
            let vol = state.volumes.get(volume_id).ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "fake state invariant violated: attachment {} without volume {volume_id}",
                        req.attachment_id
                    ),
                )
            })?;
            return Ok(attach_response(existing, vol));
        }
        return Err(ApiError::idempotency_conflict(&req.attachment_id));
    }

    let vol = state
        .volumes
        .get_mut(volume_id)
        .ok_or_else(|| not_found(volume_id))?;
    if req.expected_volume_generation != vol.generation.0 {
        return Err(ApiError::stale_generation(
            req.expected_volume_generation,
            vol.generation.0,
        ));
    }
    if !matches!(
        vol.state,
        VolumeLifecycle::Ready | VolumeLifecycle::Attached | VolumeLifecycle::Degraded
    ) {
        let current = vol.state;
        return Err(ApiError::new(
            ApiErrorCode::InvalidState,
            format!("attach requires Ready/Attached/Degraded, volume is {current:?}"),
        ));
    }

    let mode = requested_mode(req.access_mode);
    match mode {
        AccessMode::SingleWriter => {
            // A writer excludes every other attachment, readers included.
            if vol.current_writer.is_some() || !vol.attachment_ids.is_empty() {
                return Err(ApiError::new(
                    ApiErrorCode::WriterAlreadyActive,
                    format!("volume {volume_id} already has an active attachment"),
                ));
            }
        }
        AccessMode::ReadOnly => {
            if vol.current_writer.is_some() {
                return Err(ApiError::new(
                    ApiErrorCode::WriterAlreadyActive,
                    format!("volume {volume_id} already has an active writer"),
                ));
            }
        }
    }

    let attachment = Attachment {
        id: req.attachment_id.clone(),
        volume_id: volume_id.clone(),
        vm_id: req.vm_id.clone(),
        host_id: req.host_id.clone(),
        generation: Generation(1),
        // Host-scoped, ephemeral, deterministic; contains no secret material.
        frontend: Frontend::VirtioBlk {
            host_device_path: format!("/dev/volvisor-fake/{volume_id}"),
        },
        access_mode: mode,
        state: AttachmentState::Prepared,
    };
    vol.attachment_ids.push(attachment.id.clone());
    if mode == AccessMode::SingleWriter {
        vol.current_writer = Some(attachment.id.clone());
    }
    vol.state = VolumeLifecycle::Attached;
    vol.generation = vol.generation.next();
    let response = attach_response(&attachment, vol);
    state
        .attachments
        .insert(req.attachment_id.clone(), attachment);
    Ok(response)
}

fn apply_detach(
    state: &mut FakeState,
    volume_id: &VolumeId,
    attachment_id: &AttachmentId,
    req: &DetachVolumeRequest,
) -> Result<InspectVolumeResponse, ApiError> {
    req.validate()?;
    let (attachment_volume, attachment_generation) = match state.attachments.get(attachment_id) {
        Some(att) => (att.volume_id.clone(), att.generation.0),
        None => {
            return Err(ApiError::not_found(format!(
                "attachment {attachment_id} not found"
            )));
        }
    };
    if attachment_volume != *volume_id {
        return Err(ApiError::not_found(format!(
            "attachment {attachment_id} does not belong to volume {volume_id}"
        )));
    }
    if req.expected_attachment_generation != attachment_generation {
        return Err(ApiError::stale_generation(
            req.expected_attachment_generation,
            attachment_generation,
        ));
    }
    // The drain proof is a type-mandatory attestation (`DrainProof`); the
    // provider requires it but cannot verify it — authority is only
    // released with the proof present (Volume API v2 section 3).

    let vol = state
        .volumes
        .get_mut(volume_id)
        .ok_or_else(|| not_found(volume_id))?;
    state.attachments.remove(attachment_id);
    vol.attachment_ids.retain(|id| id != attachment_id);
    if vol.current_writer.as_ref() == Some(attachment_id) {
        vol.current_writer = None;
    }
    if vol.attachment_ids.is_empty() && vol.state == VolumeLifecycle::Attached {
        vol.state = VolumeLifecycle::Ready;
    }
    vol.generation = vol.generation.next();
    Ok(inspect_response(vol))
}

fn apply_grow(
    provider: &FakeProvider,
    state: &mut FakeState,
    volume_id: &VolumeId,
    req: &GrowVolumeRequest,
) -> Result<GrowVolumeResponse, ApiError> {
    validate_api_version(&req.api_version)?;
    if !provider.capabilities.contains(Capability::Resize) {
        return Err(unsupported("the resize capability is not advertised"));
    }
    let remaining = provider.capacity_bytes.saturating_sub(used_bytes(state));
    let vol = state
        .volumes
        .get_mut(volume_id)
        .ok_or_else(|| not_found(volume_id))?;
    if req.expected_generation != vol.generation.0 {
        return Err(ApiError::stale_generation(
            req.expected_generation,
            vol.generation.0,
        ));
    }
    if !matches!(
        vol.state,
        VolumeLifecycle::Ready | VolumeLifecycle::Attached | VolumeLifecycle::Degraded
    ) {
        let current = vol.state;
        return Err(ApiError::new(
            ApiErrorCode::InvalidState,
            format!("grow requires Ready/Attached/Degraded, volume is {current:?}"),
        ));
    }
    // Grow-only plus alignment, fail-closed (Volume API v2 section 4A).
    req.validate(vol.size_bytes)?;
    let delta = req.new_size_bytes - vol.size_bytes;
    if delta > remaining {
        return Err(ApiError::new(
            ApiErrorCode::NoSafeCapacity,
            format!("grow needs {delta} more bytes, {remaining} remaining"),
        ));
    }

    let has_frontend = !vol.attachment_ids.is_empty();
    vol.size_bytes = req.new_size_bytes;
    vol.generation = vol.generation.next();
    Ok(GrowVolumeResponse {
        backing_resized: true,
        // Honest notification status: the fake has no VMM integration, so an
        // attached frontend still needs a (retried) notification and a
        // detached volume has nobody to notify. Never `Notified`.
        guest_notification_status: if has_frontend {
            GrowGuestNotification::RetryRequired
        } else {
            GrowGuestNotification::NotApplicable
        },
        effective_size_bytes: vol.size_bytes,
    })
}

fn apply_delete(
    state: &mut FakeState,
    volume_id: &VolumeId,
    req: &DeleteVolumeRequest,
) -> Result<(), ApiError> {
    validate_api_version(&req.api_version)?;
    let vol = state
        .volumes
        .get_mut(volume_id)
        .ok_or_else(|| not_found(volume_id))?;
    if req.expected_generation != vol.generation.0 {
        return Err(ApiError::stale_generation(
            req.expected_generation,
            vol.generation.0,
        ));
    }
    if vol.current_writer.is_some() || !vol.attachment_ids.is_empty() {
        return Err(ApiError::new(
            ApiErrorCode::InvalidState,
            format!("volume {volume_id} must be fully detached before delete"),
        ));
    }
    match vol.state {
        VolumeLifecycle::Ready | VolumeLifecycle::Degraded | VolumeLifecycle::Failed => {}
        VolumeLifecycle::Quarantined => {
            return Err(ApiError::new(
                ApiErrorCode::ForeignDeviceState,
                format!(
                    "volume {volume_id} is quarantined: foreign backend state is \
                     investigated, never auto-deleted (AGENTS rule 7)"
                ),
            ));
        }
        other => {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!("delete requires a detached, stable state; volume is {other:?}"),
            ));
        }
    }
    if matches!(
        req.data_erasure_policy,
        volvisor_types::request::ErasurePolicy::Cryptographic
    ) {
        return Err(unsupported(
            "cryptographic erasure requires a separate evidence gate",
        ));
    }
    state.volumes.remove(volume_id);
    state.creation_payloads.remove(volume_id);
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn check_class_capability(
    provider: &FakeProvider,
    req: &CreateVolumeRequest,
) -> Result<(), ApiError> {
    let required = match req.volume_class {
        VolumeClass::NativeLocal => None,
        VolumeClass::NearlineReplicated => Some(Capability::Replicate),
        VolumeClass::CephRbd => Some(Capability::RbdClusterAdapter),
    };
    if let Some(cap) = required {
        if !provider.capabilities.contains(cap) {
            return Err(unsupported(format!(
                "volume class {:?} requires the {} capability",
                req.volume_class,
                cap.wire_name()
            )));
        }
    }
    Ok(())
}

fn check_policies(provider: &FakeProvider, req: &CreateVolumeRequest) -> Result<(), ApiError> {
    // The fake implements none of these; fail closed instead of degrading.
    if req.provisioning == Some(Provisioning::Thin) {
        return Err(unsupported(
            "thin provisioning: the fake provider is thick-only",
        ));
    }
    if req.encryption.is_some() {
        return Err(unsupported("encryption: the fake provider implements none"));
    }
    if req.migration_policy.is_some() {
        return Err(unsupported(
            "migration policy: the fake provider implements no migration semantics",
        ));
    }
    if let Some(local) = &req.local_protection {
        match local.mode {
            LocalProtectionModeRequest::None => {
                if local.min_healthy_legs.is_some() {
                    return Err(unsupported(
                        "min_healthy_legs is meaningless without a local mirror",
                    ));
                }
            }
            LocalProtectionModeRequest::Mirror => {
                if !provider.capabilities.contains(Capability::LocalMirror) {
                    return Err(unsupported(
                        "local mirror: the local_mirror capability is not advertised",
                    ));
                }
            }
            LocalProtectionModeRequest::ProviderSpecific => {
                return Err(unsupported("provider_specific local protection"));
            }
        }
    }
    Ok(())
}

fn check_placement(provider: &FakeProvider, req: &CreateVolumeRequest) -> Result<(), ApiError> {
    if let Some(placement) = &req.placement {
        if let Some(preferred) = &placement.preferred_host_id {
            if preferred.as_str() != provider.host_id {
                return Err(ApiError::new(
                    ApiErrorCode::InsufficientFailureDomains,
                    format!(
                        "cannot honor preferred_host_id {preferred}: the fake provider serves \
                         the single host {}",
                        provider.host_id
                    ),
                ));
            }
        }
        if placement.failure_domain == Some(FailureDomain::Rack) {
            return Err(ApiError::new(
                ApiErrorCode::InsufficientFailureDomains,
                "rack failure-domain placement is unavailable on the single-host fake provider",
            ));
        }
    }
    Ok(())
}

/// Canonical creation payload with the idempotency key normalized out.
///
/// Deterministic because `serde_json` maps are ordered; fail-closed if
/// serialization itself fails.
fn create_payload_key(req: &CreateVolumeRequest) -> Result<String, ApiError> {
    let mut value = serde_json::to_value(req).map_err(|e| {
        ApiError::new(
            ApiErrorCode::Internal,
            format!("creation payload canonicalization failed: {e}"),
        )
    })?;
    if let serde_json::Value::Object(map) = &mut value {
        map.remove("operation_id");
    }
    Ok(value.to_string())
}

fn requested_mode(mode: AccessModeRequest) -> AccessMode {
    match mode {
        AccessModeRequest::SingleWriter => AccessMode::SingleWriter,
        AccessModeRequest::ReadOnly => AccessMode::ReadOnly,
    }
}

fn attach_response(attachment: &Attachment, vol: &Volume) -> AttachVolumeResponse {
    AttachVolumeResponse {
        attachment_id: attachment.id.clone(),
        attachment_generation: attachment.generation.0,
        volume_generation: vol.generation.0,
        frontend: attachment.frontend.clone(),
        state: attachment.state,
    }
}

fn inspect_response(vol: &Volume) -> InspectVolumeResponse {
    InspectVolumeResponse {
        volume_id: vol.id.clone(),
        backend_class: vol.class,
        project_id: vol.project_id.clone(),
        generation: vol.generation.0,
        state: vol.state,
        provisioned_bytes: vol.size_bytes,
        // The fake is thick-only, so allocation equals provisioning.
        allocated_bytes: vol.size_bytes,
        effective_protection: vol.effective_protection,
        failure_domain: vol.failure_domain,
        health: vol.health,
        attachment_ids: vol.attachment_ids.clone(),
        current_writer: vol.current_writer.clone(),
        backend_health: Health::Unknown,
        evidence_status: vol.evidence_status,
    }
}

fn used_bytes(state: &FakeState) -> u64 {
    state.volumes.values().map(|vol| vol.size_bytes).sum()
}

fn unsupported(detail: impl Into<String>) -> ApiError {
    ApiError::new(ApiErrorCode::UnsupportedClassOrPolicy, detail)
}

fn not_found(volume_id: &VolumeId) -> ApiError {
    ApiError::not_found(format!("volume {volume_id} not found"))
}

// ---------------------------------------------------------------------------
// FakeProvider-specific tests (the generic suite lives in `conformance`)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VolumeProvider;
    use crate::conformance::{
        fixture_attach_request, fixture_create_request, fixture_delete_request,
        fixture_detach_request, fixture_grow_request,
    };
    use volvisor_types::request::{
        ErasurePolicy, LocalProtectionRequest, Placement, ReplicationModeRequest,
        ReplicationPolicyRequest,
    };

    const GIB: u64 = 1 << 30;

    fn volume_id(raw: &str) -> VolumeId {
        VolumeId::new(raw).expect("valid fixture volume id")
    }

    fn attachment_id(raw: &str) -> AttachmentId {
        AttachmentId::new(raw).expect("valid fixture attachment id")
    }

    #[test]
    fn default_advertisement() {
        let provider = FakeProvider::new();
        assert_eq!(provider.name(), "fake");
        assert_eq!(provider.capabilities(), CapabilitySet::native_local_p0());
        assert_eq!(
            provider.supported_classes(),
            &[VolumeClass::NativeLocal][..]
        );
    }

    #[tokio::test]
    async fn create_round_trip_sets_honest_defaults() {
        let provider = FakeProvider::new();
        let created = provider
            .create_volume(&fixture_create_request("fake-defaults", GIB))
            .await
            .expect("create");
        assert_eq!(created.generation, 1);
        assert_eq!(created.state, VolumeLifecycle::Ready);
        assert_eq!(created.health, Health::Unknown);
        assert_eq!(created.backend_health, Health::Unknown);
        assert_eq!(created.evidence_status, EvidenceStatus::PrototypeOnly);
        assert_eq!(created.provisioned_bytes, GIB);
        assert_eq!(created.allocated_bytes, GIB);
    }

    #[tokio::test]
    async fn attach_rejected_in_non_attachable_states() {
        let provider = FakeProvider::new();
        let vid = volume_id("fake-states");
        provider
            .create_volume(&fixture_create_request("fake-states", GIB))
            .await
            .expect("create");
        let non_attachable = [
            VolumeLifecycle::Requested,
            VolumeLifecycle::Provisioning,
            VolumeLifecycle::Detaching,
            VolumeLifecycle::Deleting,
            VolumeLifecycle::Failed,
            VolumeLifecycle::Quarantined,
        ];
        for state in non_attachable {
            provider
                .force_state(&vid, state)
                .await
                .expect("force state");
            let err = provider
                .attach_volume(
                    &vid,
                    &fixture_attach_request("fake-states", "fake-states-att", 1),
                )
                .await
                .expect_err("attach must be rejected");
            assert_eq!(err.code, ApiErrorCode::InvalidState, "state {state:?}");
        }
    }

    #[tokio::test]
    async fn next_failure_fires_once_before_applying() {
        let provider = FakeProvider::new();
        let vid = volume_id("fake-fault");
        provider
            .create_volume(&fixture_create_request("fake-fault", GIB))
            .await
            .expect("create");

        provider
            .set_next_failure(Some(ApiError::new(
                ApiErrorCode::OperationInDoubt,
                "injected in-flight failure",
            )))
            .await;
        let err = provider
            .attach_volume(
                &vid,
                &fixture_attach_request("fake-fault", "fake-fault-att", 1),
            )
            .await
            .expect_err("injected failure");
        assert_eq!(err.code, ApiErrorCode::OperationInDoubt);

        // State is observably unchanged (the mutation never applied).
        let inspected = provider.inspect_volume(&vid).await.expect("inspect");
        assert_eq!(inspected.state, VolumeLifecycle::Ready);
        assert_eq!(inspected.generation, 1);
        assert!(inspected.attachment_ids.is_empty());
        assert_eq!(provider.attachment_count().await, 0);

        // The fault cleared: the retried mutation succeeds.
        provider
            .attach_volume(
                &vid,
                &fixture_attach_request("fake-fault", "fake-fault-att", 1),
            )
            .await
            .expect("retry after injected failure");
        assert_eq!(provider.attachment_count().await, 1);
    }

    #[tokio::test]
    async fn attach_replay_never_fabricates_a_second_attachment() {
        let provider = FakeProvider::new();
        let vid = volume_id("fake-replay");
        provider
            .create_volume(&fixture_create_request("fake-replay", GIB))
            .await
            .expect("create");
        let req = fixture_attach_request("fake-replay", "fake-replay-att", 1);
        let first = provider.attach_volume(&vid, &req).await.expect("attach");

        // Crash-replayed retry carries the original (now stale) expected
        // generation; it must replay, not create a second attachment.
        let replayed = provider
            .attach_volume(&vid, &req)
            .await
            .expect("idempotent attach replay");
        assert_eq!(replayed.attachment_id, first.attachment_id);
        assert_eq!(replayed.attachment_generation, first.attachment_generation);
        assert_eq!(provider.attachment_count().await, 1);

        let inspected = provider.inspect_volume(&vid).await.expect("inspect");
        assert_eq!(inspected.generation, first.volume_generation);
        assert_eq!(inspected.attachment_ids.len(), 1);
    }

    #[tokio::test]
    async fn attach_id_reuse_with_different_payload_is_a_conflict() {
        let provider = FakeProvider::new();
        let vid = volume_id("fake-reuse");
        provider
            .create_volume(&fixture_create_request("fake-reuse", GIB))
            .await
            .expect("create");
        provider
            .attach_volume(
                &vid,
                &fixture_attach_request("fake-reuse", "fake-reuse-att", 1),
            )
            .await
            .expect("attach");

        let mut conflicting = fixture_attach_request("fake-reuse", "fake-reuse-att", 1);
        conflicting.vm_id = "another-vm".to_owned();
        let err = provider
            .attach_volume(&vid, &conflicting)
            .await
            .expect_err("attachment id reuse with a different payload");
        assert_eq!(err.code, ApiErrorCode::IdempotencyConflict);
        assert_eq!(provider.attachment_count().await, 1);
    }

    #[tokio::test]
    async fn read_only_attachments_and_writer_exclusion() {
        let provider = FakeProvider::new();
        let vid = volume_id("fake-readers");
        provider
            .create_volume(&fixture_create_request("fake-readers", GIB))
            .await
            .expect("create");

        let mut reader = fixture_attach_request("fake-readers", "fake-readers-r1", 1);
        reader.access_mode = AccessModeRequest::ReadOnly;
        provider
            .attach_volume(&vid, &reader)
            .await
            .expect("first reader");

        let mut reader2 = fixture_attach_request("fake-readers", "fake-readers-r2", 2);
        reader2.access_mode = AccessModeRequest::ReadOnly;
        provider
            .attach_volume(&vid, &reader2)
            .await
            .expect("second reader");

        let inspected = provider.inspect_volume(&vid).await.expect("inspect");
        assert_eq!(inspected.state, VolumeLifecycle::Attached);
        assert_eq!(inspected.attachment_ids.len(), 2);
        assert!(inspected.current_writer.is_none());

        // A writer is excluded while readers are attached.
        let err = provider
            .attach_volume(
                &vid,
                &fixture_attach_request("fake-readers", "fake-readers-w", 3),
            )
            .await
            .expect_err("writer cannot join readers");
        assert_eq!(err.code, ApiErrorCode::WriterAlreadyActive);

        provider
            .detach_volume(
                &vid,
                &attachment_id("fake-readers-r1"),
                &fixture_detach_request("fake-readers-r1", 1),
            )
            .await
            .expect("detach reader 1");
        let detached = provider
            .detach_volume(
                &vid,
                &attachment_id("fake-readers-r2"),
                &fixture_detach_request("fake-readers-r2", 1),
            )
            .await
            .expect("detach reader 2");
        assert_eq!(detached.state, VolumeLifecycle::Ready);
        assert!(detached.attachment_ids.is_empty());

        // With the readers gone, the writer is admitted and then excludes
        // both other writers and new readers.
        provider
            .attach_volume(
                &vid,
                &fixture_attach_request("fake-readers", "fake-readers-w", 5),
            )
            .await
            .expect("writer after readers detached");
        let err = provider
            .attach_volume(
                &vid,
                &fixture_attach_request("fake-readers", "fake-readers-w2", 6),
            )
            .await
            .expect_err("second writer");
        assert_eq!(err.code, ApiErrorCode::WriterAlreadyActive);
        let mut reader3 = fixture_attach_request("fake-readers", "fake-readers-r3", 6);
        reader3.access_mode = AccessModeRequest::ReadOnly;
        let err = provider
            .attach_volume(&vid, &reader3)
            .await
            .expect_err("reader cannot join a writer");
        assert_eq!(err.code, ApiErrorCode::WriterAlreadyActive);
    }

    #[tokio::test]
    async fn concurrent_attaches_admit_exactly_one_writer() {
        let provider = FakeProvider::new();
        let vid = volume_id("fake-race");
        provider
            .create_volume(&fixture_create_request("fake-race", GIB))
            .await
            .expect("create");
        let req_a = fixture_attach_request("fake-race", "fake-race-att-a", 1);
        let req_b = fixture_attach_request("fake-race", "fake-race-att-b", 1);
        let (a, b) = tokio::join!(
            provider.attach_volume(&vid, &req_a),
            provider.attach_volume(&vid, &req_b)
        );
        assert!(
            a.is_ok() ^ b.is_ok(),
            "exactly one concurrent writer attach may succeed (got {a:?} / {b:?})"
        );
        assert_eq!(provider.attachment_count().await, 1);
    }

    #[tokio::test]
    async fn create_and_attach_require_capabilities() {
        let no_create =
            FakeProvider::new().with_capabilities(CapabilitySet::of([Capability::Attach]));
        let err = no_create
            .create_volume(&fixture_create_request("fake-nocreate", GIB))
            .await
            .expect_err("create without the create capability");
        assert_eq!(err.code, ApiErrorCode::UnsupportedClassOrPolicy);

        let no_attach =
            FakeProvider::new().with_capabilities(CapabilitySet::of([Capability::Create]));
        let vid = volume_id("fake-noattach");
        no_attach
            .create_volume(&fixture_create_request("fake-noattach", GIB))
            .await
            .expect("create");
        let err = no_attach
            .attach_volume(
                &vid,
                &fixture_attach_request("fake-noattach", "fake-noattach-att", 1),
            )
            .await
            .expect_err("attach without the attach capability");
        assert_eq!(err.code, ApiErrorCode::UnsupportedClassOrPolicy);
    }

    #[tokio::test]
    async fn capacity_limits_are_enforced() {
        let provider = FakeProvider::new().with_capacity_bytes(2 * GIB);
        provider
            .create_volume(&fixture_create_request("fake-cap-a", 2 * GIB))
            .await
            .expect("create fills capacity");

        let err = provider
            .create_volume(&fixture_create_request("fake-cap-b", GIB))
            .await
            .expect_err("create beyond capacity");
        assert_eq!(err.code, ApiErrorCode::NoSafeCapacity);

        let err = provider
            .grow_volume(
                &volume_id("fake-cap-a"),
                &fixture_grow_request("fake-cap-a", 3 * GIB, 1),
            )
            .await
            .expect_err("grow beyond capacity");
        assert_eq!(err.code, ApiErrorCode::NoSafeCapacity);

        // Delete frees capacity again.
        provider
            .delete_volume(
                &volume_id("fake-cap-a"),
                &fixture_delete_request("fake-cap-a", 1),
            )
            .await
            .expect("delete frees capacity");
        provider
            .create_volume(&fixture_create_request("fake-cap-b", GIB))
            .await
            .expect("create after free");
    }

    #[tokio::test]
    async fn unsupported_policies_fail_closed() {
        let provider = FakeProvider::new();

        let mut thin = fixture_create_request("fake-policy-thin", GIB);
        thin.provisioning = Some(Provisioning::Thin);
        assert_eq!(
            provider.create_volume(&thin).await.unwrap_err().code,
            ApiErrorCode::UnsupportedClassOrPolicy
        );

        let mut encrypted = fixture_create_request("fake-policy-enc", GIB);
        encrypted.encryption = Some(volvisor_types::request::EncryptionRequest {
            mode: "provider-managed".to_owned(),
            key_ref: "external-secret-reference".to_owned(),
        });
        assert_eq!(
            provider.create_volume(&encrypted).await.unwrap_err().code,
            ApiErrorCode::UnsupportedClassOrPolicy
        );

        let mut mirrored = fixture_create_request("fake-policy-mirror", GIB);
        mirrored.local_protection = Some(LocalProtectionRequest {
            mode: LocalProtectionModeRequest::Mirror,
            min_healthy_legs: Some(2),
        });
        assert_eq!(
            provider.create_volume(&mirrored).await.unwrap_err().code,
            ApiErrorCode::UnsupportedClassOrPolicy
        );

        let mut legs = fixture_create_request("fake-policy-legs", GIB);
        legs.local_protection = Some(LocalProtectionRequest {
            mode: LocalProtectionModeRequest::None,
            min_healthy_legs: Some(2),
        });
        assert_eq!(
            provider.create_volume(&legs).await.unwrap_err().code,
            ApiErrorCode::UnsupportedClassOrPolicy
        );

        let mut migrating = fixture_create_request("fake-policy-migrate", GIB);
        migrating.migration_policy = Some(volvisor_types::request::MigrationPolicyRequest {
            live: "require_verified".to_owned(),
        });
        assert_eq!(
            provider.create_volume(&migrating).await.unwrap_err().code,
            ApiErrorCode::UnsupportedClassOrPolicy
        );

        let mut nearline = fixture_create_request("fake-policy-nearline", GIB);
        nearline.volume_class = VolumeClass::NearlineReplicated;
        nearline.replication = Some(ReplicationPolicyRequest {
            engine: Some("drbd9".to_owned()),
            mode: ReplicationModeRequest::Async,
            remote_replicas: 1,
            allow_degraded_create: false,
        });
        assert_eq!(
            provider.create_volume(&nearline).await.unwrap_err().code,
            ApiErrorCode::UnsupportedClassOrPolicy
        );

        let mut frontend = fixture_attach_request("fake-policy-vol", "fake-policy-att", 1);
        frontend.requested_frontend = Some("nbd".to_owned());
        provider
            .create_volume(&fixture_create_request("fake-policy-vol", GIB))
            .await
            .expect("create");
        let err = provider
            .attach_volume(&volume_id("fake-policy-vol"), &frontend)
            .await
            .expect_err("unknown frontend");
        assert_eq!(err.code, ApiErrorCode::UnsupportedClassOrPolicy);
    }

    #[tokio::test]
    async fn local_mirror_with_capability_reports_effective_protection() {
        let mut capabilities = CapabilitySet::native_local_p0();
        capabilities.insert(Capability::LocalMirror);
        let provider = FakeProvider::new().with_capabilities(capabilities);
        let mut mirrored = fixture_create_request("fake-mirror-ok", GIB);
        mirrored.local_protection = Some(LocalProtectionRequest {
            mode: LocalProtectionModeRequest::Mirror,
            min_healthy_legs: Some(2),
        });
        let created = provider
            .create_volume(&mirrored)
            .await
            .expect("mirror create with the local_mirror capability");
        assert_eq!(
            created.effective_protection.local,
            LocalProtectionAxis::Mirror
        );
        assert_eq!(
            created.effective_protection.remote,
            RemoteProtectionAxis::None,
            "a local mirror is never reported as a remote replica"
        );
    }

    #[tokio::test]
    async fn placement_constraints_fail_closed() {
        let provider = FakeProvider::new();

        let mut wrong_host = fixture_create_request("fake-place-host", GIB);
        wrong_host.placement = Some(Placement {
            preferred_host_id: Some(volvisor_types::HostId::new("other-host").expect("host id")),
            failure_domain: None,
        });
        assert_eq!(
            provider.create_volume(&wrong_host).await.unwrap_err().code,
            ApiErrorCode::InsufficientFailureDomains
        );

        let mut rack = fixture_create_request("fake-place-rack", GIB);
        rack.placement = Some(Placement {
            preferred_host_id: None,
            failure_domain: Some(FailureDomain::Rack),
        });
        assert_eq!(
            provider.create_volume(&rack).await.unwrap_err().code,
            ApiErrorCode::InsufficientFailureDomains
        );
    }

    #[tokio::test]
    async fn delete_quarantined_volume_reports_foreign_state() {
        let provider = FakeProvider::new();
        let vid = volume_id("fake-quarantine");
        provider
            .create_volume(&fixture_create_request("fake-quarantine", GIB))
            .await
            .expect("create");
        provider
            .force_state(&vid, VolumeLifecycle::Quarantined)
            .await
            .expect("force quarantined");

        let err = provider
            .delete_volume(&vid, &fixture_delete_request("fake-quarantine", 1))
            .await
            .expect_err("delete on quarantined volume");
        assert_eq!(err.code, ApiErrorCode::ForeignDeviceState);

        // The volume persists in Quarantined; it is never silently destroyed.
        let inspected = provider.inspect_volume(&vid).await.expect("inspect");
        assert_eq!(inspected.state, VolumeLifecycle::Quarantined);
    }

    #[tokio::test]
    async fn delete_rejects_cryptographic_erasure() {
        let provider = FakeProvider::new();
        let vid = volume_id("fake-crypto");
        provider
            .create_volume(&fixture_create_request("fake-crypto", GIB))
            .await
            .expect("create");
        let mut req = fixture_delete_request("fake-crypto", 1);
        req.data_erasure_policy = ErasurePolicy::Cryptographic;
        let err = provider
            .delete_volume(&vid, &req)
            .await
            .expect_err("cryptographic erasure");
        assert_eq!(err.code, ApiErrorCode::UnsupportedClassOrPolicy);
        provider
            .inspect_volume(&vid)
            .await
            .expect("volume persists");
    }

    #[tokio::test]
    async fn grow_attached_volume_requires_guest_notification_retry() {
        let provider = FakeProvider::new();
        let vid = volume_id("fake-grow-attached");
        provider
            .create_volume(&fixture_create_request("fake-grow-attached", GIB))
            .await
            .expect("create");
        provider
            .attach_volume(
                &vid,
                &fixture_attach_request("fake-grow-attached", "fake-grow-attached-att", 1),
            )
            .await
            .expect("attach");
        let grown = provider
            .grow_volume(
                &vid,
                &fixture_grow_request("fake-grow-attached", 2 * GIB, 2),
            )
            .await
            .expect("grow while attached");
        assert!(grown.backing_resized);
        assert_eq!(grown.effective_size_bytes, 2 * GIB);
        // No VMM integration exists, so an honest provider reports a pending
        // (retried) notification, never `notified`.
        assert_eq!(
            grown.guest_notification_status,
            GrowGuestNotification::RetryRequired
        );
        let inspected = provider.inspect_volume(&vid).await.expect("inspect");
        assert_eq!(inspected.generation, 3);
        assert_eq!(inspected.provisioned_bytes, 2 * GIB);
    }
}
