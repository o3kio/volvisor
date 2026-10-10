# P4b implementation plan — full VMM/storage handoff (stage B)

Status: normative for the P4b phase (this document is the plan of record;
implementation PRs cite it)
Date: 2026-10-09 (rev 4 — round-1/2/3 design review findings
folded in)
Builds on: [P4a plan](2026-10-09-p4-witness-fencing-authority.md) (merged as PR #7),
ADR-0004 Decision 3/4, ADR-0007 "Planned Cloud Hypervisor live migration is a
separate hard gate", [nearline contract v2](../../contracts/nearline-replication-v2.md)
§6/§7, [Volume API v2](../../contracts/volume-api-v2.md) §5, SPEC-0002 §7/§8.

P4a delivered the writer-authority layer: epochs, leases, the witness service,
fail-closed self-fencing, and unplanned promotion with honest classification.
P4b delivers the other half of the implementation-order item: **the coordinated
VM/storage migration transaction** — the canonical handoff state machine with
durable per-migration state and `IN_DOUBT` recovery, Cloud Hypervisor
coordination against the real `ch-remote` surface, the Volume API v2 mobility
operations, VM-wide eligibility, and the automated source-committed barrier
records that unlock `SAFE_CURRENT` without operator attestation.

## 1. Scope and staging

### In scope

1. A **migration coordinator** (new crate `volvisor-handoff`) owning the
   canonical state machine, its durable per-migration store, startup
   reconcile and `IN_DOUBT` resolution.
2. **Witness extensions** (new journaled mutations + hardening):
   per-client host identities, `RecordBarrier`/`VoidBarrier`, and batch
   (all-writable-volume) authority mutations.
3. A **provider handoff surface** (`HandoffSurface`, the
   `AdoptionSurface` pattern applied to migration): source-side
   quiesce/barrier/transfer primitives, the durable **migration-cut
   marker** in `DrbdState`, and the target-side
   promote-under-granted-lease, on the DRBD provider.
4. **VMM coordination** (`VmmController` trait): a Cloud Hypervisor
   adapter over `ch-remote` — CLI-honest, verified against the
   upstream command surface (see §5) — and a fake VMM for the test
   harness that models device-open semantics against the fake DRBD
   world.
5. **Volume API v2 mobility operations**: `CheckVmStorageMobility`,
   `PrepareNearlineHandoff`, `BarrierAndTransfer`, `ObserveHandoff`,
   plus abort semantics — journaled like every privileged mutation.
6. The **`SAFE_CURRENT` unlock**: a migration-recorded barrier is
   machine-checked evidence in the P4a adoption classifier, replacing
   operator attestation for the planned-migration crash case.

### Out of scope (recorded, each with its reason)

- **Memory pre-copy while the source VM runs.** Cloud Hypervisor's
  `vm.receive-migration` transmits the whole VM configuration and the
  receiving VMM opens every disk when the migration starts; a running
  pre-copy therefore requires a writable destination device before the
  authority transfer — exactly the temporary dual-primary shape the
  v2 contract forbids by default (AGENTS rule 17). v1 downtime is the
  full pause→snapshot→restore window (§5). A pre-copy design needs a
  CH that can receive memory with disks deferred, or a separately
  accepted fenced dual-primary contract; neither exists today.
- **Temporary dual-primary (Protocol C) handoff.** Forbidden by default;
  requires its own accepted ADR and adversarial evidence (rule 17).
- **Volvisor-launched Cloud Hypervisor processes.** The deployment (the
  O3K/CellHV consumer layer) owns VMM process lifecycle. Volvisor
  drives an *already-running* destination VMM through its API socket
  (§5). Starting VMMs would duplicate the consumer's responsibility.
  Consequence, handled honestly in §3: a dead destination VMM process
  stalls the forward path in `IN_DOUBT` with a typed detail until the
  operator restarts the receiver — volvisor never launches it.
- **Ceph RBD / native-local migration.** `CheckVmStorageMobility`
  reports them ineligible with typed reasons (contract §5); no
  implementation.
- **Snapshot transport between hosts.** The v1 cutover writes the
  memory snapshot to a configured directory that the destination host
  can read (shared filesystem, operator-provided). Volvisor does not
  copy multi-GB snapshots itself; without a shared path the migration
  is refused — at `PREPARED`, when the destination daemon verifies it
  can read the directory (not merely at config time), with a typed
  reason. Honest availability boundary, recorded in the contract text.
- **Witness garbage collection, multi-peer (>2 data ends) topologies,
  rate-control policy (CONVERGE heuristics).** The state vocabulary and
  the barrier proofs are delivered; dirty-rate-driven scheduling is a
  later refinement (the contract places CONVERGE inside `PRECOPY` as
  policy, not protocol).

### Staging (review-surface split, like P4a's two-PR shape)

- **Stage B1 (one PR): the authority substrate.** Witness extensions
  (identities, `RecordBarrier`/`VoidBarrier`, batch mutations, inspect
  exposure), the `volvisor-handoff` state machine + store + reconcile,
  the `HandoffSurface` on the DRBD provider including the migration-cut
  marker, and the `SAFE_CURRENT` adoption unlock. Fully testable
  without a VMM (the state machine is driven by a fake driver; the
  provider surface by the existing fake world).
- **Stage B2 (one PR): the cutover.** `VmmController` + the Cloud
  Hypervisor adapter + the fake VMM, the internal peer-daemon surface,
  the consumer-facing mobility endpoints, the coordinator's full
  `PREPARED → COMPLETE` drive, contract updates, and the end-to-end
  test matrix (§9).

Each stage gets its own adversarial review rounds to a clean verdict
before merge, per the standing process.

## 2. The cutover design (the single-primary constraint, honestly)

DRBD single-primary cannot demote while a VMM holds the block device
open (rule 17, ADR-0007), and Cloud Hypervisor's migration-receive
opens the destination disks when the transfer starts. These two facts
fix the only honest single-writer cutover shape:

```text
source host                                destination host
-----------                                -----------------
PREPARED: eligibility (VM-wide),           target replica verified
  migration record created                   (resource present, Secondary,
                                              connected, no fence marker)
  + snapshot dir readability
    verified by the destination
PRECOPY: DRBD replica catch-up observed
  (TrackSync); no memory pre-copy (out of scope, above)
QUIESCED: ch-remote pause
  + vm.info state == Paused (verified)
  + provider suspend_io + durable
    migration-cut marker on every
    participating volume
BARRIER_DURABLE: TrackSync proof
  (peer UpToDate, no resync, after the
  suspension fixed the boundary)
  + witness RecordBarrier (authenticated)
CUT (point of no return — the store is      [everything from here is
 written BEFORE each irreversible act]):     forward-only; observed
  store: cut=snapshotting                  as IN_DOUBT until
  ch-remote snapshot                        DESTINATION_AUTHORIZED]
  store: cut=destroying-vm
  ch-remote delete (the source device
    closes; the VM is gone — there is
    no rollback past this line)
  store: cut=demoting
  drbdadm secondary (device closed;
    role re-verified Secondary)
  store: cut=revoking
  witness RevokeSet (self-release,
    after EVERY participant proven
    Secondary — never a subset)
SOURCE_REVOKED: store records it
DESTINATION_AUTHORIZED:                    witness GrantSet (new epochs,
                                             retires leftovers — W2)
                                           store records it
                                           [IN_DOUBT window ends]
VM_RESUMED:                                ch-remote restore (pre-started
                                             destination VMM; a non-empty
                                             or half-restored VMM is
                                             destroyed first — idempotent
                                             re-drive)
                                           + ch-remote resume
                                           + attachment identity verified
COMPLETE: source reconciled Secondary,
  migration record closed, cut markers
  cleared
```

Diagram footnote: `[IN_DOUBT window ends]` marks the *normal* window
(cut entered → `DESTINATION_AUTHORIZED`). `IN_DOUBT` is additionally
reachable on a failed pre-cut void and on an unresolvable
post-authorization stall (dead destination VMM) — §3's observation
mapping and the terminal `InDoubt` state define those cases.

Design decisions, each with its rule citation:

- **D1 — snapshot/delete/restore, not live-migration.** The source VM
  is paused, snapshotted (`ch-remote snapshot file://…`), then
  destroyed (`ch-remote delete` — the API table lists no
  prerequisites, so it is legal on a paused VM and is the only
  deterministic way to close the source device; `vm.shutdown` needs a
  running guest to cooperate with ACPI). The destination restores from
  the snapshot into a pre-started empty VMM
  (`ch-remote restore source_url=file://…`), landing paused, then
  `ch-remote resume`. Every command is in the verified upstream
  surface (§5).
- **D1a — the cut is a durable, forward-only progress record.** Before
  the first irreversible act (the snapshot begins the window; the
  `delete` ends all rollback possibility), the migration store gains a
  `cut` progress field, updated durably **before** each external side
  effect (write-ahead, the P4a marker-before-demote discipline). Every
  state at or past `cut=snapshotting` is forward-only in reconcile and
  is observed as `IN_DOUBT` (with a per-step detail) until
  `DESTINATION_AUTHORIZED` — never as a rollback-eligible state, never
  as a generic `ABORTED`. There is no abort handler for cut-or-later
  states, by construction (§8 item 4).
- **D2 — the barrier is fixed by suspension, then proven by
  observation.** `QUIESCED` freezes the source data path at the kernel
  enforcement point (`drbdsetup suspend-io`) after the VMM pause is
  verified, and stamps the durable migration-cut marker into
  `DrbdState` (D6a). Only then is replication catch-up observed
  (`TrackSync`: peer disk `UpToDate`, no resync in progress,
  connection established). This ordering makes the recorded boundary
  exact rather than estimated; a catch-up observation taken before the
  freeze proves nothing about the boundary (ADR-0004 Decision 2's
  exact-prefix rule).
- **D3 — authority transfer stays two observable steps with a real
  `IN_DOUBT` window.** The witness gains batch mutations
  (`RevokeSet`/`GrantSet`), each a single journaled record so the
  all-writable-volume set is atomic within a step (nearline contract
  §6: "single VM I/O cut … prove all target barriers and fencing
  actions"). The two steps are never collapsed: the migration store
  records `SOURCE_REVOKED` durably before the grant is attempted, and
  a crash in between resolves **forward only** (retry the grant) or
  holds `IN_DOUBT` for the operator — the source never resumes
  (AGENTS rule 5, ADR-0004 Decision 4).
- **D4 — the source self-release is the P4a discipline, set-wide.**
  The batch revoke happens only after **every** participant is proven
  demoted (device closed by VM deletion, `drbdadm secondary`
  succeeded, role re-verified from `drbdsetup status`). A single
  participant's refusal holds the whole cut at `cut=demoting` and
  retries — a subset release is never issued. A self-release waives
  the next grant's W7 wait; it is earned exactly as in
  detach/adopt: never while a writer might still be serving.
- **D5 — dead-source `IN_DOUBT` converges to the P4a adoption path.**
  If the source host dies inside the window (or after the cut began),
  the destination does not need the source's migration record: the
  surviving host resolves through `adopt-and-promote`, and the
  migration-recorded barrier upgrades the classification to
  `SAFE_CURRENT` (§7). One recovery vocabulary, two entry points
  (coordinator-driven roll-forward and adoption), no special cases.
- **D6 — the coordinator runs on the source host's daemon.** It hosts
  the consumer-facing mobility API, drives the destination host's
  daemon through an internal authenticated peer API (the destination
  VMM socket is host-local, so destination-side VMM actions must be
  proxied by the destination daemon), and speaks to the witness
  directly. The destination's actions are idempotent provider
  operations driven over the peer API; its own startup reconcile (the
  P4a adoption-record pattern) covers half-done promotes.
- **D6a — the migration-cut marker makes the suspension durable
  against the provider's own reconcile.** The provider's startup
  reconcile auto-resumes a Primary whose lease it can validate; a
  mid-migration suspended Primary is exactly that shape. The cut
  marker (`runtime.migration: Option<MigrationCut>` in `DrbdState`,
  carrying the migration id and the suspension timestamp) changes
  three behaviors while set: (1) the startup reconcile reports the
  volume as migration-suspended and does **not** resume it — only the
  coordinator clears the marker; (2) the renewal pass keeps renewing
  the lease (the cut needs a live lease; the lease deadline remains
  the bound — a cut that outlives its lease fails closed through the
  existing deadline fence and lands in the adoption recovery path);
  (3) attach/detach of a migration-suspended volume is refused typed.
  A marker whose migration record no longer exists (corrupt or
  operator-removed store) is reported and left in place — fail-closed,
  operator resolution; the provider never invents a migration
  decision. The operator's resolution mechanism is defined, not
  hand-waved: a **clear-cut-marker admin operation** on the provider
  (journaled, admin-token) that requires the volume to be provably
  not-writer first — Secondary role *or* a fencing proof the operator
  supplies — and then clears the marker and reconciles the volume
  normally; the same operation covers the source-side `Failed` +
  cut-marked residue left after the destination adopts a dead
  source's volume (the adoption demoted the peer; this host's device
  must be verified closed/Secondary before the marker clears).

## 3. The migration state machine (`volvisor-handoff`)

### Types

- `MigrationId` — validated opaque string (the `OperationId` rules:
  length, charset; consumer-supplied, unique per migration).
- `HandoffState` — the canonical vocabulary, exactly the contract's:
  `Prepared`, `Precopy`, `Quiesced`, `BarrierDurable`,
  `SourceRevoked`, `DestinationAuthorized`, `VmResumed`, `Complete`,
  plus the terminal observations `InDoubt { since, detail }` and
  `Aborted { reason, at }`. The internal cut progress (`cut:
  Option<CutProgress>` — `Snapshotting | DestroyingVm | Demoting |
  Revoking`) is a sub-field of the record, not a canonical state;
  externally, a record with an active cut is observed as `IN_DOUBT`
  with the step as detail **only before `DESTINATION_AUTHORIZED`**
  (D1a) — from `DESTINATION_AUTHORIZED` on, the canonical state is
  reported with a stall detail, and an unresolvable stall (a dead
  destination VMM) is observed through the terminal `InDoubt`
  mapping. The `cut` field guides the forward re-drive through
  `VM_RESUMED` and is cleared at `Complete`.
- `MigrationRecord` — one per `MigrationId`:
  `migration_id, vm_id, source_host, target_host, participants[]`
  (each: `volume_id, expected_generation, resource, minor`),
  `state`, `cut`, `state_history[]` (append-only, monotonic — the
  observable trace `ObserveHandoff` reports), `barrier_proofs[]`
  (per-volume: boundary commit index, attestation, recorded_at),
  `abort_policy` (`AutoBeforeCut` — the only v1 policy; there is no
  abort after the cut begins), `created_at/updated_at`.
- `MigrationStore` — atomic-save JSON (write-temp + fsync + rename,
  the `DrbdState` discipline) at a configured path, keyed by
  `MigrationId`; `load`, `upsert`, `remove` (terminal records are
  retained, prunable by age later — out of scope).
- `MigrationCoordinator` — the driver. Holds: the source
  `Arc<dyn HandoffSurface>`, a witness client, a destination peer
  client (§6), a `VmmController`, the store, and a clock. Every
  transition is: perform the side effects → persist the store → only
  then report the new state; every irreversible act is preceded by its
  durable write-ahead (D1a). Transitions are idempotent: a re-drive of
  a recorded step first reconciles the external world (witness view,
  VMM state, provider state) and skips what is already true — never a
  blind re-execution with different effects.

### Reconcile (startup + retry task)

A background task (the renewal-task pattern) and a startup pass drive
`resolve(record)`. **Order matters: the reconcile first queries the
external facts — the witness view (lease state, current epoch, barrier
log) and the VMM state (VM present? paused?) — and folds them into the
store; only then does it choose abort or forward.** The stored state
alone is never trusted to classify a crash window, because every
side-effect/persist boundary can leave it stale.

- `Prepared|Precopy|Quiesced|BarrierDurable` **with no cut** →
  **abort path**: source authority is intact, so rollback is allowed
  — void the recorded barriers (W9), unsuspend every participating
  volume, clear the cut markers, `ch-remote resume` the source VM (if
  paused), discard the target-side preparation, record `Aborted`.
  Ordering is normative (G5): **an unvoided recorded barrier is a
  hard gate on any source resume** — every recorded barrier of the
  epoch must be **confirmed voided** (the `VoidBarrier` journaled and
  confirmed) before the VM is resumed. A `VoidBarrier` that cannot be
  journaled — for any reason, including witness unreachability, where
  a void may have been journaled while the response was lost — fails
  the whole rollback into `self_fence`
  (the durable `PendingFence` path) with the migration record
  `InDoubt`-annotated — the source stays paused/suspended until the
  witness is reachable again or an operator applies a fencing proof,
  never resumed with a live barrier that could later certify a false
  `SAFE_CURRENT` (writes acknowledged after the barrier's boundary
  would fall inside its attested window). Fail-closed, never a silent
  resume, never an unmarked suspension.
- Any record **with a cut** (`cut=snapshotting` … `revoking`) →
  **forward only**: re-drive the cut from the reconciled external
  facts (a VM already destroyed, a demotion already done, a revoke
  already journaled are each detected, not repeated). Re-drive
  semantics are explicit: a still-present VM at `cut=destroying-vm`
  is **destroyed as its forward step** (it is paused, barrier-recorded
  and cut-marked — no writer; the destroy is the committed
  direction); a re-run snapshot of the same paused VM overwrites into
  the same directory (the intended, safe semantics — the memory is
  identical until the VM resumes, which it never will on the source).
  The retry reuses the **same witness operation ids** for
  `RevokeSet`/`GrantSet`, **derived deterministically** — the id is a
  function of `migration_id` + the step (`revoke-set`/`grant-set`) +
  the ordered participant volume set, so a post-restart retry
  reconstructs it without having remembered the prior call (the
  journal replays the recorded outcome byte-identically; a fresh
  random id would mint a redundant epoch — safe but noisy, and
  excluded here to keep audit trails one-mutation-per-intent). The
  state stays `IN_DOUBT`-observable until `DESTINATION_AUTHORIZED`.
  If forward progress is impossible (destination daemon unreachable,
  destination VMM dead — §1), the record stays `IN_DOUBT` with a
  typed detail; the code has no rollback for these states and must
  not grow one.
- `SourceRevoked` → forward only (the same re-drive; the revoke
  already happened or the retry performs it).
- `DestinationAuthorized|VmResumed` → **forward completion**: drive
  restore (destroying any half-restored destination VM first — the
  re-drive is idempotent), resume, reconcile the source Secondary,
  clear the cut markers, `Complete`.
- `Complete|Aborted` → nothing. `InDoubt` as a *terminal* record
  (the pre-cut rollback-failure case) is re-resolved by the retry
  task once the witness is reachable: it re-attempts the abort path
  (void must confirm before any resume); it never resumes in the
  meantime. An in-cut `InDoubt` is not a terminal state — it is the
  forward re-drive's active observation until the drive lands.

The reconcile is the only writer of `state_history` besides the live
drive — both go through the same transition function, which is
append-only by construction (a transition appends, never edits).

## 4. Witness extensions (W8–W10)

New invariants, unit-tested like W1–W7:

- **W8 (caller identity binding).** The witness configuration gains a
  per-host credential map (`HostId → token`). The legacy single shared
  token remains valid **for reads only** (inspect/health); every
  holder-asserting or state-mutating call (`grant`, `renew`,
  self-`revoke`, `RecordBarrier`, `VoidBarrier`, the batch mutations)
  requires the credential bound to the asserted holder and is refused
  typed otherwise. This is a witness protocol version 2 breaking
  change, deployed with the migration feature (which cannot function
  without it); it closes the P4a-recorded residual ("per-client
  identities are P4b hardening") without leaving a shared-token path
  that could forge holder assertions. Every journaled mutation records
  the bound identity.
- **W9 (recorded barrier).** New journaled mutations:
  `RecordBarrier { volume_id, holder, boundary_commit_index,
  attestation { vm_paused_and_drained, data_path_suspended,
  peer_up_to_date }, migration_id?, recorded_at }` and
  `VoidBarrier { volume_id, migration_id, holder }`. Enforced:
  `RecordBarrier` only from the current epoch's holder (W8), while the
  epoch is current; the boundary is an **ordering token** — the
  witness's commit index at recording time, placing the barrier in the
  journal's total order — and carries no claim of being the epoch's
  final mutation (renewals after the barrier write no data and do not
  invalidate it; §7 states what the classifier actually checks).
  `VoidBarrier` only from the recording holder (W8), only before the
  epoch retires — the abort path's evidence-hygiene step so an
  aborted migration's barrier can never surface as `SAFE_CURRENT`
  evidence for a later retirement. **An unvoided barrier is a hard
  gate on any source resume** (§3 abort path): the void must be
  journaled before the source VM is resumed, and a void that cannot
  be journaled fails the rollback into `self_fence` — a resumed
  writer with a live barrier is exactly the false-`SAFE_CURRENT`
  shape W9 exists to exclude. (The malicious direction is safe: a
  holder voiding a *good* barrier merely degrades a later adoption to
  `PossibleLoss` — it loses an unlock, it cannot promote anything
  falsely.) Records are immutable once
  journaled (byte-identical replay aside, a differing re-record is an
  `IDEMPOTENCY_CONFLICT`). `inspect` exposes the per-volume barrier
  log: each barrier's attestation, migration id, recorder identity,
  commit index, and voided flag — plus, per retired epoch, the
  retirement record's commit index. That exposure is a small derived
  state/inspect addition over the journal fold (the fold sees every
  record; the registry today keeps only `last_proof`/`last_commit`,
  so per-retired-epoch indices are computed and cached at inspect
  time — new code, not a new mechanism, and called out as such).
- **W10 (batch atomicity).** `RevokeSet { releases[], migration_id }`
  and `GrantSet { requests[], migration_id }` are single journaled
  mutations: the fold applies all-or-nothing, one commit-index bump
  for the set, per-volume responses computed inside the envelope (the
  existing `MutationEnvelope` discipline). A partial failure rejects
  the whole batch typed. `GrantSet` mints new epochs per volume and
  retires any lingering epochs (W2 semantics, set-wide); `RevokeSet`
  from the source is a set of self-releases (W8-bound, every member
  proven demoted before the call — D4) that waive W7 legitimately.

The witness protocol version bumps to 2 (new routes/mutations, the W8
authn change; the client negotiates). `inspect` gains the barrier log;
the `AuthorityView` shape change is additive.

## 5. VMM coordination (`VmmController`)

### The verified Cloud Hypervisor surface

Every command the adapter issues exists in the upstream `ch-remote`
surface (checked against the cloud-hypervisor main docs on
2026-10-09; the adapter cites this in its module docs and pins the
verified command list):

| Step | Command | Verified fact |
|---|---|---|
| pause | `ch-remote --api-socket S pause` | `/vm.pause` requires the VM booted; command returns after the pause completes |
| pause proof | `ch-remote --api-socket S info` | `/vm.info` reports VM state; adapter requires `Paused` |
| snapshot | `ch-remote --api-socket S snapshot file://DIR` | `/vm.snapshot` requires the VM paused; writes `config.json`, `memory-ranges`, `state.json` |
| destroy | `ch-remote --api-socket S delete` | `/vm.delete` has no prerequisites — legal on a paused VM; releases the VM's devices |
| restore | `ch-remote --api-socket S2 restore source_url=file://DIR` | `/vm.restore` on a pre-started empty VMM; restored VM lands paused; `config.json` may be adjusted between snapshot and restore |
| resume | `ch-remote --api-socket S2 resume` | `/vm.resume` requires paused |

Not used, with reasons recorded in the adapter docs: `send-migration`/
`receive-migration` (opens destination disks at receive — the
dual-primary trap, §1 out-of-scope), `vm.shutdown` (needs a running,
cooperating guest), `power-button` (same).

The adapter runs the commands through the existing `CommandRunner`
trait (`RealRunner` in production, `FakeRunner`/custom runners in
tests) — the established argv-verified, timeout-guarded execution
path. Socket paths follow a configured convention
(`vmm.api_socket_dir`, file `{vm_id}.sock`).

### The trait (engine-neutral, ADR-0007's boundary)

```rust
trait VmmController: Send + Sync {
    fn pause(&self, vm_id: &str) -> Result<PauseProof, ApiError>;      // command + info-verified
    fn snapshot(&self, vm_id: &str, dir: &Path) -> Result<(), ApiError>;
    fn destroy(&self, vm_id: &str) -> Result<(), ApiError>;            // delete; device release
    fn restore(&self, vm_id: &str, dir: &Path, disks: &[DiskMapping]) -> Result<(), ApiError>;
    fn resume(&self, vm_id: &str) -> Result<(), ApiError>;
    fn state(&self, vm_id: &str) -> Result<VmState, ApiError>;         // Absent | Created | Running | Paused
}
```

`DiskMapping` carries the snapshot config's declared path → the target
host's promoted device path; the adapter rewrites the copied
`config.json` when they differ (DRBD minors are symmetric across the
replication pair in the common case, so the rewrite is usually a
no-op — but it is verified, never assumed). `PauseProof` is the
observed `Paused` state from `vm.info` — volvisor's own verification,
not the consumer's attestation. The adapter maps the real
`vm.info` states (`Created|Running|Paused|Shutoff`, plus the
API-not-found error) onto `VmState`, with `Absent` meaning the
not-found error on the socket; the mapping and the verified-against
CH version are recorded in the adapter's module docs. `destroy` and
`restore` are
re-drive-safe: destroying an absent VM succeeds as a no-op when
`state()` confirms `Absent` (the crash-reconcile dependency), and
`restore` refuses a non-empty VMM typed (the coordinator destroys the
half-restored VM first, §3).

### The fake VMM (test harness)

`FakeVmm` models what the cutover actually depends on: it holds the
VM's devices open (integrating with `FakeDrbd.open_devices` — the
minor is inserted while the VM "runs" and removed on `destroy`), tracks
`Absent|Created|Running|Paused` and snapshot directories (real files
in a tempdir, so a restore genuinely reads what the snapshot wrote,
and an unreadable directory genuinely fails). This makes rule 17
*provable in tests*: any coordinator bug that demotes before `destroy`
fails against the fake's busy refusal, exactly as it would on a real
host.

## 6. Daemon and API surface

### Config additions (`volvisord`)

```toml
# flat witness keys (the existing shape — config.rs uses top-level
# fields under deny_unknown_fields, not a [witness] table):
witness_url = "…"                      # existing
witness_token = "…"                    # existing; legacy, READ-ONLY on a v2
                                       # witness (inspect/health)
witness_host_token = "…"               # NEW, REQUIRED with witness_url on a
                                       # v2 witness: this host's W8 credential.
                                       # The base P4a AuthorityContext uses it
                                       # for grant/renew/self-revoke/register.
witness_renewal_interval_secs = 10     # existing, still required with witness_url
[migration]                            # new table
enabled = true                         # false until the deployment opts in
snapshot_dir = "/var/lib/volvisor/migrations"   # must be shared with the peer for cross-host cutover
peer_api_url = "http://peer-host:7780"
peer_api_token = "…"                   # daemon-to-daemon credential (distinct from the witness and consumer tokens)
[vmm]                                  # new table
ch_remote_bin = "/usr/bin/ch-remote"
api_socket_dir = "/run/volvisor/vms"
```

Validation (the `config.rs` discipline): **`witness_host_token` is
required whenever `witness_url` is set** — the witness protocol is v2
as of this phase and every state-mutating call from this daemon
(including the P4a attach/renew/release path, not only the migration
surface) authenticates as this host; a v2 witness without the
credential would break attach/detach/renewal at runtime, so config
refuses it up front rather than failing mid-operation. `witness_token`
remains valid (read-only on a v2 witness; its existing
non-loopback-required rule is unchanged — deployments keep both
tokens: the shared read token and this host's mutating credential).
`migration.enabled` requires the drbd provider + a witness + a set
`witness_host_token`; the **witness** must remain in a
failure domain distinct from both replication ends (the existing
`ensure_witness_failure_domain` check — the peer API is *expected* to
be colocated with the peer replication end, since the destination
daemon is the destination VMM's proxy (D6); only the witness is a
third domain in this topology); `snapshot_dir` must exist locally
(its cross-host readability is verified at `PREPARED` by the
destination daemon, with a typed refusal); refuse `enabled` when
`vmm` is unconfigured. The new credentials (`witness_host_token`,
`peer_api_token`) join the API journal's redaction set (the
`REDACTED_KEYS` discipline) and are never logged or echoed.

### Consumer-facing routes (source host; journaled, admin-token)

```text
POST /v2/vms/{vm_id}/check-mobility        {target_host}
  -> {eligible, reasons[], participants[]}          # read-only
POST /v2/migrations                         PrepareNearlineHandoff
  {migration_id, volume_ids[], target_host, expected_generations[]}
  -> 201 {state: PREPARED, ...}                     # idempotent by migration_id
POST /v2/migrations/{migration_id}/transfer BarrierAndTransfer
  {vm_paused_and_io_drained_proof}
  -> 202 {state}                                    # long-running; proof recorded as corroboration
GET  /v2/migrations/{migration_id}          ObserveHandoff
  -> {state, state_history[], participants[], in_doubt_detail?, cut_duration_secs?}
POST /v2/migrations/{migration_id}/abort
  -> {state}                                        # typed refusal once the cut began
```

`BarrierAndTransfer`'s consumer proof is **recorded, not trusted**:
volvisor performs and verifies its own pause (§5) — the parameter is
corroboration in the migration record, matching the contract's shape
without depending on the consumer's honesty.

### Internal peer routes (destination host; peer-token, `RequireAdmin`-style guard)

```text
POST /v2/internal/peer/prepare      {migration_id, volume_ids[], expected_generations[]}
POST /v2/internal/peer/grant        {migration_id}        # the GrantSet + promote-under-granted-lease
POST /v2/internal/peer/restore-vm   {migration_id, snapshot_dir, disks[]}
GET  /v2/internal/peer/health       -> {snapshot_dir_readable: bool, ...}
```

Each is idempotent (driven by the coordinator's retry task) and
refuses volumes not matching the migration's participant set. They are
journaled with their own operation ids (the ops.rs pipeline); an
intent-without-outcome replay resolves by inspecting the witness and
the provider state — never a blind re-execution (the same
`OPERATION_IN_DOUBT` discipline as every privileged mutation).

### Provider surface (`HandoffSurface`, the `AdoptionSurface` pattern)

```rust
trait HandoffSurface: Send + Sync {
    async fn handoff_eligibility(&self, vm_id: &str) -> Result<EligibilityReport, ApiError>;
    async fn quiesce_for_barrier(&self, volume_id: &VolumeId, migration_id: &MigrationId) -> Result<QuiesceProof, ApiError>;
    async fn track_sync(&self, volume_id: &VolumeId) -> Result<SyncProof, ApiError>;
    async fn release_source(&self, volume_id: &VolumeId, migration_id: &MigrationId) -> Result<(), ApiError>;
    async fn promote_target(&self, volume_id: &VolumeId, migration_id: &MigrationId, attach: &AttachVolumeRequest) -> Result<AttachVolumeResponse, ApiError>;
    async fn abort_prepare(&self, volume_id: &VolumeId, migration_id: &MigrationId) -> Result<(), ApiError>;
}
```

DRBD implementation notes: `quiesce_for_barrier` = suspend-io +
observed-suspended + the durable cut marker (D6a); `track_sync` =
`drbdsetup status` peer disk state `UpToDate` + no resync +
connection up, **taken after** the suspension (D2); `release_source` =
the demote-verify-release-clear path (the detach tail, minus the
consumer request shape — the demote observes the device closed,
refusing typed otherwise, never forcing); `abort_prepare` = void the
barriers, unsuspend, clear the marker.

`promote_target` is **not** a reuse of `adopt_and_promote` — it is a
sibling path, **promote-under-granted-lease**, sharing the P4a
verification core (`verify_adoption`: lineage, Secondary role, the
definition naming this host, ownership tag) and the entry-creation
tail, with three named deviations:

1. *Authority gate inverted*: adoption refuses any live lease; the
   migration path requires the live lease to be **ours at the epoch
   `GrantSet` minted** (holder = this host, epoch = the granted one,
   lease live), plus the `FencingProof` of the retired source epoch.
   A foreign live lease is still `Unsafe`.
2. *Classification branch*: the P4a classifier's `SafeCurrent`
   requires Protocol C + the registration barrier; the migration path
   adds the migration-barrier evidence class (§7) — protocol-independent,
   because the suspension + `TrackSync` attestation is the claim, not
   the steady-state protocol.
3. *Entry provenance*: the tracked entry is created with migration
   provenance (migration id, granted epoch) and the **attachment
   record** the restore's disk-path verification needs (vm id, host,
   device), not adoption's `"adopted"`-project/`Ready` stamp.

## 7. The `SAFE_CURRENT` unlock

P4a's classifier treats the registration's `RecordedBarrier` as the
only `SAFE_CURRENT` evidence (operator attestation). P4b adds the
machine-checked source. When adoption inspects the witness and finds,
for the retired source epoch, a **non-voided** `RecordBarrier` whose
recorder is W8-bound to that epoch's holder with all three
attestations true (`vm_paused_and_drained`, `data_path_suspended`,
`peer_up_to_date`), recorded while the epoch was current (the
barrier's commit index precedes the retirement record's commit index —
both in the journal fold, both exposed by `inspect`), the
classification is `SafeCurrent` — no `allow_loss` authorization
needed.

The comparison is **ordering, not terminality**: the barrier's
boundary commit index is checked to precede the retirement (the
barrier was recorded before the epoch ended), never to equal the
epoch's final mutation — renewals between barrier and retirement
write no data and are explicitly compatible (a renewal is an authority
lease extension, not a guest write; the data claim lives entirely in
the attestations). When the epoch is **still current at
classification time** (the dead-source mid-cut path, D5 — no
`RevokeSet` ever landed, so no retirement record exists yet), the
comparison target is vacuous: the check reduces to "a non-voided
barrier recorded during this same current epoch, holder-bound, all
attestations true". The retirement-index comparison applies only when
the epoch is already retired (the revoke/grant proof's commit index
is the target). A voided barrier is never evidence. Anything less
keeps the P4a boundaries (`PossibleLoss` with the recorded boundary;
`Unsafe` for unproven fencing). The classifier prefers the strongest
evidence present and records which evidence class justified the
decision in the response.

Residual, stated in code docs and the contract: the attestation's
truth lives on the source host (a compromised or buggy source can
still lie); W8 makes the recorder accountable (and removes the
shared-token forge path entirely — legacy tokens cannot record), the
journal makes the record immutable, and the barrier's claims
(paused/suspended/UpToDate) are each independently re-checkable by an
operator from the surviving host. This is the same trust class as the
P4a self-release.

## 8. Honesty and verification rules

1. No command is issued to the VMM that is not in the verified table
   (§5); the adapter's module docs cite the upstream doc sections and
   the check date. If the pinned CH version's surface differs, the
   adapter fails typed at startup (a `--version`-style capability
   check is a real-host item, §9).
2. Downtime is reported as what it is: the full pause→snapshot→
   delete→demote→grant→promote→restore→resume window. The API response
   for a completed migration carries the measured wall-clock cut
   duration. No "live" claim anywhere; the contract text says
   "coordinated handoff" (the consumer's `migration_policy.live`
   remains `require_verified`, and verification is exactly this
   machinery).
3. `TrackSync`'s Protocol A residual is recorded: after the source
   data path is suspended and the peer reports `UpToDate` with no
   resync and the connection established, the peer is current through
   the barrier *as DRBD's own state machine reports it*; the
   real-host evidence campaign (the out-of-band R4 gate) must confirm
   with the write-trace oracle. The fake models asynchronous peer
   apply so tests prove the coordinator *waits* rather than assumes.
4. No path resumes source writes after the cut begins (rule 5). The
   rollback code exists only for pre-cut states; cut-or-later states
   have no abort handler at all — the type-level guard is that the
   abort function's match arms end at `BarrierDurable`-without-cut.
5. Every consumer-facing and peer-facing mutation is journaled
   (operation id + request hash, the ops.rs pipeline); the migration
   store's transitions are separately durable, write-ahead before
   irreversible acts, and the state history is append-only.
6. Conformance: the shared provider kit stays green — the mobility
   surface is additive (`HandoffSurface` is optional, like
   `AdoptionSurface`).

## 9. Test matrix

Stage B1 (no VMM):

| # | Row |
|---|---|
| 1 | W8: a holder-bound mutation (self-revoke, RecordBarrier, batches) from the wrong host's credential is refused typed; the legacy shared token can inspect but every mutation is refused with the v2 typed error |
| 2 | W9: RecordBarrier accepted from the epoch holder while current; stale-epoch recording refused; byte-identical re-record replays, differing content conflicts; VoidBarrier from the holder before retirement voids it, after retirement refused |
| 3 | W10: RevokeSet/GrantSet apply all-or-nothing (one member failing rejects the batch); GrantSet retires lingering epochs set-wide; one commit-index bump; batch retries with the same operation id replay the recorded outcome |
| 4 | The state machine: every legal transition persists before reporting; replayed steps reconcile external facts and skip what is true; illegal transitions refused; `state_history` is append-only under concurrent observe + crash-replay (no duplication, no edits) |
| 5 | Reconcile: each pre-cut state rolls back (void barriers, unsuspend, resume, discard); rollback failure routes through `self_fence` (durable marker), never a silent resume |
| 6 | Reconcile with a cut: forward-only from every cut step, detecting already-done external facts (VM absent, role Secondary, lease revoked, epoch granted) instead of repeating them; no abort path exists for these states |
| 7 | The cut write-ahead: the store records each cut step BEFORE its external act (crash between write-ahead and act lands in the correct forward re-drive) |
| 8 | `quiesce_for_barrier`/`track_sync`: suspension observed + cut marker durable; the provider's startup reconcile does NOT resume a migration-suspended Primary (D6a); attach/detach of a cut-marked volume refused typed |
| 9 | Peer lag (fake async apply) makes the barrier wait; no convergence → typed timeout, abort before the cut |
| 10 | `release_source` refuses typed while the device is open (rule 17) and completes after `destroy`; one-of-N participants refusing holds the whole RevokeSet — never a subset release |
| 11 | The classifier: a non-voided migration barrier (all attestations, recorder-bound, pre-retirement ordering) → `SAFE_CURRENT` without `allow_loss`, protocol-independent; a barrier followed by renewals then retirement → still `SAFE_CURRENT` (ordering, not terminality); a voided barrier → never evidence; a short-boundary or partial-attestation barrier → `PossibleLoss` with the recorded boundary; no barrier → P4a behavior unchanged |
| 12 | Multi-volume: one unprepared participant refuses the whole migration (VM-wide eligibility) |

Stage B2 (fake VMM, end-to-end):

| # | Row |
|---|---|
| 13 | Happy path, two-volume VM: PREPARED→COMPLETE; VM resumed on the target (fake VMM Running, devices open on the target minors, closed on the source); witness epochs retired; barriers recorded; source Secondary |
| 14 | Rule-17 ordering proof: the demote is attempted only after the VM destroy (a coordinator regression that demotes early fails against the fake's busy device) |
| 15 | Crash injection between every pair of cut steps (the store dropped mid-drive): reconcile lands forward-only in the correct resolution; specifically crash between delete and demote, and between demote and revoke — the states the round-1 review identified — resolve forward, never to the abort path |
| 16 | Abort before the cut: source VM resumed, barriers voided, volumes unsuspended, target discarded, record `Aborted`; abort after the cut began: typed refusal. The void-failure path specifically: a rollback whose `VoidBarrier` cannot be journaled (witness unreachable) leaves the source **not resumed** and routes through `self_fence` — the unvoided barrier is a hard gate on resume |
| 16a | The clear-cut-marker admin operation: refuses while the volume is Primary/writer; clears + reconciles when Secondary or fencing-proven; journaled |
| 16b | Terminal-`InDoubt` recovery: a record stalled by a failed void re-attempts the abort once the witness is reachable → `Aborted`, source resumed (after void confirmation); never resumes while the witness is down |
| 17 | Dead source inside the window: the destination adopts (P4a path) and the recorded barrier yields `SAFE_CURRENT` — D5 convergence |
| 18 | Half-restored destination: a crashed restore is re-driven by destroying the partial VM first; a dead destination VMM stalls in `IN_DOUBT` with a typed detail |
| 19 | Snapshot-dir unreadable at the destination: `PREPARED` refuses typed (peer health check) |
| 20 | Eligibility: a VM with one native-local or ceph volume → ineligible with typed reasons; a fenced/failed volume → ineligible |
| 21 | API: journaling and idempotency for prepare/transfer/abort (replay byte-identical, hash conflict typed); ObserveHandoff reports the exact canonical states, maps cut-progress to `IN_DOUBT` with step detail, and never collapses the revoke/grant pair; the peer routes resolve intent-without-outcome by inspecting witness/provider state, never a blind re-execution |
| 22 | One-of-two target promotes failing blocks the VM restore (forward-retried; the VM is never resumed half-migrated) |
| 23 | The restore's `config.json` disk-path rewrite: matching minors (no-op) and divergent paths (rewrite verified) |
| 24 | Config validation: witness-third-domain guard, provider requirement, snapshot-dir existence, vmm requirement, `witness_host_token` required whenever `witness_url` is set (v2 witness — the base P4a surface depends on it), credential redaction in journaled payloads |

Real-host items (env-gated, like the P4a `VOLVISOR_TEST_DRBD` gate;
no CI claim): `VOLVISOR_TEST_CH=1` runs the adapter against a real
`ch-remote` + `cloud-hypervisor` pair on one host (pause/info/
snapshot/delete/restore/resume round-trip, including the device-open
semantics against a real DRBD minor); the two-host R4 evidence
campaign with the write-trace oracle remains the production gate and
is explicitly **not** claimed by this plan.

## 10. Contract and documentation updates (stage B2)

- `contracts/volume-api-v2.md` §5: the concrete routes, the
  snapshot-dir sharing requirement, the downtime statement, and the
  `BarrierAndTransfer` proof-as-corroboration semantics. **Reachability
  amendment**: the `IN_DOUBT` wording ("between `SOURCE_REVOKED` and
  `DESTINATION_AUTHORIZED`") is amended to "`IN_DOUBT` is reachable
  once the cut is entered (the durable point of no return, at or
  after the source-side barrier), when a pre-cut rollback cannot
  complete safely (a failed barrier void — fail-closed, the source is
  never resumed), and through any unresolvable post-authorization
  stall before `COMPLETE` (e.g. a dead destination VMM; a resolvable
  stall is reported as the canonical state plus a stall detail); it
  must never be reported
  as a generic `ABORTED`" — every one of these observations is the
  fail-closed direction the contract's spirit intends, and the letter
  is amended to say so.
- `contracts/nearline-replication-v2.md` §6: the same `IN_DOUBT`
  reachability amendment, plus an implementation-status
  note (the canonical states are now served by volvisor's coordinator;
  memory pre-copy and dual-primary remain out of scope with the §1
  reasons); §1's durable-state sketch gains the migration record
  pointer.
- `docs/plans/2026-10-09-p4-witness-fencing-authority.md` §9: the P4b
  preview is superseded by this plan (a pointer edit).
- The P4a residual list: the witness per-client identity item is
  closed by W8.

## 11. Completion definition for P4b

Both stages merged with clean adversarial review verdicts; the §9
matrix green in CI; the conformance kit green; fmt/clippy(-D
warnings, both feature sets)/doc clean; the contracts updated; and
every claim in this plan either implemented or explicitly listed in
§1's out-of-scope table with its reason.

Additionally, these normative invariants are completion gates in
their own right (the design reviews' findings — an implementation
that deviates from any of them has not delivered this plan):

- **G1 (D1a)**: no crash window between the VM destroy and the
  store's revocation record can reach a rollback path; cut-or-later
  is forward-only and `IN_DOUBT`-observable.
- **G2 (§7)**: the classifier's comparison is ordering-before-
  retirement with full attestations and a bound recorder — never an
  equality against an epoch's final commit index; renewals after the
  barrier do not downgrade the classification.
- **G3 (§6)**: `promote_target` is promote-under-granted-lease with
  the three named deviations from `adopt_and_promote`; it never runs
  adoption's no-live-lease gate against the granted lease.
- **G4 (D6a)**: the migration-cut marker is durable, honored by the
  provider's startup reconcile (no auto-resume), and cleared only by
  the coordinator or the fencing-gated clear-cut-marker admin
  operation (D6a).
- **G5 (§3/W9, round-2)**: an unvoided recorded barrier is a hard
  gate on any source resume; a `VoidBarrier` that cannot be
  journaled — for any reason — fails the rollback into `self_fence`,
  never a resume. Every barrier of the epoch must be confirmed voided
  before the source VM resumes.

Production support is **not** claimed: the real-host evidence
campaign (nearline contract §10) is the next implementation-order
item's prerequisite, not this phase's.
