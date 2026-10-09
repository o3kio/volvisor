//! Shared API server state.

use std::sync::Arc;

use volvisor_journal::Journal;
use volvisor_provider::{AdminSurface, AdoptionSurface, VolumeProvider};

use crate::metrics::Metrics;

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
    /// Nearline adopt-and-promote surface (P4a), when the provider
    /// implements one. `None` serves the typed 404 on the adopt route.
    pub(crate) adoption: Option<Arc<dyn AdoptionSurface>>,
    /// Durable intent journal (idempotency registry + journal-before-mutate).
    pub(crate) journal: std::sync::Mutex<Journal>,
    /// Prometheus-format counters served on `/metrics`.
    pub(crate) metrics: Arc<Metrics>,
    /// Admin bearer token guarding every mutating endpoint and the whole
    /// `/v2/admin` surface. `None` fails closed: mutations are rejected
    /// with `401` (the tokenless mode is loopback-only dev/test; the daemon
    /// refuses non-loopback binds without a token).
    pub(crate) admin_token: Option<String>,
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
            adoption: None,
            journal: std::sync::Mutex::new(journal),
            metrics: Arc::new(Metrics::new()),
            admin_token,
        }
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
}
