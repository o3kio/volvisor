//! Admission control: read timeouts, a global concurrency cap and
//! per-principal rate limits.
//!
//! ## The long-operation model (issue #45)
//!
//! A bounded request timeout must never silently abort journaled work. This
//! module therefore bounds only **read-only** routes with a wall-clock
//! timeout. Journaled mutations (every `POST`/`DELETE` that reaches
//! [`crate::ops::execute`]) are admitted under the global concurrency cap
//! and then run to completion inside the request: no execution timeout is
//! imposed, because dropping the future after the journal intent would
//! abandon a provider act whose outcome the journal has not recorded.
//!
//! There is **no** asynchronous operation handle or long-poll surface in
//! this revision. A client retry carries the same `operation_id`: the
//! journal replays a recorded outcome, or reports `OPERATION_IN_DOUBT` for
//! an intent without an outcome. See `docs/admission-control.md`.
//!
//! ## Ordering
//!
//! The router installs these layers so a request traverses, outermost first:
//!
//! 1. metrics (`record_http_request`);
//! 2. this module's `guard` — concurrency permit + read timeout;
//! 3. authentication/authorization/audit;
//! 4. this module's `rate_limit` — per-principal token bucket, after the
//!    principal is resolved;
//! 5. the body limit and the handler.
//!
//! `/healthz`, `/v2/healthz` and `/metrics` bypass both the concurrency cap
//! and the rate limiter so probes and monitoring stay available while the
//! server sheds load.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{MatchedPath, Request, State};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::Response;
use tokio::sync::Semaphore;
use volvisor_types::{ApiError, ApiErrorCode};

use crate::auth::AuthContext;
use crate::config::ApiConfig;
use crate::error::{error_response, error_response_with_retry_after};

/// Maximum number of per-principal token buckets retained.
///
/// Principal identity is a configured credential id (or one of the fixed
/// `admin_token`/`anonymous`/`peer` identities), so the real key space is
/// already bounded by configuration. This ceiling is defense in depth: if a
/// caller somehow presents more distinct keys, they share one overflow
/// bucket instead of growing the map without limit.
const MAX_RATE_LIMIT_BUCKETS: usize = 1024;

/// State carried by the admission middleware layers.
#[derive(Clone)]
pub(crate) struct Admission {
    inner: Arc<AdmissionInner>,
}

struct AdmissionInner {
    config: ApiConfig,
    permits: Semaphore,
    rate_limiter: RateLimiter,
}

impl Admission {
    /// Build the admission state.
    ///
    /// Library callers that enable rate limiting with
    /// `rate_limit_burst = 0` are normalized to a burst of one, because a
    /// zero-capacity bucket could never admit a request. The daemon's config
    /// validation refuses that same input instead (see
    /// `validate_admission_fields`), so an operator never silently gets the
    /// coercion.
    pub(crate) fn new(config: ApiConfig) -> Self {
        let burst = if config.rate_limit_per_second > 0 {
            config.rate_limit_burst.max(1)
        } else {
            config.rate_limit_burst
        };
        Self {
            inner: Arc::new(AdmissionInner {
                config,
                permits: Semaphore::new(config.max_concurrent_requests.max(1)),
                rate_limiter: RateLimiter::new(config.rate_limit_per_second, burst),
            }),
        }
    }

    /// Consume one rate-limit token for `principal`.
    ///
    /// Exposed to the authentication middleware so a credential-rejected
    /// request is charged to the anonymous bucket rather than bypassing the
    /// limiter entirely (the post-authentication limiter only runs for
    /// admitted requests).
    pub(crate) fn check_rate(&self, principal: &str) -> Result<(), Duration> {
        self.inner.rate_limiter.check(principal)
    }
}

/// Liveness, readiness and observability endpoints are never
/// admission-controlled.
fn is_exempt(route: &str) -> bool {
    matches!(
        route,
        "/healthz" | "/v2/healthz" | "/readyz" | "/v2/readyz" | "/metrics"
    )
}

/// The route's bound, if it is read-only. Journaled mutations deliberately
/// return `None` (see the module docs).
fn read_timeout(config: &ApiConfig, route: &str, method: &Method) -> Option<Duration> {
    if is_exempt(route) {
        return None;
    }
    if *method == Method::GET
        || *method == Method::HEAD
        || route == "/v2/vms/{vm_id}/check-mobility"
    {
        Some(config.read_timeout)
    } else {
        None
    }
}

/// The configured maximum accepted `GET /v2/volumes?limit=`, handed to the
/// handler through the request extensions (the router installs a fresh
/// admission instance per build; the handler has no other path to it).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ListLimit(pub(crate) usize);

/// The matched route pattern, falling back to the raw path.
fn route_of(request: &Request) -> String {
    request.extensions().get::<MatchedPath>().map_or_else(
        || request.uri().path().to_owned(),
        |m| m.as_str().to_owned(),
    )
}

/// Global concurrency cap plus the read-only route timeout.
pub(crate) async fn guard(
    State(admission): State<Admission>,
    request: Request,
    next: Next,
) -> Response {
    let route = route_of(&request);
    if is_exempt(&route) {
        return next.run(request).await;
    }

    let wait = admission.inner.config.admission_wait_timeout;
    let permit = match tokio::time::timeout(wait, admission.inner.permits.acquire()).await {
        Ok(Ok(permit)) => permit,
        Ok(Err(_closed)) => {
            return overloaded(&admission.inner.config);
        }
        Err(_elapsed) => {
            return overloaded(&admission.inner.config);
        }
    };

    let method = request.method().clone();
    let mut request = request;
    request
        .extensions_mut()
        .insert(ListLimit(admission.inner.config.max_list_limit));
    // The authentication middleware runs inside this layer and charges the
    // anonymous bucket on a denial. It reaches the same limiter through this
    // extension instead of duplicating the admission state.
    request.extensions_mut().insert(admission.clone());
    let body = match read_timeout(&admission.inner.config, &route, &method) {
        Some(limit) => match tokio::time::timeout(limit, next.run(request)).await {
            Ok(response) => response,
            Err(_elapsed) => error_response(&ApiError::new(
                ApiErrorCode::RequestTimeout,
                format!("read-only route {route} exceeded its {limit:?} timeout"),
            )),
        },
        None => next.run(request).await,
    };
    drop(permit);
    body
}

/// Per-principal token-bucket rate limit, applied after authentication has
/// inserted [`AuthContext`].
pub(crate) async fn rate_limit(
    State(admission): State<Admission>,
    request: Request,
    next: Next,
) -> Response {
    let route = route_of(&request);
    if is_exempt(&route) {
        return next.run(request).await;
    }
    let principal = if request
        .extensions()
        .get::<crate::auth::PeerRateLimitPrincipal>()
        .is_some()
    {
        "peer"
    } else {
        request
            .extensions()
            .get::<AuthContext>()
            .map_or("anonymous", |context| context.principal.as_str())
    };
    if let Err(retry_after) = admission.inner.rate_limiter.check(principal) {
        return error_response_with_retry_after(
            &ApiError::new(
                ApiErrorCode::RateLimited,
                "per-principal request rate exceeded; retry after the Retry-After delay",
            ),
            retry_after,
        );
    }
    next.run(request).await
}

/// `503 OVERLOADED` with an operator-visible retry delay.
fn overloaded(config: &ApiConfig) -> Response {
    error_response_with_retry_after(
        &ApiError::new(
            ApiErrorCode::Overloaded,
            "server concurrency cap exhausted; retry after the Retry-After delay",
        ),
        config.admission_wait_timeout,
    )
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// A bounded, per-principal token bucket.
///
/// Keys are principal ids (never bearer tokens), so no secret enters this
/// map or any log. The map never exceeds [`MAX_RATE_LIMIT_BUCKETS`] entries;
/// excess keys share one overflow bucket.
struct RateLimiter {
    per_second: u32,
    burst: u32,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    fn new(per_second: u32, burst: u32) -> Self {
        Self {
            per_second,
            burst,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Consume one token for `key`, or return the delay before a token is
    /// available. A disabled limiter (`per_second == 0`) always admits.
    fn check(&self, key: &str) -> Result<(), Duration> {
        self.check_at(key, Instant::now())
    }

    /// [`Self::check`] with an injected instant, so refill and Retry-After
    /// arithmetic can be tested without sleeping.
    fn check_at(&self, key: &str, now: Instant) -> Result<(), Duration> {
        if self.per_second == 0 {
            return Ok(());
        }
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let bucket = if buckets.contains_key(key) {
            buckets.get_mut(key)
        } else {
            let bounded_key = if buckets.len() >= MAX_RATE_LIMIT_BUCKETS {
                "__overflow__"
            } else {
                key
            };
            Some(buckets.entry(bounded_key.to_owned()).or_insert(Bucket {
                tokens: f64::from(self.burst),
                last: now,
            }))
        };
        let Some(bucket) = bucket else {
            return Ok(());
        };

        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        let refilled = elapsed * f64::from(self.per_second);
        bucket.tokens = (bucket.tokens + refilled).min(f64::from(self.burst));
        bucket.last = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            // Retry-After is whole seconds (RFC 9110 delay-seconds). The
            // refill rate is >= 1 token/s and the deficit is < 1 token, so
            // the true wait is at most one second; `ceil` reports that wait
            // and the one-second floor keeps a sub-second deficit from
            // rounding to zero.
            let seconds = (1.0 - bucket.tokens) / f64::from(self.per_second);
            Err(Duration::from_secs_f64(seconds.max(0.0).ceil().max(1.0)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_rate_limiter_admits_everything() {
        let limiter = RateLimiter::new(0, 0);
        for _ in 0..10_000 {
            assert!(limiter.check("principal").is_ok());
        }
    }

    #[test]
    fn burst_is_enforced_per_key_and_the_map_stays_bounded() {
        let limiter = RateLimiter::new(1, 2);
        assert!(limiter.check("a").is_ok());
        assert!(limiter.check("a").is_ok());
        assert!(limiter.check("a").is_err());
        // A different principal has its own bucket.
        assert!(limiter.check("b").is_ok());

        for index in 0..(MAX_RATE_LIMIT_BUCKETS + 100) {
            let _ = limiter.check(&format!("principal-{index}"));
        }
        let buckets = limiter
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(buckets.len() <= MAX_RATE_LIMIT_BUCKETS + 1);
    }

    #[test]
    fn retry_after_is_one_second_and_the_bucket_refills() {
        let limiter = RateLimiter::new(1, 1);
        let t0 = Instant::now();
        assert!(limiter.check_at("a", t0).is_ok());
        assert_eq!(
            limiter.check_at("a", t0),
            Err(Duration::from_secs(1)),
            "an exhausted bucket reports the one-second floor"
        );
        assert!(
            limiter
                .check_at("a", t0 + Duration::from_millis(500))
                .is_err(),
            "half a second does not refill a whole token at 1/s"
        );
        assert!(
            limiter.check_at("a", t0 + Duration::from_secs(1)).is_ok(),
            "one refill period admits the next request"
        );
    }

    #[test]
    fn faster_refill_still_reports_the_one_second_floor() {
        let limiter = RateLimiter::new(100, 1);
        let t0 = Instant::now();
        assert!(limiter.check_at("a", t0).is_ok());
        assert_eq!(
            limiter.check_at("a", t0),
            Err(Duration::from_secs(1)),
            "a sub-second deficit still emits a whole second"
        );
        assert!(
            limiter
                .check_at("a", t0 + Duration::from_millis(10))
                .is_ok(),
            "100/s refills a token in 10 ms"
        );
    }

    #[test]
    fn keys_are_principals_never_tokens() {
        // The limiter is constructed from the resolved principal only; this
        // test pins that the map contains exactly the supplied key and no
        // token-shaped value is ever stored as a key by this module.
        let limiter = RateLimiter::new(10, 10);
        assert!(limiter.check("reader-a").is_ok());
        let buckets = limiter
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(buckets.contains_key("reader-a"));
        assert_eq!(buckets.len(), 1);
    }
}
