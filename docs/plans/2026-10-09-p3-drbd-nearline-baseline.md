# P3 implementation plan — DRBD 9 nearline baseline (`nearline-replicated`)

Date: 2026-10-09
Status: implementing
Branch: `feat/p3-drbd-nearline-baseline`
Normative: [ADR-0007](../adr/0007-drbd9-nearline-replication-provider.md) (Proposed /
prototype selection), [nearline contract v2](../../../contracts/nearline-replication-v2.md)
§3, §4, §9A, [Volume API v2](../../../contracts/volume-api-v2.md), AGENTS rules 1–21.

ADR-0007 is **Proposed, not accepted**: this phase delivers the *prototype
baseline* the ADR calls for, with `evidence_status: PrototypeOnly` everywhere
and no production, RPO or durability claims. The next implementation-order
phase (witness/fencing and full VMM/storage handoff) owns writer epochs,
leases, unplanned promotion and migration; nothing here may pre-claim them.

## 1. Scope

In (P3 slice: "DRBD nearline baseline" per the implementation order):

- A `volvisor-drbd` crate implementing `VolumeProvider` for
  `VolumeClass::NearlineReplicated` on the **local host** (ADR-0007 deployment
  variant A: host-kernel DRBD over an operator-designated LVM volume group),
  driven through the unmodified `drbdadm`/`drbdsetup`/`blockdev`/LVM CLIs:
  argv arrays, no shell, watchdog-bounded shared `CommandRunner`.
- Volvisor owns exactly the local end of each resource: the local backing LV,
  the generated resource definition file, the resource lifecycle
  (`create-md`, `up`, `down`) and the local role (`primary`/`secondary`).
  DRBD owns replication (AGENTS rule 13: no new engine, ever).
- Single-writer attach mapped to DRBD single-primary: attach = promote, the
  `/dev/drbdN` device is the host-scoped ephemeral handle; detach = demote,
  which the **kernel refuses while the device is open** — an honest,
  enforcement-point-level single-writer release (contract §2's spirit; the
  lease/epoch machinery itself is the next phase).
- Replication policy from the request: `async`→Protocol A (default),
  `semi-sync`→B, `sync`→C, written into the resource definition and reported
  as the **effective** mode read back from the running resource; mode is
  fixed at create (runtime switching is a follow-up). Protocol A is
  possible-RPO and is never reported as durable protection (AGENTS rule 16).
- Honest status via `drbdsetup status`: role, connection state, peer disk
  states and resync progress reported as observed facts; health mapped
  conservatively; remote protection reported only from observed peer state,
  never from configured intent.
- Grow-only resize: local LV grow → `drbdadm resize` → effective size
  verified from the DRBD device itself (`blockdev --getsize64`); if the peer
  backing was not grown, the typed failure reports the honest boundary and
  the actual local size — never a silent partial claim.
- Erasure policy: `Retain` → resource down + resource file removed + backing
  LV retained (recoverable, mirroring the LVM provider's Retain); `ZeroDiscard`
  → typed `UNSUPPORTED_CLASS_OR_POLICY` (fail-closed policy negotiation, same
  reasoning as P2: DRBD/LVM discard does not guarantee block-level zeroing).
- Daemon selection: `provider = "drbd"` with fail-closed startup verification
  (`drbdadm --version` answers, the DRBD kernel module is loaded, the
  configured VG exists and is queryable, the peer node and a shared-secret
  file are configured); refuse to start otherwise.
- `FakeDrbd` deterministic world + full provider conformance-kit pass (behind
  the same per-crate class adapter P2 uses); unit/behavior tests mirror real
  CLI failure modes. Real-cluster tests behind `VOLVISOR_TEST_DRBD=1` (no
  DRBD module, utilities or second host exist in this environment or CI; the
  gate exists for real hosts).

Out (recorded follow-ups; each maps to the next implementation-order phase
or an ADR-0007 evidence item):

- Writer epochs, leases, quorum/witness, fencing and unplanned promotion
  (`SAFE_CURRENT`/`POSSIBLE_LOSS`/`UNSAFE` classification) — next phase.
- Planned migration / `BeginHandoff` / dual-primary — prohibited by default
  (contract §9A; AGENTS rule 17); no code path may enable it.
- Peer-end provisioning automation (a peer volvisor or RPC), runtime
  protocol switching, `drbd-reactor`/LINSTOR coordination, cell variant B,
  local mirror legs under the backing LV (`local_protection=mirror` → typed
  rejection in P3), snapshots, real-cluster CI job, resync rate-control
  tuning.
- `ReplicationProvider` as a *separate* engine-neutral trait surface
  (ADR-0007): P3 maps its create/attach/status/policy/delete
  responsibilities onto `VolumeProvider`; Seed/Promote/Demote/Recover/
  BeginHandoff beyond attach/detach arrive with the fencing phase.

## 2. Crate structure

- New workspace member `volvisor-drbd`: `state` (durable JSON, 0600, atomic
  save — same conventions as `volvisor-ceph`), `resgen` (deterministic
  resource-definition file generation + atomic write), `report` (strict
  parsing of `drbdsetup status` text and LVM `lvs` JSON), `provider` (the
  `VolumeProvider` impl), `tests/common` (FakeDrbd world), conformance via
  the shared kit macro behind a class adapter.
- LV naming/tagging mirrors `volvisor-lvm`'s conventions (injective
  `vol-{sanitized-id}-{hash8}`, `volvisor.owner`/`volvisor.generation`
  LV tags) so nearline backing LVs are identifiable and never confused with
  `native-local` LVs (different, operator-designated VG).
- Minor numbers and peer ports are volvisor-allocated from persisted
  counters in state (bounded ranges; exhaustion is a typed
  `NO_SAFE_CAPACITY`-class refusal, never a silent reuse).

## 3. Command set and resource files (all argv; never shell)

- `drbdadm --version` — startup verification.
- `drbdsetup status <res>` — role, connection and peer disk state (the
  machine-readable form and exact grammar must be verified against the real
  drbd-utils source before the parser is written; the fake then emits the
  verified shape verbatim — the P2 lesson: never invent output shapes).
- `drbdadm -c <volvisor .res file> create-md <res>` (fresh metadata),
  `up <res>`, `down <res>`, `primary <res>` / `secondary <res>`,
  `resize <res>`.
- Every `drbdadm` invocation passes `-c` pointing at **volvisor's own
  single-resource file**: no global `/etc/drbd.conf` reliance, no chance of
  touching a foreign resource (rule 7).
- LVM: `vgs`/`lvs` (existence, size, tags), `lvcreate`, `lvremove` — the
  same argv forms `volvisor-lvm` uses.
- `blockdev --getsize64 /dev/drbdN` — post-resize effective size proof.
- Resource file: generated under a configured directory
  (`drbd_config_dir`, default `/etc/drbd.d`), named
  `volvisor-<resource>.res`, with a "managed by volvisor" header, explicit
  `device /dev/drbdN` (our allocated minor), `protocol` from the requested
  mode, `meta-disk internal`, both `on` nodes (local + configured peer) with
  addresses/ports and the `shared-secret` from the configured secret file —
  the secret is read at generation time from a path reference, never
  configured inline, never logged, and the generated file is 0600.

## 4. Ownership and identity (rule 7)

- Resource name and LV name are the same injective
  `vol-{sanitized-volume-id}-{hash8}` scheme; the resource file, LV tags
  (`volvisor.owner`, `volvisor.generation`) and state entry agree.
- Before every mutation: the resource file must exist with our header, and
  `lvs` must show the backing LV with matching owner tag. Mismatch or
  missing markers on state we have → volume marked `Failed`, never adopted
  or silently removed; reconcile clears a stale attachment record exactly as
  P2 does (audit trail included) while never touching an actual device.
- A resource/LV without our markers is foreign: reported, never touched.
  `primary --force` (initial seeding of a fresh resource) is used **only**
  on a resource volvisor just created whose local LV is provably fresh; a
  peer observed `UpToDate` at first connection is foreign data — the
  resource is refused, never overwritten.

## 5. Single writer and attach (contract §3, §9A)

- Attach: verify ownership → `drbdadm primary` → verify the role and device
  via `drbdsetup status` → record the attachment (`/dev/drbdN` handle,
  `prepared` state; no VMM integration, honestly not advertised/active).
  A second attach with a recorded attachment is the contract's typed
  `WRITER_ALREADY_ACTIVE` rejection; a resource found Primary without a
  record is a typed `INVALID_STATE` rejection and a reconcile `Failed`.
- Detach: verify the record and role → `drbdadm secondary` → verify. If the
  device is still open, the kernel refuses the demotion — surfaced as the
  typed error it is (with the drain-proof remedy), never forced (rule 17).
- Read-only (shared-reader) attach is rejected with a typed
  `UNSUPPORTED_CLASS_OR_POLICY`: dual-primary is forbidden by default and a
  read-only claim over a writable primary would be a fail-open lie (same
  reasoning as P2).
- Delete requires full detachment and Secondary role, plus no live
  attachment record; the resource file is removed only after `down`
  succeeds and is verified.

## 6. Replication policy, health and honesty (rules 12, 16)

- `allow_degraded_create=false` (default): create succeeds only once the
  peer connection is established (the resource reports the peer);
  `true`: create succeeds while the peer is absent and the volume is
  honestly reported degraded (`health: Degraded`, remote protection not
  established) until reconcile observes the peer.
- Health mapping (observed facts only): connected + local and peer disks
  `UpToDate` → `Healthy`; resync/paused/peer `Inconsistent` or connection
  down → `Degraded`; local disk failure or unknown → `Unhealthy`/`Unknown`.
  Protocol A never upgrades remote reporting to a durability claim: the
  remote protection axis states that a remote replica is *currently
  established and observed `UpToDate`*, not that acknowledged writes are
  remotely durable.
- Inspect reports the effective mode read back from the resource (never the
  requested value alone) and `evidence_status: PrototypeOnly`.
- Capacity: `vgs` free extents in the nearline VG minus headroom; typed
  `NO_SAFE_CAPACITY` refusal. LV extent rounding: requested vs effective
  size recorded like LVM.

## 7. Peer model (P3 boundary)

- The peer host (name, address) and its backing LV are
  **operator-provisioned** out of band: the generated resource file defines
  both ends and the operator deploys the identical definition on the peer
  (a future peer-volvisor automates this). Volvisor never writes to the
  peer, never adopts peer state beyond what `drbdsetup status` observes,
  and never claims the peer is volvisor-managed.
- Grow requires the peer backing to be grown first (operator action);
  volvisor grows the local LV, resizes, and fails closed with the honest
  boundary if the effective device size did not reach the request.

## 8. Daemon and config

- `provider = "drbd"` with: `drbd_vg_name` (operator-designated, must
  exist), `drbd_config_dir`, `drbd_node_name` (local `on` name, validated
  against `uname -n` output), `drbd_peer_name` + `drbd_peer_address`,
  `drbd_shared_secret_file` (path reference, required — v1 invariants keep
  peer authentication binding), `drbd_port_range`, `drbd_state_path`
  (default `<journal_dir>/drbd-state.json`).
- Startup: version query succeeds, module present, VG exists, peer and
  secret configured. Any failure → refuse to start (fail-closed).
- No `AdminSurface` (operator-designated VG; no device claiming) — admin
  routes return the existing typed 404.

## 9. Milestones

- M0: this plan.
- M1: `volvisor-drbd` core: state, resgen, report parsing, provider impl
  (create/inspect/list/attach/detach/grow/delete/reconcile).
- M2: FakeDrbd world, conformance-kit pass, failure injection (foreign
  resource, demotion refused while open, peer absent, resync, seeding
  refusal on UpToDate peer, grow boundary), `VOLVISOR_TEST_DRBD=1` path.
- M3: daemon config + wiring + e2e (fake path), README/config example.
- M4: adversarial review → fix → merge loop (PR #5), as for PRs #3/#4.

## 10. Compliance mapping (AGENTS non-negotiables)

- r3/r16: async ≠ RPO=0; effective mode reported from the resource; quorum
  is not configured or claimed in P3 (no witness).
- r4/r8: single-writer via recorded attachment + kernel-enforced
  single-primary role; the journal pipeline is inherited unchanged.
- r7: read-only discovery; foreign resources/LVs never adopted or removed;
  `drbdadm -c` scopes every mutation to volvisor's own resource file.
- r13: no new replication engine; only unmodified drbd-utils/LVM CLIs.
- r12: `evidence_status: PrototypeOnly`; no production, RPO or durability
  claims; health and remote protection are observed facts only.
- r17: single-primary only; demotion never forced; dual-primary has no code
  path.
