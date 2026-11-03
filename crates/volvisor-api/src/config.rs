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
    /// Bound on a read-only route's provider call.
    pub read_timeout: Duration,
    /// Bound on waiting for a global concurrency permit before answering
    /// `503 OVERLOADED` with `Retry-After`.
    pub admission_wait_timeout: Duration,
    /// Global cap on in-flight requests. `/healthz`, `/v2/healthz` and
    /// `/metrics` bypass the cap so probes and monitoring stay available.
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
            admission_wait_timeout: DEFAULT_ADMISSION_WAIT_TIMEOUT,
            max_concurrent_requests: DEFAULT_MAX_CONCURRENT_REQUESTS,
            rate_limit_per_second: DEFAULT_RATE_LIMIT_PER_SECOND,
            rate_limit_burst: DEFAULT_RATE_LIMIT_BURST,
            max_list_limit: DEFAULT_MAX_LIST_LIMIT,
        }
    }
}
