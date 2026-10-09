# Volvisor P0 Implementation Plan — Control Plane Bootstrap

Status: Active implementation plan (P0 of SPEC-0002 section 12)
Date: 2026-10-09
Normative: [Volume API v2](../contracts/volume-api-v2.md), [SPEC-0002](../specs/SPEC-0002-volvisor-volume-virtualization.md), [Nearline contract v2](../contracts/nearline-replication-v2.md), [ADR-0003](../adr/0003-tiered-volume-virtualization.md), [AGENTS](../../AGENTS.md)

## 1. Goal

Implement **P0** of the SPEC-0002 section 12 sequence — *"stable IDs, device claiming,
single-volume API/attachments and provider conformance"* — plus the first vertical slice of
**P1** (native-local LVM provider), as industrial-grade Rust software:

- a typed, fail-closed domain model with zero stringly-typed states;
- a durable intent journal with crash-replay and idempotency (`operation_id` + immutable
  request hash);
- an engine-neutral provider abstraction with capability negotiation;
- a native-local LVM provider (thick LV first) with read-only discovery, explicit device
  claiming and journal-before-mutate semantics;
- an HTTP/JSON API server implementing CreateVolume, InspectVolume, ListVolumes,
  AttachVolume, DetachVolume, GrowVolume, DeleteVolume exactly per the Volume API v2
  contract, with generation checks, single-writer enforcement and typed errors;
- unit + integration + crash-replay tests and CI that fails on warning.

## 2. Non-goals (this phase)

- No nearline/DRBD provider, no Ceph adapter, no migration/handoff execution
  (`MIGRATION_UNSUPPORTED_LOCAL_STORAGE` must be returned fail-closed for native-local).
- No thin pools, no local mirror, no encryption, no snapshots/clones (capability flags
  absent → requests fail with `UNSUPPORTED_CLASS_OR_POLICY`, never silently ignored).
- No multi-host distribution: single-host daemon with a local journal. The journal format
  and state machine must be designed so a future coordinator can own them.
- No production-support claims. Everything is prototype evidence per AGENTS rule 12.

## 3. Language and stack

- Rust 2024 edition, pinned toolchain (`rust-toolchain.toml`).
- `axum` + `tokio` for the API server; `serde`/`serde_json` for contract-shaped JSON.
- `nix`/`rustix` for fsync/dirent/locking primitives; **no `unsafe` outside audited
  wrappers** (target: zero `unsafe` in this phase).
- `proptest` for state-machine properties, `tempfile` for test isolation.
- LVM is driven through a `CommandRunner` trait wrapping `lvm2` CLI
  (JSON output where available); unit tests run against a recorded/fake runner, real-LVM
  integration tests are gated behind `VOLVISOR_TEST_LVM=1` + root + loop devices.
- CI: fmt --check, clippy -D warnings, test, doc, cargo-audit/deny; MSRV 1.85 (the
  edition-2024 minimum) enforced by a dedicated CI job against the committed lockfile.

## 4. Workspace layout

```text
Cargo.toml               workspace (resolver 3)
rust-toolchain.toml
deny.toml  rustfmt.toml  .clippy.toml
.github/workflows/ci.yaml
crates/
  volvisor-types/        domain vocabulary: the single source of truth for IDs, states,
                         errors, capabilities, request/response types, domain records
  volvisor-journal/      durable append-only intent journal + idempotency registry
  volvisor-provider/     VolumeProvider trait, capability negotiation, fake provider,
                         provider conformance test kit
  volvisor-lvm/          native-local LVM provider (discovery, claiming, volumes)
  volvisor-api/          axum HTTP/JSON server: contract surface, validation, routing
  volvisord/             daemon binary: config, wiring, startup replay
docs/plans/2026-10-09-p0-implementation.md   (this file)
```

Dependency direction: `api`, `lvm` → `provider` → `types`; `journal` → `types`;
`volvisord` → all. No cycles.

## 5. Crate contracts

### 5.1 volvisor-types (foundation — implemented first, by the lead)

- **Opaque IDs**: newtypes `VolumeId`, `PoolId`, `DeviceId`, `AttachmentId`, `OperationId`,
  `ProjectId`, `HostId`, `MigrationId` — non-empty, charset-validated, no `/` or control
  chars; refuse to construct from raw Linux device names, BDF or Ceph image names
  (documented, runtime-validated length/charset only).
- **States** (serde SCREAMING_CASE, exactly the canonical vocabularies):
  - Volume: `Requested, Provisioning, Ready, Degraded, Attaching, Attached, Detaching,
    Deleting, Failed, Quarantined` (API v2 section 7).
  - Migration: `PREPARED, PRECOPY, QUIESCED, BARRIER_DURABLE, SOURCE_REVOKED, IN_DOUBT,
    DESTINATION_AUTHORIZED, VM_RESUMED, COMPLETE, ABORTED` (nearline section 6 —
    enum exists from day one so no later rename).
  - Online move: `PREPARING, COPYING, MIRROR_READY, PIVOTED, COMPLETE, FAILED, IN_DOUBT`
    with `FAILED` only legal before `PIVOTED` (invariant tested).
- **Typed errors**: `ApiError` enum with every code from API v2 section 7
  (`UNSUPPORTED_CLASS_OR_POLICY` … `CEPH_CLUSTER_UNHEALTHY`) + `STALE_GENERATION`,
  `IDEMPOTENCY_CONFLICT`; mapped to HTTP 4xx/5xx and a machine-readable JSON body.
  Unknown backend conditions map to `unknown` health, never to a healthy default.
- **Domain records**: `PhysicalDevice`, `Pool`, `Volume`, `Attachment`, `Replica`,
  `Migration` per SPEC-0002 section 2 (opaque IDs, generations, `data_epoch`,
  `effective_protection` with independent local/remote axes).
- **Requests/responses**: CreateVolumeRequest (exact contract fields incl.
  `api_version: "volvisor.volume.v2"` validation), InspectVolumeResponse (contract
  section 2 field list), Attach/Detach (expected generations, access_mode,
  idempotency), GrowVolume, DeleteVolume. Requested-vs-effective policy pairs.
- **Capabilities**: bitflag-like set from API v2 section 8. This phase advertises only
  `create`, `attach`, `resize` (grow), for the native-local provider.
- Property tests: state transitions only along legal edges; error codes round-trip.

### 5.2 volvisor-journal

- Append-only file, length-prefixed + CRC32C framed records, `fsync` before
  acknowledging a write; atomic truncate/rotate on startup after successful replay.
- Record types: `Intent { operation_id, request_hash, op }`, `Outcome { operation_id,
  result }`, `Checkpoint`. Idempotency registry derived from replay: same
  `operation_id` + same hash → replay recorded response; same ID + different hash →
  `IDEMPOTENCY_CONFLICT` (fail closed, contract section 7).
- Journal-before-mutate: providers may only perform destructive actions after the
  intent record is durable; the daemon refuses to start mutations otherwise
  (enforced by API layer ordering, tested by fault injection).
- Single-writer: lock file (flock) on the journal directory; second daemon fails fast.
- Crash-replay tests: at every record boundary, kill (simulated by re-opening after
  partial writes incl. torn last record), replay must converge to a consistent state
  with no fabricated attachments (AGENTS rules 4/8; API section 3 "a crash must not
  fabricate a second attachment").

### 5.3 volvisor-provider

- `trait VolumeProvider`: async `create_volume`, `inspect_volume`, `attach`, `detach`,
  `grow`, `delete`, `capabilities`, `discover` (read-only), `claim_device`,
  `release_device`. Attach returns a **host-scoped, ephemeral backend handle** —
  never secrets (API section 3).
- Every mutation takes `expected_generation`; stale → `STALE_GENERATION`.
- `FakeProvider` (in-memory, deterministic, fault-injectable) used by API tests and the
  **provider conformance kit**: a shared `#[test]` suite every provider must pass
  (idempotent create, generation fencing, single-writer, detach-drain, delete
  preconditions, unknown-health truthfulness).

### 5.4 volvisor-lvm (native-local, P1 slice)

- Read-only discovery: `lsblk --json` + `/dev/disk/by-id` resolution; stable identity =
  serial + NVMe NGUID/EUI when present; **never** `/dev/nvmeXnY` or BDF as identity
  (SPEC-0002 section 3). No signature scanning beyond read; no wipe on discovery.
- Device claim: journal intent → verify identity/ownership/foreign signatures absent →
  `pvcreate` + `vgcreate` under a scoped destructive-authorization token
  (config-provided; CLI enforces "never on unclaimed device").
- Volume: thick `lvcreate` on the claimed VG; volume→LV cross-reference persisted in
  the journal (backend_private_ref is provider-internal, never exposed to tenants).
- Attach: no real VMM in this phase — attach records the attachment, enforces exactly
  one active writable attachment, and returns the host device path as the ephemeral
  handle with `prepared` state (advertised/active are VMM-integration states, reported
  honestly as not-yet-implemented in `evidence_status`).
- Grow: `lvextend` (grow-only; shrink requests rejected), verify actual size via
  `lvs`, report `guest_notification_status: NotApplicable` honestly.
- Delete: only when fully `Detached`, correct generation, no dependents;
  `lvremove` + ownership reconciliation; foreign/mismatched LVM state → `Quarantined`,
  never auto-adopted (AGENTS rule 7). A detach drain-grace timer is deferred to the
  VMM-integration milestone: with `prepared`-only attachments no guest I/O can be in
  flight, so the contract's fully-detached precondition (API v2 section 4) is exactly
  satisfied today.
- Unit tests against a fake command runner; integration tests (env-gated) create a
  loop-backed PV/VG, run the full lifecycle, and simulate crash-replay.

### 5.5 volvisor-api + volvisord

- Endpoints (JSON, contract-shaped field names):
  `POST /v2/volumes` (CreateVolume), `GET /v2/volumes` (List),
  `GET /v2/volumes/{id}` (Inspect), `POST /v2/volumes/{id}/attach`,
  `POST /v2/volumes/{id}/detach`, `POST /v2/volumes/{id}/grow`,
  `DELETE /v2/volumes/{id}`, `GET /v2/healthz` (daemon liveness; **not** volume
  health), `GET /v2/capabilities`.
- Every mutation: validate `api_version`, resolve idempotency via journal, check
  expected generation, journal intent, execute provider op, journal outcome.
- Fail-closed validation: unsupported class/policy/field → `UNSUPPORTED_CLASS_OR_POLICY`
  with the offending field named; unknown fields rejected, not ignored.
- `volvisord`: config file (paths, provider selection, auth token for admin ops),
  startup = lock journal → replay → reconcile provider state → serve. Structured JSON
  logs (no secrets — SPEC-0002 section 9), `/metrics` with Prometheus counters.

## 6. Milestones

| M | Content | DoD |
|---|---|---|
| M0 | Plan merged; workspace scaffold (crates, toolchain, CI skeleton) | `cargo build` + `ci.yaml` green on empty crates |
| M1 | `volvisor-types` complete | property tests + serde round-trip; zero warnings |
| M2 | `volvisor-journal` complete | torn-write/crash-replay tests pass; idempotency conflicts detected |
| M3 | `volvisor-provider` + fake + conformance kit | fake passes conformance suite |
| M4 | `volvisor-api` on fake provider | full API integration tests incl. idempotency + generation fencing |
| M5 | `volvisor-lvm` native-local slice | unit suite green; env-gated LVM lifecycle test green on this host |
| M6 | `volvisord` + docs + CI hardening | end-to-end daemon test; PR reviewed |

## 7. Testing and evidence strategy

- **Unit**: every state machine edge, error mapping, ID validation.
- **Property (proptest)**: random legal operation sequences on the fake provider; no
  invariant (single writer, generation monotonicity, no fabricated attachments,
  FAILED-only-before-pivot) ever violated.
- **Crash replay**: journal fault injection at every record boundary.
- **Integration (env-gated)**: real LVM on loop devices (requires root; skipped
  automatically otherwise) — satisfies "crash replay at every mutation" style checks at
  the control-plane level; full real-host matrix per SPEC-0002 section 11 remains a
  later, separately recorded gate.
- **Honesty checks**: tests assert `unknown` (never healthy) for unproven states;
  `MIGRATION_UNSUPPORTED_LOCAL_STORAGE` for native-local migration requests.

## 8. Risks

| Risk | Mitigation |
|---|---|
| LVM CLI output drift across versions | pin parsed JSON fields; version probe at startup; integration tests gate |
| Torn journal writes on real power loss | CRC framing + fsync + replay-tolerant truncation, tested |
| Scope creep into P2/P3 | capability flags stay minimal; unsupported → typed rejection |
| Concurrent daemon corruption | flock single-writer; startup replay reconciles |
| Contract drift | types crate is generated-by-hand mirror of the contract; conformance kit asserts contract field names verbatim |

## 9. Explicit compliance mapping (AGENTS non-negotiables)

- r1/r7: identity newtypes + read-only discovery + no foreign adoption (M1/M5).
- r4/r8: journal-before-mutate, idempotent + generation-fenced mutations (M2/M4).
- r6/API§3: single-writer attachment enforcement incl. crash replay (M2/M4).
- r12: no production claims; `evidence_status` field exists and reports honestly.
- r14/§4A: grow-only resize; online-move API surface present but returns
  unsupported for LVM provider until separately qualified.

## 10. Review round 1 (2026-10-09) — findings disposition

Adversarial review of PR #3 produced six findings (F1-F6) and seven nits; all were
fixed in the same PR:

- F1 extent rounding: LVM rounds LVs up to the physical-extent boundary; create/grow
  now treat `actual >= requested` as success, persist and report the **effective**
  (rounded) size, and keep the requested size for idempotent-create replay
  comparison. No orphaned LVs; conformance fixtures stay extent-aligned.
- F2 delete of `Failed` volumes: an absent LV is skipped instead of failing
  `lvremove`, so a `Failed` volume can always be deleted and its pool released.
- F3 claim/release journaling: device claim/release is exposed as an
  `AdminSurface` trait routed through the same journal pipeline (durable intent
  before the destructive `pvcreate`/`vgcreate`); release failure leaves state
  matching observed reality with a remediation hint instead of error-looping.
- F4 fail-closed admin auth: mutating endpoints are rejected when no
  `admin_token` is configured, and non-loopback binds without a token are
  refused at startup.
- F5 journal confidentiality: journal, lock and state files are mode 0600, and
  journaled payloads redact credential-bearing fields (`encryption.key_ref`,
  `authorization_token`).
- F6 test realism: the LVM fake now mirrors real LVM behavior (missing-LV
  `lvremove` failure, name-collision failure, extent rounding); an HTTP-level
  concurrent same-`operation_id` test, a journal-append fault-injection test
  (feature `test-faults`) and a proptest state-machine suite were added.
- Nits: LV names are injective (sanitized id + hash suffix, never dash-leading),
  LVM commands carry a watchdog timeout, `RealRunner` no longer stalls unbounded,
  MSRV is CI-enforced, cargo-deny is version-pinned, and the drain-grace deferral
  is documented in section 5.4.

### Recorded follow-ups (accepted, out of P0 scope)

- **Volume-id ABA**: deleting and re-creating a volume with the same id restarts
  generation numbering at 1. A stale client certificate from a prior incarnation
  could match the new one. Mitigation candidate: per-id generation tombstones.
- **Journal compaction**: the append-only log grows without bound in P0.
- **Blocking command execution**: LVM commands run on the async runtime threads,
  bounded by the watchdog timeout; move to `spawn_blocking` when contention is
  measured.
- **Admin surface coverage in conformance kit**: claim/release semantics are
  tested per-provider, not yet by the shared kit.
