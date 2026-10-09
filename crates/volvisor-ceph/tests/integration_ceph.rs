//! Real-Ceph integration test against an existing external cluster
//! (feature + env gated).
//!
//! Compile gate: `--features integration-ceph`. Runtime gate: the test
//! skips itself unless `VOLVISOR_TEST_CEPH=1` is set **and** the cluster
//! parameters are provided through
//!
//! - `VOLVISOR_CEPH_FSID` — the cluster FSID (`ceph fsid`),
//! - `VOLVISOR_CEPH_MONS` — comma-separated monitor addresses,
//! - `VOLVISOR_CEPH_POOL` — the pool test images are created in,
//! - `VOLVISOR_CEPH_USER` — the full Ceph entity name passed via
//!   `--name` (e.g. `client.volvisor`).
//!
//! The tests are read-mostly: one volume is created, mapped, grown,
//! unmapped and moved to the RBD trash in the configured pool, and the
//! credentials are resolved by the ceph CLI itself (the daemon-side
//! keyring); no key material is ever passed through this file.
//!
//! Test-kit code: `expect`/`unwrap` are allowed here by convention.

#![cfg(feature = "integration-ceph")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use volvisor_ceph::provider::{CephProviderConfig, CephRbdProvider, image_name_for};
use volvisor_ceph::{CommandOutput, CommandRunner, RealRunner};
use volvisor_provider::VolumeProvider;
use volvisor_provider::conformance::{
    fixture_attach_request, fixture_create_request, fixture_delete_request, fixture_detach_request,
    fixture_grow_request,
};
use volvisor_types::domain::VolumeClass;
use volvisor_types::request::{AttachVolumeResponse, CreateVolumeRequest, InspectVolumeResponse};
use volvisor_types::{ApiErrorCode, AttachmentState, Frontend, VolumeId, VolumeLifecycle};

/// The volume identity driven through the lifecycle.
const VOLUME: &str = "itest-ceph-vol";
/// Volume size used in the lifecycle.
const VOLUME_BYTES: u64 = 64 << 20;
/// Grown volume size.
const GROWN_BYTES: u64 = 128 << 20;

fn volume_id() -> VolumeId {
    VolumeId::new(VOLUME).expect("valid volume id")
}

/// Runtime gate for the real-Ceph tests: the opt-in flag **and** a
/// complete cluster configuration must both be present (anything else
/// skips, so an unrelated environment never sees failures).
fn ceph_tests_ready() -> Option<CephProviderConfig> {
    if std::env::var("VOLVISOR_TEST_CEPH").ok().as_deref() != Some("1") {
        return None;
    }
    cluster_config()
}

/// The skip note printed when the gate above does not open.
const SKIP_NOTE: &str = "skipping real-Ceph integration test: set VOLVISOR_TEST_CEPH=1 plus \
                         VOLVISOR_CEPH_FSID/_MONS/_POOL/_USER (requires the ceph CLI, a \
                         reachable cluster and root for rbd map) to run it";

/// The cluster parameters from the environment (all four or nothing).
fn cluster_config() -> Option<CephProviderConfig> {
    Some(CephProviderConfig {
        cluster_fsid: std::env::var("VOLVISOR_CEPH_FSID").ok()?,
        mon_hosts: std::env::var("VOLVISOR_CEPH_MONS")
            .ok()?
            .split(',')
            .map(|mon| mon.trim().to_owned())
            .filter(|mon| !mon.is_empty())
            .collect(),
        pool: std::env::var("VOLVISOR_CEPH_POOL").ok()?,
        user: std::env::var("VOLVISOR_CEPH_USER").ok()?,
    })
}

/// A valid `ceph-rbd` create request (the shared kit fixture is
/// hardwired to `native-local`; only the class field differs here).
fn create_request(size_bytes: u64) -> CreateVolumeRequest {
    let mut request = fixture_create_request(VOLUME, size_bytes);
    request.volume_class = VolumeClass::CephRbd;
    request
}

/// A real cluster under test: the provider, the shared real runner (for
/// independent verification queries) and the verified configuration.
struct Cluster {
    /// The provider under test.
    provider: Arc<CephRbdProvider>,
    /// The real runner shared with the provider.
    runner: Arc<dyn CommandRunner>,
    /// The verified cluster configuration.
    config: CephProviderConfig,
}

/// Set up a provider over the env-configured cluster (the runtime gate
/// has already produced the configuration).
fn setup_cluster(
    dir: &std::path::Path,
    config: CephProviderConfig,
) -> Result<Cluster, volvisor_types::ApiError> {
    config.validate()?;
    let runner: Arc<dyn CommandRunner> = Arc::new(RealRunner::default());
    let state_path = dir.join("state.json");
    let provider =
        CephRbdProvider::new(Arc::clone(&runner), config.clone(), state_path).map(Arc::new)?;
    Ok(Cluster {
        provider,
        runner,
        config,
    })
}

/// Run one `rbd` query with the same argv shape the provider uses.
fn rbd_query(cluster: &Cluster, tail: &[&str]) -> Result<CommandOutput, volvisor_types::ApiError> {
    let mut args = vec![
        "-m".to_owned(),
        cluster.config.mon_hosts.join(","),
        "--name".to_owned(),
        cluster.config.user.clone(),
    ];
    args.extend(tail.iter().map(|arg| (*arg).to_owned()));
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    cluster.runner.run("rbd", &refs)
}

/// Moves the test image to the trash on every path, including assertion
/// failures and panics, unless the lifecycle already did.
struct TrashGuard {
    cluster: Option<Cluster>,
}

impl TrashGuard {
    /// Disarm the guard: the lifecycle itself moved the image.
    fn disarm(mut self) {
        self.cluster = None;
    }
}
impl Drop for TrashGuard {
    fn drop(&mut self) {
        let Some(cluster) = self.cluster.take() else {
            return;
        };
        let image_name = image_name_for(&volume_id());
        let spec = format!("{}/{}", cluster.config.pool, image_name);
        // Best-effort: `rbd trash move` fails harmlessly when the image
        // is already gone (deleted or trashed by the lifecycle).
        drop(rbd_query(&cluster, &["trash", "move", &spec]));
    }
}

/// Phase: create a volume and assert the honest post-create contract.
async fn phase_create(provider: &CephRbdProvider) -> InspectVolumeResponse {
    let created = provider
        .create_volume(&create_request(VOLUME_BYTES))
        .await
        .expect("create");
    assert_eq!(created.generation, 1);
    assert_eq!(created.state, VolumeLifecycle::Ready);
    // RBD is byte-granular: the effective size is exactly the request.
    assert_eq!(created.provisioned_bytes, VOLUME_BYTES);
    assert_eq!(created.allocated_bytes, VOLUME_BYTES);
    created
}

/// Phase: attach (single-writer) and assert the prepared frontend handle.
async fn phase_attach(provider: &CephRbdProvider) -> AttachVolumeResponse {
    let attached = provider
        .attach_volume(
            &volume_id(),
            &fixture_attach_request(VOLUME, "itest-ceph-att", 1),
        )
        .await
        .expect("attach");
    assert_eq!(attached.attachment_generation, 1);
    assert_eq!(attached.volume_generation, 2);
    assert_eq!(attached.state, AttachmentState::Prepared);
    let host_device_path = match &attached.frontend {
        Frontend::VirtioBlk { host_device_path } => host_device_path.as_str(),
        // The provider only ever grants virtio-blk; any other frontend
        // here fails the assertion below with a clear message.
        Frontend::PciPassthrough { .. } => "a pci-passthrough frontend",
    };
    assert!(
        host_device_path.starts_with("/dev/rbd"),
        "the frontend handle is the rbd mapping device, got {host_device_path}"
    );

    // A second writer is rejected (single-writer fencing).
    let err = provider
        .attach_volume(
            &volume_id(),
            &fixture_attach_request(VOLUME, "itest-ceph-att-2", 2),
        )
        .await
        .expect_err("second writer must be rejected");
    assert_eq!(err.code, ApiErrorCode::WriterAlreadyActive);
    attached
}

/// Phase: grow while attached; the notification honestly stays retryable.
async fn phase_grow(provider: &CephRbdProvider, generation: u64) {
    let grown = provider
        .grow_volume(
            &volume_id(),
            &fixture_grow_request(VOLUME, GROWN_BYTES, generation),
        )
        .await
        .expect("grow");
    assert!(grown.backing_resized);
    assert_eq!(grown.effective_size_bytes, GROWN_BYTES);
    assert_eq!(
        grown.guest_notification_status,
        volvisor_types::request::GrowGuestNotification::RetryRequired
    );
}

/// Phase: detach with a drain proof; the volume returns to Ready.
async fn phase_detach(provider: &CephRbdProvider, attached: &AttachVolumeResponse) -> u64 {
    let detached = provider
        .detach_volume(
            &volume_id(),
            &attached.attachment_id,
            &fixture_detach_request("itest-ceph-att", attached.attachment_generation),
        )
        .await
        .expect("detach");
    assert_eq!(detached.state, VolumeLifecycle::Ready);
    detached.generation
}

/// Phase: delete with Retain, then verify the trash placement through
/// rbd's own report (never the exit status alone).
async fn phase_delete(provider: &CephRbdProvider, generation: u64) {
    provider
        .delete_volume(&volume_id(), &fixture_delete_request(VOLUME, generation))
        .await
        .expect("delete");
    let err = provider
        .inspect_volume(&volume_id())
        .await
        .expect_err("deleted volume is gone");
    assert_eq!(err.code, ApiErrorCode::NotFound);
}

#[tokio::test]
async fn ceph_lifecycle_on_a_real_cluster() -> Result<(), volvisor_types::ApiError> {
    let Some(config) = ceph_tests_ready() else {
        eprintln!("{SKIP_NOTE}");
        return Ok(());
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let cluster = setup_cluster(dir.path(), config)?;
    let provider = cluster.provider.clone();
    let guard = TrashGuard {
        cluster: Some(cluster),
    };

    // Read-only discovery reflects the configured pool honestly.
    let pools = provider.discover_pools()?;
    assert_eq!(pools.len(), 1);
    assert_eq!(pools[0].backend_class, VolumeClass::CephRbd);

    // The full lifecycle against the real cluster.
    phase_create(&provider).await;
    let attached = phase_attach(&provider).await;
    phase_grow(&provider, attached.volume_generation).await;
    let generation = phase_detach(&provider, &attached).await;
    phase_delete(&provider, generation).await;

    // Independent verification through rbd itself: the image is absent
    // from the pool listing and present in the trash.
    let cluster = guard
        .cluster
        .as_ref()
        .expect("guarded cluster until the end");
    let image_name = image_name_for(&volume_id());
    let listed = rbd_query(
        cluster,
        &["ls", "--pool", &cluster.config.pool, "--format", "json"],
    )?;
    assert!(listed.success, "rbd ls must succeed");
    assert!(
        !listed.stdout.contains(&image_name),
        "the image must be gone from the pool"
    );
    let trash = rbd_query(
        cluster,
        &[
            "trash",
            "ls",
            "--pool",
            &cluster.config.pool,
            "--format",
            "json",
        ],
    )?;
    assert!(trash.success, "rbd trash ls must succeed");
    assert!(
        trash.stdout.contains(&image_name),
        "the image must be in the trash"
    );

    // Startup reconciliation over the now-empty state reports nothing.
    let report = provider.reconcile()?;
    assert_eq!(report.missing_volumes, []);
    assert_eq!(report.mismatched_volumes, []);
    assert_eq!(report.stale_mappings, []);

    // The lifecycle itself trashed the image: disarm the cleanup guard.
    guard.disarm();
    Ok(())
}

#[tokio::test]
async fn ceph_constructor_refuses_a_mis_pointed_cluster() -> Result<(), volvisor_types::ApiError> {
    let Some(mut config) = ceph_tests_ready() else {
        eprintln!("{SKIP_NOTE}");
        return Ok(());
    };

    let dir = tempfile::tempdir().expect("tempdir");
    // A wrong FSID must never be adopted, and nothing may be written.
    config.cluster_fsid = format!("{}-wrong", config.cluster_fsid);
    let runner: Arc<dyn CommandRunner> = Arc::new(RealRunner::default());
    let state_path = dir.path().join("state.json");
    let err = CephRbdProvider::new(runner, config, state_path.clone())
        .map(|_| ())
        .expect_err("a mis-pointed cluster must be refused");
    assert_eq!(err.code, ApiErrorCode::ForeignDeviceState);
    assert!(!state_path.exists(), "no state may be written on refusal");
    Ok(())
}
