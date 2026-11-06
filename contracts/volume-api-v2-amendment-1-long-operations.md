# Volume API v2 — Amendment 1: long-operation handles and mutation response budgets

Status: **Proposed amendment — not accepted, not implemented, not normative**
Date: 2026-11-06
Applies to: [Volume API v2](volume-api-v2.md), sections 7 and 12
Related: [§7](volume-api-v2.md#7-admission-state-and-failures),
[§12](volume-api-v2.md#12-admission-control-timeouts-body-limits-and-pagination),
[admission control](../docs/admission-control.md),
[ADR-0015](../docs/adr/0015-write-path-critical-section-invariant.md),
[ADR-0016](../docs/adr/0016-semantic-change-and-default-scenario-gate.md),
[issue #45](https://github.com/o3kio/volvisor/issues/45),
[production-readiness closure plan](../docs/plans/2026-11-06-production-readiness-closure.md)

This document proposes a change; it does not amend the contract until accepted. `volume-api-v2.md` §12 remains authoritative and still says there is no
asynchronous operation handle.

## 1. Problem
Volume API v2 mutations are synchronous and journaled: validate, fsync the intent, run the provider act, journal the outcome, return it. Only
read-only routes have wall-clock timeouts and per-route overrides; a journaled mutation is deliberately never bounded, because dropping its future
after intent would abandon an act whose outcome is not recorded.

A client therefore cannot detach, poll or reattach after a timeout, cannot distinguish "still running" from "interrupted before the outcome was
recorded" without retrying, and operators cannot give mutating routes distinct response bounds without changing the default or aborting journaled
work. Issue #45 leaves this open and forbids silently aborting journaled work. This amendment proposes a bounded response wait plus a resumable
**observation** handle; "resumable" does not mean the provider act restarts after a crash.

## 2. Proposed decision

### 2.1 Opt-in asynchronous response
A mutating request may send `Prefer: respond-async` (case-insensitive token among comma-separated preferences; unknown preferences are ignored). It applies to the
journaled consumer/admin routes in §2.3; internal `/v2/internal/peer/*` mutations stay synchronous under their separate daemon-to-daemon protocol.
The migration `transfer` route already has an asynchronous §5 shape (`202 {state}` plus `GET /v2/migrations/{migration_id}`): `Prefer: respond-async`
on that route keeps the §5 body and does **not** add a `/v2/operations` handle in this amendment; unifying the two observation surfaces is an open
question (§6.6).

When recognized: (1) validation and admission run first, so any validation, authorization, rate-limit, concurrency, body-size or `JOURNAL_FULL` refusal is
returned normally and is **not** turned into `202`; (2) the intent is journaled and fsynced exactly as today, and the act starts or continues
independently of the HTTP connection; (3) unless a terminal outcome is already recorded, answer `202 Accepted` with `Location: /v2/operations/{operation_id}`, `Preference-Applied: respond-async` and `{"operation_id":"op-...","state":"in_progress"}`.

A terminal result is never replaced by a fabricated `202`: if the outcome is recorded when the response is committed, return the recorded status and
body. A detached act keeps its admission permit until its outcome is journaled; `202` must not become an unbounded work launcher.

### 2.2 Operation resource
`GET /v2/operations/{operation_id}` is read-only, bounded by the existing read timeout (optional `[admission.route_timeouts]` key `get_operation_secs`), and reports only journal-derived state:

| `state` | Meaning | Journal evidence |
|---|---|---|
| `in_progress` | This daemon holds the act and it has not finished. | intent, no outcome, live owner |
| `succeeded` | The act completed successfully. | recorded success outcome |
| `failed` | The act completed with a recorded failure. | recorded failure outcome |
| `in_doubt` | The act may have executed; no outcome is recorded. Includes every outcome-less intent observed after restart. | intent, no outcome, no live owner |

The terminal body is `{"operation_id":"op-...","state":"succeeded","http_status":200,"result":{...}}`. `result` is the exact recorded body (on failure the recorded `{"code","message"}`); `http_status` is present when recorded. No request
payload, progress or ETA is exposed. Never report `in_progress` for a previous process's act, and never `succeeded` without a recorded success. Evicted or
tombstoned operations answer `410 OPERATION_EXPIRED`; unknown ones `404 NOT_FOUND`; a `derived:` id `400 INVALID_REQUEST`, before lookup.

The resource is scoped to the same principal and project as the mutation; an `operation_id` is not a capability URL and knowing it grants nothing. Another
principal/project sees `404`. The current journal record lacks this binding, so safe scoping is required work, not a claim (§6.1).

### 2.3 Per-route mutation response budgets
`[admission.mutation_response_timeouts]` is an optional matched-path table. An absent entry means no budget, preserving today's synchronous behavior exactly; there is no global
mutation fallback. Values are whole seconds in `1..=3600`; unknown or out-of-range keys are refused at startup, as with `[admission.route_timeouts]`.

| Keys | Matched paths |
|---|---|
| `create_volume_secs`; `delete_volume_secs` | `POST /v2/volumes`; `DELETE /v2/volumes/{volume_id}` |
| `attach_volume_secs`; `detach_volume_secs`; `report_attachment_state_secs`; `grow_volume_secs`; `move_backing_secs` | `POST /v2/volumes/{volume_id}/attach`; `/detach`; `/attachment-state`; `/grow`; `/move-backing` |
| `claim_device_secs`; `release_device_secs`; `lend_device_secs`; `reclaim_device_secs`; `resolve_quarantine_secs` | `POST /v2/admin/devices/{device_id}/claim`; `/release`; `/lend`; `/reclaim`; `/resolve-quarantine` |
| `adopt_volume_secs`; `clear_cut_marker_secs` | `POST /v2/admin/nearline/{volume_id}/adopt`; `/clear-cut-marker` |
| `prepare_migration_secs`; `transfer_migration_secs`; `abort_migration_secs` | `POST /v2/migrations`; `POST /v2/migrations/{migration_id}/transfer`; `/abort` |

A budget bounds only how long the HTTP response waits. On expiry, if the outcome is recorded the recorded result is returned; if the act is still
running the server answers `202` with the `Location` handle. It never answers `504 REQUEST_TIMEOUT` for a mutation and never cancels, compensates or drops the act.
`504` remains a read-only-route answer. For `transfer_migration_secs`, expiry returns the existing §5 `202 {state}` response, not a new operation handle.

### 2.4 Idempotency, restart and retention
Idempotency is unchanged: `operation_id` plus the exact immutable request hash. A terminal replay returns the recorded status and body, never `202`. A
different payload under the same id is `409 IDEMPOTENCY_CONFLICT`. An evicted or tombstoned operation answers `410 OPERATION_EXPIRED` and is never re-executed. An in-flight intent
without `Prefer` keeps today's `409 OPERATION_IN_DOUBT`; with `Prefer` it returns the existing `202` handle without re-executing.

On restart every outcome-less intent is `in_doubt` until replay or reconciliation records an outcome. The handle never resumes or restarts a provider
act, and an outcome-append failure after a successful act leaves the handle `in_doubt`, never falsely `succeeded`. Retention is the journal's; the operation
resource has no separate store and polling does not pin or extend an operation. In-flight intents remain retained under existing bounds (ADR-0014).

### 2.5 Compatibility
On the default configuration (empty budgets, no `Prefer`), an existing client sees no change: the mutation is byte-for-byte today's synchronous request
and response. The header, `202`, `Location` route and config table are additive. An old daemon ignores the header and never serves `GET`; a `Prefer`
client still receives the normal synchronous result (or `404` on `GET`). A budget is explicit operator opt-in for that route and can return `202` to a
client that did not send `Prefer`; enabling one is a deployment decision that requires clients to follow the handle. Contract acceptance is still
required; no compatibility is claimed before it.

## 3. Fail-closed and refusal rules

| Condition | Proposed answer |
|---|---|
| Unknown operation id | `404 NOT_FOUND`; no state invented |
| Evicted or tombstoned operation | `410 OPERATION_EXPIRED`; confirm the effect with a read before a new id |
| Different principal or project | `404 NOT_FOUND`; do not reveal another tenant's operation |
| Same principal, insufficient role | `403 FORBIDDEN` |
| Ownership binding missing or ambiguous | fail closed; do not serve unscoped state (§6.1) |
| Reserved `derived:` id | `400 INVALID_REQUEST`, before lookup |
| New intent cannot be journaled (`JOURNAL_FULL`) | `503 JOURNAL_FULL`; no act, no `202`, no handle |
| Outcome append fails after the act | no success state; handle reports `in_doubt` |
| Mutation response budget expires while running | `202` + handle; never `504`, never abort |

Knowing an `operation_id` is never authorization.

## 4. Proposed default scenarios — only when implemented
These IDs are proposals. Do not add them to [`docs/testing/default-integration-scenarios.md`](../docs/testing/default-integration-scenarios.md) until the implementation exists and passes the fast acceptance budget.

| ID | Proposed exact test name | What it asserts |
|---|---|---|
| OP-01 | `operations::tests::async_opt_in_returns_202_and_polls_to_the_recorded_outcome` | Parked act; `Prefer` returns `202` with `Location` and `in_progress`; `GET` reports `in_progress`; release; `GET` reports `succeeded` with recorded status/body. |
| OP-02 | `operations::tests::default_synchronous_response_is_unchanged` | No `Prefer`, no budget: today's status/body, no `Location`, no handle; terminal replay is byte-identical. |
| OP-03 | `operations::tests::mutation_response_budget_expiry_returns_a_handle_and_does_not_abort_the_act` | No `Prefer`, budget set; it elapses on a parked act: `202`, never `504`; act is not cancelled; release records the outcome; replay returns it. |
| OP-04 | `operations::tests::operation_replay_conflict_and_expiry_are_unchanged` | Same id/hash terminal replay is byte-identical; different hash is `409`; evicted id is `410`; no re-execution. |
| OP-05 | `operations::tests::a_restarted_outcome_less_intent_reports_in_doubt` | Intent without outcome across restart reports `in_doubt`, never `in_progress`/`succeeded`; retry does not re-execute. |
| OP-06 | `operations::tests::the_operation_resource_is_scoped_to_the_mutation_principal_and_project` | Same principal can read; another principal/project gets `404`; role mismatch `403`; `operation_id` alone grants nothing. |
| OP-07 | `operations::tests::operation_handle_refusals_are_fail_closed` | Unknown `404`, expired `410`, `derived:` `400`; `JOURNAL_FULL` on the mutation is `503` with no handle and no act. |

## 5. Alternatives considered
- **Long-poll only.** Rejected as the only model: it still ties observation to one connection, loses observation on disconnect, gives no stable resource
  for a later client, and holds a concurrency permit for the whole act.
- **Synchronous only.** Rejected: it leaves #45's gap and forces clients and proxies to choose between unbounded waits and abandoning journaled work.
- **Abort on wall-clock expiry.** Rejected: dropping or cancelling the act after the intent abandons a journaled mutation whose outcome is not recorded,
  violating AGENTS rule 8 and §7/§12. A response budget changes when the response is sent, not whether the act runs.
- **Client-side timeout only.** Rejected: it creates no server state, cannot distinguish running from interrupted, and encourages new operation ids for
  retries, risking duplicate acts.

## 6. Open questions
1. **Ownership binding.** Current `Intent`/`Outcome` records carry `operation_id`, request hash, kind and payload, but no principal, project or role. Scoping
   §2.2 requires durable work: extend the record or add a side index written atomically with the intent, with N-1 compatibility, migration/rollback
   and an ADR-0016 scenario. Until then, fail closed.
2. **`202` after completion.** Recommended: if the outcome is recorded before the `202` is committed or the budget expires, return the terminal
   result synchronously; never `202` for a terminal operation. The race resolution remains open.
3. **Cleanup and retention.** Recommended: no separate operation store in the first slice; journal retention governs, polling never pins, eviction
   answers `410`. A longer retention window is a separate decision because it grows durable state and changes the tombstone/fingerprint horizon.
4. **`Prefer` interactions.** Recommended: case-insensitive `respond-async` token, ignored unknown preferences, `Preference-Applied` only on `202`, and clients must
   accept a synchronous terminal response because proxies may strip the header. `Prefer` is not authorization or a guarantee of `202`.
5. **Detached work and admission.** Recommended: a detached act keeps its concurrency permit until the outcome is journaled. A separate detached-work
   cap, if needed, is an open implementation decision.
6. **Transfer route and unified observation.** The migration `transfer` route already has a §5 `202 {state}` and `GET /v2/migrations/{migration_id}`.
   Recommended: keep that shape and exclude it from `/v2/operations` in the first slice; a unified handle needs its own conformance edit to §5, §12 and
   `docs/admission-control.md`, and is not claimed here.

## 7. Non-claims
- No implementation is claimed: there is no `/v2/operations` route, no `Prefer` handling and no `[admission.mutation_response_timeouts]` parser here.
- This amendment is not accepted and does not change the normative contract.
- No production-support, SLA, latency, durability or completion guarantee is claimed; the handle reports journal state only.
- No provider act is claimed resumable after a crash; `in_doubt` is the honest state.
- The synchronous default, idempotency, retention, journal bounds and existing error semantics are unchanged.
- The OP scenarios are proposals, not catalog entries, and have not been run.
