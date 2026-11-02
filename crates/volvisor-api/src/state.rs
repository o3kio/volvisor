//! Shared API server state.

use std::sync::Arc;

use volvisor_handoff::MigrationSurface;
use volvisor_journal::Journal;
use volvisor_provider::{
    AdminSurface, AdoptionSurface, GrowNotifier, HandoffSurface, LendingSurface, VolumeProvider,
};

use crate::auth::AuthPolicy;
use crate::metrics::Metrics;
use crate::peer::PeerRouteContext;

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
    /// Prometheus-format counters served on `/metrics`.
    pub(crate) metrics: Arc<Metrics>,
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
            metrics: Arc::new(Metrics::new()),
            auth: AuthPolicy::new(admin_token),
            crash: None,
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
