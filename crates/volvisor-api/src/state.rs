//! Shared API server state.

use std::sync::Arc;

use volvisor_journal::Journal;
use volvisor_provider::VolumeProvider;

use crate::metrics::Metrics;

/// Cheaply cloneable handle to the API server state.
pub type SharedState = Arc<AppState>;

/// State shared by every endpoint: the storage backend, the durable intent
/// journal, metrics and the optional admin token.
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
    /// Durable intent journal (idempotency registry + journal-before-mutate).
    pub(crate) journal: std::sync::Mutex<Journal>,
    /// Prometheus-format counters served on `/metrics`.
    pub(crate) metrics: Arc<Metrics>,
    /// Admin bearer token guarding mutating endpoints; `None` disables
    /// authentication (P0 host-local default).
    pub(crate) admin_token: Option<String>,
}

impl AppState {
    /// Build the server state around an opened journal and a provider.
    #[must_use]
    #[allow(clippy::new_without_default)] // a Journal needs a directory; no sane Default exists
    pub fn new(
        provider: Arc<dyn VolumeProvider>,
        journal: Journal,
        admin_token: Option<String>,
    ) -> Self {
        Self {
            provider,
            journal: std::sync::Mutex::new(journal),
            metrics: Arc::new(Metrics::new()),
            admin_token,
        }
    }
}
