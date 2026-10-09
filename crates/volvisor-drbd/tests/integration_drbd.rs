//! Real-cluster DRBD integration test against an operator-provisioned
//! peer (feature + env gated).
//!
//! Compile gate: `--features integration-drbd`. Runtime gate: the test
//! skips itself unless `VOLVISOR_TEST_DRBD=1` is set **and** the
//! deployment parameters are provided through
//!
//! - `VOLVISOR_DRBD_VG` — the nearline volume group (must exist),
//! - `VOLVISOR_DRBD_CONFIG_DIR` — the drbd config dir (must exist,
//!   e.g. `/etc/drbd.d`; generated `volvisor-*.res` files land there),
//! - `VOLVISOR_DRBD_NODE_NAME` — the local `on` name (must match
//!   `uname -n`),
//! - `VOLVISOR_DRBD_LOCAL_ADDRESS` — the local replication IPv4,
//! - `VOLVISOR_DRBD_PEER_NAME` — the peer's `on` name,
//! - `VOLVISOR_DRBD_PEER_ADDRESS` — the peer as `<ipv4>:<port>`,
//! - `VOLVISOR_DRBD_SECRET_FILE` — the peer shared secret (non-empty,
//!   owner-only).
//!
//! Operator prerequisites (plan §7 deployment model): the peer host
//! runs the same drbd-utils generation with the kernel module loaded,
//! and the operator pre-provisions the peer's backing LV at
//! `VOLVISOR_DRBD_PEER_BACKING_BYTES` (or larger) under the SAME
//! `/dev/<vg>/<lv>` path the generated definition names, fresh
//! (never seeded). The test requires root (device node access).
//!
//! One volume is created, seeded, promoted, grown, demoted and deleted
//! (Retain: resource down, res file removed, backing LV retained); a
//! cleanup guard best-effort-removes the LV on every path.
//!
//! OUTSTANDING CONFIRMATION: the `drbdmeta` create-md semantics the
//! simulated world models — re-initialize (succeed) over an all-zero
//! data area even with existing metadata, refuse ("Operation refused")
//! only over non-zero data — derive from the drbdmeta source, not from
//! a run on a real cluster: no CI slice executes these gated tests.
//! The crash-recovery disposition in the provider depends on exactly
//! this distinction, so the first operator run against a real
//! drbd-utils build must observe the create-md behavior documented
//! here before it is trusted.
//!
//! Test-kit code: `expect`/`unwrap` are allowed here by convention.

#![cfg(feature = "integration-drbd")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;

use volvisor_drbd::provider::{DrbdProvider, DrbdProviderConfig, resource_name_for};
use volvisor_drbd::{CommandRunner, RealRunner};
use volvisor_provider::VolumeProvider;
use volvisor_provider::conformance::{
    fixture_attach_request, fixture_delete_request, fixture_detach_request, fixture_grow_request,
};
use volvisor_types::domain::VolumeClass;
use volvisor_types::request::{
    CreateVolumeRequest, ReplicationModeRequest, ReplicationPolicyRequest,
};
use volvisor_types::{ApiErrorCode, AttachmentState, Frontend, VolumeId, VolumeLifecycle};

/// The volume identity driven through the lifecycle.
const VOLUME: &str = "itest-drbd-vol";
/// Volume size used in the lifecycle (64 MiB, extent-aligned).
const VOLUME_BYTES: u64 = 64 << 20;
/// Grown volume size (the peer backing must be at least this large).
const GROWN_BYTES: u64 = 128 << 20;
/// The operator-declared DRBD minor range used by the test provider.
const MINOR_MIN: u32 = 200;
/// See [`MINOR_MIN`].
const MINOR_MAX: u32 = 210;
/// The operator-declared local replication port range.
const PORT_MIN: u16 = 7990;
/// See [`PORT_MIN`].
const PORT_MAX: u16 = 7999;

fn volume_id() -> VolumeId {
    VolumeId::new(VOLUME).expect("valid volume id")
}

/// The skip note printed when the gate below does not open.
const SKIP_NOTE: &str = "skipping real-DRBD integration test: set VOLVISOR_TEST_DRBD=1 plus \
                         VOLVISOR_DRBD_VG/_CONFIG_DIR/_NODE_NAME/_LOCAL_ADDRESS/_PEER_NAME/\
                         _PEER_ADDRESS/_SECRET_FILE (requires the drbd kernel module, \
                         drbd-utils, a fresh peer backing LV and root) to run it";

/// Runtime gate for the real-DRBD tests: the opt-in flag **and** a
/// complete deployment configuration must both be present (anything
/// else skips, so an unrelated environment never sees failures).
fn drbd_tests_ready() -> Option<DrbdProviderConfig> {
    if std::env::var("VOLVISOR_TEST_DRBD").ok().as_deref() != Some("1") {
        return None;
    }
    Some(DrbdProviderConfig {
        vg_name: std::env::var("VOLVISOR_DRBD_VG").ok()?,
        config_dir: PathBuf::from(std::env::var("VOLVISOR_DRBD_CONFIG_DIR").ok()?),
        node_name: std::env::var("VOLVISOR_DRBD_NODE_NAME").ok()?,
        local_address: std::env::var("VOLVISOR_DRBD_LOCAL_ADDRESS").ok()?,
        peer_name: std::env::var("VOLVISOR_DRBD_PEER_NAME").ok()?,
        peer_address: std::env::var("VOLVISOR_DRBD_PEER_ADDRESS").ok()?,
        shared_secret_file: PathBuf::from(std::env::var("VOLVISOR_DRBD_SECRET_FILE").ok()?),
        port_min: PORT_MIN,
        port_max: PORT_MAX,
        minor_min: MINOR_MIN,
        minor_max: MINOR_MAX,
        // The real /proc on a real host.
        proc_root: PathBuf::from("/proc"),
    })
}

/// A valid `nearline-replicated` create request (the shared kit fixture
/// is hardwired to `native-local` and carries no replication policy).
fn create_request(size_bytes: u64) -> CreateVolumeRequest {
    let mut request = volvisor_provider::conformance::fixture_create_request(VOLUME, size_bytes);
    request.volume_class = VolumeClass::NearlineReplicated;
    request.replication = Some(ReplicationPolicyRequest {
        engine: Some("drbd9".to_owned()),
        mode: ReplicationModeRequest::Async,
        remote_replicas: 1,
        allow_degraded_create: false,
    });
    request
}

/// A real deployment under test: the provider plus the shared real
/// runner (for independent verification queries).
struct Cluster {
    /// The provider under test.
    provider: Arc<DrbdProvider>,
    /// The real runner shared with the provider.
    runner: Arc<dyn CommandRunner>,
    /// The verified deployment configuration.
    config: DrbdProviderConfig,
}

/// Set up a provider over the env-configured deployment (the runtime
/// gate has already produced the configuration).
fn setup_cluster(
    dir: &std::path::Path,
    config: DrbdProviderConfig,
) -> Result<Cluster, volvisor_types::ApiError> {
    let runner: Arc<dyn CommandRunner> = Arc::new(RealRunner::default());
    let state_path = dir.join("state.json");
    let provider =
        DrbdProvider::new(Arc::clone(&runner), config.clone(), state_path).map(Arc::new)?;
    Ok(Cluster {
        provider,
        runner,
        config,
    })
}

/// Best-effort teardown of the test LV on every path, including
/// assertion failures and panics, unless the lifecycle already removed
/// the state (delete retains the LV, so the guard is the cleanup).
struct CleanupGuard {
    cluster: Option<Cluster>,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let Some(cluster) = self.cluster.take() else {
            return;
        };
        let resource = resource_name_for(&volume_id());
        // Best-effort, in order: bring the resource down through our
        // own scoped file (fails harmlessly when delete already removed
        // it), then remove the backing LV (fails harmlessly when the
        // resource is still attached or the LV is gone).
        let res_file = cluster
            .config
            .config_dir
            .join(format!("volvisor-{resource}.res"));
        drop(cluster.runner.run(
            "drbdadm",
            &["-c", &res_file.to_string_lossy(), "down", &resource],
        ));
        let spec = format!("{}/{}", cluster.config.vg_name, resource);
        drop(cluster.runner.run("lvremove", &["--yes", &spec]));
    }
}

/// Phase: create a volume and assert the honest post-create contract.
async fn phase_create(provider: &DrbdProvider) -> u64 {
    let created = provider
        .create_volume(&create_request(VOLUME_BYTES))
        .await
        .expect("create");
    assert_eq!(created.generation, 1);
    assert_eq!(created.state, VolumeLifecycle::Ready);
    // Thick LVM rounds to whole extents: at-or-above the request.
    assert!(
        created.provisioned_bytes >= VOLUME_BYTES,
        "the device must cover the request (extent rounding allowed)"
    );
    created.provisioned_bytes
}

/// Phase: attach (single-writer) and assert the prepared frontend
/// handle over the DRBD device.
async fn phase_attach(provider: &DrbdProvider) -> volvisor_types::request::AttachVolumeResponse {
    let attached = provider
        .attach_volume(
            &volume_id(),
            &fixture_attach_request(VOLUME, "itest-drbd-att", 1),
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
        host_device_path.starts_with("/dev/drbd"),
        "the frontend handle is the DRBD device, got {host_device_path}"
    );

    // A second writer is rejected (single-writer fencing).
    let err = provider
        .attach_volume(
            &volume_id(),
            &fixture_attach_request(VOLUME, "itest-drbd-att-2", 2),
        )
        .await
        .expect_err("second writer must be rejected");
    assert_eq!(err.code, ApiErrorCode::WriterAlreadyActive);
    attached
}

/// Phase: grow while attached; the notification honestly stays
/// retryable (the peer backing must have been grown by the operator).
async fn phase_grow(provider: &DrbdProvider, generation: u64) {
    let grown = provider
        .grow_volume(
            &volume_id(),
            &fixture_grow_request(VOLUME, GROWN_BYTES, generation),
        )
        .await
        .expect("grow");
    assert!(grown.backing_resized);
    assert!(grown.effective_size_bytes >= GROWN_BYTES);
    assert_eq!(
        grown.guest_notification_status,
        volvisor_types::request::GrowGuestNotification::RetryRequired
    );
}

/// Phase: detach with a drain proof; the volume returns to Ready.
async fn phase_detach(
    provider: &DrbdProvider,
    attached: &volvisor_types::request::AttachVolumeResponse,
) -> u64 {
    let detached = provider
        .detach_volume(
            &volume_id(),
            &attached.attachment_id,
            &fixture_detach_request("itest-drbd-att", attached.attachment_generation),
        )
        .await
        .expect("detach");
    assert_eq!(detached.state, VolumeLifecycle::Ready);
    assert!(detached.generation > attached.volume_generation);
    detached.generation
}

#[tokio::test]
async fn drbd_lifecycle_on_a_real_cluster() -> Result<(), volvisor_types::ApiError> {
    let Some(config) = drbd_tests_ready() else {
        eprintln!("{SKIP_NOTE}");
        return Ok(());
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let cluster = setup_cluster(dir.path(), config)?;
    let provider = cluster.provider.clone();
    let mut guard = CleanupGuard {
        cluster: Some(cluster),
    };

    let created_size = phase_create(&provider).await;
    let attached = phase_attach(&provider).await;
    phase_grow(&provider, attached.volume_generation).await;
    let generation = phase_detach(&provider, &attached).await;
    assert!(
        provider
            .inspect_volume(&volume_id())
            .await
            .expect("inspect after grow")
            .provisioned_bytes
            >= created_size
    );

    // Delete with Retain, then verify through LVM's own report that the
    // backing LV was retained (never the exit status alone).
    provider
        .delete_volume(&volume_id(), &fixture_delete_request(VOLUME, generation))
        .await
        .expect("delete");
    let err = provider
        .inspect_volume(&volume_id())
        .await
        .expect_err("deleted volume is gone");
    assert_eq!(err.code, ApiErrorCode::NotFound);

    let cluster = guard.cluster.take().expect("guarded cluster until the end");
    let resource = resource_name_for(&volume_id());
    let lvs = cluster
        .runner
        .run(
            "lvs",
            &[
                "--reportformat",
                "json",
                "--units",
                "b",
                "--nosuffix",
                "-o",
                "vg_name,lv_name,lv_size,lv_tags",
            ],
        )
        .expect("lvs query");
    assert!(lvs.success, "lvs must succeed");
    assert!(
        lvs.stdout.contains(&resource),
        "Retain must keep the backing LV {resource}"
    );
    let res_file = cluster
        .config
        .config_dir
        .join(format!("volvisor-{resource}.res"));
    assert!(!res_file.exists(), "delete removes the res file");

    // The lifecycle retained the LV: remove it here through LVM itself
    // (the resource is down and the device closed, so this must
    // succeed); the guard stays armed only for the failure paths.
    let removed = cluster.runner.run(
        "lvremove",
        &["--yes", &format!("{}/{}", cluster.config.vg_name, resource)],
    )?;
    assert!(removed.success, "the retained test LV must be removable");
    Ok(())
}

#[tokio::test]
async fn drbd_constructor_refuses_the_wrong_host() -> Result<(), volvisor_types::ApiError> {
    let Some(mut config) = drbd_tests_ready() else {
        eprintln!("{SKIP_NOTE}");
        return Ok(());
    };

    let dir = tempfile::tempdir().expect("tempdir");
    // A node name that cannot match `uname -n` must be refused, and
    // nothing may be written.
    config.node_name = format!("{}-wrong", config.node_name);
    let runner: Arc<dyn CommandRunner> = Arc::new(RealRunner::default());
    let state_path = dir.path().join("state.json");
    let err = DrbdProvider::new(runner, config, state_path.clone())
        .map(|_| ())
        .expect_err("the wrong host must be refused");
    assert_eq!(err.code, ApiErrorCode::ForeignDeviceState);
    assert!(!state_path.exists(), "no state may be written on refusal");
    Ok(())
}
