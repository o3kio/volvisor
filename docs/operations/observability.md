# Observability: metrics, readiness and traces

Status: documents the implemented observability surface (issue #44)
Date: 2026-11-03
Applies to: `volvisord` (the Volume API v2 HTTP surface)
Normative context: [Volume API v2 contract](../../contracts/volume-api-v2.md),
[admission control](../admission-control.md),
[real-backend test rigs](../testing/real-backends.md)

This guide states what the daemon measures, what each metric actually means,
which labels are fixed, and which requested measurements are **not** exposed.
It does not invent samples for unavailable state.

## Endpoints

| Endpoint | Meaning | Status codes |
|---|---|---|
| `GET /healthz`, `GET /v2/healthz` | Liveness only: the process is serving HTTP. Never claims storage health. | `200` |
| `GET /readyz`, `GET /v2/readyz` | Readiness: journal open, startup complete, and a configured witness transport-probe succeeds. | `200` ready, `503` not ready |
| `GET /metrics` | Prometheus text exposition of this process's metrics. | `200` |

Readiness and liveness are deliberately distinct. A daemon that is alive but
has not finished startup (or whose configured witness has become
unreachable) stays `200` on `/healthz` and answers `503` on `/readyz`.

Readiness body (the field names are stable):

```json
{
  "status": "ready",
  "startup_complete": true,
  "checks": {
    "journal": "open",
    "provider": "reconciled",
    "witness": "reachable"
  }
}
```

- `startup_complete` flips only after the journal has opened/replayed, the
  provider has been constructed/reconciled, and the listener has bound.
- `checks.journal` is `open` or `closed`; `checks.provider` is `reconciled`
  or `pending_startup` (the same startup gate).
- `checks.witness` is `reachable`, `unreachable`, or `not_probed`. With no
  witness URL configured, `not_probed` is reported and readiness does **not**
  fail on that unknown. A configured witness that fails the bounded probe
  makes readiness `503`.

The witness probe is a TCP connect with a 2-second bound. It establishes
transport reachability only: it does not authenticate, inspect authority,
prove quorum, or claim data safety. A listener on the target port that is not
the witness would still read as `reachable`; use the witness's own health and
audit surfaces for semantic health.

`/healthz`, `/v2/healthz`, `/readyz`, `/v2/readyz` and `/metrics` bypass
admission control (concurrency cap, rate limiter and read timeout), so probes
and scraping keep working while load is shed. The four probe paths are
public so an orchestrator can call them without credentials. `/metrics`
requires read access when scoped credentials are configured; in the legacy
admin-token-only or tokenless posture reads stay open, as documented in
[security deployment](../security/deployment.md). Successful probes are not
audited, matching the liveness behavior.

## Metric families

All output is hand-formatted Prometheus text; there is no Prometheus or
OpenTelemetry crate dependency. Families are emitted deterministically
(labels sorted), and an unavailable sample is omitted rather than rendered
as a fabricated zero.

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `http_requests_total` | counter | `route`, `code` | Served HTTP requests by matched route pattern and exact status code. |
| `http_request_duration_seconds` | histogram | `route`, `class` | Full request latency by route pattern and status class. |
| `http_requests_in_flight` | gauge | — | Requests currently being served, including the `/metrics` scrape that reads the gauge. |
| `operations_total` | counter | `kind`, `outcome` | Resolved volume operations by operation kind and outcome. |
| `provider_operation_duration_seconds` | histogram | `kind`, `outcome` | Latency of provider calls that actually executed, by operation kind and `success`/`failure`. |
| `journal_append_failures_total` | counter | `stage`, `kind` | Journal appends that failed after the operation reached the journal pipeline, by stage (`intent`, `outcome`) and operation kind. |
| `journal_open` | gauge | — | `1` while the daemon holds the journal open, `0` if the journal mutex is unusable. |
| `journal_records` | gauge | — | Valid journal records (replayed plus appended), sampled from the live `Journal`. |
| `journal_log_bytes` | gauge | — | Size of the journal log file in bytes. Omitted if it cannot be stat'ed. |
| `reconcile_age_seconds` | gauge | `scope` | Seconds since the last completed reconcile pass: `startup`, `grow`, or `move`. |
| `witness_reachable` | gauge | — | `1`/`0` from the last witness transport probe. Absent until a probe target exists. |

`operations_total.outcome` is one of `success`, `failure`, `replayed`,
`in_doubt`, `conflict`, `resolved`, `re_drive`. Operation kinds are the
compile-time constants in `volvisor-api/src/ops.rs` (`create_volume`,
`attach_volume`, `detach_volume`, `report_attachment_state`, `grow_volume`,
`delete_volume`, `move_volume_backing`, `claim_device`, `release_device`,
`lend_device`, `reclaim_device`, `resolve_quarantine`, `adopt_volume`,
`clear_cut_marker`, the migration kinds, and the peer kinds).

Latency histograms use one fixed bucket set in seconds:

```text
0.005 0.01 0.025 0.05 0.1 0.25 0.5 1 2.5 5 10 +Inf
```

`provider_operation_duration_seconds` observes only an **executed** provider
call. A journal replay or an in-doubt refusal performs no provider call, so
it does not add a provider sample. The HTTP histogram observes every served
request, including probes, errors and unmatched routes.

### Time-series refreshes

- `/metrics` samples the journal under the journal lock for that scrape, so
  `journal_records` and `journal_log_bytes` are real current values, not
  cached counters.
- If a witness probe target is configured, `/metrics` refreshes
  `witness_reachable` with the same bounded TCP probe that `/readyz` uses.
  Without a target the gauge is absent.
- `reconcile_age_seconds{scope="startup"}` is stamped when the serving state
  is constructed (after journal replay and provider construction/reconcile).
  `grow` and `move` are stamped only after a successful background pass on a
  daemon that runs those tasks. A failing pass does not refresh the sample,
  so the age grows and an alert can fire.

## Cardinality bounds

The label space is fixed by code and configuration, never by consumer data:

- `route` is the axum matched route **pattern** from a fixed route table
  (on the order of 30 patterns), or the literal `<unmatched>` for the
  fallback. Raw paths and volume/attachment/device/migration/VM ids are never
  labels.
- `code` is the exact HTTP status produced by the daemon, a bounded set for
  this API; the latency `class` is one of `1xx`, `2xx`, `3xx`, `4xx`, `5xx`.
- `kind` and `outcome` are compile-time constants; `stage` is `intent` or
  `outcome`; `scope` is `startup`, `grow` or `move`.
- No label contains a principal, role, bearer token, URL, request body,
  error string or free-form detail.

The load test also asserts this empirically: after 200 unique volume ids on a
matched route and 50 unique raw paths on the fallback, the `route` label set
stays within the fixed table, contains `<unmatched>`, and contains none of
the generated ids/paths. The series count is therefore bounded by the route
table (times a small status set) and fixed operation enums, not by tenants or
volumes.

## Tracing and logs

The existing `tracing` subscriber and JSON log format are preserved. Spans
carry the caller's `operation_id` so one request can be followed across:

- `operation` (API/ops pipeline, fields `kind`, `operation_id`);
- `provider_call` (provider call, fields `provider`, `kind`,
  `operation_id`), with a completion event carrying outcome and elapsed ms;
- `peer_call` (peer HTTP client, field `path`);
- `witness_inspect` (field `volume_id`), `vmm_state` and `vmm_call` (VMM
  controller), and `peer_grant` (field `migration_id`) on the handoff path.

The audit event, journal events and existing structured logs are unchanged.
A committed example for one create and one attach is in
[`traces/attach.jsonl`](traces/attach.jsonl); see
[`traces/README.md`](traces/README.md) for generation and the migration-trace
status.

## Example alert rules

[`alerts/volvisor.rules.yml`](alerts/volvisor.rules.yml) contains example
Prometheus rules for journal append failures, stale background reconcile,
witness down, an error-rate spike, a latency-budget example and load-shedding.
They are **examples, not tuned SLOs**: thresholds and windows must be set
from a deployment's real traffic and provider latencies. They assume the
scrape job is named `volvisor`; adjust that matcher.

Readiness is not exported as a metric. Probe `/readyz` with your orchestrator
or a blackbox exporter; do not infer readiness from `up` alone.

## What is intentionally not measured

- **State-save duration.** Provider state saves are performed inside the LVM,
  Ceph and DRBD providers (`save_state_atomic` and equivalents); the API
  state has no aggregate observation point. Adding a gauge would require
  instrumenting each provider's durable write, and a per-operation label
  would add cardinality. The omission is deliberate; provider logs remain the
  current evidence, and a future per-provider histogram would need its own
  cardinality review.
- **Lease/epoch state.** Epoch and lease facts are per-volume authority
  observations (current epoch, lease state, remaining seconds). Exposing them
  as process gauges would require a volume-id label (unbounded) and an active
  enumeration/inspection path, and a single process-wide "lease state" would
  be meaningless. Use the witness view, migration logs and audit events
  instead. No aggregate gauge is fabricated.
- **A real migration trace.** Not run in this workspace; see
  [`traces/README.md`](traces/README.md) for the missing prerequisites. The
  in-repo migration end-to-end test uses simulated DRBD worlds, a loopback
  witness and a fake VMM, so it is not presented as a real cross-host trace.

## Evidence (2026-11-03)

Commands run from the branch worktree at commit
`d164495c623bfb15bd6c7f17c2c71a1b49930222` (the change that introduced this
surface; later documentation-only commits do not alter it):

```text
cargo fmt --all --check
CARGO_BUILD_JOBS=4 cargo clippy --workspace --locked --all-targets -- -D warnings
CARGO_BUILD_JOBS=4 cargo test --workspace --locked
cargo test -p volvisor-api --test observability_load -- --ignored --nocapture
cargo test -p volvisor-api --test readiness -- --nocapture
cargo test -p volvisor-api --test trace_attach -- --ignored --nocapture
cargo +1.85 check --workspace --locked --all-targets
```

Results:

- Workspace tests: all suites passed; `0 failed`.
- `observability_load`: `1 passed`. It drove one real create and 250
  requests, observed 8 parked requests in `http_requests_in_flight`, saw the
  gauge return to the scrape-only baseline of `1`, observed the provider
  histogram count of 1, the HTTP `4xx` count of 208 on
  `/v2/volumes/{volume_id}` and 50 on `<unmatched>`, and read
  `journal_records = 2` / a positive `journal_log_bytes` from the real
  journal.
- `readiness`: `3 passed`. The test flips readiness from `503` to `200` after
  `mark_ready`, drops a real loopback witness listener and observes
  `503`/`witness: unreachable` while `/healthz` stays `200`, and confirms
  `not_probed` does not fail readiness. A separate case proves probes still
  answer with a one-permit concurrency cap.
- The attach trace was generated from the real router and filtered to JSON
  lines; no bearer token appears in it.

The load and trace tests are `#[ignore]`d so the ordinary workspace test run
stays fast; they are explicit evidence commands, not silent skips in CI.
