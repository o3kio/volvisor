//! Shared API server state.

use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use volvisor_handoff::MigrationSurface;
use volvisor_journal::Journal;
use volvisor_provider::{
    AdminSurface, AdoptionSurface, GrowNotifier, HandoffSurface, LendingSurface, VolumeProvider,
};

use crate::auth::AuthPolicy;
use crate::metrics::{JournalSnapshot, Metrics};
use crate::peer::PeerRouteContext;

/// Bound on the readiness witness transport probe.
const WITNESS_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a witness probe result is reused before another TCP connect.
///
/// `/readyz` is public and `/metrics` is scraped frequently; without this
/// cache every call would open a fresh outbound connection to the witness.
const WITNESS_PROBE_CACHE: Duration = Duration::from_secs(1);

/// Process-local readiness state.
///
/// `startup_complete` is set by [`AppState::mark_ready`] only after the
/// daemon has opened/replayed the journal, constructed and reconciled the
/// provider, and bound its listener. The witness probe target is present only
/// when the daemon was configured with a witness URL; absent means
/// `not_probed`, which is reported honestly rather than treated as either
/// reachable or unreachable.
#[derive(Debug)]
struct Readiness {
    startup_complete: AtomicBool,
    /// Resolved once at startup; a DNS change after start is not re-resolved
    /// by the readiness probe (see [`AppState::with_witness_probe`]).
    witness_probe: Option<SocketAddr>,
    /// Serializes witness probes so a burst of `/readyz` calls opens at most
    /// one outbound connection at a time.
    probe_gate: tokio::sync::Mutex<()>,
    /// Last probe result and when it was observed.
    probe_cache: std::sync::Mutex<Option<(Instant, bool)>>,
}

impl Default for Readiness {
    fn default() -> Self {
        Self {
            startup_complete: AtomicBool::new(false),
            witness_probe: None,
            probe_gate: tokio::sync::Mutex::const_new(()),
            probe_cache: std::sync::Mutex::new(None),
        }
    }
}

/// Cheaply cloneable handle to the API server state.
pub type SharedState = Arc<AppState>;

/// State shared by every endpoint: the storage backend, the optional
/// privileged admin surface, the durable intent journal, metrics and the
/// optional admin token.
///
/// The journal mutex is a [`std::sync::Mutex`] because P0 journal appends are
/// synchronous, fsynced filesystem writes. Critical sections are short and
/// are **never** held across an `.await`: every journal interaction acquires
/// the lock, performs the append or lookup, and drops the guard before any
/// asynchronous provider call (see the `ops` module for the single execution
/// pipeline that owns this ordering).
pub struct AppState {
    /// Storage backend driving every operation.
    pub(crate) provider: Arc<dyn VolumeProvider>,
    /// Privileged device-enrollment surface, when the provider implements
    /// one. `None` serves `404` on the `/v2/admin` routes.
    pub(crate) admin: Option<Arc<dyn AdminSurface>>,
    /// Device-lending surface (P9-A, the rook-cell contract's lending
    /// operations), when the provider implements one over durable
    /// ownership records. `None` serves the typed 404 on the lending
    /// routes.
    pub(crate) lending: Option<Arc<dyn LendingSurface>>,
    /// Nearline adopt-and-promote surface (P4a), when the provider
    /// implements one. `None` serves the typed 404 on the adopt route.
    pub(crate) adoption: Option<Arc<dyn AdoptionSurface>>,
    /// Coordinated-handoff surface (P4b stage B2), when the provider
    /// implements one. `None` serves the typed 404 on the mobility
    /// routes (`check-mobility` and the whole peer surface).
    pub(crate) handoff: Option<Arc<dyn HandoffSurface>>,
    /// Consumer-facing mobility surface (P4b stage B2): the daemon's
    /// coordinator wrapper behind the five `/v2/migrations` routes.
    /// `None` serves the typed 404 on those routes.
    pub(crate) migration: Option<Arc<dyn MigrationSurface>>,
    /// Grow-notification seam (P6-B, ADR-0006 first slice part 1):
    /// the engine the grow operation composes inside the journal's
    /// execute closure. `None` — every provider without a VMM
    /// integration — keeps the provider's own placeholder status
    /// (see [`AppState::with_grow_notifier`]).
    pub(crate) grow_notifier: Option<Arc<dyn GrowNotifier>>,
    /// The destination-side context of the internal peer routes (P4b
    /// stage B2): the witness connection, the VMM controller, the
    /// provider surfaces and the target-preparation store. `None`
    /// serves the typed 404 on `/v2/internal/peer/*` (this daemon is
    /// not migration-enabled as a destination).
    pub(crate) peer_ctx: Option<Arc<PeerRouteContext>>,
    /// The daemon-to-daemon credential guarding the internal peer
    /// routes — deliberately distinct from both the admin token and
    /// the witness credentials (plan §6). `None` fails closed: the
    /// peer routes are rejected with `401` (a peer surface without
    /// its own credential must not be callable).
    pub(crate) peer_token: Option<String>,
    /// Durable intent journal (idempotency registry + journal-before-mutate).
    pub(crate) journal: std::sync::Mutex<Journal>,
    /// Prometheus-format metrics served on `/metrics`.
    pub(crate) metrics: Arc<Metrics>,
    /// Startup/readiness state and the optional witness probe target.
    readiness: Readiness,
    /// Scoped credential policy guarding the API surface. Built from the
    /// legacy `admin_token` and/or the named `[credentials]` entries; an
    /// empty policy fails closed for mutations/admin (the tokenless mode is
    /// loopback-only dev/test).
    pub(crate) auth: AuthPolicy,
    /// The journal-append crash hook (P5 plan §3.1), when the
    /// constructing test rig attached one. `None` — every production
    /// path — is fully inert: the pipeline consults nothing. Doc-gated
    /// trust class: no route, config or input can set it, only
    /// [`AppState::with_crash_hooks`], which only the campaign rig
    /// calls (see the `crash` module docs).
    pub(crate) crash: Option<Arc<crate::crash::CrashHooks>>,
}

impl AppState {
    /// Build the server state around an opened journal, a provider and —
    /// when the provider exposes one – its privileged admin surface.
    #[must_use]
    #[allow(clippy::new_without_default)] // a Journal needs a directory; no sane Default exists
    pub fn new(
        provider: Arc<dyn VolumeProvider>,
        admin: Option<Arc<dyn AdminSurface>>,
        journal: Journal,
        admin_token: Option<String>,
    ) -> Self {
        let metrics = Arc::new(Metrics::new());
        // `AppState::new` is reached only after the caller opened/replayed the
        // journal and constructed the provider (whose constructor performs
        // its startup reconcile). Record that real boundary as the startup
        // reconcile sample; `mark_ready` still gates readiness on the daemon
        // reaching the serving path.
        metrics.record_reconcile_complete("startup");
        Self {
            provider,
            admin,
            lending: None,
            adoption: None,
            handoff: None,
            migration: None,
            grow_notifier: None,
            peer_ctx: None,
            peer_token: None,
            journal: std::sync::Mutex::new(journal),
            metrics,
            readiness: Readiness::default(),
            auth: AuthPolicy::new(admin_token),
            crash: None,
        }
    }

    /// Attach the real witness transport-probe target used by `/readyz` and
    /// the `witness_reachable` gauge. `None` leaves witness reachability
    /// `not_probed` rather than inventing a value.
    ///
    /// The address is the one resolved from the configured witness URL at
    /// startup and is not re-resolved by the probe. A DNS change after start
    /// therefore requires a daemon restart before readiness reflects it; the
    /// driver's own connections re-resolve per connect.
    #[must_use]
    pub fn with_witness_probe(mut self, address: SocketAddr) -> Self {
        self.readiness.witness_probe = Some(address);
        self
    }

    /// A cheap handle to the in-process metrics, for background tasks that
    /// record real reconcile passes.
    #[must_use]
    pub fn metrics_handle(&self) -> Arc<Metrics> {
        Arc::clone(&self.metrics)
    }

    /// Mark startup complete: journal open/replayed, provider
    /// constructed/reconciled, listener bound. `/readyz` answers not-ready
    /// until this is called.
    pub fn mark_ready(&self) {
        self.readiness
            .startup_complete
            .store(true, Ordering::Release);
    }

    /// Whether the daemon has reached its serving path.
    #[must_use]
    pub fn startup_complete(&self) -> bool {
        self.readiness.startup_complete.load(Ordering::Acquire)
    }

    /// Probe the configured witness by TCP transport reachability, bounded
    /// and single-flight. `None` means no probe is configured (`not_probed`);
    /// `Some(false)` is a failed probe. Results are reused for one second so
    /// a public `/readyz` flood cannot open one outbound connection per
    /// request. The gauge is updated with the same observation.
    pub(crate) async fn probe_witness(&self) -> Option<bool> {
        let address = self.readiness.witness_probe?;

        // Serialize probes. Callers that waited re-check the cache below, so
        // a concurrent burst resolves to one connection and one observation.
        let _gate = self.readiness.probe_gate.lock().await;
        if let Some((observed_at, reachable)) = *self
            .readiness
            .probe_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            if observed_at.elapsed() < WITNESS_PROBE_CACHE {
                self.metrics.set_witness_reachable(Some(reachable));
                return Some(reachable);
            }
        }

        let reachable = tokio::task::spawn_blocking(move || {
            TcpStream::connect_timeout(&address, WITNESS_PROBE_TIMEOUT).is_ok()
        })
        .await
        .unwrap_or(false);
        *self
            .readiness
            .probe_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((Instant::now(), reachable));
        self.metrics.set_witness_reachable(Some(reachable));
        Some(reachable)
    }

    /// A live journal sample for `/metrics`; a poisoned mutex is reported as
    /// a closed journal rather than a fabricated record count.
    pub(crate) fn journal_snapshot(&self) -> JournalSnapshot {
        match self.journal.lock() {
            Ok(journal) => JournalSnapshot {
                records: Some(journal.record_count()),
                log_bytes: std::fs::metadata(journal.log_path())
                    .ok()
                    .map(|metadata| metadata.len()),
                compactions: Some(journal.compactions()),
                compaction_failures: Some(journal.compaction_failures()),
                retained_operations: Some(
                    u64::try_from(journal.retained_operations()).unwrap_or(u64::MAX),
                ),
                tombstones: Some(u64::try_from(journal.tombstone_count()).unwrap_or(u64::MAX)),
                open: true,
            },
            Err(_) => JournalSnapshot {
                records: None,
                log_bytes: None,
                compactions: None,
                compaction_failures: None,
                retained_operations: None,
                tombstones: None,
                open: false,
            },
        }
    }

    /// Attach scoped named credentials. When the list is non-empty, read
    /// routes (volume metadata, capabilities and `/metrics`) also require a
    /// credential; the legacy `admin_token` behavior is unchanged.
    #[must_use]
    pub fn with_credentials(mut self, credentials: Vec<crate::auth::Credential>) -> Self {
        self.auth = self.auth.with_credentials(credentials);
        self
    }

    /// Attach the journal-append crash hook (P5 plan §3.1). **Test
    /// rig only** — the doc-gated trust class (see the `crash` module
    /// docs): the hook is inert until its armed table is set, and
    /// only the constructing rig can arm it; no route, request or
    /// config path reaches it. A state built without this builder
    /// never consults the crash hook at all.
    #[must_use]
    pub fn with_crash_hooks(mut self, crash: Arc<crate::crash::CrashHooks>) -> Self {
        self.crash = Some(crash);
        self
    }

    /// Attach the device-lending surface (P9-A, the rook-cell
    /// contract's lending operations over claimed devices): the
    /// provider must also be the admin provider of this state — the
    /// lending routes operate on the same durable ownership records
    /// the claim/release routes drive. A state built without this
    /// builder serves the typed 404 on the lending routes (the
    /// optional-surface pattern).
    #[must_use]
    pub fn with_lending(mut self, lending: Arc<dyn LendingSurface>) -> Self {
        self.lending = Some(lending);
        self
    }

    /// Attach the nearline adopt-and-promote surface (P4a plan §6): the
    /// provider must also be the volume provider of this state — the
    /// adopt route operates on the same engine that serves the volume
    /// operations.
    #[must_use]
    pub fn with_adoption(mut self, adoption: Arc<dyn AdoptionSurface>) -> Self {
        self.adoption = Some(adoption);
        self
    }

    /// Attach the provider's coordinated-handoff surface (P4b plan §6,
    /// stage B2): `check-mobility` reads it, and the destination-side
    /// peer routes verify through it. The provider must also be the
    /// volume provider of this state.
    #[must_use]
    pub fn with_handoff(mut self, handoff: Arc<dyn HandoffSurface>) -> Self {
        self.handoff = Some(handoff);
        self
    }

    /// Attach the consumer-facing mobility surface (P4b plan §6, stage
    /// B2): the daemon's coordinator wrapper the five `/v2/migrations`
    /// routes drive.
    #[must_use]
    pub fn with_migration(mut self, migration: Arc<dyn MigrationSurface>) -> Self {
        self.migration = Some(migration);
        self
    }

    /// Attach the grow-notification seam (P6-B, ADR-0006 first slice
    /// part 1): the engine the grow operation composes **inside**
    /// the journal's execute closure, after the provider resized the
    /// backing, to drive the VMM capacity notification and report the
    /// honest `guest_notification_status` (the journaled outcome
    /// carries it and replays byte-compatibly). A state built
    /// without this builder keeps the provider's own placeholder
    /// status — the honest answer for providers with no VMM
    /// integration (the fake, ceph, drbd).
    #[must_use]
    pub fn with_grow_notifier(mut self, notifier: Arc<dyn GrowNotifier>) -> Self {
        self.grow_notifier = Some(notifier);
        self
    }

    /// Attach the internal peer routes (P4b plan §6, stage B2): the
    /// destination-side context plus the daemon-to-daemon credential
    /// that guards them. A `None` token fails closed (the routes are
    /// served, but every call is rejected `401` — a peer surface
    /// without its own credential must not be callable).
    #[must_use]
    pub fn with_peer_routes(
        mut self,
        token: Option<String>,
        context: Arc<PeerRouteContext>,
    ) -> Self {
        self.peer_token = token;
        self.peer_ctx = Some(context);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use volvisor_provider::fake::FakeProvider;

    fn state_with_probe(address: SocketAddr) -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temporary journal directory");
        let journal = Journal::open(dir.path()).expect("journal open");
        let provider: Arc<dyn VolumeProvider> = Arc::new(FakeProvider::new());
        let state = AppState::new(provider, None, journal, None).with_witness_probe(address);
        (state, dir)
    }

    #[tokio::test]
    async fn witness_probe_result_is_cached_for_one_second() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe target");
        let address = listener.local_addr().expect("probe address");
        let (state, _dir) = state_with_probe(address);

        assert_eq!(state.probe_witness().await, Some(true));
        // The cache must answer without a second outbound connect, so it
        // stays `true` even after the listener is gone.
        drop(listener);
        assert_eq!(
            state.probe_witness().await,
            Some(true),
            "a probe within the cache window must reuse the observation"
        );

        tokio::time::sleep(WITNESS_PROBE_CACHE + Duration::from_millis(50)).await;
        assert_eq!(
            state.probe_witness().await,
            Some(false),
            "an expired cache must perform a fresh probe"
        );
    }

    #[tokio::test]
    async fn no_configured_probe_is_not_probed() {
        let dir = tempfile::tempdir().expect("temporary journal directory");
        let journal = Journal::open(dir.path()).expect("journal open");
        let provider: Arc<dyn VolumeProvider> = Arc::new(FakeProvider::new());
        let state = AppState::new(provider, None, journal, None);
        assert_eq!(
            state.probe_witness().await,
            None,
            "no configured target must report not_probed"
        );
    }
}
