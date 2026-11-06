//! HTTP-surface configuration (P0, extended by issue #45 admission control).

use std::time::Duration;

/// Default request body limit: 1 MiB.
pub const DEFAULT_MAX_BODY_BYTES: usize = 1024 * 1024;

/// Default bound on a read-only provider call (list/inspect/capabilities).
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Default bound on waiting for a global concurrency permit. Exhausting
/// this wait is answered with a typed `503 OVERLOADED`, never an unbounded
/// queue.
pub const DEFAULT_ADMISSION_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Default global cap on in-flight requests (excluding liveness and
/// metrics).
pub const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 256;

/// Default per-principal token-bucket refill rate. Zero disables the
/// limiter. Both [`ApiConfig::default`] and the daemon's `[admission]` table
/// default to disabled, so rate limiting is opt-in and legacy client
/// behavior is preserved until an operator sets a non-zero rate.
pub const DEFAULT_RATE_LIMIT_PER_SECOND: u32 = 0;

/// Default per-principal token-bucket burst size (disabled limiter).
pub const DEFAULT_RATE_LIMIT_BURST: u32 = 0;

/// Default maximum page size accepted by `GET /v2/volumes?limit=`.
pub const DEFAULT_MAX_LIST_LIMIT: usize = 1000;

/// Optional per-route overrides for the read-only timeout (issue #45).
///
/// Each field is keyed by the axum **matched path** (the route pattern, not
/// the request URI). `None` falls back to [`ApiConfig::read_timeout`]; the
/// default configuration sets every field to `None`, so the existing single
/// bound is preserved. Mutating routes never consult these values: only
/// `GET`/`HEAD` and the read-only
/// `POST /v2/vms/{vm_id}/check-mobility` and
/// `POST /v2/internal/peer/verify-lineage` routes are bounded (see
/// `admission::read_timeout`). An override of zero would time out every
/// matching read immediately; the daemon therefore validates each override
/// to `1..=3600` seconds. Library callers should apply the same bound.
///
/// The daemon exposes these through `[admission.route_timeouts]`; see
/// `docs/admission-control.md` for the exact key-to-path mapping.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RouteTimeouts {
    /// `GET /v2/volumes` (list volumes).
    pub list_volumes: Option<Duration>,
    /// `GET /v2/volumes/{volume_id}` (inspect volume).
    pub inspect_volume: Option<Duration>,
    /// `GET /v2/capabilities`.
    pub capabilities: Option<Duration>,
    /// `GET /v2/migrations/{migration_id}` (observe handoff).
    pub observe_migration: Option<Duration>,
    /// `GET /v2/admin/devices` (device list).
    pub list_devices: Option<Duration>,
    /// `GET /v2/admin/devices/{device_id}/ownership`.
    pub device_ownership: Option<Duration>,
    /// `POST /v2/vms/{vm_id}/check-mobility` (read-only eligibility check).
    pub check_mobility: Option<Duration>,
    /// `GET /v2/internal/peer/health` (read-only peer health probe).
    pub peer_health: Option<Duration>,
    /// `POST /v2/internal/peer/verify-lineage` (read-only barrier-time
    /// lineage observation; never journaled).
    pub verify_lineage: Option<Duration>,
}

impl RouteTimeouts {
    /// The override configured for a matched path, if any.
    #[must_use]
    pub fn override_for(&self, route: &str) -> Option<Duration> {
        match route {
            "/v2/volumes" => self.list_volumes,
            "/v2/volumes/{volume_id}" => self.inspect_volume,
            "/v2/capabilities" => self.capabilities,
            "/v2/migrations/{migration_id}" => self.observe_migration,
            "/v2/admin/devices" => self.list_devices,
            "/v2/admin/devices/{device_id}/ownership" => self.device_ownership,
            "/v2/vms/{vm_id}/check-mobility" => self.check_mobility,
            "/v2/internal/peer/health" => self.peer_health,
            "/v2/internal/peer/verify-lineage" => self.verify_lineage,
            _ => None,
        }
    }
}

/// Configuration of the HTTP surface itself.
///
/// This is deliberately *not* the daemon configuration: paths, provider
/// selection and wiring belong to `volvisord`. Only the knobs the HTTP layer
/// itself consumes live here.
///
/// ## Long-operation model
///
/// `read_timeout` bounds **read-only** routes only. Journaled mutations are
/// never wrapped in a wall-clock timeout: the request owns the operation to
/// completion, and a client retry uses the same `operation_id` so the
/// journal either replays the recorded outcome or reports
/// `OPERATION_IN_DOUBT`. There is no asynchronous operation handle or
/// long-poll surface. See `docs/admission-control.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApiConfig {
    /// Maximum accepted request body size in bytes. Larger bodies are
    /// rejected before parsing, including streamed/chunked bodies that
    /// never declared a `Content-Length`.
    pub max_body_bytes: usize,
    /// Default bound on a read-only route's provider call. A matching
    /// [`RouteTimeouts`] override takes precedence.
    pub read_timeout: Duration,
    /// Optional per-route overrides of [`Self::read_timeout`].
    pub route_timeouts: RouteTimeouts,
    /// Bound on waiting for a global concurrency permit before answering
    /// `503 OVERLOADED` with `Retry-After`.
    pub admission_wait_timeout: Duration,
    /// Global cap on in-flight requests. `/healthz`, `/v2/healthz`,
    /// `/readyz`, `/v2/readyz` and `/metrics` bypass the cap so probes and
    /// monitoring stay available.
    pub max_concurrent_requests: usize,
    /// Per-principal token refill rate; zero disables per-principal rate
    /// limiting.
    pub rate_limit_per_second: u32,
    /// Per-principal token-bucket burst. Must be at least one when
    /// `rate_limit_per_second` is non-zero.
    pub rate_limit_burst: u32,
    /// Maximum accepted `limit` on `GET /v2/volumes`.
    pub max_list_limit: usize,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            read_timeout: DEFAULT_READ_TIMEOUT,
            route_timeouts: RouteTimeouts::default(),
            admission_wait_timeout: DEFAULT_ADMISSION_WAIT_TIMEOUT,
            max_concurrent_requests: DEFAULT_MAX_CONCURRENT_REQUESTS,
            rate_limit_per_second: DEFAULT_RATE_LIMIT_PER_SECOND,
            rate_limit_burst: DEFAULT_RATE_LIMIT_BURST,
            max_list_limit: DEFAULT_MAX_LIST_LIMIT,
        }
    }
}
