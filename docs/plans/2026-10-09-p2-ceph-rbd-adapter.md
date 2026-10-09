# P2 implementation plan — external Ceph RBD adapter (`ceph-rbd`)

Date: 2026-10-09
Status: implementing
Branch: `feat/p2-ceph-rbd-adapter`
Normative: [ADR-0005](../adr/0005-ceph-rbd-and-managed-osds.md) Phase A,
[SPEC-0002](../specs/SPEC-0002-volvisor-volume-virtualization.md) §12 P2 slice,
[Volume API v2](../../../contracts/volume-api-v2.md), AGENTS rules 1–21.

## 1. Scope

In (P2 slice: "existing-Ceph RBD adapter with shared-backend attach/migration
validation"):

- A `volvisor-ceph` crate implementing `VolumeProvider` for `VolumeClass::CephRbd`
  against an **existing, externally operated** Ceph cluster, driven through the
  CLI (`ceph`, `rbd`) exactly like the LVM provider drives `lvm2`: argv arrays,
  no shell, watchdog-bounded runner, JSON output parsed permissively.
- Ownership proof for every mutation: injective image names plus an immutable
  `volvisor` ownership record in RBD image metadata, verified before any
  destructive action; never adopt foreign images (AGENTS rule 7).
- Single-writer attach via the RBD `exclusive-lock` image feature; attach maps
  the image (`rbd map`) and returns the `/dev/rbd/...` device as the ephemeral
  host-scoped handle; detach unmaps after verifying the mapping.
- Honest capacity (`ceph df` pool statistics), honest health reflection
  (`ceph health`: OK→Healthy, WARN→Degraded, ERR→Unhealthy, query
  failure→Unknown — never fabricated), grow-only resize (`rbd resize`).
- Erasure policy: `Retain` → `rbd trash move` (recoverable), `ZeroDiscard` →
  typed `UNSUPPORTED_CLASS_OR_POLICY` (Ceph reclaim does not guarantee
  block-level zeroing; fail-closed policy negotiation instead of a false
  claim — API v2 §4D).
- Daemon selection: `provider = "ceph"` with fail-closed startup verification
  (cluster FSID match, pool exists and is queryable, credentials usable);
  refuse to start otherwise.
- `FakeCeph` deterministic world + full provider conformance-kit pass; unit
  tests mirror real CLI failure modes (missing image, name collision, lock
  held, map/unmap races). Real-cluster integration tests behind
  `VOLVISOR_TEST_CEPH=1` (no cluster exists in this environment or CI; the
  gate exists for real hosts, mirroring `VOLVISOR_TEST_LVM`).

Out (recorded follow-ups):

- Managed OSD/Rook cells (ADR-0005 Phase B / ADR-0008) — separately gated.
- `librbd` frontend, `rbd-nbd`, live-migration handoff proofs, snapshot/clone,
  per-tenant Ceph credentials, trash GC (expired-image purging), feature
  verification on existing images at adoption (P2 adopts on ownership
  metadata alone).
- CI job for real-cluster tests (no cluster available; the env-gated test
  path is the hook).

## 2. Crate structure

- `CommandRunner`/`RealRunner`/`FakeRunner`/`CommandOutput` move from
  `volvisor-lvm` to `volvisor-provider` (`runner` module) so both engine
  adapters share one command-execution discipline (watchdog, pipe draining,
  no-shell, timeouts). `volvisor-lvm` re-exports them; no public API break.
- New member `volvisor-ceph`: `runner` usage, `state` (durable JSON, 0600,
  atomic save), `report` (permissive JSON parsing of `ceph`/`rbd` output),
  `provider` (the `VolumeProvider` impl), `tests/common` (FakeCeph world),
  conformance via the shared kit macro.

## 3. Command set (all argv; never shell)

- `ceph fsid` — startup identity verification against configured FSID.
- `ceph health detail --format json` — health reflection.
- `ceph df --format json` — pool usable/RAW statistics for capacity.
- `rbd create --image-feature exclusive-lock,layering -s <size>B <pool>/<image>`
- `rbd info --format json <pool>/<image>` — size verification (feature
  verification on adoption is a recorded follow-up, not part of P2).
- `rbd ls --pool <pool> --format json`; `rbd ls --pool <pool> --lsv2 --format json`
  where metadata is needed (or `rbd image-meta get/set <pool>/<image> <key>`).
- `rbd resize -s <size>B <pool>/<image>` (grow-only by construction — the
  branch runs only when actual < new, and `rbd` independently refuses a
  shrink; note `--allow-shrink` is a boost bool switch, so the
  `--allow-shrink=false` argv form is a parse error on real clusters).
- `rbd trash move` / `rbd trash ls` (Retain erasure).
- `rbd map --image <image> --pool <pool>` / `rbd unmap <device>` /
  `rbd showmapped --format json`.

## 4. Ownership and identity (ADR-0005)

- Image name = `vol-{sanitized-volume-id-truncated}-{hash8}` (same injective
  scheme as LVM, never dash-leading, ≤113 chars).
- On create: after `rbd create`, set image metadata
  `volvisor.owner=<volume_id>`, `volvisor.generation=<n>` via
  `rbd image-meta set`; verification reads them back before state is
  persisted. An image whose metadata cannot be set/read back is removed
  (best-effort `rbd rm`) and the create fails honestly.
- Before every mutation: `rbd info` + `rbd image-meta get volvisor.owner` must
  match the state entry. Mismatch or missing metadata on an image we have
  state for → volume marked `Failed`, never auto-adopted or silently removed;
  reconcile also clears the stale attachment record such a volume may carry
  (the backing it referenced is gone or no longer provably ours — a lingering
  record would wedge detach/delete forever) while never touching an actual
  device, and preserves the cleared record (device name, whether a zombie
  mapping remains) in the reconcile report as the audit trail. Images in our
  pool **without** our metadata are foreign: listed by discovery
  as foreign, never touched (rule 7).
- Volume state keeps `requested_size_bytes` + effective `size_bytes` (RBD
  sizes are byte-granular — no extent rounding — but the requested size is
  still recorded for idempotent-create replay comparison, mirroring LVM).

## 5. Single writer and attach (contract §3)

- Every volvisor-owned image is created with `exclusive-lock`; verifying
  that an existing image carries the feature before adopting it is a
  recorded follow-up (adoption is decided by ownership metadata alone in
  P2).
- Attach: verify ownership → `rbd map` → verify the device appears in
  `rbd showmapped` for exactly this image → record the mapping as the
  host-scoped ephemeral handle with `prepared` state (no VMM integration in
  P2; advertised/active honestly not implemented).
- Detach: verify the mapping exists and belongs to us → `rbd unmap` → verify
  gone. A second attach with a recorded attachment is the contract's typed
  `WRITER_ALREADY_ACTIVE` rejection; a stale unrecorded mapping is a typed
  `INVALID_STATE` rejection; crash-recovered stale mappings are detected via
  `showmapped` at reconcile and reported honestly. Read-only (shared-reader)
  attachments are rejected with a typed `UNSUPPORTED_CLASS_OR_POLICY`: the
  prototype has no qualified multi-reader contract, and a `--read-only`-less
  mapping recorded as read-only would be a fail-open lie.
- Delete of an owned image also refuses a typed `INVALID_STATE` while any
  live mapping serves the full pool/image spec without a volvisor
  attachment record (`rbd trash move` succeeds on an in-use image on a
  real cluster — the in-use check lives in trash purge, not the move);
  the remedy is the operator's manual `rbd unmap`. A delete replay that
  finds the image already in the trash (our own half-finished move)
  completes instead of wedging; image names are injective in the pool,
  so a trash hit is indistinguishable from — and operationally
  equivalent to — our own replay: only the state entry is dropped,
  never the trashed image.
- Shared-backend note: a `ceph-rbd` volume does not pin the volume to one
  host the way a local LV does — placement constraints reflect that the
  cluster (not a host) backs the volume — but migration eligibility is still
  reported as unproven (no VMM integration, `evidence_status: PrototypeOnly`).

## 6. Capacity and health honesty

- Capacity check: `ceph df` → pool `bytes_used`/`max_avail`; create/grow
  compare against `max_avail` minus a headroom and reject with typed
  `NO_SAFE_CAPACITY`. RBD quotas are not set in P2.
- Health: `ceph health detail` mapped OK→`Healthy`, WARN→`Degraded`,
  ERR→`Unhealthy`; query failure → `Unknown`. Never convert Ceph's health into a
  Volvisor-made durability guarantee (ADR-0005): pool protection is reported
  from the pool policy query (`ceph osd pool get <pool> size`/`min_size`) as
  `RemoteProtectionAxis` policy facts, not as replication promises;
  erasure-coded pools are not specially interpreted in P2 (their `size`
  reflects the EC profile's k+m, and decoding EC semantics is a follow-up).

## 7. Daemon and config

- `provider = "ceph"` with: `ceph_cluster_fsid`, `ceph_mon_hosts` (1..=9
  entries), `ceph_pool`, `ceph_user` (default `client.volvisor`),
  `ceph_state_path` (durable state, default `<journal_dir>/ceph-state.json`).
  Credentials are resolved entirely by the `ceph`/`rbd` CLIs from the host's
  standard keyring/configuration conventions — volvisor passes only `--name`
  (the full entity, e.g. `client.volvisor`) and `-m` and never reads, stores or
  logs key material.
- Startup: FSID must match configuration exactly; pool must exist; a health
  query must succeed. Any mismatch → refuse to start (fail-closed; a
  mis-pointed cluster must never be adopted).
- No `AdminSurface` (external cluster; no device claiming) — admin routes
  return the existing typed 404.

## 8. Milestones

- M0: runner move to `volvisor-provider` (mechanical, lvm re-export; workspace
  stays green).
- M1: `volvisor-ceph` core: state, report parsing, provider impl
  (create/inspect/list/attach/detach/grow/delete/reconcile).
- M2: FakeCeph world, conformance-kit pass, unit tests incl. failure
  injection (foreign image, lock held, missing image, map races, trash),
  `VOLVISOR_TEST_CEPH=1` integration-test path.
- M3: daemon config + wiring + e2e (fake path), README/config example.
- M4: adversarial review → fix → merge loop (PR #4), as for PR #3.

## 9. Compliance mapping (AGENTS non-negotiables)

- r7: read-only discovery; foreign images never adopted or removed.
- r8: same journal pipeline (durable intent before mutation, idempotent,
  fail-closed on replay conflicts) — inherited from the API layer unchanged.
- r9: RBD images are volumes; no OSD management anywhere in P2.
- r3/r4: single-writer via exclusive-lock; no fencing weakening for tests.
- r12: `evidence_status: PrototypeOnly`; no production-support claims; no
  RPO/durability claims beyond reporting Ceph's own health/policy facts.
- r13: no third-party source reuse; only unmodified `ceph`/`rbd` CLIs.
