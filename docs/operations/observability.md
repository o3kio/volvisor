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

The witness probe is a TCP connect with a 2-second bound. The target address
is resolved once at startup; a DNS change after start requires a daemon
restart before readiness reflects it (the witness driver itself re-resolves
per connect). `/readyz` and `/metrics` share a one-second probe cache and a
single-flight gate, so a burst of public `/readyz` calls opens at most one
outbound connection and reuses the same observation. The probe establishes
transport reachability only: it does not authenticate, inspect authority,
prove quorum, or claim data safety. A listener on the target port that is not
the witness would still read as `reachable`; use the witness's own health and
audit surfaces for semantic health.

`/healthz`, `/v2/healthz`, `/readyz`, `/v2/readyz` and `/metrics` bypass
admission control (concurrency cap, rate limiter and read timeout), so probes
and scraping keep working while load is shed. The four probe paths are
public so an orchestrator can call them without credentials. `/metrics`
requires read access whenever any credential is configured, including the
legacy `admin_token`; only the tokenless posture leaves reads open, as
documented in [security deployment](../security/deployment.md). Successful
probes are not audited, matching the liveness behavior.

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
| `journal_compactions_total` | counter | — | Successful compactions since the daemon opened the journal. |
| `journal_compaction_failures_total` | counter | — | Failed compaction attempts since open. A sustained increase means the log is not being bounded; alert on any increase and inspect the daemon log for the `journal compaction failed` warning (the detail is also available as `Journal::last_compaction_error`). A failure counted after the snapshot rename already succeeded means only the directory fsync is pending: the in-process log is bounded, the fsync is retried and cleared by the next append, and the counter still records that the rename's crash durability is not yet proven. |
| `journal_retained_operations` | gauge | — | Terminal plus in-flight operations currently retained in the idempotency registry. |
| `journal_tombstones` | gauge | — | Exact evicted-operation fingerprints currently in the tombstone ring. |
| `reconcile_age_seconds` | gauge | `scope` | Seconds since the last completed reconcile pass, or since the `grow`/`move` task started if no pass has completed: `startup`, `grow`, or `move`. |
| `witness_reachable` | gauge | — | `1`/`0` from the last witness transport probe. Absent without a configured probe target (and before the first probe). |

`operations_total.outcome` is one of `success`, `failure`, `replayed`,
`in_doubt`, `conflict`, `resolved`, `re_drive`, `expired`. `resolved` is a
terminal recovery outcome: a retry proved by inspection that the act had
landed, so the operation returns the recorded result without a second
provider call. `expired` is a terminal fail-closed refusal: the operation's
journal entry was evicted by bounded retention, so it is never re-executed
(`OPERATION_EXPIRED`, HTTP 410). `re_drive` is not terminal: a retry proved
the act had **not** landed and is re-executing, so the same operation later
records its `success`/`failure`.
Operation kinds are the
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

- `/metrics` and `/readyz` sample the journal on the blocking pool, so a
  stalled fsync cannot park a Tokio runtime worker; the readiness response
  itself still waits on that blocking sample (bounded by the fsync, which is
  not timeout-cancelled), while `/healthz` and other requests keep running.
  The sample is the real journal state, not a cached counter. A failed
  blocking join is rendered as a closed journal, never as a fabricated zero.
- If a witness probe target is configured, `/metrics` refreshes
  `witness_reachable` with the same bounded, cached TCP probe that `/readyz`
  uses. Without a target the gauge is absent.
- `reconcile_age_seconds{scope="startup"}` is stamped when the serving state
  is constructed (after journal replay and provider construction/reconcile).
- `reconcile_age_seconds{scope="grow"|"move"}` is stamped when the background
  task starts, not only after its first success. A daemon whose every pass
  fails from boot still exposes a growing age, so `VolvisorReconcileStale`
  can fire; the first successful pass overwrites the sample.

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

The audit event, journal events and failure logs remain at INFO. The
pre-dispatch `accepting <op>` line and the successful provider-completion
line are DEBUG: they duplicate the audit/metrics record and dominated the
measured log volume (issue #64's residual). Set `RUST_LOG=debug` only while
diagnosing; the default filter stays `info`.

The daemon writes JSON logs to stdout. The packaged
`volvisord.service` leaves capture to systemd/journald, which owns retention
and rotation; the daemon does not claim an internal rotating file sink. A
deployment that redirects stdout to a file must rotate it itself, for
example with `logrotate` using `copytruncate` (the daemon keeps the fd open)
and a bounded `rotate`/`maxsize`. The 2026-11-04 benchmark's 2.57 GB file was
an unrotated stdout capture over ~30 minutes, not a journal or state file.

A committed example for one create and one attach is in
[`traces/attach.jsonl`](traces/attach.jsonl); see
[`traces/README.md`](traces/README.md) for generation and the migration-trace
status.

Validate the committed trace evidence (JSON shape, RFC 3339 timestamps,
bounded span names, `operation`/`provider_call` id correlation and absence of
secrets) with:

```text
cargo test -p volvisor-api --test trace_evidence --locked
python3 scripts/validate-traces.py docs/operations/traces/attach.jsonl
```

The integration test runs in the ordinary workspace suite. The script exits
`0` for valid input, `1` for an invalid trace and `2` for a usage error, and
accepts every committed `docs/operations/traces/*.jsonl` file.

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
- `observability_load`: `1 passed`. It drove one real create, 200 unique-id
  reads on a matched route and 50 unique-path fallbacks, observed 8 parked
  requests in `http_requests_in_flight`, saw the gauge return to the
  scrape-only baseline of `1`, observed the provider
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
