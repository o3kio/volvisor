//! Behavior tests for the DRBD9 nearline provider over the simulated
//! [`FakeDrbd`] world (see [`common`]).
//!
//! Each test drives the real provider through the real
//! [`VolumeProvider`] surface; the world's fault-injection matrix and
//! state pins exercise the fail-closed rules the conformance kit
//! cannot express: seeding only over provably-fresh resources, foreign
//! peer data never overwritten, crash-reclaim of half-created LVs,
//! zombie primaries never demoted, busy-device demotion refusal, the
//! honest grow boundary against a smaller peer backing, every
//! reconcile branch, and the `-c` scoping of every `drbdadm` call.
//!
//! Test-kit code: `expect`/`unwrap` are allowed here by convention.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::collections::{BTreeMap, VecDeque};

use common::{Fixture, PEER_NODE, SEED_MINOR, VG, fixture, provider_from, seed_lv, seed_volume};
use volvisor_drbd::provider::resource_name_for;
use volvisor_drbd::report::{DiskState, Role};
use volvisor_drbd::resgen::res_file_path;
use volvisor_drbd::state::DrbdState;
use volvisor_provider::VolumeProvider;
use volvisor_types::domain::{Frontend, RemoteProtectionAxis, VolumeClass};
use volvisor_types::request::{
    AccessModeRequest, AttachVolumeRequest, CreateVolumeRequest, DeleteVolumeRequest,
    DetachVolumeRequest, DrainProof, ErasurePolicy, GrowVolumeRequest, ReplicationModeRequest,
    ReplicationPolicyRequest,
};
use volvisor_types::{
    ApiErrorCode, AttachmentId, AttachmentState, HostId, OperationId, ProjectId, VolumeId,
    VolumeLifecycle,
};

/// One gibibyte (extent-aligned under the fixture's 4-MiB extents).
const GIB: u64 = 1 << 30;

/// A minimal valid nearline create request.
fn nearline_create(volume_id: &str, size_bytes: u64) -> CreateVolumeRequest {
    nearline_create_degraded(volume_id, size_bytes, false)
}

/// A nearline create request with an explicit `allow_degraded_create`.
fn nearline_create_degraded(
    volume_id: &str,
    size_bytes: u64,
    allow_degraded: bool,
) -> CreateVolumeRequest {
    nearline_create_protocol(
        volume_id,
        size_bytes,
        ReplicationModeRequest::Async,
        allow_degraded,
    )
}

/// A nearline create request with an explicit replication protocol.
fn nearline_create_protocol(
    volume_id: &str,
    size_bytes: u64,
    mode: ReplicationModeRequest,
    allow_degraded: bool,
) -> CreateVolumeRequest {
    CreateVolumeRequest {
        api_version: "volvisor.volume.v2".to_owned(),
        operation_id: OperationId::new(format!("op-create-{volume_id}")).expect("valid id"),
        project_id: ProjectId::new("behavior-project").expect("valid id"),
        volume_id: VolumeId::new(volume_id).expect("valid id"),
        volume_class: VolumeClass::NearlineReplicated,
        size_bytes,
        logical_block_size: None,
        provisioning: None,
        placement: None,
        local_protection: None,
        replication: Some(ReplicationPolicyRequest {
            engine: Some("drbd9".to_owned()),
            mode,
            remote_replicas: 1,
            allow_degraded_create: allow_degraded,
        }),
        migration_policy: None,
        encryption: None,
    }
}

/// A single-writer attach request.
fn attach_req(volume_id: &str, expected_generation: u64) -> AttachVolumeRequest {
    AttachVolumeRequest {
        api_version: "volvisor.volume.v2".to_owned(),
        operation_id: OperationId::new(format!("op-attach-{volume_id}")).expect("valid id"),
        vm_id: "behavior-vm".to_owned(),
        host_id: HostId::new("behavior-host").expect("valid id"),
        attachment_id: AttachmentId::new(format!("att-{volume_id}")).expect("valid id"),
        expected_volume_generation: expected_generation,
        access_mode: AccessModeRequest::SingleWriter,
        requested_frontend: None,
        vmm_disk_id: None,
    }
}

/// A drained detach request.
fn detach_req(attachment_id: &str, expected_generation: u64) -> DetachVolumeRequest {
    DetachVolumeRequest {
        api_version: "volvisor.volume.v2".to_owned(),
        operation_id: OperationId::new(format!("op-detach-{attachment_id}")).expect("valid id"),
        expected_attachment_generation: expected_generation,
        vm_stopped_or_io_drained_proof: DrainProof::VmStopped,
    }
}

/// A grow request.
fn grow_req(volume_id: &str, new_size: u64, expected_generation: u64) -> GrowVolumeRequest {
    GrowVolumeRequest {
        api_version: "volvisor.volume.v2".to_owned(),
        operation_id: OperationId::new(format!("op-grow-{volume_id}")).expect("valid id"),
        new_size_bytes: new_size,
        expected_generation,
    }
}

/// A retain delete request.
fn delete_req(volume_id: &str, expected_generation: u64) -> DeleteVolumeRequest {
    DeleteVolumeRequest {
        api_version: "volvisor.volume.v2".to_owned(),
        operation_id: OperationId::new(format!("op-delete-{volume_id}")).expect("valid id"),
        expected_generation,
        data_erasure_policy: ErasurePolicy::Retain,
    }
}

/// The stored volume entry from the state file.
fn stored(fixture: &Fixture, volume_id: &str) -> volvisor_drbd::state::StoredVolume {
    let state = DrbdState::load(&fixture.state_path).expect("load state");
    state
        .volume(&VolumeId::new(volume_id).expect("valid id"))
        .cloned()
        .expect("volume is in state")
}

/// The resource name a volume id maps to.
fn resource_of(volume_id: &str) -> String {
    resource_name_for(&VolumeId::new(volume_id).expect("valid id"))
}

// ---------------------------------------------------------------------------
// Create, seeding and policy negotiation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_seeds_the_replica_and_records_verified_facts() {
    let fixture = fixture();
    let response = fixture
        .provider
        .create_volume(&nearline_create("seed-me", GIB))
        .await
        .expect("create");
    assert_eq!(response.state, VolumeLifecycle::Ready);
    assert_eq!(response.generation, 1);
    assert_eq!(response.provisioned_bytes, GIB);
    // The seeding promote/demote ran: both ends UpToDate, Secondary.
    let resource = resource_of("seed-me");
    let world = fixture.world.lock().expect("world");
    let state = world.resources.get(&resource).expect("resource is up");
    assert_eq!(state.role, Role::Secondary);
    assert_eq!(state.local_disk, DiskState::UpToDate);
    assert_eq!(state.peer_disk, DiskState::UpToDate);
    assert!(world.peer_overwritten, "seeding ran primary --force");
    assert_eq!(state.device_size, GIB);
    let lv = world.lvs.get(&format!("{VG}/{resource}")).expect("LV");
    assert_eq!(lv.size, GIB);
    assert!(lv.tags.contains(&"volvisor.owner=seed-me".to_owned()));
    drop(world);
    // The state entry records the seeding and the allocated minor.
    let stored = stored(&fixture, "seed-me");
    assert!(stored.runtime.seeded);
    assert!((10..=20).contains(&stored.entry.minor));
    assert_eq!(stored.entry.size_bytes, GIB);
}

#[tokio::test]
async fn create_writes_the_res_file_owner_only_with_the_verified_shape() {
    let fixture = fixture();
    fixture
        .provider
        .create_volume(&nearline_create("res-file", GIB))
        .await
        .expect("create");
    let path = res_file_path(&fixture.base.join("drbd.d"), &resource_of("res-file"));
    let content = std::fs::read_to_string(&path).expect("res file exists");
    assert!(content.starts_with("# managed by volvisor"));
    assert!(content.contains("meta-disk internal;"));
    assert!(content.contains("protocol A;"));
    assert!(content.contains("cram-hmac-alg sha1;"));
    assert!(content.contains(&format!("on {PEER_NODE} {{")));
    // The secret rides the 0600 file only.
    assert!(content.contains("shared-secret \"fixture-peer-secret\";"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the res file is owner-only");
    }
}

#[tokio::test]
async fn create_conflicts_on_a_different_payload_for_the_same_identity() {
    let fixture = fixture();
    fixture
        .provider
        .create_volume(&nearline_create("conflict", GIB))
        .await
        .expect("create");
    let error = fixture
        .provider
        .create_volume(&nearline_create("conflict", 2 * GIB))
        .await
        .expect_err("different payload");
    assert_eq!(error.code, ApiErrorCode::IdempotencyConflict);
    // Exactly one LV exists for the identity.
    let count = fixture
        .world
        .lock()
        .expect("world")
        .lvs
        .keys()
        .filter(|key| key.contains("vol-"))
        .count();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn create_refuses_policies_outside_the_drbd9_profile() {
    let fixture = fixture();
    let refusal = |req: CreateVolumeRequest| {
        let provider = fixture.provider.clone();
        async move {
            provider
                .create_volume(&req)
                .await
                .expect_err("refused")
                .code
        }
    };
    // Wrong engine.
    let mut req = nearline_create("engine", GIB);
    req.replication = Some(ReplicationPolicyRequest {
        engine: Some("ceph".to_owned()),
        mode: ReplicationModeRequest::Async,
        remote_replicas: 1,
        allow_degraded_create: false,
    });
    assert_eq!(refusal(req).await, ApiErrorCode::UnsupportedClassOrPolicy);
    // Two remote replicas are not the P3 deployment shape.
    let mut req = nearline_create("replicas", GIB);
    req.replication = Some(ReplicationPolicyRequest {
        engine: Some("drbd9".to_owned()),
        mode: ReplicationModeRequest::Async,
        remote_replicas: 2,
        allow_degraded_create: false,
    });
    assert_eq!(refusal(req).await, ApiErrorCode::UnsupportedClassOrPolicy);
    // Missing replication policy entirely.
    let mut req = nearline_create("no-policy", GIB);
    req.replication = None;
    assert_eq!(refusal(req).await, ApiErrorCode::UnsupportedClassOrPolicy);
    // A foreign volume class.
    let mut req = nearline_create("class", GIB);
    req.volume_class = VolumeClass::NativeLocal;
    assert_eq!(refusal(req).await, ApiErrorCode::UnsupportedClassOrPolicy);
    // Nothing was created by any refusal.
    assert!(fixture.world.lock().expect("world").lvs.is_empty());
}

#[tokio::test]
async fn create_refuses_placements_outside_the_local_host() {
    let fixture = fixture();
    // A rack failure domain is beyond the two-host P3 shape.
    let mut req = nearline_create("rack", GIB);
    req.placement = Some(volvisor_types::request::Placement {
        preferred_host_id: None,
        failure_domain: Some(volvisor_types::domain::FailureDomain::Rack),
    });
    let error = fixture
        .provider
        .create_volume(&req)
        .await
        .expect_err("rack placement");
    assert_eq!(error.code, ApiErrorCode::InsufficientFailureDomains);
    // A remote preferred host is not served by a local provider.
    let mut req = nearline_create("remote-host", GIB);
    req.placement = Some(volvisor_types::request::Placement {
        preferred_host_id: Some(HostId::new("other-host").expect("valid id")),
        failure_domain: None,
    });
    let error = fixture
        .provider
        .create_volume(&req)
        .await
        .expect_err("remote placement");
    assert_eq!(error.code, ApiErrorCode::InsufficientFailureDomains);
}

#[tokio::test]
async fn create_refuses_when_the_vg_lacks_safe_capacity() {
    let fixture = fixture();
    let error = fixture
        .provider
        .create_volume(&nearline_create("too-big", 200 * GIB))
        .await
        .expect_err("capacity");
    assert_eq!(error.code, ApiErrorCode::NoSafeCapacity);
    assert!(fixture.world.lock().expect("world").lvs.is_empty());
}

#[tokio::test]
async fn create_lv_sizes_are_extent_rounded() {
    let fixture = fixture();
    // 1 GiB + 512 B rounds up to one more 4-MiB extent.
    let requested = GIB + 512;
    let response = fixture
        .provider
        .create_volume(&nearline_create("rounded", requested))
        .await
        .expect("create");
    let expected = GIB + (4 << 20);
    assert_eq!(response.provisioned_bytes, expected);
    assert_eq!(response.allocated_bytes, expected);
    let resource = resource_of("rounded");
    let world = fixture.world.lock().expect("world");
    assert_eq!(
        world.lvs.get(&format!("{VG}/{resource}")).expect("LV").size,
        expected
    );
}

#[tokio::test]
async fn foreign_peer_data_is_never_overwritten() {
    let fixture = fixture();
    // The peer reports data for a freshly connected resource.
    fixture.world.lock().expect("world").new_peer_disk = DiskState::UpToDate;
    let error = fixture
        .provider
        .create_volume(&nearline_create("foreign-peer", GIB))
        .await
        .expect_err("foreign peer data");
    assert_eq!(error.code, ApiErrorCode::ForeignDeviceState);
    let world = fixture.world.lock().expect("world");
    // A resource this call created is torn down completely.
    assert!(world.resources.is_empty());
    assert!(world.lvs.is_empty());
    assert!(!world.peer_overwritten, "primary --force never ran");
    drop(world);
    assert!(
        !stored_maybe(&fixture, "foreign-peer"),
        "no state entry survives"
    );
}

#[tokio::test]
async fn a_primary_peer_is_foreign_data() {
    let fixture = fixture();
    fixture.world.lock().expect("world").new_peer_role = Role::Primary;
    let error = fixture
        .provider
        .create_volume(&nearline_create("primary-peer", GIB))
        .await
        .expect_err("primary peer");
    assert_eq!(error.code, ApiErrorCode::ForeignDeviceState);
    assert!(!fixture.world.lock().expect("world").peer_overwritten);
}

#[tokio::test]
async fn absent_peer_without_allow_degraded_is_refused_and_torn_down() {
    let fixture = fixture();
    fixture.world.lock().expect("world").peer_online = false;
    let error = fixture
        .provider
        .create_volume(&nearline_create("no-peer", GIB))
        .await
        .expect_err("absent peer");
    assert_eq!(error.code, ApiErrorCode::UnsupportedClassOrPolicy);
    let world = fixture.world.lock().expect("world");
    assert!(world.lvs.is_empty(), "the fresh LV is removed");
    assert!(world.resources.is_empty());
    drop(world);
    let res = res_file_path(&fixture.base.join("drbd.d"), &resource_of("no-peer"));
    assert!(!res.exists(), "the res file is removed");
}

#[tokio::test]
async fn degraded_create_leaves_the_volume_unseeded_until_reconcile_sees_a_fresh_peer() {
    let fixture = fixture();
    fixture.world.lock().expect("world").peer_online = false;
    let response = fixture
        .provider
        .create_volume(&nearline_create_degraded("degraded", GIB, true))
        .await
        .expect("degraded create");
    assert_eq!(response.state, VolumeLifecycle::Ready);
    assert!(!stored(&fixture, "degraded").runtime.seeded);

    // Attach is refused: the replica is not established.
    let error = fixture
        .provider
        .attach_volume(
            &VolumeId::new("degraded").expect("id"),
            &attach_req("degraded", 1),
        )
        .await
        .expect_err("unseeded attach");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
    let resource = resource_of("degraded");
    assert_eq!(
        fixture
            .world
            .lock()
            .expect("world")
            .resources
            .get(&resource)
            .expect("resource up")
            .role,
        Role::Secondary,
        "no promotion was attempted"
    );

    // The peer appears: reconcile seeds the resource, then attach works.
    fixture.world.lock().expect("world").peer_online = true;
    let report = fixture.provider.reconcile().expect("reconcile");
    assert_eq!(report.seeded_volumes.len(), 1);
    assert!(stored(&fixture, "degraded").runtime.seeded);
    let attached = fixture
        .provider
        .attach_volume(
            &VolumeId::new("degraded").expect("id"),
            &attach_req("degraded", 1),
        )
        .await
        .expect("attach after seeding");
    assert_eq!(attached.state, AttachmentState::Prepared);
}

/// Whether a state entry exists for `volume_id`.
fn stored_maybe(fixture: &Fixture, volume_id: &str) -> bool {
    let state = DrbdState::load(&fixture.state_path).expect("load state");
    state
        .volume(&VolumeId::new(volume_id).expect("valid id"))
        .is_some()
}

// ---------------------------------------------------------------------------
// Crash-window reclaim of a half-created volume
// ---------------------------------------------------------------------------

/// Seed the world half-way through a crashed create: owned LV,
/// metadata, res file (minor/port `minor`/`port`) and — when `up` — a
/// running resource. `lv_has_data` pins whether the crashed
/// predecessor got as far as writing data (a completed seed leaves a
/// non-zero data area; a crash before seeding leaves an all-zero LV —
/// real drbdmeta's create-md decision rests on exactly this).
fn seed_crashed_create(
    fixture: &Fixture,
    volume_id: &str,
    size_bytes: u64,
    minor: u32,
    port: u16,
    up: bool,
    lv_has_data: bool,
) {
    let resource = resource_of(volume_id);
    seed_lv(
        &fixture.world,
        VG,
        &resource,
        size_bytes,
        Some(volume_id),
        lv_has_data,
    );
    common::write_seed_res_file(&fixture.base, &resource, minor, port);
    let mut world = fixture.world.lock().expect("world");
    world.metadata.insert(resource.clone());
    if up {
        world.resources.insert(
            resource,
            common::FakeResource {
                minor,
                role: Role::Secondary,
                local_disk: DiskState::Inconsistent,
                peer_disk: DiskState::Inconsistent,
                peer_role: Role::Secondary,
                resyncing: false,
                device_size: size_bytes,
                peer_node: PEER_NODE.to_owned(),
                blocks: BTreeMap::new(),
                apply_queue: VecDeque::new(),
                write_seq: 0,
            },
        );
    }
}

#[tokio::test]
async fn a_crashed_create_with_the_resource_up_is_reclaimed_and_adopted() {
    let fixture = fixture();
    seed_crashed_create(&fixture, "crash-up", GIB, 13, 7903, true, false);
    let response = fixture
        .provider
        .create_volume(&nearline_create("crash-up", GIB))
        .await
        .expect("reclaim");
    assert_eq!(response.state, VolumeLifecycle::Ready);
    // The crashed predecessor's resource is verifiably up: its minor
    // and port are ADOPTED from the surviving resource file.
    let stored = stored(&fixture, "crash-up");
    assert_eq!(stored.entry.minor, 13);
    assert_eq!(stored.entry.port, 7903);
    assert!(stored.runtime.seeded, "the fresh resource was seeded");
}

#[tokio::test]
async fn a_crashed_create_with_the_resource_down_keeps_a_fresh_allocation() {
    let fixture = fixture();
    seed_crashed_create(&fixture, "crash-down", GIB, 13, 7903, false, false);
    let response = fixture
        .provider
        .create_volume(&nearline_create("crash-down", GIB))
        .await
        .expect("reclaim");
    assert_eq!(response.state, VolumeLifecycle::Ready);
    // The resource is down: the stale file is overwritten and the
    // fresh (first) allocation survives.
    let stored = stored(&fixture, "crash-down");
    assert_eq!(stored.entry.minor, 10);
    assert_eq!(stored.entry.port, 7900);
    assert!(stored.runtime.seeded);
}

/// Real drbdmeta semantics: over an ALL-ZERO LV that carries a crashed
/// predecessor's metadata, create-md RE-INITIALIZES (succeeds and
/// rewrites) instead of refusing — the create converges through the
/// plain create-md + up path, and the world records that create-md
/// actually ran over the pre-existing metadata.
#[tokio::test]
async fn create_md_reinitializes_metadata_on_an_all_zero_predecessor_lv() {
    let fixture = fixture();
    let resource = resource_of("reinit");
    seed_crashed_create(&fixture, "reinit", GIB, 13, 7903, false, false);
    assert!(
        fixture
            .world
            .lock()
            .expect("world")
            .metadata
            .contains(&resource),
        "the predecessor left metadata behind"
    );
    let response = fixture
        .provider
        .create_volume(&nearline_create("reinit", GIB))
        .await
        .expect("create over predecessor metadata");
    assert_eq!(response.state, VolumeLifecycle::Ready);
    let world = fixture.world.lock().expect("world");
    assert!(
        world.create_md_ran.contains(&resource),
        "create-md SUCCEEDED over existing metadata (re-init, not refusal)"
    );
    assert!(world.metadata.contains(&resource));
}

/// Real drbdmeta semantics: create-md refuses only when the backing
/// LV carries NON-ZERO data ("Operation refused"). An owned orphan LV
/// with data but no DRBD metadata can neither be re-initialized nor
/// brought up: the refusal surfaces typed and the LV survives for the
/// operator.
#[tokio::test]
async fn an_owned_lv_with_data_and_no_metadata_fails_create_md_and_survives() {
    let fixture = fixture();
    let resource = resource_of("orphan-data");
    seed_lv(
        &fixture.world,
        VG,
        &resource,
        GIB,
        Some("orphan-data"),
        true,
    );
    let error = fixture
        .provider
        .create_volume(&nearline_create("orphan-data", GIB))
        .await
        .expect_err("create-md refuses non-zero data");
    assert_eq!(error.code, ApiErrorCode::Internal);
    assert!(
        error.detail.contains("Operation refused"),
        "the real drbdmeta refusal text surfaces: {error:?}"
    );
    let world = fixture.world.lock().expect("world");
    // The reclaimed LV is never destroyed (fresh_lv is false), and no
    // create-md ran over it.
    assert!(world.lvs.contains_key(&format!("{VG}/{resource}")));
    assert!(!world.create_md_ran.contains(&resource));
    assert!(!world.metadata.contains(&resource));
}

#[tokio::test]
async fn a_crashed_local_uptodate_disk_is_a_state_loss_replay() {
    let fixture = fixture();
    // The predecessor seeded (data on the LV) but died before the
    // state save: the local disk already holds data on OUR owned
    // resource. Real drbdmeta refuses create-md over the non-zero LV,
    // `up` adopts the predecessor's valid metadata, and the status
    // answers with the data-holding disk — the replay adoption the
    // provider performs.
    seed_crashed_create(&fixture, "replay", GIB, 13, 7903, true, true);
    let resource = resource_of("replay");
    {
        let mut world = fixture.world.lock().expect("world");
        world
            .resources
            .get_mut(&resource)
            .expect("resource")
            .local_disk = DiskState::UpToDate;
    }
    let response = fixture
        .provider
        .create_volume(&nearline_create("replay", GIB))
        .await
        .expect("state-loss replay");
    assert_eq!(response.state, VolumeLifecycle::Ready);
    // The data-holding disk proves a predecessor's seed ran: adopted
    // as seeded, never re-forced.
    assert!(stored(&fixture, "replay").runtime.seeded);
    assert!(!fixture.world.lock().expect("world").peer_overwritten);
    // create-md was REFUSED (non-zero data) and the refusal was not
    // fatal: `up` adopted the predecessor's metadata.
    let world = fixture.world.lock().expect("world");
    assert!(!world.create_md_ran.contains(&resource));
    assert!(world.metadata.contains(&resource));
}

#[tokio::test]
async fn a_crashed_create_refuses_a_foreign_lv() {
    let fixture = fixture();
    let resource = resource_of("foreign-lv");
    seed_lv(&fixture.world, VG, &resource, GIB, None, false);
    let error = fixture
        .provider
        .create_volume(&nearline_create("foreign-lv", GIB))
        .await
        .expect_err("foreign LV");
    assert_eq!(error.code, ApiErrorCode::ForeignDeviceState);
    // Foreign state is never destroyed.
    assert!(
        fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&format!("{VG}/{resource}"))
    );
}

#[tokio::test]
async fn a_crashed_create_refuses_a_smaller_owned_lv() {
    let fixture = fixture();
    let resource = resource_of("small-orphan");
    seed_lv(
        &fixture.world,
        VG,
        &resource,
        GIB / 2,
        Some("small-orphan"),
        false,
    );
    let error = fixture
        .provider
        .create_volume(&nearline_create("small-orphan", GIB))
        .await
        .expect_err("smaller orphan");
    assert_eq!(error.code, ApiErrorCode::Internal);
    // The owned LV survives for the operator.
    assert!(
        fixture
            .world
            .lock()
            .expect("world")
            .lvs
            .contains_key(&format!("{VG}/{resource}"))
    );
}

#[tokio::test]
async fn an_adopted_minor_claimed_by_another_volume_is_refused() {
    // vol-a owns minor SEED_MINOR in state; vol-b's crashed predecessor
    // left a resource file claiming the SAME minor, with the resource
    // verifiably up. The provider loads vol-a's state at startup.
    let fixture = common::fixture_after(|base, world| {
        seed_volume(base, world, "vol-a", GIB);
    });
    seed_crashed_create(&fixture, "vol-b", GIB, SEED_MINOR, 7905, true, false);
    let error = fixture
        .provider
        .create_volume(&nearline_create("vol-b", GIB))
        .await
        .expect_err("claimed minor");
    assert_eq!(error.code, ApiErrorCode::Internal);
    assert!(error.detail.contains("claimed by volume"));
}

#[tokio::test]
async fn a_failed_bring_up_tears_down_the_fresh_lv() {
    let fixture = fixture();
    fixture.world.lock().expect("world").fail_up = true;
    let error = fixture
        .provider
        .create_volume(&nearline_create("no-up", GIB))
        .await
        .expect_err("up fails");
    assert_eq!(error.code, ApiErrorCode::Internal);
    let world = fixture.world.lock().expect("world");
    assert!(world.lvs.is_empty(), "the fresh LV is removed");
    assert!(world.resources.is_empty());
    drop(world);
    let res = res_file_path(&fixture.base.join("drbd.d"), &resource_of("no-up"));
    assert!(!res.exists());
}

#[tokio::test]
async fn an_unexpected_lvcreate_stderr_is_internal() {
    let fixture = fixture();
    fixture.world.lock().expect("world").fail_lvcreate = true;
    let error = fixture
        .provider
        .create_volume(&nearline_create("bad-create", GIB))
        .await
        .expect_err("lvcreate fails");
    assert_eq!(error.code, ApiErrorCode::Internal);
}

// ---------------------------------------------------------------------------
// Attach / detach (single-primary single-writer)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn attach_promotes_and_detaches_the_single_writer() {
    let fixture = fixture();
    let volume_id = VolumeId::new("lifecycle").expect("id");
    fixture
        .provider
        .create_volume(&nearline_create("lifecycle", GIB))
        .await
        .expect("create");
    let attached = fixture
        .provider
        .attach_volume(&volume_id, &attach_req("lifecycle", 1))
        .await
        .expect("attach");
    assert_eq!(attached.state, AttachmentState::Prepared);
    assert_eq!(attached.attachment_generation, 1);
    assert_eq!(attached.volume_generation, 2);
    assert!(matches!(
        attached.frontend,
        Frontend::VirtioBlk { ref host_device_path } if host_device_path.starts_with("/dev/drbd")
    ));
    assert_eq!(
        fixture
            .world
            .lock()
            .expect("world")
            .resources
            .get(&resource_of("lifecycle"))
            .expect("resource")
            .role,
        Role::Primary
    );
    let inspected = fixture
        .provider
        .inspect_volume(&volume_id)
        .await
        .expect("inspect");
    assert_eq!(inspected.state, VolumeLifecycle::Attached);
    assert_eq!(
        inspected.current_writer.as_ref(),
        Some(&attached.attachment_id)
    );

    let detached = fixture
        .provider
        .detach_volume(
            &volume_id,
            &attached.attachment_id,
            &detach_req("lifecycle", attached.attachment_generation),
        )
        .await
        .expect("detach");
    assert_eq!(detached.state, VolumeLifecycle::Ready);
    assert_eq!(detached.current_writer, None);
    assert_eq!(detached.generation, attached.volume_generation + 1);
    assert_eq!(
        fixture
            .world
            .lock()
            .expect("world")
            .resources
            .get(&resource_of("lifecycle"))
            .expect("resource")
            .role,
        Role::Secondary
    );
}

#[tokio::test]
async fn attach_replay_is_idempotent_and_never_fabricates_a_second_writer() {
    let fixture = fixture();
    let volume_id = VolumeId::new("replay-att").expect("id");
    fixture
        .provider
        .create_volume(&nearline_create("replay-att", GIB))
        .await
        .expect("create");
    let first = fixture
        .provider
        .attach_volume(&volume_id, &attach_req("replay-att", 1))
        .await
        .expect("attach");
    // The identical retry (e.g. after a caller crash) replays the
    // recorded response.
    let second = fixture
        .provider
        .attach_volume(&volume_id, &attach_req("replay-att", 999))
        .await
        .expect("attach replay");
    assert_eq!(second, first);
    let inspected = fixture
        .provider
        .inspect_volume(&volume_id)
        .await
        .expect("inspect");
    assert_eq!(inspected.attachment_ids.len(), 1);
}

#[tokio::test]
async fn read_only_attach_is_refused_before_any_mutation() {
    let fixture = fixture();
    let volume_id = VolumeId::new("ro").expect("id");
    fixture
        .provider
        .create_volume(&nearline_create("ro", GIB))
        .await
        .expect("create");
    let mut req = attach_req("ro", 1);
    req.access_mode = AccessModeRequest::ReadOnly;
    let error = fixture
        .provider
        .attach_volume(&volume_id, &req)
        .await
        .expect_err("read-only attach");
    assert_eq!(error.code, ApiErrorCode::UnsupportedClassOrPolicy);
    // No promotion, no attachment.
    assert_eq!(
        fixture
            .world
            .lock()
            .expect("world")
            .resources
            .get(&resource_of("ro"))
            .expect("resource")
            .role,
        Role::Secondary
    );
    assert!(stored(&fixture, "ro").runtime.attachment.is_none());
}

#[tokio::test]
async fn attach_refuses_a_zombie_primary_and_never_demotes_it() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "zombie", GIB);
    let provider = provider_from(&fixture.state_path, &fixture.world);
    // Someone promoted the resource out of band.
    fixture
        .world
        .lock()
        .expect("world")
        .resources
        .get_mut(&resource_of("zombie"))
        .expect("resource")
        .role = Role::Primary;
    let volume_id = VolumeId::new("zombie").expect("id");
    let error = provider
        .attach_volume(&volume_id, &attach_req("zombie", 1))
        .await
        .expect_err("zombie primary");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
    assert_eq!(
        fixture
            .world
            .lock()
            .expect("world")
            .resources
            .get(&resource_of("zombie"))
            .expect("resource")
            .role,
        Role::Primary,
        "the zombie is never demoted (rule 17)"
    );
}

#[tokio::test]
async fn attach_refuses_a_downed_resource() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "down-att", GIB);
    fixture
        .world
        .lock()
        .expect("world")
        .resources
        .remove(&resource_of("down-att"));
    let provider = provider_from(&fixture.state_path, &fixture.world);
    let error = provider
        .attach_volume(
            &VolumeId::new("down-att").expect("id"),
            &attach_req("down-att", 1),
        )
        .await
        .expect_err("down resource");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
}

#[tokio::test]
async fn detach_refuses_while_the_device_is_busy() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "busy", GIB);
    let provider = provider_from(&fixture.state_path, &fixture.world);
    let volume_id = VolumeId::new("busy").expect("id");
    let attached = provider
        .attach_volume(&volume_id, &attach_req("busy", 1))
        .await
        .expect("attach");
    // The guest still holds the device open.
    fixture
        .world
        .lock()
        .expect("world")
        .open_devices
        .insert(SEED_MINOR);
    let error = provider
        .detach_volume(
            &volume_id,
            &attached.attachment_id,
            &detach_req("busy", attached.attachment_generation),
        )
        .await
        .expect_err("busy device");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
    // The writer stays attached and the resource stays Primary.
    assert_eq!(
        fixture
            .world
            .lock()
            .expect("world")
            .resources
            .get(&resource_of("busy"))
            .expect("resource")
            .role,
        Role::Primary
    );
    assert!(stored(&fixture, "busy").runtime.attachment.is_some());
}

#[tokio::test]
async fn detach_with_an_unexpected_stderr_is_internal() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "weird-detach", GIB);
    let provider = provider_from(&fixture.state_path, &fixture.world);
    let volume_id = VolumeId::new("weird-detach").expect("id");
    let attached = provider
        .attach_volume(&volume_id, &attach_req("weird-detach", 1))
        .await
        .expect("attach");
    fixture.world.lock().expect("world").fail_secondary = true;
    let error = provider
        .detach_volume(
            &volume_id,
            &attached.attachment_id,
            &detach_req("weird-detach", attached.attachment_generation),
        )
        .await
        .expect_err("unexpected stderr");
    assert_eq!(error.code, ApiErrorCode::Internal);
}

#[tokio::test]
async fn an_already_secondary_recorded_attachment_points_to_reconcile() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "half-detach", GIB);
    let provider = provider_from(&fixture.state_path, &fixture.world);
    let volume_id = VolumeId::new("half-detach").expect("id");
    let attached = provider
        .attach_volume(&volume_id, &attach_req("half-detach", 1))
        .await
        .expect("attach");
    // The demotion succeeded but the state save did not.
    fixture
        .world
        .lock()
        .expect("world")
        .resources
        .get_mut(&resource_of("half-detach"))
        .expect("resource")
        .role = Role::Secondary;
    let error = provider
        .detach_volume(
            &volume_id,
            &attached.attachment_id,
            &detach_req("half-detach", attached.attachment_generation),
        )
        .await
        .expect_err("interrupted detach");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
    // Reconcile owns the completion: the record is cleared and the
    // volume returns to Ready.
    let report = provider.reconcile().expect("reconcile");
    assert_eq!(report.cleared_attachments.len(), 1);
    assert_eq!(
        report.cleared_attachments[0].reason,
        volvisor_drbd::ClearedAttachmentReason::InterruptedDetach
    );
    let inspected = provider.inspect_volume(&volume_id).await.expect("inspect");
    assert_eq!(inspected.state, VolumeLifecycle::Ready);
    assert_eq!(inspected.current_writer, None);
}

// ---------------------------------------------------------------------------
// Grow
// ---------------------------------------------------------------------------

#[tokio::test]
async fn grow_extends_the_backing_and_the_device() {
    let fixture = fixture();
    let volume_id = VolumeId::new("grow").expect("id");
    fixture
        .provider
        .create_volume(&nearline_create("grow", GIB))
        .await
        .expect("create");
    let grown = fixture
        .provider
        .grow_volume(&volume_id, &grow_req("grow", 2 * GIB, 1))
        .await
        .expect("grow");
    assert!(grown.backing_resized);
    assert_eq!(grown.effective_size_bytes, 2 * GIB);
    let resource = resource_of("grow");
    let world = fixture.world.lock().expect("world");
    assert_eq!(
        world.lvs.get(&format!("{VG}/{resource}")).expect("LV").size,
        2 * GIB
    );
    assert_eq!(
        world
            .resources
            .get(&resource)
            .expect("resource")
            .device_size,
        2 * GIB
    );
    drop(world);
    let stored = stored(&fixture, "grow");
    assert_eq!(stored.entry.size_bytes, 2 * GIB);
    assert_eq!(stored.entry.generation, 2);
}

#[tokio::test]
async fn grow_persists_the_honest_boundary_against_a_smaller_peer_backing() {
    let fixture = fixture();
    let volume_id = VolumeId::new("boundary").expect("id");
    fixture
        .provider
        .create_volume(&nearline_create("boundary", GIB))
        .await
        .expect("create");
    // The operator did not grow the peer's backing.
    fixture.world.lock().expect("world").peer_lv_size = Some(GIB);
    let error = fixture
        .provider
        .grow_volume(&volume_id, &grow_req("boundary", 2 * GIB, 1))
        .await
        .expect_err("honest boundary");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
    // The local LV grew, the device stayed at the smaller peer size,
    // and the OBSERVED size plus the generation bump were persisted
    // before the failure.
    let resource = resource_of("boundary");
    let world = fixture.world.lock().expect("world");
    assert_eq!(
        world.lvs.get(&format!("{VG}/{resource}")).expect("LV").size,
        2 * GIB
    );
    assert_eq!(
        world
            .resources
            .get(&resource)
            .expect("resource")
            .device_size,
        GIB
    );
    drop(world);
    let stored = stored(&fixture, "boundary");
    assert_eq!(stored.entry.size_bytes, GIB, "the observed size");
    assert_eq!(stored.entry.generation, 2, "the generation bump");
}

#[tokio::test]
async fn grow_short_circuits_when_the_device_already_meets_the_request() {
    let fixture = fixture();
    let volume_id = VolumeId::new("pre-grown").expect("id");
    fixture
        .provider
        .create_volume(&nearline_create("pre-grown", GIB))
        .await
        .expect("create");
    // An unrecorded grow met the target already (a crash window).
    fixture
        .world
        .lock()
        .expect("world")
        .resources
        .get_mut(&resource_of("pre-grown"))
        .expect("resource")
        .device_size = 2 * GIB;
    let grown = fixture
        .provider
        .grow_volume(&volume_id, &grow_req("pre-grown", 2 * GIB, 1))
        .await
        .expect("short-circuit");
    assert!(!grown.backing_resized, "only the record caught up");
    assert_eq!(grown.effective_size_bytes, 2 * GIB);
    assert!(
        fixture
            .runner
            .invocations()
            .iter()
            .all(|invocation| invocation.program != "lvextend")
    );
}

#[tokio::test]
async fn grow_refuses_without_capacity() {
    let fixture = fixture();
    let volume_id = VolumeId::new("no-room").expect("id");
    fixture
        .provider
        .create_volume(&nearline_create("no-room", GIB))
        .await
        .expect("create");
    // Shrink the reported free space to below the growth delta plus
    // headroom.
    fixture.world.lock().expect("world").vg_free = GIB + GIB / 2;
    let error = fixture
        .provider
        .grow_volume(&volume_id, &grow_req("no-room", 2 * GIB, 1))
        .await
        .expect_err("capacity");
    assert_eq!(error.code, ApiErrorCode::NoSafeCapacity);
}

// ---------------------------------------------------------------------------
// Reconciliation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reconcile_marks_a_vanished_backing_failed_and_clears_the_record() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "vanished", GIB);
    let provider = provider_from(&fixture.state_path, &fixture.world);
    // Attach, then destroy the backing LV out of band.
    let volume_id = VolumeId::new("vanished").expect("id");
    let attached = provider
        .attach_volume(&volume_id, &attach_req("vanished", 1))
        .await
        .expect("attach");
    fixture
        .world
        .lock()
        .expect("world")
        .lvs
        .remove(&format!("{VG}/{}", resource_of("vanished")));
    let report = provider.reconcile().expect("reconcile");
    assert_eq!(report.missing_volumes, vec![volume_id.clone()]);
    assert_eq!(report.cleared_attachments.len(), 1);
    assert_eq!(
        report.cleared_attachments[0].reason,
        volvisor_drbd::ClearedAttachmentReason::VanishedBacking
    );
    let stored = stored(&fixture, "vanished");
    assert_eq!(stored.runtime.state, VolumeLifecycle::Failed);
    assert!(stored.runtime.attachment.is_none());
    let _ = attached;
}

#[tokio::test]
async fn reconcile_marks_an_ownership_mismatch_failed() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "mismatch", GIB);
    // The LV is retagged to another owner out of band.
    let resource = resource_of("mismatch");
    {
        let mut world = fixture.world.lock().expect("world");
        let lv = world.lvs.get_mut(&format!("{VG}/{resource}")).expect("LV");
        lv.tags = vec!["volvisor.owner=someone-else".to_owned()];
    }
    let provider = provider_from(&fixture.state_path, &fixture.world);
    let report = provider.reconcile().expect("reconcile");
    assert_eq!(
        report.mismatched_volumes,
        vec![VolumeId::new("mismatch").expect("id")]
    );
    assert_eq!(
        stored(&fixture, "mismatch").runtime.state,
        VolumeLifecycle::Failed
    );
}

#[tokio::test]
async fn reconcile_marks_a_missing_res_file_failed_but_keeps_the_record() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "no-res", GIB);
    let provider = provider_from(&fixture.state_path, &fixture.world);
    let volume_id = VolumeId::new("no-res").expect("id");
    let attached = provider
        .attach_volume(&volume_id, &attach_req("no-res", 1))
        .await
        .expect("attach");
    std::fs::remove_file(res_file_path(
        &fixture.base.join("drbd.d"),
        &resource_of("no-res"),
    ))
    .expect("remove res file");
    let report = provider.reconcile().expect("reconcile");
    assert_eq!(report.resource_file_mismatches, vec![volume_id]);
    // The record is KEPT: the resource may still be live and the record
    // is the only authority trail.
    let stored = stored(&fixture, "no-res");
    assert_eq!(stored.runtime.state, VolumeLifecycle::Failed);
    assert!(stored.runtime.attachment.is_some());
    assert_eq!(
        fixture
            .world
            .lock()
            .expect("world")
            .resources
            .get(&resource_of("no-res"))
            .expect("resource")
            .role,
        Role::Primary,
        "the live writer is never demoted by reconcile"
    );
    let _ = attached;
}

#[tokio::test]
async fn reconcile_marks_a_downed_resource_failed_and_clears_the_record() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "downed", GIB);
    let provider = provider_from(&fixture.state_path, &fixture.world);
    let volume_id = VolumeId::new("downed").expect("id");
    let attached = provider
        .attach_volume(&volume_id, &attach_req("downed", 1))
        .await
        .expect("attach");
    fixture
        .world
        .lock()
        .expect("world")
        .resources
        .remove(&resource_of("downed"));
    let report = provider.reconcile().expect("reconcile");
    assert_eq!(report.downed_volumes, vec![volume_id.clone()]);
    assert_eq!(report.cleared_attachments.len(), 1);
    assert_eq!(
        report.cleared_attachments[0].reason,
        volvisor_drbd::ClearedAttachmentReason::ResourceDown
    );
    let stored = stored(&fixture, "downed");
    assert_eq!(stored.runtime.state, VolumeLifecycle::Failed);
    assert!(stored.runtime.attachment.is_none());
    let _ = attached;
}

#[tokio::test]
async fn reconcile_reports_a_zombie_primary_without_demoting_it() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "zombie-rec", GIB);
    let provider = provider_from(&fixture.state_path, &fixture.world);
    fixture
        .world
        .lock()
        .expect("world")
        .resources
        .get_mut(&resource_of("zombie-rec"))
        .expect("resource")
        .role = Role::Primary;
    let report = provider.reconcile().expect("reconcile");
    assert_eq!(
        report.zombie_primaries,
        vec![VolumeId::new("zombie-rec").expect("id")]
    );
    assert_eq!(
        stored(&fixture, "zombie-rec").runtime.state,
        VolumeLifecycle::Failed
    );
    assert_eq!(
        fixture
            .world
            .lock()
            .expect("world")
            .resources
            .get(&resource_of("zombie-rec"))
            .expect("resource")
            .role,
        Role::Primary,
        "never demoted (rule 17)"
    );
}

#[tokio::test]
async fn reconcile_fails_a_foreign_peer_on_an_unseeded_volume() {
    let fixture = fixture();
    // A degraded-created volume whose peer appeared HOLDING DATA.
    fixture.world.lock().expect("world").peer_online = false;
    let response = fixture
        .provider
        .create_volume(&nearline_create_degraded("foreign-later", GIB, true))
        .await
        .expect("degraded create");
    assert_eq!(response.state, VolumeLifecycle::Ready);
    {
        let mut world = fixture.world.lock().expect("world");
        world.peer_online = true;
        world
            .resources
            .get_mut(&resource_of("foreign-later"))
            .expect("resource")
            .peer_disk = DiskState::UpToDate;
    }
    let report = fixture.provider.reconcile().expect("reconcile");
    assert_eq!(
        report.foreign_peer_volumes,
        vec![VolumeId::new("foreign-later").expect("id")]
    );
    assert_eq!(
        stored(&fixture, "foreign-later").runtime.state,
        VolumeLifecycle::Failed
    );
    assert!(!fixture.world.lock().expect("world").peer_overwritten);
}

#[tokio::test]
async fn reconcile_adopts_a_crashed_seed_while_the_resync_is_in_flight() {
    // The round-2 grammar fix, end to end: a predecessor crashed in
    // the window between `primary --force` and the state save. Its
    // resource is up MID-RESYNC — local UpToDate, peer Inconsistent —
    // so `drbdsetup status` answers with the real replication-first
    // peer-device line (`replication:SyncSource peer-disk:Inconsistent
    // done:37.50`). This test pins that the real-order line drives the
    // full crashed-seed lifecycle: adoption, a quiet steady-state
    // pass, honest Degraded-while-resyncing health, Healthy once
    // caught up. It deliberately does NOT claim to pin the peer-disk
    // capture itself: the pre-fix order-reversed parser silently
    // DROPPED peer_disk from this line (no parse error), and the seed
    // adoption here flows from the data-holding LOCAL disk — peer
    // observability must not gate crash recovery. The capture fact is
    // pinned by the report.rs unit tests
    // (`parses_resync_progress_on_the_peer_disk_line` and the
    // verbatim 9.29.0 fixtures), which fail under exactly that old
    // parser.
    let fixture = fixture();
    let volume_id = VolumeId::new("crash-seed").expect("id");
    seed_volume(&fixture.base, &fixture.world, "crash-seed", GIB);

    // Reconstruct the crash-time state: the seed resync is still
    // running (peer Inconsistent mid-resync — the fake emits the real
    // replication-first line for exactly this state), and the
    // predecessor died before the seeded flag was saved.
    {
        let mut world = fixture.world.lock().expect("world");
        let resource = world
            .resources
            .get_mut(&resource_of("crash-seed"))
            .expect("resource");
        resource.peer_disk = DiskState::Inconsistent;
        resource.resyncing = true;
    }
    {
        let mut state = DrbdState::load(&fixture.state_path).expect("load state");
        state
            .volume_mut(&volume_id)
            .expect("stored volume")
            .runtime
            .seeded = false;
        state.save(&fixture.state_path).expect("save state");
    }

    // A fresh provider boots over the crashed state: its startup
    // reconcile reads the real status line and adopts the seed — the
    // data-holding local disk proves the predecessor's `primary
    // --force` ran. No foreign verdict, no failure, no re-force.
    let provider = provider_from(&fixture.state_path, &fixture.world);
    let stored_volume = stored(&fixture, "crash-seed");
    assert_eq!(stored_volume.runtime.state, VolumeLifecycle::Ready);
    assert!(stored_volume.runtime.seeded);

    // The steady-state pass is quiet (idempotent: nothing left to
    // heal, nothing to fail).
    let report = provider.reconcile().expect("reconcile");
    assert_eq!(report.foreign_peer_volumes, Vec::<VolumeId>::new());
    assert_eq!(report.missing_volumes, Vec::<VolumeId>::new());
    assert_eq!(report.downed_volumes, Vec::<VolumeId>::new());
    assert_eq!(report.zombie_primaries, Vec::<VolumeId>::new());
    assert_eq!(report.seeded_volumes, Vec::<VolumeId>::new());

    // While the resync runs, health is honest: Degraded overall (the
    // replica is not caught up), Healthy on the local backend axis.
    let inspected = provider.inspect_volume(&volume_id).await.expect("inspect");
    assert_eq!(inspected.health, volvisor_types::domain::Health::Degraded);
    assert_eq!(
        inspected.backend_health,
        volvisor_types::domain::Health::Healthy
    );

    // Once the peer catches up (resync completed out of band), the
    // same status grammar — now without the replication token —
    // reports the volume Healthy.
    {
        let mut world = fixture.world.lock().expect("world");
        let resource = world
            .resources
            .get_mut(&resource_of("crash-seed"))
            .expect("resource");
        resource.peer_disk = DiskState::UpToDate;
        resource.resyncing = false;
    }
    let inspected = provider.inspect_volume(&volume_id).await.expect("inspect");
    assert_eq!(inspected.health, volvisor_types::domain::Health::Healthy);
}

#[tokio::test]
async fn reconcile_heals_an_unrecorded_grow() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "heal-grow", GIB);
    let provider = provider_from(&fixture.state_path, &fixture.world);
    // The backing and device grew out of band (a crash window after a
    // completed lvextend + resize).
    {
        let resource = resource_of("heal-grow");
        let mut world = fixture.world.lock().expect("world");
        let lv = world.lvs.get_mut(&format!("{VG}/{resource}")).expect("LV");
        lv.size = 2 * GIB;
        world
            .resources
            .get_mut(&resource)
            .expect("resource")
            .device_size = 2 * GIB;
    }
    let report = provider.reconcile().expect("reconcile");
    assert_eq!(
        report.healed_grown,
        vec![VolumeId::new("heal-grow").expect("id")]
    );
    assert_eq!(stored(&fixture, "heal-grow").entry.size_bytes, 2 * GIB);
}

#[tokio::test]
async fn reconcile_fails_a_shrunk_device() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "shrink", GIB);
    let provider = provider_from(&fixture.state_path, &fixture.world);
    fixture
        .world
        .lock()
        .expect("world")
        .resources
        .get_mut(&resource_of("shrink"))
        .expect("resource")
        .device_size = GIB / 2;
    let report = provider.reconcile().expect("reconcile");
    assert_eq!(
        report.shrunk_volumes,
        vec![VolumeId::new("shrink").expect("id")]
    );
    assert_eq!(
        stored(&fixture, "shrink").runtime.state,
        VolumeLifecycle::Failed
    );
}

#[tokio::test]
async fn reconcile_leaves_a_transiently_unverifiable_volume_untouched() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "opaque", GIB);
    let provider = provider_from(&fixture.state_path, &fixture.world);
    fixture.world.lock().expect("world").fail_status = true;
    let report = provider.reconcile().expect("reconcile");
    assert_eq!(report.unverifiable_volumes.len(), 1);
    assert_eq!(
        report.unverifiable_volumes[0].volume_id,
        VolumeId::new("opaque").expect("id")
    );
    // Untouched: still Ready, not guessed into Failed.
    assert_eq!(
        stored(&fixture, "opaque").runtime.state,
        VolumeLifecycle::Ready
    );
}

#[tokio::test]
async fn reconcile_reports_untracked_and_foreign_lvs_without_touching_them() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "tracked", GIB);
    let provider = provider_from(&fixture.state_path, &fixture.world);
    seed_lv(
        &fixture.world,
        VG,
        "vol-orphan",
        GIB,
        Some("a-volume-without-state"),
        false,
    );
    seed_lv(&fixture.world, VG, "not-ours", GIB, None, false);
    let report = provider.reconcile().expect("reconcile");
    assert_eq!(report.untracked_owned_lvs, vec!["vol-orphan".to_owned()]);
    assert_eq!(report.foreign_lvs, vec!["not-ours".to_owned()]);
    let world = fixture.world.lock().expect("world");
    assert!(world.lvs.contains_key(&format!("{VG}/vol-orphan")));
    assert!(world.lvs.contains_key(&format!("{VG}/not-ours")));
}

// ---------------------------------------------------------------------------
// Inspect honesty
// ---------------------------------------------------------------------------

#[tokio::test]
async fn inspect_reflects_observed_health_and_the_remote_axis() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "healthy", GIB);
    let provider = provider_from(&fixture.state_path, &fixture.world);
    let volume_id = VolumeId::new("healthy").expect("id");

    // Connected, both ends UpToDate, no resync: Healthy, and the remote
    // axis states the observed replica (never a durability claim).
    let inspected = provider.inspect_volume(&volume_id).await.expect("inspect");
    assert_eq!(inspected.health, volvisor_types::domain::Health::Healthy);
    assert_eq!(
        inspected.backend_health,
        volvisor_types::domain::Health::Healthy
    );
    assert!(matches!(
        inspected.effective_protection.remote,
        volvisor_types::domain::RemoteProtectionAxis::AsynchronousPeer
    ));
    assert_eq!(inspected.provisioned_bytes, GIB);

    // A resyncing peer is Degraded with no remote claim.
    {
        let mut world = fixture.world.lock().expect("world");
        let state = world
            .resources
            .get_mut(&resource_of("healthy"))
            .expect("resource");
        state.peer_disk = DiskState::Inconsistent;
        state.resyncing = true;
    }
    let inspected = provider.inspect_volume(&volume_id).await.expect("inspect");
    assert_eq!(inspected.health, volvisor_types::domain::Health::Degraded);
    assert_eq!(
        inspected.effective_protection.remote,
        volvisor_types::domain::RemoteProtectionAxis::None
    );

    // A lost connection keeps the local axis honest and drops the
    // remote claim.
    fixture.world.lock().expect("world").peer_online = false;
    let inspected = provider.inspect_volume(&volume_id).await.expect("inspect");
    assert_eq!(inspected.health, volvisor_types::domain::Health::Degraded);
    assert_eq!(
        inspected.backend_health,
        volvisor_types::domain::Health::Healthy
    );
    assert_eq!(
        inspected.effective_protection.remote,
        volvisor_types::domain::RemoteProtectionAxis::None
    );
}

/// The remote protection axis is classified by the protocol READ BACK
/// from the resource's own definition file, not by the recorded
/// intent: Protocol C reports the synchronous peer, Protocol B stays
/// the asynchronous axis (remote memory arrival is still possible-RPO
/// on peer loss).
#[tokio::test]
async fn inspect_classifies_the_remote_axis_by_the_protocol_read_back() {
    let fixture = fixture();

    // Protocol C (Sync): the established replica is the synchronous
    // peer.
    fixture
        .provider
        .create_volume(&nearline_create_protocol(
            "sync-vol",
            GIB,
            ReplicationModeRequest::Sync,
            false,
        ))
        .await
        .expect("create protocol C");
    let inspected = fixture
        .provider
        .inspect_volume(&VolumeId::new("sync-vol").expect("id"))
        .await
        .expect("inspect");
    assert!(matches!(
        inspected.effective_protection.remote,
        RemoteProtectionAxis::SynchronousPeer
    ));
    // The definition the axis was classified by really carries C.
    let res = std::fs::read_to_string(res_file_path(
        &fixture.base.join("drbd.d"),
        &resource_of("sync-vol"),
    ))
    .expect("res file");
    assert!(res.contains("protocol C;"));

    // Protocol B (SemiSync): remote MEMORY arrival only — still the
    // asynchronous axis, never the synchronous one.
    fixture
        .provider
        .create_volume(&nearline_create_protocol(
            "semisync-vol",
            GIB,
            ReplicationModeRequest::SemiSync,
            false,
        ))
        .await
        .expect("create protocol B");
    let inspected = fixture
        .provider
        .inspect_volume(&VolumeId::new("semisync-vol").expect("id"))
        .await
        .expect("inspect");
    assert!(matches!(
        inspected.effective_protection.remote,
        RemoteProtectionAxis::AsynchronousPeer
    ));

    // A definition file that can no longer be read back is an honest
    // unknown: no remote claim even for Protocol C.
    std::fs::remove_file(res_file_path(
        &fixture.base.join("drbd.d"),
        &resource_of("sync-vol"),
    ))
    .expect("remove res file");
    let inspected = fixture
        .provider
        .inspect_volume(&VolumeId::new("sync-vol").expect("id"))
        .await
        .expect("inspect");
    assert_eq!(
        inspected.effective_protection.remote,
        RemoteProtectionAxis::None
    );
}

#[tokio::test]
async fn inspect_reports_unknown_axes_for_a_downed_resource() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "down-inspect", GIB);
    let provider = provider_from(&fixture.state_path, &fixture.world);
    fixture
        .world
        .lock()
        .expect("world")
        .resources
        .remove(&resource_of("down-inspect"));
    let inspected = provider
        .inspect_volume(&VolumeId::new("down-inspect").expect("id"))
        .await
        .expect("inspect");
    assert_eq!(inspected.health, volvisor_types::domain::Health::Unknown);
    assert_eq!(
        inspected.backend_health,
        volvisor_types::domain::Health::Unknown
    );
    // The sizes come from lvs, never fabricated.
    assert_eq!(inspected.provisioned_bytes, GIB);
}

#[tokio::test]
async fn inspect_reports_failed_for_a_backing_smaller_than_the_record() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "shrunk-lv", GIB);
    let provider = provider_from(&fixture.state_path, &fixture.world);
    {
        let resource = resource_of("shrunk-lv");
        let mut world = fixture.world.lock().expect("world");
        let lv = world.lvs.get_mut(&format!("{VG}/{resource}")).expect("LV");
        lv.size = GIB / 2;
    }
    let inspected = provider
        .inspect_volume(&VolumeId::new("shrunk-lv").expect("id"))
        .await
        .expect("inspect");
    assert_eq!(inspected.state, VolumeLifecycle::Failed);
    assert_eq!(inspected.health, volvisor_types::domain::Health::Unhealthy);
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_downs_the_resource_removes_the_file_and_retains_the_lv() {
    let fixture = fixture();
    let volume_id = VolumeId::new("gone").expect("id");
    fixture
        .provider
        .create_volume(&nearline_create("gone", GIB))
        .await
        .expect("create");
    fixture
        .provider
        .delete_volume(&volume_id, &delete_req("gone", 1))
        .await
        .expect("delete");
    let resource = resource_of("gone");
    {
        let world = fixture.world.lock().expect("world");
        assert!(world.resources.is_empty(), "the resource is down");
        assert!(
            world.lvs.contains_key(&format!("{VG}/{resource}")),
            "Retain keeps the backing LV"
        );
    }
    assert!(!res_file_path(&fixture.base.join("drbd.d"), &resource).exists());
    assert!(!stored_maybe(&fixture, "gone"));
    let error = fixture
        .provider
        .inspect_volume(&volume_id)
        .await
        .expect_err("deleted volume");
    assert_eq!(error.code, ApiErrorCode::NotFound);
}

#[tokio::test]
async fn delete_refuses_zero_discard_and_attached_volumes() {
    let fixture = fixture();
    let volume_id = VolumeId::new("no-zero").expect("id");
    fixture
        .provider
        .create_volume(&nearline_create("no-zero", GIB))
        .await
        .expect("create");
    let mut req = delete_req("no-zero", 1);
    req.data_erasure_policy = ErasurePolicy::ZeroDiscard;
    let error = fixture
        .provider
        .delete_volume(&volume_id, &req)
        .await
        .expect_err("zero discard");
    assert_eq!(error.code, ApiErrorCode::UnsupportedClassOrPolicy);

    let attached = fixture
        .provider
        .attach_volume(&volume_id, &attach_req("no-zero", 1))
        .await
        .expect("attach");
    let error = fixture
        .provider
        .delete_volume(
            &volume_id,
            &delete_req("no-zero", attached.volume_generation),
        )
        .await
        .expect_err("attached delete");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
    // The writer is still attached and Primary.
    assert_eq!(
        fixture
            .world
            .lock()
            .expect("world")
            .resources
            .get(&resource_of("no-zero"))
            .expect("resource")
            .role,
        Role::Primary
    );
}

#[tokio::test]
async fn delete_never_demotes_a_zombie_primary() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "zombie-del", GIB);
    let provider = provider_from(&fixture.state_path, &fixture.world);
    fixture
        .world
        .lock()
        .expect("world")
        .resources
        .get_mut(&resource_of("zombie-del"))
        .expect("resource")
        .role = Role::Primary;
    let error = provider
        .delete_volume(
            &VolumeId::new("zombie-del").expect("id"),
            &delete_req("zombie-del", 1),
        )
        .await
        .expect_err("zombie delete");
    assert_eq!(error.code, ApiErrorCode::InvalidState);
    assert_eq!(
        fixture
            .world
            .lock()
            .expect("world")
            .resources
            .get(&resource_of("zombie-del"))
            .expect("resource")
            .role,
        Role::Primary
    );
}

#[tokio::test]
async fn delete_refuses_a_mismatched_res_file() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "tampered", GIB);
    let provider = provider_from(&fixture.state_path, &fixture.world);
    // The operator's file was replaced by a foreign definition.
    let path = res_file_path(&fixture.base.join("drbd.d"), &resource_of("tampered"));
    std::fs::write(&path, "resource someone-else {\n}\n").expect("tamper");
    let error = provider
        .delete_volume(
            &VolumeId::new("tampered").expect("id"),
            &delete_req("tampered", 1),
        )
        .await
        .expect_err("mismatched res file");
    assert_eq!(error.code, ApiErrorCode::ForeignDeviceState);
    assert!(path.exists(), "the foreign file is never destroyed");
}

#[tokio::test]
async fn delete_removes_a_failed_volume_whose_backing_already_vanished() {
    let fixture = fixture();
    seed_volume(&fixture.base, &fixture.world, "failed-gone", GIB);
    let provider = provider_from(&fixture.state_path, &fixture.world);
    fixture
        .world
        .lock()
        .expect("world")
        .lvs
        .remove(&format!("{VG}/{}", resource_of("failed-gone")));
    let report = provider.reconcile().expect("reconcile");
    assert_eq!(report.missing_volumes.len(), 1);
    // Delete of the Failed, backing-less entry is a clean record
    // removal.
    provider
        .delete_volume(
            &VolumeId::new("failed-gone").expect("id"),
            &delete_req("failed-gone", 1),
        )
        .await
        .expect("delete failed volume");
    assert!(!stored_maybe(&fixture, "failed-gone"));
}

// ---------------------------------------------------------------------------
// Startup verification and the argv contract
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_constructor_refuses_a_wrong_host() {
    let (base, state_path) = {
        let fixture = fixture();
        (fixture.base.clone(), fixture.state_path.clone())
    };
    let world = std::sync::Arc::new(std::sync::Mutex::new(common::FakeDrbd::default()));
    world.lock().expect("world").node_name = "some-other-host".to_owned();
    let runner = common::FakeDrbd::runner(&world);
    let error = volvisor_drbd::DrbdProvider::new(runner, common::config_for(&base), state_path)
        .err()
        .expect("wrong host");
    assert_eq!(error.code, ApiErrorCode::ForeignDeviceState);
}

#[tokio::test]
async fn the_constructor_refuses_an_absent_toolchain() {
    let (base, state_path) = {
        let fixture = fixture();
        (fixture.base.clone(), fixture.state_path.clone())
    };
    let world = std::sync::Arc::new(std::sync::Mutex::new(common::FakeDrbd::default()));
    world.lock().expect("world").fail_drbdadm_version = true;
    let runner = common::FakeDrbd::runner(&world);
    let error = volvisor_drbd::DrbdProvider::new(runner, common::config_for(&base), state_path)
        .err()
        .expect("no drbd-utils");
    assert_eq!(error.code, ApiErrorCode::Internal);
}

#[tokio::test]
async fn the_constructor_refuses_an_unqueryable_vg() {
    let (base, state_path) = {
        let fixture = fixture();
        (fixture.base.clone(), fixture.state_path.clone())
    };
    let world = std::sync::Arc::new(std::sync::Mutex::new(common::FakeDrbd::default()));
    // The VG the configuration names is not reported by vgs.
    world.lock().expect("world").vg_name = "some-other-vg".to_owned();
    let runner = common::FakeDrbd::runner(&world);
    let error = volvisor_drbd::DrbdProvider::new(runner, common::config_for(&base), state_path)
        .err()
        .expect("no vg");
    assert_eq!(error.code, ApiErrorCode::NotFound);
}

#[tokio::test]
async fn every_drbdadm_invocation_is_scoped_to_our_own_res_file() {
    let fixture = fixture();
    // A full lifecycle exercises every drbdadm verb.
    let volume_id = VolumeId::new("argv").expect("id");
    fixture
        .provider
        .create_volume(&nearline_create("argv", GIB))
        .await
        .expect("create");
    let attached = fixture
        .provider
        .attach_volume(&volume_id, &attach_req("argv", 1))
        .await
        .expect("attach");
    fixture
        .provider
        .detach_volume(
            &volume_id,
            &attached.attachment_id,
            &detach_req("argv", attached.attachment_generation),
        )
        .await
        .expect("detach");
    fixture
        .provider
        .delete_volume(
            &volume_id,
            &delete_req("argv", attached.volume_generation + 1),
        )
        .await
        .expect("delete");
    let resource = resource_of("argv");
    for invocation in fixture.runner.invocations() {
        if invocation.program != "drbdadm" {
            continue;
        }
        if invocation.args == ["--version"] {
            continue;
        }
        assert_eq!(
            invocation.args.first().map(String::as_str),
            Some("-c"),
            "every action is scoped: {invocation:?}"
        );
        assert_eq!(
            invocation.args.get(1).map(String::as_str),
            Some(
                res_file_path(&fixture.base.join("drbd.d"), &resource)
                    .to_str()
                    .expect("utf-8 path")
            ),
            "the scope is exactly volvisor's res file: {invocation:?}"
        );
        assert_eq!(
            invocation.args.last().map(String::as_str),
            Some(resource.as_str()),
            "the resource is ours: {invocation:?}"
        );
    }
}
