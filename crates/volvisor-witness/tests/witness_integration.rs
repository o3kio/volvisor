//! Loopback integration tests for the witness HTTP surface and client.
//!
//! Real axum server on an ephemeral port, real hyper client, real
//! journal-backed registry in a temp directory, deterministic injected
//! clock — no transport shortcuts. Covers the P4a plan §8 "witness
//! server" rows: auth fail-closed, protocol-version refusal, full
//! operation round-trips, `FENCE_PENDING` retry-after recovery through
//! the client, and journal-backed restart mid-traffic. Plus the P4b
//! plan §9 stage-B1 rows 1–3: the W8 caller-identity binding (host
//! credentials mutate, the legacy shared token is read-only), the W9
//! barrier record/void lifecycle, and the W10 batch mutations
//! (all-or-nothing, one commit-index bump, idempotent replay).
//!
//! Test-kit code: `expect`/`unwrap` are allowed here by convention.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use volvisor_types::{
    BarrierAttestation, EndpointBacking, HostId, LeaseId, MigrationId, OperationId, VolumeId,
    WriterEpoch,
};
use volvisor_witness::client::{HttpWitnessConnection, WitnessConnection};
use volvisor_witness::proto::{
    BatchGrantVolume, BatchRelease, GrantRequest, GrantSetRequest, RecordBarrierRequest,
    RegisterRequest, RegistrationContent, RenewRequest, RevokeRequest, RevokeSetRequest,
    VoidBarrierRequest, WITNESS_PROTOCOL_VERSION, WitnessError,
};
use volvisor_witness::registry::{WitnessCore, WitnessCoreConfig};
use volvisor_witness::server::{WitnessServerState, router};

/// Deterministic knobs: ttl 100s, grace 5s, budget 5s (W7 wait 10s past
/// a lease's recorded end), matching the registry unit tests.
fn test_config() -> WitnessCoreConfig {
    WitnessCoreConfig {
        lease_ttl_secs: 100,
        lease_grace_secs: 5,
        suspend_budget_secs: 5,
    }
}

/// The legacy shared admin token (read-only on a v2 witness).
const ADMIN_TOKEN: &str = "admin-shared-token";
/// Per-host credentials (W8): node-1 and node-2 each hold their own.
fn host_tokens() -> BTreeMap<String, String> {
    let mut tokens = BTreeMap::new();
    tokens.insert("node-1".to_owned(), "host-1-token".to_owned());
    tokens.insert("node-2".to_owned(), "host-2-token".to_owned());
    tokens
}

struct Server {
    addr: SocketAddr,
    handle: tokio::task::JoinHandle<()>,
}

async fn spawn_witness(
    dir: &Path,
    token: Option<String>,
    host_tokens: BTreeMap<String, String>,
    clock: Arc<AtomicU64>,
) -> Server {
    let core = WitnessCore::open(dir, test_config()).expect("witness core opens");
    let state = Arc::new(WitnessServerState::with_clock(
        core,
        token,
        host_tokens,
        Arc::new(move || clock.load(Ordering::SeqCst)),
    ));
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ephemeral bind");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("server serves");
    });
    Server { addr, handle }
}

/// A witness with both host credentials and the legacy shared token.
async fn standard_witness(dir: &Path, clock: Arc<AtomicU64>) -> Server {
    spawn_witness(dir, Some(ADMIN_TOKEN.to_owned()), host_tokens(), clock).await
}

fn client_for(server: &Server, token: Option<String>) -> HttpWitnessConnection {
    HttpWitnessConnection::new(
        format!("http://{}", server.addr),
        token,
        Duration::from_secs(5),
    )
}

/// The client presenting host(n)'s credential (the mutating surface).
fn host_client(server: &Server, n: u64) -> HttpWitnessConnection {
    client_for(server, Some(format!("host-{n}-token")))
}

/// The client presenting the legacy shared token (read-only).
fn legacy_client(server: &Server) -> HttpWitnessConnection {
    client_for(server, Some(ADMIN_TOKEN.to_owned()))
}

fn volume(n: u64) -> VolumeId {
    VolumeId::new(format!("vol-{n}")).expect("valid volume id")
}

fn host(n: u64) -> HostId {
    HostId::new(format!("node-{n}")).expect("valid host id")
}

fn op(n: u64) -> OperationId {
    OperationId::new(format!("op-{n}")).expect("valid operation id")
}

fn migration(n: u64) -> MigrationId {
    MigrationId::new(format!("mig-{n}")).expect("valid migration id")
}

fn content() -> RegistrationContent {
    RegistrationContent {
        lineage_uuids: vec!["0000000000000004".to_owned(), "0000000000000005".to_owned()],
        endpoints: vec![
            EndpointBacking {
                host_id: host(1),
                backing: "vg-near/vol-abc-00000001".to_owned(),
                volvisor_created: true,
            },
            EndpointBacking {
                host_id: host(2),
                backing: "vg-near/vol-abc-00000001".to_owned(),
                volvisor_created: false,
            },
        ],
        barrier: None,
    }
}

fn register_request(n: u64) -> RegisterRequest {
    RegisterRequest {
        protocol_version: WITNESS_PROTOCOL_VERSION,
        operation_id: op(n),
        content: content(),
    }
}

fn grant_request(n: u64, host_n: u64) -> GrantRequest {
    GrantRequest {
        protocol_version: WITNESS_PROTOCOL_VERSION,
        operation_id: op(n),
        host_id: host(host_n),
    }
}

fn record_barrier_request(n: u64, host_n: u64, epoch: WriterEpoch) -> RecordBarrierRequest {
    RecordBarrierRequest {
        protocol_version: WITNESS_PROTOCOL_VERSION,
        operation_id: op(n),
        host_id: host(host_n),
        epoch,
        attestation: BarrierAttestation {
            vm_paused_and_drained: true,
            data_path_suspended: true,
            peer_up_to_date: true,
        },
        migration_id: Some(migration(n)),
    }
}

/// A void of the barrier recorded by `record_barrier_request(n, ..)`:
/// the same migration scopes the void to that barrier, under a fresh
/// operation id (the record's id is already journaled).
fn void_barrier_request(
    n: u64,
    void_op: u64,
    host_n: u64,
    epoch: WriterEpoch,
) -> VoidBarrierRequest {
    VoidBarrierRequest {
        protocol_version: WITNESS_PROTOCOL_VERSION,
        operation_id: op(void_op),
        host_id: host(host_n),
        epoch,
        migration_id: Some(migration(n)),
    }
}

fn revoke_set_request(n: u64, host_n: u64, releases: Vec<BatchRelease>) -> RevokeSetRequest {
    RevokeSetRequest {
        protocol_version: WITNESS_PROTOCOL_VERSION,
        operation_id: op(n),
        host_id: host(host_n),
        migration_id: Some(migration(n)),
        releases,
    }
}

fn grant_set_request(n: u64, host_n: u64, volumes: Vec<VolumeId>) -> GrantSetRequest {
    GrantSetRequest {
        protocol_version: WITNESS_PROTOCOL_VERSION,
        operation_id: op(n),
        host_id: host(host_n),
        migration_id: Some(migration(n)),
        requests: volumes
            .into_iter()
            .map(|volume_id| BatchGrantVolume { volume_id })
            .collect(),
    }
}

fn release(volume_id: &VolumeId, epoch: WriterEpoch) -> BatchRelease {
    BatchRelease {
        volume_id: volume_id.clone(),
        epoch,
    }
}

/// Register + grant `volume` to host 1; returns the grant.
async fn granted_to_host_1(
    client: &HttpWitnessConnection,
    volume_id: &VolumeId,
    op_register: u64,
    op_grant: u64,
) -> volvisor_witness::proto::GrantResponse {
    client
        .register(volume_id, register_request(op_register))
        .await
        .expect("register");
    client
        .grant(volume_id, grant_request(op_grant, 1))
        .await
        .expect("grant")
}

#[tokio::test]
async fn auth_is_fail_closed_over_http() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(AtomicU64::new(1_000));
    let server = standard_witness(dir.path(), clock.clone()).await;

    // No token: refused (never permitted).
    let unauthenticated = client_for(&server, None);
    let err = unauthenticated
        .register(&volume(1), register_request(1))
        .await
        .expect_err("unauthenticated register refused");
    assert_eq!(err, WitnessError::Unauthorized);

    // Wrong token: refused.
    let wrong = client_for(&server, Some("wrong".to_owned()));
    let err = wrong
        .grant(&volume(1), grant_request(2, 1))
        .await
        .expect_err("wrong token refused");
    assert_eq!(err, WitnessError::Unauthorized);

    // A host credential works.
    host_client(&server, 1)
        .register(&volume(1), register_request(3))
        .await
        .expect("host-credential register");
}

#[tokio::test]
async fn tokenless_server_rejects_everything() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(AtomicU64::new(1_000));
    // Tokenless mode is the fail-closed default even on loopback (the
    // storage-daemon convention: the *binder* decides loopback-only).
    let server = spawn_witness(dir.path(), None, host_tokens(), clock.clone()).await;
    let client = client_for(&server, None);
    let err = client
        .register(&volume(1), register_request(1))
        .await
        .expect_err("tokenless server rejects");
    assert_eq!(err, WitnessError::Unauthorized);
}

// ------------------------------------------------------------ P4b W8

/// Plan §9 row 1: the W8 caller-identity binding over HTTP. A
/// holder-bound mutation from the wrong host's credential is refused
/// typed; the legacy shared token can inspect but every mutation is
/// refused with the v2 typed error.
#[tokio::test]
async fn w8_identity_binding_over_http() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(AtomicU64::new(1_000));
    let server = standard_witness(dir.path(), clock.clone()).await;
    let host_1 = host_client(&server, 1);
    let host_2 = host_client(&server, 2);
    let legacy = legacy_client(&server);

    let grant = granted_to_host_1(&host_1, &volume(1), 1, 2).await;
    assert_eq!(grant.epoch, WriterEpoch(1));

    // Self-revoke asserting node-1 while presenting node-2's
    // credential: the wrong host's credential is refused typed.
    let err = host_2
        .revoke(
            &volume(1),
            RevokeRequest {
                protocol_version: WITNESS_PROTOCOL_VERSION,
                operation_id: op(3),
                host_id: host(1),
                epoch: grant.epoch,
                authorization: None,
                power_off: None,
            },
        )
        .await
        .expect_err("wrong-host self-revoke refused");
    assert_eq!(err, WitnessError::IdentityRequired);

    // Record-barrier from the wrong host's credential: refused typed.
    let err = host_2
        .record_barrier(&volume(1), record_barrier_request(4, 1, grant.epoch))
        .await
        .expect_err("wrong-host record-barrier refused");
    assert_eq!(err, WitnessError::IdentityRequired);

    // Batches from the wrong host's credential: refused typed.
    let err = host_2
        .revoke_set(revoke_set_request(
            5,
            1,
            vec![release(&volume(1), grant.epoch)],
        ))
        .await
        .expect_err("wrong-host revoke-set refused");
    assert_eq!(err, WitnessError::IdentityRequired);

    // The legacy shared token is read-only on a v2 witness: every
    // mutation is the typed FORBIDDEN refusal (403, not 401 — the
    // token authenticates, the identity does not authorize).
    let err = legacy
        .grant(&volume(1), grant_request(6, 1))
        .await
        .expect_err("legacy grant refused");
    assert_eq!(err, WitnessError::IdentityRequired);
    let err = legacy
        .revoke(
            &volume(1),
            RevokeRequest {
                protocol_version: WITNESS_PROTOCOL_VERSION,
                operation_id: op(7),
                host_id: host(1),
                epoch: grant.epoch,
                authorization: None,
                power_off: None,
            },
        )
        .await
        .expect_err("legacy self-revoke refused");
    assert_eq!(err, WitnessError::IdentityRequired);
    let err = legacy
        .grant_set(grant_set_request(8, 1, vec![volume(1)]))
        .await
        .expect_err("legacy grant-set refused");
    assert_eq!(err, WitnessError::IdentityRequired);

    // Both the legacy and the host credential can read.
    let view = legacy.inspect(&volume(1)).await.expect("legacy inspect");
    assert_eq!(view.holder, Some(host(1)));
    let view = host_1.inspect(&volume(1)).await.expect("host inspect");
    assert_eq!(view.current_epoch, WriterEpoch(1));

    // The holder's own credential still mutates: the binding refuses
    // only mismatches, never the rightful holder.
    host_1
        .revoke(
            &volume(1),
            RevokeRequest {
                protocol_version: WITNESS_PROTOCOL_VERSION,
                operation_id: op(9),
                host_id: host(1),
                epoch: grant.epoch,
                authorization: None,
                power_off: None,
            },
        )
        .await
        .expect("rightful self-release");
}

// ------------------------------------------------------------ P4a rows

#[tokio::test]
async fn full_round_trip_over_http_with_client() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(AtomicU64::new(1_000));
    let server = standard_witness(dir.path(), clock.clone()).await;
    let client = host_client(&server, 1);
    let peer = host_client(&server, 2);

    client
        .register(&volume(1), register_request(1))
        .await
        .expect("register");

    let grant = client
        .grant(&volume(1), grant_request(2, 1))
        .await
        .expect("grant");
    assert_eq!(grant.epoch, WriterEpoch(1));
    assert_eq!(grant.lease_ttl_secs, 100);

    // Inspect reports the live lease with a duration-from-response.
    let view = client.inspect(&volume(1)).await.expect("inspect");
    assert_eq!(view.lease_remaining_secs, Some(100));
    assert_eq!(view.holder, Some(host(1)));

    // Renewal extends from the (advanced) clock.
    clock.store(1_050, Ordering::SeqCst);
    let renewed = client
        .renew(
            &volume(1),
            RenewRequest {
                protocol_version: WITNESS_PROTOCOL_VERSION,
                operation_id: op(3),
                host_id: host(1),
                epoch: grant.epoch,
                lease_id: grant.lease_id,
            },
        )
        .await
        .expect("renew");
    assert_eq!(renewed.remaining_secs, 100);

    // Self-release (detach path), then an immediate re-grant by the
    // other host: no fence wait after a self-release.
    clock.store(1_060, Ordering::SeqCst);
    client
        .revoke(
            &volume(1),
            RevokeRequest {
                protocol_version: WITNESS_PROTOCOL_VERSION,
                operation_id: op(4),
                host_id: host(1),
                epoch: grant.epoch,
                authorization: None,
                power_off: None,
            },
        )
        .await
        .expect("self-release");
    let second = peer
        .grant(&volume(1), grant_request(5, 2))
        .await
        .expect("grant after self-release");
    assert_eq!(second.epoch, WriterEpoch(2));
    assert_eq!(second.lease_id, LeaseId(2));

    // The fenced writer's stale renewal is rejected end-to-end with the
    // current epoch (W4 through the whole transport).
    let err = client
        .renew(
            &volume(1),
            RenewRequest {
                protocol_version: WITNESS_PROTOCOL_VERSION,
                operation_id: op(6),
                host_id: host(1),
                epoch: grant.epoch,
                lease_id: grant.lease_id,
            },
        )
        .await
        .expect_err("stale renewal rejected");
    assert_eq!(
        err,
        WitnessError::StaleEpoch {
            current_epoch: WriterEpoch(2)
        }
    );
}

#[tokio::test]
async fn fence_pending_round_trips_with_retry_after() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(AtomicU64::new(1_000));
    let server = standard_witness(dir.path(), clock.clone()).await;
    let client = host_client(&server, 1);
    let peer = host_client(&server, 2);

    client
        .register(&volume(1), register_request(1))
        .await
        .expect("register");
    let grant = client
        .grant(&volume(1), grant_request(2, 1))
        .await
        .expect("grant");
    // Lease end 1_100; fence window until 1_110. Host 2 tries at 1_105.
    clock.store(1_105, Ordering::SeqCst);
    let err = peer
        .grant(&volume(1), grant_request(3, 2))
        .await
        .expect_err("fence window holds");
    assert_eq!(
        err,
        WitnessError::FencePending {
            retry_after_secs: 5
        }
    );
    // Retry after the window: succeeds.
    clock.store(1_110, Ordering::SeqCst);
    let granted = peer
        .grant(&volume(1), grant_request(4, 2))
        .await
        .expect("grant after window");
    assert_eq!(granted.epoch, WriterEpoch(2));
    assert!(granted.fencing_proof.commit_index > grant.fencing_proof.commit_index);
}

#[tokio::test]
async fn protocol_version_mismatch_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(AtomicU64::new(1_000));
    let server = standard_witness(dir.path(), clock.clone()).await;
    let client = host_client(&server, 1);

    let mut request = register_request(1);
    request.protocol_version = WITNESS_PROTOCOL_VERSION + 1;
    let err = client
        .register(&volume(1), request)
        .await
        .expect_err("version mismatch refused");
    assert!(matches!(err, WitnessError::InvalidRequest(_)));

    // Unknown volume through the transport: typed not-found.
    let err = client
        .inspect(&volume(9))
        .await
        .expect_err("unknown volume");
    assert_eq!(err, WitnessError::UnknownVolume);
}

#[tokio::test]
async fn restart_preserves_authority_mid_traffic() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(AtomicU64::new(1_000));
    let server = standard_witness(dir.path(), clock.clone()).await;
    let client = host_client(&server, 1);
    client
        .register(&volume(1), register_request(1))
        .await
        .expect("register");
    let grant = client
        .grant(&volume(1), grant_request(2, 1))
        .await
        .expect("grant");

    // "Crash": drop the server (and with it the core holding the flock),
    // restart on the same journal directory.
    server.handle.abort();
    // Give the aborted task a moment to release the listener/lock.
    tokio::time::sleep(Duration::from_millis(50)).await;

    clock.store(1_050, Ordering::SeqCst);
    let server = standard_witness(dir.path(), clock.clone()).await;
    let client = host_client(&server, 1);

    // Epochs and lease survive; the lease is still live with the
    // remaining duration recomputed against the advanced clock.
    let view = client.inspect(&volume(1)).await.expect("inspect");
    assert_eq!(view.current_epoch, WriterEpoch(1));
    assert_eq!(view.lease_remaining_secs, Some(50));

    // A retry of the original grant's operation id replays byte-identically.
    let replayed = client
        .grant(&volume(1), grant_request(2, 1))
        .await
        .expect("idempotent grant replay after restart");
    assert_eq!(replayed, grant);
}

#[tokio::test]
async fn unreachable_witness_is_a_transport_signal() {
    // A connection to a closed port reports Unreachable — the signal a
    // writer uses to keep serving until its W5 local deadline, never a
    // typed witness refusal.
    let client = HttpWitnessConnection::new(
        "http://127.0.0.1:1",
        Some("secret-token".to_owned()),
        Duration::from_millis(500),
    );
    let err = client
        .inspect(&volume(1))
        .await
        .expect_err("unreachable witness");
    assert!(matches!(err, WitnessError::Unreachable(_)));
}

// ------------------------------------------------------------ P4b W9

/// Plan §9 row 2: the W9 barrier lifecycle. Recording is accepted from
/// the epoch holder while live; stale-epoch recording and wrong-host
/// recording are refused; byte-identical retries replay and differing
/// content conflicts; voiding works before retirement and is refused
/// after it; a second barrier for the same epoch appends.
#[tokio::test]
async fn w9_record_and_void_barriers_over_http() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(AtomicU64::new(1_000));
    let server = standard_witness(dir.path(), clock.clone()).await;
    let host_1 = host_client(&server, 1);
    let host_2 = host_client(&server, 2);

    let grant = granted_to_host_1(&host_1, &volume(1), 1, 2).await;

    // Accepted from the epoch holder while live: the witness stamps
    // the boundary commit index and the recording time.
    clock.store(1_020, Ordering::SeqCst);
    let first = host_1
        .record_barrier(&volume(1), record_barrier_request(3, 1, grant.epoch))
        .await
        .expect("record barrier");
    assert_eq!(first.barrier.holder, host(1));
    assert_eq!(first.barrier.epoch, grant.epoch);
    assert_eq!(first.barrier.recorded_at, 1_020);
    assert!(!first.barrier.voided);
    assert!(first.barrier.boundary_commit_index > grant.fencing_proof.commit_index);

    // Stale-epoch recording is refused.
    let err = host_1
        .record_barrier(&volume(1), record_barrier_request(4, 1, WriterEpoch(7)))
        .await
        .expect_err("stale-epoch barrier refused");
    assert_eq!(
        err,
        WitnessError::StaleEpoch {
            current_epoch: WriterEpoch(1)
        }
    );

    // A barrier recorded by a request asserting the holder while
    // presenting another host's credential: refused (W8).
    let err = host_2
        .record_barrier(&volume(1), record_barrier_request(5, 1, grant.epoch))
        .await
        .expect_err("wrong-host barrier refused");
    assert_eq!(err, WitnessError::IdentityRequired);

    // Byte-identical retry with the same operation id: replays.
    let replay = host_1
        .record_barrier(&volume(1), record_barrier_request(3, 1, grant.epoch))
        .await
        .expect("barrier retry replays");
    assert_eq!(replay, first);

    // The same operation id with different content: typed conflict.
    let mut conflicting = record_barrier_request(3, 1, grant.epoch);
    conflicting.attestation.peer_up_to_date = false;
    let err = host_1
        .record_barrier(&volume(1), conflicting)
        .await
        .expect_err("differing re-record conflicts");
    assert_eq!(err, WitnessError::IdempotencyConflict);

    // A second barrier for the same epoch is allowed (appends): the
    // classifier picks evidence, the witness records.
    let second = host_1
        .record_barrier(&volume(1), record_barrier_request(6, 1, grant.epoch))
        .await
        .expect("second barrier appends");
    assert_ne!(
        second.barrier.boundary_commit_index,
        first.barrier.boundary_commit_index
    );
    let view = host_1.inspect(&volume(1)).await.expect("inspect");
    assert_eq!(view.barriers.len(), 2);
    assert!(view.barriers.iter().all(|barrier| !barrier.voided));

    // Void before retirement: the migration-scoped void targets the
    // first barrier (op 3 carries migration 3, op 6 carries migration 6).
    let voided = host_1
        .void_barrier(&volume(1), void_barrier_request(3, 9, 1, grant.epoch))
        .await
        .expect("void before retirement");
    assert_eq!(
        voided.barrier.boundary_commit_index,
        first.barrier.boundary_commit_index
    );
    assert!(voided.barrier.voided);
    let view = host_1.inspect(&volume(1)).await.expect("inspect");
    assert_eq!(view.barriers.len(), 2);
    assert!(view.barriers[0].voided);
    assert!(!view.barriers[1].voided);

    // Retire the epoch (self-release, then a new grant), then void the
    // remaining barrier of the retired epoch: refused — evidence
    // hygiene, never retroactive tampering.
    clock.store(1_050, Ordering::SeqCst);
    host_1
        .revoke(
            &volume(1),
            RevokeRequest {
                protocol_version: WITNESS_PROTOCOL_VERSION,
                operation_id: op(7),
                host_id: host(1),
                epoch: grant.epoch,
                authorization: None,
                power_off: None,
            },
        )
        .await
        .expect("self-release");
    host_2
        .grant(&volume(1), grant_request(8, 2))
        .await
        .expect("new epoch");
    let err = host_1
        .void_barrier(&volume(1), void_barrier_request(6, 10, 1, grant.epoch))
        .await
        .expect_err("void after retirement refused");
    assert!(matches!(err, WitnessError::InvalidRequest(_)));

    // The retirement is exposed for ordering: epoch 1 retired at the
    // new grant's commit index, which postdates both barriers. (The
    // first grant had already implicitly retired the pre-authority
    // epoch 0 — that record is included too.)
    let view = host_1.inspect(&volume(1)).await.expect("inspect");
    assert_eq!(view.retirements.len(), 2);
    assert_eq!(view.retirements[1].epoch, WriterEpoch(1));
    assert!(view.retirements[1].commit_index > second.barrier.boundary_commit_index);
}

/// Crash/replay: barriers and retirements are fold-derived, so they
/// survive a witness restart on the same journal.
#[tokio::test]
async fn w9_barriers_and_retirements_survive_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(AtomicU64::new(1_000));
    let server = standard_witness(dir.path(), clock.clone()).await;
    let host_1 = host_client(&server, 1);

    let grant = granted_to_host_1(&host_1, &volume(1), 1, 2).await;
    host_1
        .record_barrier(&volume(1), record_barrier_request(3, 1, grant.epoch))
        .await
        .expect("record barrier");

    server.handle.abort();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let server = standard_witness(dir.path(), clock.clone()).await;
    let host_1 = host_client(&server, 1);
    let view = host_1.inspect(&volume(1)).await.expect("inspect");
    assert_eq!(view.barriers.len(), 1);
    assert_eq!(view.barriers[0].epoch, grant.epoch);
    assert!(!view.barriers[0].voided);
    assert_eq!(view.retirements.len(), 1);
    assert_eq!(view.retirements[0].epoch, WriterEpoch::pre_authority());
}

// ------------------------------------------------------------ P4b W10

/// Plan §9 row 3: batch mutations are all-or-nothing with one
/// commit-index bump per batch, and idempotent under retry.
#[tokio::test]
async fn w10_revoke_set_is_atomic_and_idempotent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(AtomicU64::new(1_000));
    let server = standard_witness(dir.path(), clock.clone()).await;
    let host_1 = host_client(&server, 1);
    let host_2 = host_client(&server, 2);

    let grant_1 = granted_to_host_1(&host_1, &volume(1), 1, 2).await;
    let grant_2 = granted_to_host_1(&host_1, &volume(2), 3, 4).await;
    // vol-1's own last commit (its grant); the batch must leave it
    // untouched when refused.
    let commit_before_vol_1 = host_1
        .inspect(&volume(1))
        .await
        .expect("inspect")
        .commit_index;
    // The global watermark through the last pre-batch mutation
    // (vol-2's grant) — the batch's single bump lands on this + 1.
    let commit_before_watermark = host_1
        .inspect(&volume(2))
        .await
        .expect("inspect")
        .commit_index;

    // One bad member (an unregistered volume) refuses the whole batch:
    // nothing journaled — vol-1's lease is untouched and its commit
    // index unchanged.
    let err = host_1
        .revoke_set(revoke_set_request(
            5,
            1,
            vec![
                release(&volume(1), grant_1.epoch),
                release(&volume(9), WriterEpoch(1)),
            ],
        ))
        .await
        .expect_err("one bad member refuses the batch");
    assert_eq!(err, WitnessError::UnknownVolume);
    let view = host_1.inspect(&volume(1)).await.expect("inspect");
    assert_eq!(view.lease_state, volvisor_types::LeaseState::Live);
    assert_eq!(view.commit_index, commit_before_vol_1);
    let view = host_1.inspect(&volume(2)).await.expect("inspect");
    assert_eq!(view.lease_state, volvisor_types::LeaseState::Live);

    // A member held by another host is not a self-release: the whole
    // batch is refused typed (the batch host presents its own
    // credential; vol-1 is held by node-1).
    let err = host_2
        .revoke_set(revoke_set_request(
            6,
            2,
            vec![release(&volume(1), grant_1.epoch)],
        ))
        .await
        .expect_err("another holder's lease refuses the batch");
    assert!(matches!(err, WitnessError::InvalidRequest(_)));

    // The good batch: both members self-release, one commit-index bump
    // for the set (every member's proof shares it).
    let outcome = host_1
        .revoke_set(revoke_set_request(
            7,
            1,
            vec![
                release(&volume(1), grant_1.epoch),
                release(&volume(2), grant_2.epoch),
            ],
        ))
        .await
        .expect("revoke-set");
    assert_eq!(outcome.releases.len(), 2);
    let shared_index = outcome.releases[0].fencing_proof.commit_index;
    assert_eq!(outcome.releases[1].fencing_proof.commit_index, shared_index);
    // ONE commit-index bump for the set (the global watermark moved by
    // exactly one, and every member's proof shares the new index).
    assert_eq!(shared_index, commit_before_watermark + 1);
    for (member, grant) in [(1, &grant_1), (2, &grant_2)] {
        let view = host_1.inspect(&volume(member)).await.expect("inspect");
        assert_eq!(view.lease_state, volvisor_types::LeaseState::Revoked);
        assert_eq!(view.commit_index, shared_index);
        assert_eq!(view.retirements.len(), 2);
        assert_eq!(view.retirements[1].epoch, grant.epoch);
        assert_eq!(view.retirements[1].commit_index, shared_index);
    }

    // A byte-identical retry with the same operation id replays the
    // recorded outcome.
    let replay = host_1
        .revoke_set(revoke_set_request(
            7,
            1,
            vec![
                release(&volume(1), grant_1.epoch),
                release(&volume(2), grant_2.epoch),
            ],
        ))
        .await
        .expect("batch retry replays");
    assert_eq!(replay, outcome);
}

/// Plan §9 row 3: `grant_set` is all-or-nothing, mints one epoch per
/// member with a single commit-index bump, retires lingering epochs
/// set-wide, and replays idempotently.
#[tokio::test]
async fn w10_grant_set_is_atomic_retires_and_idempotent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(AtomicU64::new(1_000));
    let server = standard_witness(dir.path(), clock.clone()).await;
    let host_1 = host_client(&server, 1);
    let host_2 = host_client(&server, 2);

    let grant_1 = granted_to_host_1(&host_1, &volume(1), 1, 2).await;
    let grant_2 = granted_to_host_1(&host_1, &volume(2), 3, 4).await;
    // Self-release both so the destination may grant immediately (W7
    // waived — the set-wide D4 discipline).
    host_1
        .revoke_set(revoke_set_request(
            5,
            1,
            vec![
                release(&volume(1), grant_1.epoch),
                release(&volume(2), grant_2.epoch),
            ],
        ))
        .await
        .expect("revoke-set");
    let commit_before = host_2
        .inspect(&volume(1))
        .await
        .expect("inspect")
        .commit_index;

    // All-or-nothing: an unregistered member refuses the batch with
    // nothing journaled.
    let err = host_2
        .grant_set(grant_set_request(6, 2, vec![volume(1), volume(9)]))
        .await
        .expect_err("one bad member refuses the batch");
    assert_eq!(err, WitnessError::UnknownVolume);
    let view = host_2.inspect(&volume(1)).await.expect("inspect");
    assert_eq!(view.commit_index, commit_before);
    assert_eq!(view.lease_state, volvisor_types::LeaseState::Revoked);

    // The good batch: new epochs per member, one commit-index bump for
    // the set, retirements recorded for the retired epochs.
    let outcome = host_2
        .grant_set(grant_set_request(7, 2, vec![volume(1), volume(2)]))
        .await
        .expect("grant-set");
    assert_eq!(outcome.grants.len(), 2);
    let shared_index = outcome.grants[0].fencing_proof.commit_index;
    for (member, grant) in outcome.grants.iter().enumerate() {
        let previous_epoch = if member == 0 {
            grant_1.epoch
        } else {
            grant_2.epoch
        };
        assert_eq!(grant.epoch, WriterEpoch(previous_epoch.0 + 1));
        assert_eq!(grant.lease_ttl_secs, 100);
        assert_eq!(grant.fencing_proof.commit_index, shared_index);
        let view = host_2.inspect(&grant.volume_id).await.expect("inspect");
        assert_eq!(view.current_epoch, grant.epoch);
        assert_eq!(view.holder, Some(host(2)));
        assert_eq!(view.lease_state, volvisor_types::LeaseState::Live);
        assert_eq!(view.commit_index, shared_index);
    }
    assert_eq!(shared_index, commit_before + 1);

    // A byte-identical retry replays the recorded outcome.
    let replay = host_2
        .grant_set(grant_set_request(7, 2, vec![volume(1), volume(2)]))
        .await
        .expect("batch retry replays");
    assert_eq!(replay, outcome);

    // All-or-nothing on W1: a member with a live lease (now held by
    // host 2 itself — re-granting while live is a client bug refused
    // the same way) refuses the batch with nothing journaled.
    let commit_after = host_2
        .inspect(&volume(1))
        .await
        .expect("inspect")
        .commit_index;
    let err = host_2
        .grant_set(grant_set_request(8, 2, vec![volume(1), volume(2)]))
        .await
        .expect_err("live member lease refuses the batch");
    assert_eq!(
        err,
        WitnessError::LeaseHeld {
            current_epoch: WriterEpoch(2)
        }
    );
    assert_eq!(
        host_2
            .inspect(&volume(1))
            .await
            .expect("inspect")
            .commit_index,
        commit_after
    );
}

/// Plan §9 row 3 (the lingering-epoch case): a `grant-set` after the
/// previous leases expired past the fence window retires the lingering
/// epochs set-wide — new epochs, retirements populated.
#[tokio::test]
async fn w10_grant_set_retires_lingering_epochs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(AtomicU64::new(1_000));
    let server = standard_witness(dir.path(), clock.clone()).await;
    let host_1 = host_client(&server, 1);
    let host_2 = host_client(&server, 2);

    let grant_1 = granted_to_host_1(&host_1, &volume(1), 1, 2).await;
    let _grant_2 = granted_to_host_1(&host_1, &volume(2), 3, 4).await;

    // Let both leases expire past the fence window (end 1_100, window
    // until 1_110): the epochs linger, unfenced-by-time.
    clock.store(1_110, Ordering::SeqCst);
    let outcome = host_2
        .grant_set(grant_set_request(5, 2, vec![volume(1), volume(2)]))
        .await
        .expect("grant-set after the fence window");
    assert_eq!(outcome.grants.len(), 2);
    for grant in &outcome.grants {
        assert_eq!(grant.epoch, WriterEpoch(2));
        assert_eq!(grant.fencing_proof.retired_epoch, WriterEpoch(1));
        let view = host_2.inspect(&grant.volume_id).await.expect("inspect");
        // The lingering epoch 1 is retired at the batch's commit index.
        assert_eq!(view.retirements.len(), 2);
        assert_eq!(view.retirements[1].epoch, grant_1.epoch);
        assert_eq!(
            view.retirements[1].commit_index,
            grant.fencing_proof.commit_index
        );
    }
}
