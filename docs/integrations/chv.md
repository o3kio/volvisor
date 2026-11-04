# Cloud Hypervisor (CHV) consumer integration

Status: **Volvisor-side consumer contract and conformance test implemented;
no supported CHV/Volvisor pair is pinned.**

CHV must not move its Volvisor pin yet. No tagged Volvisor release exists
([#47](https://github.com/o3kio/volvisor/issues/47)), and the recorded
decision is to wait for a signed, tagged release before pinning
([CHV pin decision](../releases/v0.1.0.md#chv-pin-decision)). This guide
documents the Volvisor-side behavior a CHV consumer can rely on today; it is
not a statement that the pair is released, supported, or production-ready.

The Volvisor-side sequence is exercised by
[`crates/volvisor-api/tests/chv_consumer.rs`](../../crates/volvisor-api/tests/chv_consumer.rs)
through the real router and a real on-disk journal.

## Capability negotiation

The consumer begins with:

```http
GET /v2/capabilities
```

```json
{
  "provider": "lvm-native-local",
  "capabilities": [
    "attach",
    "attachment_state_report",
    "create",
    "resize"
  ],
  "supported_classes": ["native-local"]
}
```

The exact token is `attachment_state_report`. Its presence means the
provider implements section 3's consumer evidence reporting. Its absence means
the provider does not, and `POST /v2/volumes/{volume_id}/attachment-state`
answers a **typed `404 NOT_FOUND`** (the provider trait's default behavior).

When the token is absent:

- treat reporting as **capability absent**, not as a crash, a transport error
  or an indefinite retry loop;
- do not report `advertised`/`active`; reconcile from `GET /v2/volumes/{id}`
  instead;
- a repeated report with the same `operation_id` replays the recorded typed
  404 verbatim (it is a journaled terminal outcome).

In this revision the token is implemented by the `native-local` LVM provider
and the conformance fake. `ceph-rbd` and `nearline-replicated` do not advertise
it and answer the route's typed 404.

## Attachment state machine and evidence

The contract ranks evidence `prepared < advertised < active`. State may only
move **up**; an equal or lower report is a successful idempotent no-op
(`applied=false`) carrying the current projection. There is no downgrade.

| State | Who may set it | Required evidence | CHV v1 |
|---|---|---|---|
| `prepared` | Volvisor/provider at attach | provider-recorded; no consumer evidence | Consumes it |
| `advertised` | Consumer report | `vmm_configured` or `guest_confirmed` | **Ceiling** |
| `active` | Consumer report | `guest_confirmed` only | **Never claimed** |

- `prepared` is provider-recorded. A consumer never reports it (a `prepared`
  target is `INVALID_REQUEST`).
- `advertised` means the consumer installed the returned frontend into its
  VMM. `vmm_configured` is the honest basis for that; `guest_confirmed` is
  also accepted because it is stronger.
- `active` requires a **positive guest-side observation**
  (`guest_confirmed`). `active` + `vmm_configured` is an unsupported pairing
  and is a typed `400 INVALID_REQUEST`.
- CHV v1 stops at `advertised`. Reporting `active` from CHV v1 is a contract
  violation; `active` needs a separate accepted guest-evidence design.
- A report is an **attestation**. Volvisor records it verbatim and never
  promotes it to a provider-verified fact or a liveness signal.
- A report never advances `attachment_generation`; that token remains the
  detach fence. Ordering uses `state_revision`, not wall-clock time.

## Request and response examples

Attach first; the response carries the frontend and the provider-recorded
state:

```json
{
  "attachment_id": "att-chv-1",
  "attachment_generation": 1,
  "volume_generation": 2,
  "frontend": {
    "virtio_blk": {
      "host_device_path": "/dev/volvisor/vol-chv-1"
    }
  },
  "state": "prepared"
}
```

Report the configured frontend:

```http
POST /v2/volumes/vol-chv-1/attachment-state
Content-Type: application/json
```

```json
{
  "api_version": "volvisor.volume.v2",
  "operation_id": "chv-op-advertised-1",
  "attachment_id": "att-chv-1",
  "expected_attachment_generation": 1,
  "state": "advertised",
  "evidence": "vmm_configured",
  "reporter": "chv",
  "detail": "virtio-blk frontend configured in CHV"
}
```

```json
{
  "attachment_id": "att-chv-1",
  "state": "advertised",
  "state_revision": 2,
  "evidence": "vmm_configured",
  "reporter": "chv",
  "recorded_at_ms": 1762300000000,
  "attachment_generation": 1,
  "applied": true
}
```

Re-observe through inspect rather than replaying the old report:

```json
{
  "attachment_id": "att-chv-1",
  "vm_id": "vm-1",
  "host_id": "host-a",
  "access_mode": "single_writer",
  "attachment_generation": 1,
  "frontend": {
    "virtio_blk": {
      "host_device_path": "/dev/volvisor/vol-chv-1"
    }
  },
  "state": "advertised",
  "state_revision": 2,
  "evidence": "vmm_configured",
  "reporter": "chv",
  "recorded_at_ms": 1762300000000
}
```

`detail` is optional, bounded to 1024 bytes, and journal-only diagnostic
context. `reporter` is an untrusted label, bounded to 1024 bytes; it is never
authentication and must not contain control characters.

## Stale generations and idempotent replay

`expected_attachment_generation` is the attachment-instance precondition:

- a mismatch is a typed `409 STALE_GENERATION`; the report is journaled as a
  terminal outcome and **does not move state**;
- a report after detach is a typed `404 NOT_FOUND` for a new `operation_id`;
- a consumed attachment id cannot be re-attached; that is
  `IDEMPOTENCY_CONFLICT`, so a delayed report can never label a new instance.

On `STALE_GENERATION`, stop reporting for that instance. Re-read
`attachments[]` and re-attach with a fresh attachment id if the VM still needs
the volume. Do not retry the stale request under a new operation id expecting
it to apply.

Every mutation is idempotent by `operation_id` + exact immutable request hash:

- retry the **same** request (same `operation_id`, same payload) to get the
  recorded outcome verbatim, including across a daemon restart and even after
  a later detach;
- reusing the same `operation_id` with a different payload is
  `IDEMPOTENCY_CONFLICT`;
- do not mint a new `operation_id` to "retry" a report whose outcome is
  unknown; first re-observe with `GET /v2/volumes/{volume_id}`;
- a retry whose operation was evicted by journal retention answers
  `OPERATION_EXPIRED` (HTTP 410); confirm the prior effect with a read and
  submit a new operation id.

The conformance test proves journal replay across a dropped and rebuilt
`AppState` over the same journal. Its fresh `FakeProvider` has no rebuilt
in-memory attachments; the replay is served from the journal's recorded
outcome, not reconstructed provider state. A production daemon rebuilds
provider state by reconcile, which this test does not claim to cover.

## Version matrix

| CHV version | Volvisor version / commit | Capability token | Status |
|---|---|---|---|
| CHV v1, current pin | Revision predating `attachment_state_report`; not a released/tagged build | Not advertised by that pin | **No supported pair. Pin must not move.** CHV must not report above what its pin supports. |
| CHV v1 | Phase A baseline `aad1720` (development) | `attachment_state_report` | **Development/CI only — not a pin, not a release.** The Phase B consumer contract test (added after this commit) passes against this product code. |
| CHV v1 | Any tagged Volvisor release | — | **None pinned.** No tagged Volvisor release exists ([#47](https://github.com/o3kio/volvisor/issues/47)); the pin waits for one. |
| CHV guest-evidence design (future) | — | `attachment_state_report` plus an accepted guest-observation design | **Not implemented, not claimed.** `active` remains unavailable to CHV v1. |

`aad1720` is the Phase A baseline commit, cited only to identify the
development revision under test. It is **not** a release, a tag, or a pin, and
it confers no support status. Phase B adds tests and documentation on top of
it; neither state is a pinnable release.

## Explicit non-claims

- **No production support, SLA, availability or durability claim.** Nothing
  here is production readiness.
- **No zero-RPO claim.** Asynchronous replication can lose the newest
  acknowledged tail on host loss; a witness is authority, not data
  replication.
- **No live migration.** CHV v1 does not claim cross-host VM live migration
  through this integration.
- **No CHV `active` claim.** CHV v1's ceiling is `advertised`; `active`
  requires guest evidence and is not implemented or claimed by CHV v1.
- **No hardware, real-VMM or signed-release evidence.** The conformance test
  uses the in-process router and a fake provider; it is not real media, a real
  Cloud Hypervisor process, or a released artifact.
- **No pinned CHV/Volvisor pair.** The pin update is a conscious change in the
  CHV repository, after a tagged Volvisor release exists; this repository does
  not edit CHV.

## References

- [Volume API v2 contract, section 3](../../contracts/volume-api-v2.md)
  (`AttachVolume` / `DetachVolume` / `ReportAttachmentState`) and
  [section 8](../../contracts/volume-api-v2.md) (capability tokens).
- [CHV consumer conformance test](../../crates/volvisor-api/tests/chv_consumer.rs).
- [CHV pin decision](../releases/v0.1.0.md#chv-pin-decision).
- [Provider authoring guide](../provider-authoring.md) for the shared
  semantics and conformance/parity suites.
- [Support matrix](../../SUPPORT_MATRIX.md) for the honest claim boundary.
