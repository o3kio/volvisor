# Admission control, long operations and backpressure

Status: documents the implemented admission-control surface (issue #45)
Date: 2026-11-03
Applies to: `volvisord` (the Volume API v2 HTTP surface)
Normative context: [Volume API v2 contract](../contracts/volume-api-v2.md),
[docs/configuration.md](configuration.md), [security deployment guide](security/deployment.md)

This guide is deliberately explicit about what the daemon enforces and what
it does **not** implement. Load shedding must never be confused with
durability or with a resumable operation model.

## The long-operation model

Volvisor's mutating routes are **synchronous and journaled**:

1. the handler validates the request;
2. the intent is fsynced to the journal;
3. the provider mutation runs inside the request;
4. the outcome is journaled and returned.

There is **no asynchronous operation handle and no long-poll surface** in
this revision. A client cannot detach a mutation, poll a job id, or resume a
partial provider act through this API.

The admission layer therefore applies a wall-clock timeout **only to
read-only routes**. A journaled mutation is admitted under the concurrency
cap and then allowed to run to completion: no request timeout drops its
future after the journal intent, because that would abandon a provider act
whose outcome the journal has not recorded.

What a client should do instead:

- Retry a mutation with the **same `operation_id`** and byte-identical body.
  The journal replays a recorded outcome, or answers `OPERATION_IN_DOUBT` for
  an intent without an outcome. Never mint a new operation id for a retry.
- A client-side or network timeout does not create server-side
  resumability. If the client disconnects after the intent is durable, a
  later retry observes the honest journal state (replay or
  `OPERATION_IN_DOUBT`); the daemon never pretends the act can be resumed.
- Read-only calls are safe to retry after `503 OVERLOADED`,
  `429 RATE_LIMITED` or `504 REQUEST_TIMEOUT`.

**Explicitly not implemented:** a resumable operation handle / async job
surface. Issue #45's long-operation checkbox that asks for such a handle
stays unchecked; this document records the synchronous model instead of
claiming a capability the daemon does not have.

## Read-route timeout

`admission.read_timeout_secs` (default `30`) is the **fallback** bound for
every read-only provider call. The optional `[admission.route_timeouts]`
table overrides it per route. Keys name the axum **matched path** (the route
pattern, not the request URI); a read-only route without an override falls
back to `read_timeout_secs`, so the default is unchanged.

| Override key | Matched path | Route |
|---|---|---|
| `list_volumes_secs` | `/v2/volumes` | `GET /v2/volumes` |
| `inspect_volume_secs` | `/v2/volumes/{volume_id}` | `GET /v2/volumes/{volume_id}` |
| `capabilities_secs` | `/v2/capabilities` | `GET /v2/capabilities` |
| `observe_migration_secs` | `/v2/migrations/{migration_id}` | `GET /v2/migrations/{migration_id}` |
| `list_devices_secs` | `/v2/admin/devices` | `GET /v2/admin/devices` |
| `device_ownership_secs` | `/v2/admin/devices/{device_id}/ownership` | `GET /v2/admin/devices/{device_id}/ownership` |
| `check_mobility_secs` | `/v2/vms/{vm_id}/check-mobility` | `POST /v2/vms/{vm_id}/check-mobility` (read-only) |
| `peer_health_secs` | `/v2/internal/peer/health` | `GET /v2/internal/peer/health` |
| `verify_lineage_secs` | `/v2/internal/peer/verify-lineage` | `POST /v2/internal/peer/verify-lineage` (read-only) |

Two of these routes are `POST` but **read-only**: `check-mobility` and
`verify-lineage` never journal and are observations, so they are bounded
like `GET`/`HEAD`. Journaled mutations are never bounded.

Every key is optional; each value is whole seconds in `1..=3600`, the same
range as `read_timeout_secs`. Unknown keys are refused at startup
(`deny_unknown_fields`), and overlapping routes are not possible because the
key space is the matched path exactly.

```toml
[admission]
read_timeout_secs = 30

[admission.route_timeouts]
list_volumes_secs = 60          # large listings may legitimately be slow
inspect_volume_secs = 10        # a single metadata read should be quick
check_mobility_secs = 120       # provider-wide eligibility observation
verify_lineage_secs = 120       # barrier-time DRBD lineage observation
```

On expiry the daemon answers `504 REQUEST_TIMEOUT`, naming the matched route
and the **effective** limit (the override if one matched, otherwise the
fallback):

```http
HTTP/1.1 504 Gateway Timeout
Content-Type: application/json

{"code":"REQUEST_TIMEOUT","message":"read-only route /v2/volumes exceeded its 60s timeout"}
```

`/healthz`, `/v2/healthz`, `/readyz`, `/v2/readyz` and `/metrics` are local
and unbounded. Mutations are never bounded by any entry, including when a
mutating method shares a matched path with an overridden read route (for
example `POST /v2/volumes` versus `GET /v2/volumes`): the selector first
admits only `GET`/`HEAD` and the read-only `check-mobility` and
`verify-lineage` routes, then looks up an override.

`HEAD` requests are bounded exactly like `GET` (axum routes `HEAD` to the
`GET` handler), so a hung provider cannot pin a `HEAD` request past the
timeout.

## Global concurrency cap

`admission.max_concurrent_requests` (default `256`) caps in-flight requests
that do not bypass admission control. A request waits at most
`admission.queue_timeout_secs` (default `5`) for a permit. If no permit
becomes available, the daemon sheds it:

```http
HTTP/1.1 503 Service Unavailable
Retry-After: 5

{"code":"OVERLOADED","message":"server concurrency cap exhausted; retry after the Retry-After delay"}
```

The permit is held for the whole request, including a journaled mutation's
provider call. A slow or hung provider therefore consumes a bounded number
of permits; it cannot create unbounded tasks or connections. `/healthz`,
`/v2/healthz`, `/readyz`, `/v2/readyz` and `/metrics` bypass the cap so
liveness, readiness and monitoring stay available during overload.

## Per-principal rate limits (opt-in)

Per-principal rate limiting is **disabled by default**: both
`admission.rate_limit_per_second` and `admission.rate_limit_burst` default
to `0`. This preserves legacy client behavior; the concurrency cap, read
timeout and body limit stay on by default because they shed overload without
changing the success path for well-behaved clients. Rate limiting is a
behavioral change, so an operator must opt in explicitly.

When enabled, `rate_limit_per_second` and `rate_limit_burst` form a
per-principal token bucket. The key is the authenticated principal id from
the scoped credential (`AuthContext`), the fixed `admin_token` identity, or
the fixed `peer` identity for traffic authenticated by the daemon-to-daemon
peer token. Requests with no valid identity share the fixed `anonymous`
bucket. **Bearer tokens are never used as keys and never logged.**

A request rejected by authentication or authorization is charged to the
`anonymous` bucket before its `401`/`403` is returned, so a credential
brute-force flood is rate-limited rather than bypassing the limiter — this
includes denials on the otherwise admission-exempt probe and `/metrics`
routes. When the anonymous bucket is exhausted the rejection becomes
`429 RATE_LIMITED`.

Enable it in the daemon config:

```toml
[admission]
rate_limit_per_second = 100
rate_limit_burst = 200
```

- A refused request is answered `429 Too Many Requests` with `Retry-After`
  and `{"code":"RATE_LIMITED", ...}`. `Retry-After` is whole seconds. The
  refill rate is at least 1 token/s and the deficit is below one token, so
  the computed wait is at most one second; it is rounded up with a
  one-second floor, so the emitted value is always `1`.
- Different principals have independent buckets.
- The bucket map is bounded by the configured principal set plus a hard
  defensive ceiling; excess keys share one overflow bucket, so a hostile
  label cannot grow memory without bound.
- Setting `rate_limit_per_second = 0` (the default) disables the limiter and
  `rate_limit_burst` is ignored.
- The legacy read-only posture (no scoped credentials) shares the single
  `anonymous` bucket, because there is no stronger identity to key on. Use
  scoped credentials **and** enable rate limiting on any shared network.

## Pagination for `GET /v2/volumes`

- `limit` — optional page size, `1..=admission.max_list_limit` (default
  maximum `1000`). Out-of-range values are `400 INVALID_REQUEST`.
- `cursor` — opaque continuation token from the previous page's
  `next_cursor`. Clients echo it back and never parse it. An invalid cursor
  is `400 INVALID_REQUEST`. A cursor is only valid with `limit`; a cursor
  without `limit` is also `400 INVALID_REQUEST`, rather than a silently
  truncated unlimited listing.
- Ordering is stable: `volume_id` ascending, independent of the provider's
  iteration order.
- The response keeps `volumes` and adds `next_cursor` only while more entries
  follow.
- **Compatibility choice:** a request *without* `limit` preserves the legacy
  unlimited listing and carries no cursor. An existing client therefore can
  never silently lose volumes. New clients should always send `limit` to get
  a bounded response.
- The cursor is a resume point, not a snapshot: entries created or deleted
  between pages can affect later pages. It never duplicates or skips an
  entry that is stable across the two requests.

## Request body limits

`max_body_bytes` (default `1 MiB`) bounds every request body. The limit is
enforced while the body is read, so it applies both to a body that declares
`Content-Length` and to a streamed/chunked body that never declares one.
Oversized bodies are rejected before parsing and before any journal write:

```http
HTTP/1.1 413 Payload Too Large
Content-Type: application/json

{"code":"PAYLOAD_TOO_LARGE","message":"request body exceeds the configured maximum"}
```

A request with an oversized body does not consume a journal record and does
not reach the provider.

## Operator backpressure guidance

- Watch `http_requests_total{code="429"}` and
  `http_requests_total{code="503"}` on `/metrics`. They are the existing
  per-route counters; admission control adds no separate metric family.
- Honour `Retry-After` and add jitter. Retry **reads** freely; retry
  **mutations** only with the same `operation_id`.
- Size `max_concurrent_requests` to the provider's real concurrency (LVM,
  Ceph and DRBD calls are not all equally parallel). A cap far above provider
  capacity only moves the queue into the provider.
- Keep `/healthz` and `/metrics` reachable; they bypass admission control by
  design. Do not put them behind the same concurrency limit at a proxy.
- A reverse proxy or load balancer may impose additional limits; keep its
  timeouts shorter than client patience but longer than your slowest
  legitimate **read**. It must not impose a timeout that silently abandons a
  journaled mutation that the daemon is still running.
- The daemon does not implement slowloris defense at the application layer;
  bound request headers/body timing at the listener or proxy as well. This
  matters most for authenticated mutating requests: a permit is acquired
  before the body is read, and mutations are deliberately never
  wall-clock-bounded, so `max_concurrent_requests` connections that dribble
  a body can exhaust the cap and turn every other request (reads included)
  into `503 OVERLOADED`. Size client/proxy body timeouts accordingly.

## Defaults at a glance

| Knob | Default | Meaning |
|---|---|---|
| `max_body_bytes` | `1048576` | Request body ceiling (sized and streamed). |
| `admission.read_timeout_secs` | `30` | Fallback read-only timeout; a matching `[admission.route_timeouts]` override wins (`504 REQUEST_TIMEOUT`). |
| `admission.route_timeouts` | empty | Optional per-route overrides (matched-path keys, each `1..=3600` seconds); unset keys fall back. |
| `admission.queue_timeout_secs` | `5` | Concurrency-permit wait (`503 OVERLOADED`). |
| `admission.max_concurrent_requests` | `256` | Global in-flight cap (probes/metrics exempt). |
| `admission.rate_limit_per_second` | `0` (disabled) | Per-principal refill rate; opt-in, `0` keeps legacy behavior. |
| `admission.rate_limit_burst` | `0` (disabled) | Per-principal burst; ignored while the rate is `0`. |
| `admission.max_list_limit` | `1000` | Maximum accepted `GET /v2/volumes?limit=`. |
