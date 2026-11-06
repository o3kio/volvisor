# Volvisor architecture

Status: describes the **implemented prototype** in this workspace. It is not
a production-support statement, an SLA, or evidence that any failure scenario
has been run. Read it with [SUPPORT_MATRIX.md](SUPPORT_MATRIX.md) and
[docs/releases/pre-production-readiness.md](docs/releases/pre-production-readiness.md).

This page is the map, not the normative rulebook. The v2 ADRs, SPEC-0002 and
the v2 contracts are authoritative; if this page disagrees with them, they win.
ADR-0001/0002, SPEC-0001 and the v1 contracts are superseded historical drafts
(see [AGENTS.md](AGENTS.md)).

## 1. Crate map and dependency direction

The workspace has 13 members (verified against
`cargo metadata --no-deps --format-version 1`). Every crate is a library;
`volvisord` also ships the two daemon binaries, and `volvisor-campaign` ships
the `campaign-summary` evidence binary.

| Crate | Kind | Responsibility | Direct internal dependencies |
|---|---|---|---|
| `volvisor-types` | library | Typed domain vocabulary: opaque identities, canonical state vocabularies, error taxonomy, capabilities, request/response shapes, schema probes. | none |
| `volvisor-journal` | library | Durable intent/outcome journal with bounded retention and snapshot compaction, replay-derived idempotency registry, single-writer `flock`. | `volvisor-types` |
| `volvisor-provider` | library | Engine-neutral `VolumeProvider` trait, shared semantics, fake provider, conformance kit, admin/lending/handoff/VMM/grow seams. | `volvisor-types` |
| `volvisor-lvm` | library | `native-local` provider over host LVM: discovery, claiming, thick LVs, attachments, grow, same-VG `pvmove`. | `volvisor-types`, `volvisor-provider` |
| `volvisor-ceph` | library | `ceph-rbd` adapter for an existing external Ceph cluster: FSID/pool checks, RBD mapping, `exclusive-lock`, trash policy. | `volvisor-types`, `volvisor-provider` |
| `volvisor-drbd` | library | Local end of the `nearline-replicated` DRBD 9 resource: generated `.res`, lifecycle, roles, authority, state. | `volvisor-types`, `volvisor-provider`, `volvisor-witness` |
| `volvisor-witness` | library | Third-party writer-authority service core: journal-backed epoch/lease registry, witness HTTP protocol, client adapters. | `volvisor-types`, `volvisor-journal`, `volvisor-tls` |
| `volvisor-handoff` | library | Engine/transport-neutral migration state machine, per-migration durable record store, forward-only cut and `IN_DOUBT` reconcile. | `volvisor-types` |
| `volvisor-api` | library | Volume API v2 router: admission, auth/audit, handlers, journal-before-mutate pipeline, admin/peer/migration surfaces. | `volvisor-types`, `volvisor-journal`, `volvisor-provider`, `volvisor-witness`, `volvisor-handoff` |
| `volvisor-tls` | library | Shared rustls server/client configuration for the API and witness daemons. | none |
| `volvisord` | library + binaries | Composition root and daemons: `volvisord` and `volvisor-witnessd`; config, startup order, provider wiring, background retry/renewal tasks. | `volvisor-types`, `volvisor-journal`, `volvisor-provider`, `volvisor-lvm`, `volvisor-ceph`, `volvisor-drbd`, `volvisor-witness`, `volvisor-api`, `volvisor-handoff`, `volvisor-tls` |
| `volvisor-drbd-testkit` | test-only library | Stateful fake DRBD+LVM world shared by DRBD and daemon tests, including a modelled content/data path. | `volvisor-types`, `volvisor-provider`, `volvisor-drbd` |
| `volvisor-campaign` | test-only library + `campaign-summary` binary | Aggressive failure campaign harness, write-trace oracle, evidence records, Tier S/R/V scaffolding. | `volvisor-api`, `volvisord`, `volvisor-provider`, `volvisor-drbd`, `volvisor-drbd-testkit`, `volvisor-handoff`, `volvisor-lvm`, `volvisor-journal` (test-faults), `volvisor-types`, `volvisor-witness` |

Dependency direction is intentionally acyclic:

```text
                         volvisor-types
                    /          |          \
          volvisor-journal  volvisor-provider  volvisor-tls
                 |          /      |      \          |
          volvisor-witness  /   volvisor-lvm \        |
                 |        /    volvisor-ceph  \       |
                 |       /     volvisor-drbd --+------+
                 |      /            |
                 \ volvisor-handoff  |
                      \      |       |
                       volvisor-api |
                              \     /
                              volvisord
                                   |
                  +----------------+----------------+
                  |                                 |
        volvisor-drbd-testkit              volvisor-campaign
             (test-only)                      (test-only)
```

No production crate depends on `volvisor-campaign` or
`volvisor-drbd-testkit`; they exist only in test binaries. The `test-faults`
feature is enabled only in test/dev dependency graphs, never in a production
build.

## 2. Volume API v2 request flow

### Startup (safety-relevant order)

`volvisord` loads and validates configuration, initializes structured logging,
opens and replays the journal (`Journal::open` acquires the exclusive
`journal.lock`, truncates only an incomplete torn tail, and may compact the
log or evict terminal operations beyond the configured retention bound before
serving), constructs the configured provider and runs its startup
reconciliation, then binds the listener, signals `READY=1` when a service
manager is present, and serves.

`volvisor-witnessd` has the same shape in its own state directory: load
configuration, open/replay the witness journal under its lock, bind, signal
readiness, serve.

### Mutating request path

The request traversal is:

1. **Metrics → admission → auth → rate limit → body limit → handler.**
   Admission applies a global concurrency cap and a read-only timeout; auth
   resolves the principal, enforces the route's role and emits a structured
   audit event; the per-principal rate limiter runs after authentication.
   Health/readiness/metrics probes bypass the load-shedding layers.
2. **Typed validation** in the handler. Rejections happen before anything is
   journaled.
3. **Journal lookup** (`volvisor-api::ops::execute`). The key is
   `operation_id` plus an immutable request hash:
   - recorded outcome + same hash → replay the stored response byte-for-byte;
   - intent + same hash + no outcome → fail closed with
     `OPERATION_IN_DOUBT`;
   - same `operation_id` + different hash → `IDEMPOTENCY_CONFLICT`;
   - a fingerprint in the durable compaction tombstone ring →
     `OPERATION_EXPIRED` (HTTP 410). Within that retained ring an evicted
     operation can never be re-executed or resurrected; an operation evicted
     before the ring's window is indistinguishable from a new one and may be
     executed again, so size `max_tombstones` above the longest retry
     horizon. In-flight and pinned operations are never evicted.
4. **Journal intent**, with credential material redacted, is appended and
   fsynced **before** the provider mutation.
5. **Provider mutation** under generation fences, single-writer admission and
   ownership checks. The journal lock is never held across the provider call.
6. **Journal outcome**, fsynced, then returned. A success response is the
   exact body a later replay receives. If the outcome itself cannot be
   journaled, the caller still gets the truthful success and a later replay
   fails closed as in doubt.

Read-only routes do not journal. Admission/auth refusals are not journaled.
A journaled mutation is never abandoned by a wall-clock timeout; a retry uses
the same `operation_id`.

### Provider implementations behind that path

- `volvisor-lvm` drives `lvm2`/`util-linux`/`sysfs` through the shell-free
  command runner, verifies ownership metadata and effective sizes, and keeps
  atomic JSON state.
- `volvisor-ceph` drives the `ceph`/`rbd` CLIs against one configured external
  cluster, verifies FSID/pool/health at startup, uses `exclusive-lock`, and
  keeps atomic JSON state.
- `volvisor-drbd` drives unmodified DRBD 9 userland (`drbdadm`, `drbdsetup`,
  `blockdev`) and LVM, owns only the local end of each resource, and verifies
  roles from observed status rather than exit codes.

## 3. Witness, peer and migration paths

### Witness (writer authority)

For witness-managed nearline volumes, the witness is a separate process (and
must be a separate failure domain by deployment) that linearizes writer epochs
and leases. It holds only authority metadata, never tenant blocks or volume
data. `WitnessCore` is a fold over its own journal: grant/renew/revoke/register
intents and outcomes, with fencing proofs and commit indices. Established
writers keep serving until their lease deadline and then self-fence; witness
loss blocks new authority, renewals and failover. See
[contracts/nearline-replication-v2.md](contracts/nearline-replication-v2.md)
and [docs/plans/2026-10-09-p4-witness-fencing-authority.md](docs/plans/2026-10-09-p4-witness-fencing-authority.md).

### Planned migration / handoff

The consumer migration routes (`POST /v2/migrations`, `.../transfer`,
`.../abort`, `GET /v2/migrations/{id}`) are served by `volvisor-api` and drive
`volvisor-handoff::MigrationCoordinator`. The coordinator:

1. writes each cut step durably **before** the external act (write-ahead,
   forward-only cut);
2. prepares the destination through the destination daemon's peer route;
3. pauses/drains the source VMM, records a witness barrier, and revokes the
   source epoch;
4. grants the destination epoch through the witness and promotes the target;
5. restores/resumes the VM only after destination authorization.

`SOURCE_REVOKED` and `DESTINATION_AUTHORIZED` are deliberately separate
observable states. Once the cut is entered there is no abort handler; an
unresolvable step is observed as `IN_DOUBT`, never silently reverted to a
source write or reported as a generic abort. See
[contracts/nearline-replication-v2.md](contracts/nearline-replication-v2.md)
§6, [contracts/volume-api-v2.md](contracts/volume-api-v2.md) §5 and
[docs/plans/2026-10-09-p4b-vmm-storage-handoff.md](docs/plans/2026-10-09-p4b-vmm-storage-handoff.md).

### Peer path

The `/v2/internal/peer/*` routes are the destination-side surface for prepare,
lineage verification, grant, VM restore, discard and health. They are guarded
by the daemon-to-daemon `peer_api_token` (`RequirePeer`), deliberately distinct
from the consumer admin/operator credentials. A daemon without the migration
surface serves a typed `404`. Peer mutations use the same journal pipeline but
resolve an in-flight intent by **inspection** (witness lease state, durable
target preparation, observed VM state); they never blindly re-execute a
mutation that might already have happened.

The handoff surface is currently wired only for the `drbd` provider.
`native-local` attached storage is not live-migratable; Ceph shared-backend
migration is not claimed.

## 4. Invariants

These are binding summaries; the authoritative wording is in
[AGENTS.md](AGENTS.md), [contracts/volume-api-v2.md](contracts/volume-api-v2.md),
[contracts/nearline-replication-v2.md](contracts/nearline-replication-v2.md)
and [docs/schema-compatibility.md](docs/schema-compatibility.md).

1. **Distinct resources.** A disk, pool, LV, replica, VM attachment and PCI
   passthrough lease have independent identity, generation and ownership.
   `native-local` is logical volumes on host disks, not whole-controller VFIO.
2. **Journal before mutate.** Every privileged mutation validates, journals
   and fsyncs its intent, then acts, then journals the outcome. `IN_DOUBT` on
   an intent without an outcome is fail-closed; it is never re-executed
   blindly.
3. **Fsync before ack.** Journal appends are acknowledged only after
   `sync_all`; witness responses only after the outcome record is fsynced;
   provider/migration state saves write temp → fsync file → rename → fsync
   directory.
4. **Idempotency by operation id and immutable request hash.** Same id and
   hash replays the recorded response; same id with a different payload is a
   typed conflict. Bounded retention durably evicts terminal operations and
   keeps the exact fingerprint in the tombstone ring: a retry **within** that
   window is refused with `OPERATION_EXPIRED` rather than re-executed. An
   operation evicted before the ring's window is indistinguishable from a new
   one and may execute again, so size `max_tombstones` above the retry
   horizon. The reserved `derived:` namespace is unavailable to consumers.
5. **Stale generations fail closed.** Volume and attachment generation
   mismatches are typed conflicts. A stale writer learns it is fenced; fencing,
   quorum and error reporting are never weakened to make a test pass.
6. **Single-writer discipline.** One journal lock per directory (`journal.lock`,
   exclusive non-blocking `flock`); one writable attachment per volume where
   the provider supports it; DRBD single-primary for nearline. Foreign backend
   state is reported, never adopted or reformatted.
7. **Schema compatibility.** Durable files carry versions. A newer schema is
   refused typed and left intact; an older supported schema upgrades on its
   first successful write. There is no in-place downgrade or hand-editing.
8. **No silent protection claims.** A local mirror is not a remote replica;
   async replication can lose its acknowledged tail; DRBD protocols A/B/C are
   distinct durability contracts. No zero-RPO claim follows from a healthy
   async replica.
9. **No secret material at rest in journals.** Credential references and
   authorization tokens are redacted before journaling; logs and evidence
   bundles never contain tokens, private keys or shared secrets.
10. **Discovery is read-only.** Hardware identity is never solely a Linux
    device name or PCI BDF; no destructive adoption of foreign state.

## 5. Failure model

### Crash seams

| Seam | Durable state | Honest outcome |
|---|---|---|
| After intent, before provider act | Journal has intent, no outcome | Retry with the same `operation_id` → `OPERATION_IN_DOUBT`; nothing is re-executed blindly |
| During provider act | Journal intent; backend may have changed | Startup reconcile / inspection classifies the world; do not infer success from an exit code alone |
| After provider act, before outcome | Journal intent, provider state maybe saved | Same-hash replay fails closed as in doubt; operator/background reconcile handles the backend |
| Provider state save before rename | Old state file | Old durable state survives; any compensation follows the recorded intent |
| Provider state save after rename, before directory fsync | New state file | Rename is the commit; the new state is read |
| Migration record / peer preparation save | Per-entry JSON | Atomic old-or-new record; a cut is forward-only and unresolved work is `IN_DOUBT` |
| Witness intent/outcome | Witness journal | Outcome fsynced before response; an intent without outcome is rolled forward at startup from its embedded computed response |
| Torn journal tail | `journal.log` | Only an incomplete trailing frame is truncated; a complete CRC-valid but undecodable/future frame refuses startup |
| Compaction crash | `journal.log` | The snapshot is written to a temporary file, fsynced and renamed; a partial snapshot is ignored and the previous complete log replays. A rename whose directory fsync failed blocks later appends until it succeeds |
| Evicted terminal operation | `journal.log` snapshot tombstones | A retry **within** the retained tombstone ring fails closed with `OPERATION_EXPIRED`; a post-snapshot record for a snapshot tombstone is an inconsistent log and refuses startup. Once the ring has rolled past an operation it is indistinguishable from a new one and may re-execute |

Backend resources are reconciled against durable records at startup. A missing
LV/image/resource is marked failed/degraded honestly; a foreign resource is
reported, never touched; a journaled move/migration with an unknown outcome
parks `IN_DOUBT` until observation can resolve it.

### Fail-closed refusals

Typed refusals are the expected behavior for stale generations, idempotency
conflicts, foreign device state, unsupported classes/policies, future schema
versions, missing credentials, and unwired optional surfaces. `IN_DOUBT` is a
legitimate state, not a generic failure to be retried under a new id. A
success response always corresponds to persisted observed state or an
explicitly accepted asynchronous obligation.

## 6. Where state lives and what is authoritative

Paths below are the documented defaults; some are configurable. The exact
store/version matrix is [docs/schema-compatibility.md](docs/schema-compatibility.md).

| Path | Contents | Authority |
|---|---|---|
| `<journal_dir>/journal.log` | Length-prefixed, CRC-checked intent/outcome/checkpoint frames; after compaction, one snapshot base frame plus later records | **Authoritative** for API idempotency and operation outcomes; replayed on startup; bounded by retention |
| `<journal_dir>/journal.log.compact` | In-progress compaction snapshot (temporary) | Never authoritative; ignored after a crash and replaced by the next compaction |
| `<journal_dir>/journal.lock` | `flock` target | Lock only; no durable state/schema |
| `<journal_dir>/lvm-state.json` (or `lvm_state_path`) | LVM volume cross-references, attachments, device claims, move records, and the legacy decode-only attachment tombstone filter | **Authoritative** provider-local state; reconciled against LVs at startup |
| `<journal_dir>/ceph-state.json` (or `ceph_state_path`) | RBD image cross-references and attachments | **Authoritative** provider-local state; reconciled against RBD at startup |
| `<journal_dir>/drbd-state.json` (or `drbd_state_path`) | DRBD volume/resource cross-references, attachments, authority block, counters | **Authoritative** provider-local state; reconciled against DRBD/LVM at startup |
| `<journal_dir>/migrations/<migration_id>.json` | Per-migration record, state history, cut write-ahead | **Authoritative** recoverable form of the handoff |
| `<journal_dir>/peer-preparations/<migration_id>.json` | Destination target preparation and grant attempts | **Authoritative** destination preparation state |
| `<journal_dir>/grow-notifications.json` | Outstanding grow-notification obligations | Durable obligation store; **not yet covered** by the schema policy (documented gap) |
| `<journal_dir>/ownership.json` | Lending/device ownership records | Durable ownership store; **not yet covered** by the schema policy (documented gap) |
| Witness `<state_dir>/journal.log` | Witness intents/outcomes, epochs, leases, barriers | **Authoritative** for writer authority; folded on startup; no tenant blocks |
| `drbd_config_dir/volvisor-<resource>.res` | Generated DRBD resource definitions | Runtime configuration derived from provider state; secret file kept separate and owner-only |
| `migration.snapshot_dir` | VMM snapshot/restore artifacts | Operational migration artifact, not an authority record |
| Backend resources (LVs, RBD images, DRBD resources, device ownership metadata) | The actual storage | Reconciled facts; treated as Volvisor-owned only when ownership/generation metadata matches |

`journal.log` and the provider state files are complementary: the journal is
the operation/idempotency log, while provider files are the provider's
inventory/attachment/claim records. Neither is safe to hand-edit or truncate.
The witness registry is derived from its journal, not a second format. The
migration record and peer preparation are authority-relevant state, never an
advisory cache.

## 7. Not production supported

**No class in this repository is production-supported.** This matches
[AGENTS.md](AGENTS.md) rule 12: every production-support claim must match the
exact implementation source, hardware, VMM/backend versions and real-host
failure evidence, and no class has passed the required failure and evidence
gates. The current posture is an **implemented prototype**; `native-local` is
the only release-scope `supported` provider and even its real suite is a
loop-device gate, not real-NVMe qualification. `ceph-rbd` and
`nearline-replicated` are release-scope `not-run` non-claims. See
[SUPPORT_MATRIX.md](SUPPORT_MATRIX.md),
[release-scope.toml](release-scope.toml) and
[docs/releases/pre-production-readiness.md](docs/releases/pre-production-readiness.md).

## 8. Bus factor and ownership

The project is currently maintained under the repository-owning organization
`o3kio`; no per-area human owner is recorded in the repository beyond that
organization. Release and restore work therefore still depend on a small set
of people, and the Ceph/DRBD/campaign suites depend on externally provisioned
hardware. See [docs/bus-factor.md](docs/bus-factor.md) for the risk register
and concrete mitigations, and [.github/CODEOWNERS](.github/CODEOWNERS) for the
structural review routing that is in place.

## 9. Further reading

Normative v2 documents:

- [ADR-0003 — volume architecture](docs/adr/0003-tiered-volume-virtualization.md)
- [ADR-0004 — nearline replication and migration](docs/adr/0004-nearline-replication-and-mobility.md)
- [ADR-0005 — Ceph RBD and managed OSDs](docs/adr/0005-ceph-rbd-and-managed-osds.md)
- [ADR-0006 — online resize and local relocation](docs/adr/0006-online-resize-and-live-local-block-relocation.md)
- [ADR-0007 — DRBD 9 nearline provider](docs/adr/0007-drbd9-nearline-replication-provider.md)
- [ADR-0008 — experimental Rook-only cells](docs/adr/0008-rook-only-hyperconverged-cells.md)
- [ADR-0009 — distribution and packaging](docs/adr/0009-distribution-and-packaging.md)
- [ADR-0010 — Tier V real-VMM verification](docs/adr/0010-tier-v-real-vmm-verification.md)
- [ADR-0011 — local-mirror experiment declined](docs/adr/0011-local-mirror-experiment-decline.md)
- [ADR-0012 — monotonic attachment-generation fence](docs/adr/0012-monotonic-attachment-generation-fence.md) (Accepted 2026-11-06)
- [ADR-0013 — durable attachment-generation allocator](docs/adr/0013-durable-attachment-generation-allocator.md) (Accepted 2026-11-06)
- [ADR-0014 — journal retention bounds and exhaustion](docs/adr/0014-journal-bounds-and-exhaustion-contract.md) (Accepted 2026-11-06)
- [ADR-0015 — write-path critical-section invariant](docs/adr/0015-write-path-critical-section-invariant.md) (Accepted 2026-11-06)
- [ADR-0016 — semantic-change and default-scenario gate](docs/adr/0016-semantic-change-and-default-scenario-gate.md) (Accepted 2026-11-06)
- [SPEC-0002 — v2 implementation specification](docs/specs/SPEC-0002-volvisor-volume-virtualization.md)
- [Volume API v2](contracts/volume-api-v2.md)
- [Nearline replication v2](contracts/nearline-replication-v2.md)
- [Rook-cell experimental v0](contracts/rook-cell-experimental-v0.md)

Operational and contributor material:

- [Start here](docs/START_HERE.md)
- [Contributor guide](CONTRIBUTING.md)
- [Support matrix](SUPPORT_MATRIX.md)
- [Configuration reference](docs/configuration.md)
- [Schema compatibility](docs/schema-compatibility.md)
- [Default integration scenarios](docs/testing/default-integration-scenarios.md)
- [Real-backend rigs and release gate](docs/testing/real-backends.md)
- [Day-2 runbooks](docs/runbooks/day-2/README.md)
- [Evidence standards](docs/evidence/README.md)
- [Provider authoring guide](docs/provider-authoring.md)
- [Admission control](docs/admission-control.md)
- [Observability](docs/operations/observability.md)
- [Security deployment guide](docs/security/deployment.md)
