//! Shared fixtures for the `volvisor-drbd` integration tests: the
//! extracted [`volvisor_drbd_testkit`] crate (the simulated DRBD + LVM
//! world, the seeding helpers and the conformance-kit adapter), shared
//! with the daemon-level end-to-end tests — plus the loopback
//! witness-server fixture the writer-authority and handoff kits share:
//! a [`Server`] whose [`Server::stop`] is the deterministic
//! unreachability gate (a stop that PROVES no request can complete,
//! never an asynchronous-close race).
//!
//! Test-kit code: `expect`/`unwrap` are allowed here by convention.

// The module is compiled into every `volvisor-drbd` test binary, but
// only the witness-driving ones (authority, handoff) use the server
// fixture — its items would trip `dead_code` in the others.
#![allow(dead_code)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use volvisor_witness::registry::{WitnessCore, WitnessCoreConfig};
use volvisor_witness::server::{WitnessServerState, router};

pub use volvisor_drbd_testkit::*;

/// The bound on [`Server::stop`]'s drain: hyper 1.x graceful shutdown
/// has no internal deadline, so a wedged in-flight connection would
/// otherwise hang the stop forever. Both kits await every request
/// before they stop, so the drain only has to close idle keep-alive
/// connections — milliseconds — and five seconds stays generous even
/// under parallel-suite load; expiring it is a kit failure (see
/// [`DrainTimedOut`]), never a pass.
const DRAIN_BOUND: Duration = Duration::from_secs(5);

/// The per-kit witness credentials [`spawn_witness`] registers.
pub struct WitnessTokens<'a> {
    /// The legacy shared token: the v2 read-only credential the
    /// kit's inspector client presents.
    pub shared: &'a str,
    /// [`NODE`](volvisor_drbd_testkit::NODE)'s W8 mutating credential.
    pub node: &'a str,
    /// [`PEER_NODE`](volvisor_drbd_testkit::PEER_NODE)'s W8 mutating
    /// credential.
    pub peer: &'a str,
}

/// [`Server::stop`]'s error: the graceful-shutdown drain did not
/// complete within [`DRAIN_BOUND`] — a connection task is wedged (an
/// in-flight request never finished), so the witness is NOT provably
/// unreachable. A kit failure the caller must fail the test on,
/// never a pass.
pub struct DrainTimedOut {
    /// The bound that expired.
    bound: Duration,
}

impl std::fmt::Debug for DrainTimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the witness drain did not complete within {:?}: a connection task is \
             wedged (an in-flight request never finished) — the stop did NOT prove \
             unreachability; a kit failure, never a pass",
            self.bound
        )
    }
}

/// The loopback witness server: the serve task plus the
/// graceful-shutdown trigger that makes [`Server::stop`] a
/// deterministic unreachability gate (see its docs).
pub struct Server {
    /// The server's loopback address (the kit's client builders
    /// construct their connections against it).
    pub addr: SocketAddr,
    /// The shutdown trigger: dropped by [`Server::stop`] to start
    /// the drain. `None` once stopped.
    shutdown: Option<tokio::sync::watch::Sender<()>>,
    /// The serve task: taken and awaited by [`Server::stop`] (the
    /// drain barrier). `None` once stopped.
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl Server {
    /// Make the witness deterministically unreachable, and PROVE it:
    /// drop the shutdown trigger, then await the serve future to
    /// completion. Axum's graceful shutdown gives that completion an
    /// exact meaning — the serve future returns only after the
    /// listener is dropped (new connections are refused) AND every
    /// already-accepted connection's task has exited (in-flight
    /// requests drained; no idle keep-alive connection is left
    /// serviceable). After this returns, no request can complete
    /// against the server, whatever the client does with its pooled
    /// connections.
    ///
    /// The old shape — `handle.abort()` — raced exactly that: the
    /// abort closes the lingering connections asynchronously (the
    /// serve future's drop closes the signal channel; each
    /// connection task then gracefully shuts down whenever it is
    /// next polled), so a request dispatched right after the abort
    /// over the client's pooled keep-alive connection could still be
    /// read and processed before the close landed — a renewal
    /// completing against the "unreachable" witness (the recorded
    /// ~1-in-20 full-suite flake in
    /// `an_unreachable_witness_defers_renewal_until_the_deadline`;
    /// the probe evidence: the renewal is dispatched over the
    /// lingering connection in most runs — a `SendRequest` transport
    /// error, not `Connect` — and whether the close or the request
    /// wins is a scheduling race load can flip).
    ///
    /// The drain is bounded (hyper 1.x graceful shutdown has no
    /// internal deadline, so a wedged in-flight connection would
    /// otherwise hang this forever): if it does not complete within
    /// [`DRAIN_BOUND`] the stop returns [`DrainTimedOut`] — a kit
    /// failure (the drain did not complete; unreachability is NOT
    /// proven), never a pass.
    ///
    /// Idempotent: a second call is a no-op (and still `Ok`).
    ///
    /// # Errors
    ///
    /// [`DrainTimedOut`] when the drain did not complete within
    /// [`DRAIN_BOUND`] — a connection task is wedged.
    pub async fn stop(&mut self) -> Result<(), DrainTimedOut> {
        drop(self.shutdown.take());
        if let Some(handle) = self.handle.take() {
            tokio::time::timeout(DRAIN_BOUND, handle)
                .await
                .map_err(|_| DrainTimedOut { bound: DRAIN_BOUND })?
                .expect("the witness serve task does not panic");
        }
        Ok(())
    }
}

/// Spawn the loopback witness server the writer-authority and
/// handoff kits drive: a real journal-backed [`WitnessCore`] under
/// `dir` (deterministic knobs: ttl 100 s — the value both kits'
/// `TTL` const pins — grace 5 s, budget 5 s, so the W7 wait ends
/// 10 s past a lease's recorded end), served by real axum HTTP on
/// an ephemeral loopback port, with the injected `clock` (lease
/// expiry, fence windows) and the kit's `tokens`. Stopping it
/// through [`Server::stop`] is the deterministic unreachability
/// gate.
pub async fn spawn_witness(dir: &Path, clock: Arc<AtomicU64>, tokens: WitnessTokens<'_>) -> Server {
    let core = WitnessCore::open(
        dir,
        WitnessCoreConfig {
            lease_ttl_secs: 100,
            lease_grace_secs: 5,
            suspend_budget_secs: 5,
        },
    )
    .expect("witness core opens");
    let mut host_tokens = std::collections::BTreeMap::new();
    host_tokens.insert(NODE.to_owned(), tokens.node.to_owned());
    host_tokens.insert(PEER_NODE.to_owned(), tokens.peer.to_owned());
    let state = Arc::new(WitnessServerState::with_clock(
        core,
        Some(tokens.shared.to_owned()),
        host_tokens,
        Arc::new(move || clock.load(Ordering::SeqCst)),
    ));
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    // Graceful shutdown over a watch trigger: dropping the sender
    // (`Server::stop`) completes the signal future, the serve loop
    // stops accepting, tells every connection task to drain, and
    // waits for all of them — the drain barrier `stop` awaits.
    let (shutdown, mut shutdown_rx) = tokio::sync::watch::channel(());
    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.changed().await;
            })
            .await
            .expect("server serves");
    });
    Server {
        addr,
        shutdown: Some(shutdown),
        handle: Some(handle),
    }
}
