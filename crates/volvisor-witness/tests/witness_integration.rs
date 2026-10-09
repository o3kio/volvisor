//! Loopback integration tests for the witness HTTP surface and client.
//!
//! Real axum server on an ephemeral port, real hyper client, real
//! journal-backed registry in a temp directory, deterministic injected
//! clock — no transport shortcuts. Covers the P4a plan §8 "witness
//! server" rows: auth fail-closed, protocol-version refusal, full
//! operation round-trips, `FENCE_PENDING` retry-after recovery through
//! the client, and journal-backed restart mid-traffic.
//!
//! Test-kit code: `expect`/`unwrap` are allowed here by convention.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use volvisor_types::{EndpointBacking, HostId, LeaseId, OperationId, VolumeId, WriterEpoch};
use volvisor_witness::client::{HttpWitnessConnection, WitnessConnection};
use volvisor_witness::proto::{
    GrantRequest, RegisterRequest, RegistrationContent, RenewRequest, RevokeRequest,
    WITNESS_PROTOCOL_VERSION, WitnessError,
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

struct Server {
    addr: SocketAddr,
    handle: tokio::task::JoinHandle<()>,
}

async fn spawn_witness(dir: &Path, token: Option<String>, clock: Arc<AtomicU64>) -> Server {
    let core = WitnessCore::open(dir, test_config()).expect("witness core opens");
    let state = Arc::new(WitnessServerState::with_clock(
        core,
        token,
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

fn client_for(server: &Server, token: Option<String>) -> HttpWitnessConnection {
    HttpWitnessConnection::new(
        format!("http://{}", server.addr),
        token,
        Duration::from_secs(5),
    )
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

#[tokio::test]
async fn auth_is_fail_closed_over_http() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(AtomicU64::new(1_000));
    let server = spawn_witness(dir.path(), Some("secret-token".to_owned()), clock.clone()).await;

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

    // Correct token: works.
    let authenticated = client_for(&server, Some("secret-token".to_owned()));
    authenticated
        .register(&volume(1), register_request(3))
        .await
        .expect("authenticated register");
}

#[tokio::test]
async fn tokenless_server_rejects_everything() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(AtomicU64::new(1_000));
    // Tokenless mode is the fail-closed default even on loopback (the
    // storage-daemon convention: the *binder* decides loopback-only).
    let server = spawn_witness(dir.path(), None, clock.clone()).await;
    let client = client_for(&server, None);
    let err = client
        .register(&volume(1), register_request(1))
        .await
        .expect_err("tokenless server rejects");
    assert_eq!(err, WitnessError::Unauthorized);
}

#[tokio::test]
async fn full_round_trip_over_http_with_client() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(AtomicU64::new(1_000));
    let server = spawn_witness(dir.path(), Some("secret-token".to_owned()), clock.clone()).await;
    let client = client_for(&server, Some("secret-token".to_owned()));

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
    let second = client
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
    let server = spawn_witness(dir.path(), Some("secret-token".to_owned()), clock.clone()).await;
    let client = client_for(&server, Some("secret-token".to_owned()));

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
    let err = client
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
    let granted = client
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
    let server = spawn_witness(dir.path(), Some("secret-token".to_owned()), clock.clone()).await;
    let client = client_for(&server, Some("secret-token".to_owned()));

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
    let server = spawn_witness(dir.path(), Some("secret-token".to_owned()), clock.clone()).await;
    let client = client_for(&server, Some("secret-token".to_owned()));
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
    let server = spawn_witness(dir.path(), Some("secret-token".to_owned()), clock.clone()).await;
    let client = client_for(&server, Some("secret-token".to_owned()));

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
