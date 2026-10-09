# P4b implementation plan — full VMM/storage handoff (stage B)

Status: normative for the P4b phase (this document is the plan of record;
implementation PRs cite it)
Date: 2026-10-09
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
   per-client host identities, `RecordBarrier`, and batch
   (all-writable-volume) authority mutations.
3. A **provider handoff surface** (`HandoffSurface`, the
   `AdoptionSurface` pattern applied to migration): source-side
   quiesce/barrier/transfer primitives and the target-side
   promote-with-proof, on the DRBD provider.
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
- **Ceph RBD / native-local migration.** `CheckVmStorageMobility`
  reports them ineligible with typed reasons (contract §5); no
  implementation.
- **Snapshot transport between hosts.** The v1 cutover writes the
  memory snapshot to a configured directory that the destination host
  can read (shared filesystem, operator-provided). Volvisor does not
  copy multi-GB snapshots itself; without a shared path the migration
  is refused at `PREPARED` with a typed reason. Honest availability
  boundary, recorded in the contract text.
- **Witness garbage collection, multi-peer (>2 data ends) topologies,
  rate-control policy (CONVERGE heuristics).** The state vocabulary and
  the barrier proofs are delivered; dirty-rate-driven scheduling is a
  later refinement (the contract places CONVERGE inside `PRECOPY` as
  policy, not protocol).

### Staging (review-surface split, like P4a's two-PR shape)

- **Stage B1 (one PR): the authority substrate.** Witness extensions
  (identities, `RecordBarrier`, batch mutations, inspect exposure),
  the `volvisor-handoff` state machine + store + reconcile, the
  `HandoffSurface` on the DRBD provider, and the
  `SAFE_CURRENT` adoption unlock. Fully testable without a VMM (the
  state machine is driven by a fake driver; the provider surface by
  the existing fake world).
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
PRECOPY: DRBD replica catch-up observed
  (TrackSync); no memory pre-copy (out of scope, above)
QUIESCED: ch-remote pause
  + vm.info state == Paused (verified)
  + provider suspend_io on every
    participating volume (enforcement point)
BARRIER_DURABLE: TrackSync proof
  (peer UpToDate, no resync, after the
  suspension fixed the boundary)
  + witness RecordBarrier (authenticated)
SOURCE_REVOKED: ch-remote snapshot          (memory+state to the shared
  + ch-remote delete (VM destroyed —          snapshot dir)
    the source device closes)
  + drbdadm secondary (demote, device
    now closed; verified Secondary)
  + witness batch revoke (self-release,
    after proven demotion — the P4a
    release-after-proof discipline)
  + migration store: SOURCE_REVOKED       [IN_DOUBT window begins]
DESTINATION_AUTHORIZED:                    witness batch grant (new epochs,
                                             retires leftovers — W2)
                                           migration store records it
                                           [IN_DOUBT window ends]
VM_RESUMED:                                ch-remote restore (pre-started
                                             destination VMM, snapshot dir,
                                             disk paths checked against the
                                             promoted devices)
                                           + ch-remote resume
                                           + attachment identity verified
COMPLETE: source reconciled Secondary,
  migration record closed
```

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
- **D2 — the barrier is fixed by suspension, then proven by
  observation.** `QUIESCED` freezes the source data path at the kernel
  enforcement point (`drbdsetup suspend-io`) after the VMM pause is
  verified. Only then is replication catch-up observed (`TrackSync`:
  peer disk `UpToDate`, no resync in progress, connection
  established). This ordering makes the recorded boundary exact rather
  than estimated; a catch-up observation taken before the freeze
  proves nothing about the boundary (ADR-0004 Decision 2's
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
- **D4 — the source self-release is the P4a discipline.** The batch
  revoke happens only after a **proven** demotion (device closed by
  VM deletion, `drbdadm secondary` succeeded, role re-verified from
  `drbdsetup status`). A self-release waives the next grant's W7 wait;
  it is earned exactly as in detach/adopt: never while a writer might
  still be serving.
- **D5 — dead-source `IN_DOUBT` converges to the P4a adoption path.**
  If the source host dies inside the window (or after
  `SOURCE_REVOKED`), the destination does not need the source's
  migration record: the surviving host resolves through
  `adopt-and-promote`, and the migration-recorded barrier upgrades the
  classification to `SAFE_CURRENT` (§7). One recovery vocabulary, two
  entry points (coordinator-driven roll-forward and adoption), no
  special cases.
- **D6 — the coordinator runs on the source host's daemon.** It hosts
  the consumer-facing mobility API, drives the destination host's
  daemon through an internal authenticated peer API (the destination
  VMM socket is host-local, so destination-side VMM actions must be
  proxied by the destination daemon), and speaks to the witness
  directly. The destination's actions are idempotent provider
  operations driven over the peer API; its own startup reconcile (the
  P4a adoption-record pattern) covers half-done promotes.

## 3. The migration state machine (`volvisor-handoff`)

### Types

- `MigrationId` — validated opaque string (the `OperationId` rules:
  length, charset; consumer-supplied, unique per migration).
- `HandoffState` — the canonical vocabulary, exactly the contract's:
  `Prepared`, `Precopy`, `Quiesced`, `BarrierDurable`,
  `SourceRevoked`, `DestinationAuthorized`, `VmResumed`, `Complete`,
  plus the terminal observations `InDoubt { since, detail }` and
  `Aborted { reason, at }`. Serde-tagged, forward-compatible
  (`deny_unknown_fields` per the house schema rules; new fields go
  through schema bumps, not silent rewrites).
- `MigrationRecord` — one per `MigrationId`:
  `migration_id, vm_id, source_host, target_host, participants[]`
  (each: `volume_id, expected_generation, resource, minor`),
  `state`, `state_history[]` (append-only, monotonic — the observable
  trace `ObserveHandoff` reports), `barrier_proofs[]` (per-volume:
  boundary commit index, attestation, recorded_at),
  `abort_policy` (`AutoBeforeSourceRevoked` — the only v1 policy),
  `created_at/updated_at`.
- `MigrationStore` — atomic-save JSON (write-temp + fsync + rename,
  the `DrbdState` discipline) at a configured path, keyed by
  `MigrationId`; `load`, `upsert`, `remove` (terminal records are
  retained, prunable by age later — out of scope).
- `MigrationCoordinator` — the driver. Holds: the source
  `Arc<dyn HandoffSurface>`, a witness client, a destination peer
  client (§6), a `VmmController`, the store, and a clock. Every
  transition is: perform the side effects → persist the store → only
  then report the new state. Transitions are idempotent (replay of a
  recorded step is a no-op check, never a re-execution with different
  effects).

### Reconcile (startup + retry task)

A background task (the renewal-task pattern) and a startup pass drive
`resolve(record)`:

- `Prepared|Precopy|Quiesced|BarrierDurable` → **abort path**: source
  authority is intact, so rollback is allowed — unsuspend every
  participating volume, `ch-remote resume` the source VM (if paused),
  discard the target-side preparation, record `Aborted`. If the
  rollback itself fails, the volume is left suspended with the
  migration record `InDoubt`-annotated — fail-closed, never a silent
  resume.
- `SourceRevoked` → **forward only**: retry the destination grant
  batch until it succeeds or the operator intervenes; the state stays
  `InDoubt`-observable (`SourceRevoked` with an in-flight flag; the
  API surfaces it as `IN_DOUBT` per the contract's terminal
  observation). The source VM is already destroyed; there is no
  rollback and the code must not grow one.
- `DestinationAuthorized|VmResumed` → **forward completion**: drive
  restore/resume/reconcile to `Complete`.
- `Complete|Aborted` → nothing.

The reconcile is the only writer of `state_history` besides the live
drive — both go through the same transition function.

## 4. Witness extensions (W8–W10)

New invariants, unit-tested like W1–W7:

- **W8 (caller identity binding).** The witness configuration grows a
  per-host credential map (`HostId → token`; the existing single
  shared token remains valid as a legacy *unbound* credential, and
  every mutation it performs is journaled with
  `identity: unbound-legacy` and logged — deployment hardening, not a
  silent trust change). A mutation that asserts a holder (`grant`'s
  `host_id`, `renew`'s, a self-`revoke`, `RecordBarrier`'s recorder)
  is accepted only from that holder's credential. This closes the
  P4a-recorded residual ("per-client identities are P4b hardening")
  and is the trust root for W9.
- **W9 (recorded barrier).** New journaled mutation
  `RecordBarrier { volume_id, holder, boundary_commit_index,
  attestation { vm_paused_and_drained, data_path_suspended,
  peer_up_to_date }, migration_id?, recorded_at }`. Enforced: only the
  current epoch's holder (W8) may record; the boundary must be ≤ that
  epoch's last commit index; records are immutable once journaled
  (a re-record for the same epoch is an `IDEMPOTENCY_CONFLICT` unless
  byte-identical). `inspect` exposes the per-volume barrier log (the
  latest barrier + the epoch/boundary it attests), replacing
  registration-time operator attestation as the machine-checkable
  evidence source. The registration barrier (P4a) remains valid
  evidence; a migration-recorded barrier at the retired epoch's final
  commit index is strictly stronger.
- **W10 (batch atomicity).** `RevokeSet { releases[], migration_id }`
  and `GrantSet { requests[], migration_id }` are single journaled
  mutations: the fold applies all-or-nothing, one commit-index bump
  for the set, per-volume responses computed inside the envelope (the
  existing `MutationEnvelope` discipline). A partial failure rejects
  the whole batch typed. `GrantSet` mints new epochs per volume and
  retires any lingering epochs (W2 semantics, set-wide); `RevokeSet`
  from the source is a set of self-releases (W8-bound) that waive W7
  legitimately — every member was proven demoted before the call.

The witness protocol version bumps to 2 (new routes/mutations; the
client negotiates). `inspect` gains the barrier log; the `AuthorityView`
shape change is additive.

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
    fn state(&self, vm_id: &str) -> Result<VmState, ApiError>;
}
```

`DiskMapping` carries the snapshot config's declared path → the target
host's promoted device path; the adapter rewrites the copied
`config.json` when they differ (DRBD minors are symmetric across the
replication pair in the common case, so the rewrite is usually a
no-op — but it is verified, never assumed). `PauseProof` is the
observed `Paused` state from `vm.info` — volvisor's own verification,
not the consumer's attestation.

### The fake VMM (test harness)

`FakeVmm` models what the cutover actually depends on: it holds the
VM's devices open (integrating with `FakeDrbd.open_devices` — the
minor is inserted while the VM "runs" and removed on `destroy`), tracks
`Running|Paused|Destroyed` and snapshot directories (real files in a
tempdir, so a restore genuinely reads what the snapshot wrote). This
makes rule 17 *provable in tests*: any coordinator bug that demotes
before `destroy` fails against the fake's busy refusal, exactly as it
would on a real host.

## 6. Daemon and API surface

### Config additions (`volvisord`)

```toml
[migration]
enabled = true                       # false until the deployment opts in
snapshot_dir = "/var/lib/volvisor/migrations"   # must be shared with the peer for cross-host cutover
peer_api_url = "http://peer-host:7780"
peer_api_token = "…"                 # daemon-to-daemon credential (distinct from the witness and consumer tokens)
witness_host_token = "…"             # this host's W8 credential
[vmm]
ch_remote_bin = "/usr/bin/ch-remote"
api_socket_dir = "/run/volvisor/vms"
```

Validation (the `config.rs` discipline): `migration.enabled` requires
the drbd provider + a witness; `peer_api_url` must not share a failure
domain with the local replication end or the witness (the existing
`ensure_witness_failure_domain` checks extended); `snapshot_dir` must
exist; refuse `enabled` when `vmm` is unconfigured.

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
  -> {state, state_history[], participants[], in_doubt_detail?}
POST /v2/migrations/{migration_id}/abort
  -> {state}                                        # typed refusal after SOURCE_REVOKED
```

`BarrierAndTransfer`'s consumer proof is **recorded, not trusted**:
volvisor performs and verifies its own pause (§5) — the parameter is
corroboration in the migration record, matching the contract's shape
without depending on the consumer's honesty.

### Internal peer routes (destination host; peer-token, `RequireAdmin`-style guard)

```text
POST /v2/internal/peer/prepare      {migration_id, volume_ids[], expected_generations[]}
POST /v2/internal/peer/grant        {migration_id}        # the GrantSet + promote-with-proof
POST /v2/internal/peer/restore-vm   {migration_id, snapshot_dir, disks[]}
GET  /v2/internal/peer/health
```

Each is idempotent (driven by the coordinator's retry task) and
refuses volumes not matching the migration's participant set.

### Provider surface (`HandoffSurface`, the `AdoptionSurface` pattern)

```rust
trait HandoffSurface: Send + Sync {
    async fn handoff_eligibility(&self, vm_id: &str) -> Result<EligibilityReport, ApiError>;
    async fn quiesce_for_barrier(&self, volume_id: &VolumeId, migration_id: &MigrationId) -> Result<QuiesceProof, ApiError>;
    async fn track_sync(&self, volume_id: &VolumeId) -> Result<SyncProof, ApiError>;
    async fn release_source(&self, volume_id: &VolumeId, migration_id: &MigrationId) -> Result<(), ApiError>;
    async fn promote_target(&self, volume_id: &VolumeId, migration_id: &MigrationId) -> Result<AttachVolumeResponse, ApiError>;
    async fn abort_prepare(&self, volume_id: &VolumeId, migration_id: &MigrationId) -> Result<(), ApiError>;
}
```

DRBD implementation notes: `quiesce_for_barrier` = suspend-io +
observed-suspended; `track_sync` = `drbdsetup status` peer disk state
`UpToDate` + no resync + connection up, **taken after** the suspension
(D2); `release_source` = the demote-verify-release-clear path (the
detach tail, minus the consumer request shape — and the demote must
observe the device closed, refusing typed otherwise, never forcing);
`promote_target` = the adopt-and-promote core with the migration
context (no `allow_loss`: the recorded barrier is the proof, §7).

## 7. The `SAFE_CURRENT` unlock

P4a's classifier treats the registration's `RecordedBarrier` as the
only `SAFE_CURRENT` evidence (operator attestation). P4b adds the
machine-checked source: when adoption inspects the witness and finds a
migration-recorded barrier for the retired epoch whose
`boundary_commit_index` equals that epoch's last commit index, with
`peer_up_to_date` attested and the recorder W8-bound to the retired
holder, the classification is `SafeCurrent` — no `allow_loss`
authorization needed. Anything less keeps the P4a boundaries
(`PossibleLoss` with the recorded boundary; `Unsafe` for unproven
fencing). The classifier prefers the strongest evidence present and
records which evidence class justified the decision in the response.

Residual, stated in code docs and the contract: the attestation's
truth lives on the source host (a compromised or buggy source can
still lie); W8 makes the recorder accountable, the journal makes the
record immutable, and the barrier's claims (paused/suspended/UpToDate)
are each independently re-checkable by an operator from the surviving
host. This is the same trust class as the P4a self-release.

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
4. No path resumes source writes after `SOURCE_REVOKED` (rule 5). The
   rollback code only exists for pre-revocation states and lives
   behind a type-level state guard (`SourceRevoked` and later states
   have no `abort` handler at all).
5. Every consumer-facing and peer-facing mutation is journaled
   (operation id + request hash, the ops.rs pipeline); the migration
   store's transitions are separately durable and the state history is
   append-only.
6. Conformance: the shared provider kit stays green — the mobility
   surface is additive (`HandoffSurface` is optional, like
   `AdoptionSurface`).

## 9. Test matrix

Stage B1 (no VMM):

| # | Row |
|---|---|
| 1 | W8: a holder-bound mutation (self-revoke, RecordBarrier) from the wrong host's credential is refused typed; the legacy shared token still works and journals `unbound-legacy` |
| 2 | W9: RecordBarrier accepted from the epoch holder, boundary ≤ last commit index; over-boundary and stale-epoch recordings refused; re-record byte-identical replays, different content conflicts |
| 3 | W10: RevokeSet/GrantSet apply all-or-nothing (one member failing rejects the batch); GrantSet retires lingering epochs set-wide; one commit-index bump |
| 4 | The state machine: every legal transition persists before reporting; replayed steps are no-ops; illegal transitions refused |
| 5 | Reconcile: each pre-revocation state rolls back (unsuspend + resume + discard); rollback failure → suspended + InDoubt-annotated, never a silent resume |
| 6 | Reconcile: SourceRevoked rolls forward only; the grant retry eventually completes; no code path re-grants the source |
| 7 | `quiesce_for_barrier`/`track_sync` on the DRBD provider: suspension observed; peer lag (fake async apply) makes the barrier wait; no convergence → typed timeout, abort before revocation |
| 8 | `release_source` refuses typed while the (fake) device is open — rule 17 — and completes after `destroy` |
| 9 | The classifier: a migration-recorded barrier at the final commit index → `SAFE_CURRENT` without `allow_loss`; a short-boundary barrier → `PossibleLoss` with the recorded boundary; no barrier → P4a behavior unchanged |
| 10 | Multi-volume: one unprepared participant refuses the whole migration (VM-wide eligibility) |

Stage B2 (fake VMM, end-to-end):

| # | Row |
|---|---|
| 11 | Happy path, two-volume VM: PREPARED→COMPLETE; VM resumed on the target (fake VMM state Running, devices open on the target minors, closed on the source); witness epochs retired; barriers recorded; source Secondary |
| 12 | Rule-17 ordering proof: the demote is attempted only after the VM destroy (a coordinator regression that demotes early fails against the fake's busy device) |
| 13 | Crash injection between every pair of states (the store is dropped mid-drive): reconcile lands in the correct resolution per §3; specifically crash between revoke and grant → IN_DOUBT observable, forward-only |
| 14 | Abort before barrier: source VM resumed, volumes unsuspended, target discarded, record `Aborted` |
| 15 | Abort after SOURCE_REVOKED: typed refusal |
| 16 | Dead source inside the window: the destination adopts (P4a path) and the recorded barrier yields `SAFE_CURRENT` — D5 convergence |
| 17 | Eligibility: a VM with one native-local or ceph volume → ineligible with typed reasons; a fenced/failed volume → ineligible |
| 18 | API: journaling and idempotency for prepare/transfer/abort (replay byte-identical, hash conflict typed); ObserveHandoff reports the exact canonical states and never collapses the revoke/grant pair |
| 19 | Peer API: wrong token refused; non-participant volume refused; idempotent re-drive |
| 20 | Config validation: the failure-domain and provider guards of §6 |

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
  `BarrierAndTransfer` proof-as-corroboration semantics.
- `contracts/nearline-replication-v2.md` §6: an implementation-status
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
§1's out-of-scope table with its reason. Production support is **not**
claimed: the real-host evidence campaign (nearline contract §10) is
the next implementation-order item's prerequisite, not this phase's.
