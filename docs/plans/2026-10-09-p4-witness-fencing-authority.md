# P4 implementation plan — witness, fencing, writer authority (stage A)

Date: 2026-10-09
Status: implementing
Branch: `feat/p4a-witness-authority-fencing`
Normative: [ADR-0004](../adr/0004-nearline-replication-and-mobility.md) Decisions 1/2/4/5,
[nearline contract v2](../../contracts/nearline-replication-v2.md) §1, §2, §7, §8,
[ADR-0007](../adr/0007-drbd9-nearline-replication-provider.md) (engine boundary),
[Volume API v2](../../contracts/volume-api-v2.md), AGENTS rules 1–21.

This is **stage A** of the implementation-order item "witness/fencing and full
VMM/storage handoff". The phase deliberately splits into two PRs because its
two halves have different review surfaces:

- **P4a (this plan)**: the writer-authority layer — epochs, leases, a witness
  service, fail-closed self-fencing in the DRBD provider, and unplanned
  promotion with honest classification (`SAFE_CURRENT` / `POSSIBLE_LOSS` /
  `UNSAFE`).
- **P4b (recorded here, planned separately)**: the migration half — the
  engine-neutral `ReplicationProvider` boundary with its `BeginHandoff`
  surface, the canonical handoff state machine (`PREPARED → … → COMPLETE`
  with `IN_DOUBT`), VMM coordination (Cloud Hypervisor pause/drain/resume
  proofs) and the Volume API v2 mobility operations. The trait split arrives
  in P4b, where a second consumer (the handoff coordinator) actually exists;
  introducing it earlier would be speculative generality. The **authority
  protocol defined here is the engine-neutral boundary of P4a** (ADR-0007's
  "retain a versioned, provider-neutral boundary" is satisfied by the
  witness protocol types, not by a premature Rust trait).

ADR-0007 remains **Proposed, not accepted**: everything here ships
`evidence_status: PrototypeOnly`, with no production, RPO or availability
claims. AGENTS rule 4 (never weaken fencing to make a test pass) and rule 5
(no blind source resume after revocation) govern every decision below.

## 1. Scope

In (P4a):

- Authority domain types in `volvisor-types`: `WriterEpoch`, `LeaseId`,
  lease/holder state, `FencingProof`, `PromotionClassification` (contract §1
  durable-state fields `writer_epoch`, `authoritative_writer`, `lease_proof`,
  `authority_commit_index`).
- A new workspace member `volvisor-witness`: the third-party authority
  service (contract §2: "A third witness may hold authority metadata without
  holding tenant blocks"). Durable, journal-backed epoch/lease registry with
  grant/renew/revoke/register/inspect semantics; an HTTP/JSON server on the
  existing daemon conventions (axum, bearer token, fail-closed auth,
  loopback-only without token); a client used by the storage daemon; a
  `volvisor-witnessd` binary (second `[[bin]]` of the `volvisord` crate,
  sharing its config-loading conventions).
- `volvisor-drbd` integration: lease acquisition gates promotion on attach;
  detach releases; per-volume epoch/lease persisted in state; a
  `renew_leases` entry point; reconcile self-fencing with the fail-closed
  lease policy; adoption + classification + promotion for unplanned
  failover on the surviving host.
- `volvisord`: witness configuration (endpoint, token, lease TTL, renewal
  interval, fence quarantine), a background lease-renewal task, and admin
  endpoints for authority observation and adopt/promote.
- `volvisor-api`: the nearline inspect response gains an `authority`
  section (observed epoch/lease state — contract §1 observability); an
  admin adopt/promote endpoint. Volume API v2 contract text updated to
  match exactly what is implemented.
- A `witness register` admin operation: record an existing P3-era volume
  into the witness (operator-attested adoption of current DRBD facts). The
  witness then linearizes all **future** authority for it; no historical
  claims are made. This is the migration path for volumes created before
  P4 — without registration they cannot be adopted on a peer (fail-closed
  typed refusal, not a silent gap).

Out (recorded follow-ups; each maps to P4b or later):

- Planned migration / `BeginHandoff` / dual-primary — prohibited by default
  (contract §9A; AGENTS rule 17); no code path may enable it. The
  `SOURCE_REVOKED → DESTINATION_AUTHORIZED` transaction, `IN_DOUBT`
  semantics, `ObserveHandoff`, VMM pause/drain proofs and Cloud Hypervisor
  coordination are P4b.
- The `ReplicationProvider` trait split (P4b, see staging note above).
- DRBD-native quorum/fencing wiring in the generated `.res` (`quorum`,
  `on-no-quorum=io-error`, `fencing resource-and-stonith`, a `fence-peer`
  handler): requires peer-side `.res` coordination that P3's
  operator-provisioned peer model does not give us. The HTTP witness is the
  volvisor-level arbiter exactly as contract §2 permits; DRBD-native
  enforcement is the recorded hard-fencing follow-up (see §7).
- Witness clustering / multi-witness quorum: P4a ships a single witness per
  authority domain. Its loss degrades to the documented fail-closed lease
  policy (contract §7 row "Witness/quorum lost"), never to weaker fencing.
- Live-attachment I/O statistics, lease history API, audit export tooling.

## 2. Authority model (contract §2)

### Types (`volvisor-types`)

- `WriterEpoch(u64)`: strictly increasing per volume lineage, never reused,
  never shrunk — "including after crash, promotion, cancellation and
  failback" (contract §2). Epoch 0 is pre-authority (P3-era volumes).
- `LeaseId(u64)`: opaque, unique per grant.
- `AuthorityView` (what inspect surfaces): current epoch, current holder
  (`host_id`), lease state (`live` / `expired` / `revoked` / `none`), the
  witness commit index that last changed it.
- `FencingProof`: the witness's durable statement that epoch `e` of volume
  `v` was retired at commit index `c` (either by an explicit revoke or by
  the grant of a strictly newer epoch, which durably retires all older
  ones). Verifiable by re-querying the witness; carried in responses, never
  trusted from the fenced host.
- `PromotionClassification`: `SafeCurrent`, `PossibleLoss { boundary:
  Known(seq) | Unknown, authorization: Option<…> }`, `Unsafe { reasons }`.

### Witness invariants (the correctness core; every one is unit-tested)

- **W1 (single live lease)**: at most one live (unexpired, unrevoked) lease
  per volume at any time. A competing grant against a live lease is a
  typed `LEASE_HELD` refusal.
- **W2 (grant retires the past)**: granting epoch `e+1` durably records the
  retirement of all epochs `≤ e` **before** the response is returned. The
  grant record is itself the `FencingProof` (fencing is irrevocable because
  the witness will never accept a renewal for a retired epoch).
- **W3 (monotonic across crash)**: witness state is derived by replaying
  its journal (`volvisor-journal`); epochs and commit indices never shrink
  after a restart. The journal's fsync-before-ack, CRC-checked frames,
  torn-tail truncation and flock single-writer enforcement are exactly the
  durability properties the witness needs — no new log format.
- **W4 (stale renewal rejected)**: renewing a retired epoch is a typed
  `STALE_EPOCH` refusal carrying the current epoch, so a stale writer
  *learns* it is fenced instead of guessing.
- **W5 (expiry is witness-local truth)**: lease expiry is evaluated only by
  the witness against its own clock at decision points (grant, renew,
  inspect). Writers never interpret expiry themselves — their policy is
  purely "cannot renew → fence after grace". This removes cross-host clock
  skew from the correctness argument; the documented residual assumption is
  witness-clock sanity (operator NTP responsibility, stated in config docs).
- **W6 (forced revocation is recorded)**: revoking a **live** lease (the
  manual STONITH path against an alive-but-partitioned source) requires an
  explicit authorization record (operator identity + reason) journaled with
  the revocation. It is never silent and never inferred.

### Dual-write window analysis (the honest fence boundary)

Lease + self-fencing bounds — but cannot **prove** — the absence of a
second writer: the fenced host self-fences at (expiry + grace), while the
witness may grant the new epoch at (expiry + quarantine). P4a therefore
requires **quarantine ≥ grace + worst-case demote latency**, making the
dual-write window empty under the documented timing assumption, and states
the assumption openly. Proof-grade elimination needs data-path epoch
enforcement (DRBD-native quorum/fencing — the recorded follow-up) or
STONITH. Two mitigating facts are also documented: DRBD itself refuses the
candidate's promotion while a connection to a live primary exists
(dual-primary is impossible without `--force`), and volvisor **never** runs
`primary --force` on an unfenced peer (the classifier refuses — §5).

## 3. Witness service

- `volvisor-witness`: `core` (registry semantics over `Journal`),
  `proto` (versioned request/response types, `deny_unknown_fields`),
  `server` (axum), `client` (HTTP/JSON, same loopback-friendly conventions
  as `volvisor-api`; hyper-based, no new heavyweight dependency trees).
  Records use the journal's generic `Intent`/`Outcome` payload surface with
  witness-specific `op_kind`s and typed payloads — the idempotency registry
  (same `operation_id` + request hash) gives replay-safe grants for free.
- Operations: `register(volume_id, lineage attestation)`,
  `grant(volume_id, host_id, operation_id) → {epoch, lease, fencing proof}`,
  `renew(volume_id, epoch, lease_id)`, `revoke(volume_id, epoch,
  authorization)`, `inspect(volume_id) → AuthorityView`.
- Deployment: `volvisor-witnessd` on a **third failure domain** (any host
  that is neither data node; the storage daemon refuses a witness endpoint
  whose host equals its own replication address — a config-time
  same-failure-domain guard).
- Availability honesty: witness loss blocks **new** grants, renewals past
  deadline (→ self-fencing per policy) and failover — never established
  guest I/O before the lease deadline. This is the contract's conscious
  safety/availability tradeoff, restated in the config docs.

## 4. DRBD provider integration

- **Attach**: after the P3 ownership verification, acquire (or renew) the
  lease **before** `drbdadm primary`; a witness refusal or unreachable
  witness is a typed `INVALID_STATE`/`UNAVAILABLE`-class refusal — a new
  writer is never admitted without authority (contract §2). The granted
  epoch and lease id are persisted in the volume's runtime state
  (`authority: {epoch, lease_id, acquired_at, expires_at}`).
- **Detach**: demote (existing rules), then release the lease
  (`revoke` self-initiated, journaled), then save. Crash between demote
  and release leaves an expired lease — harmless (expiry + W1 bound it).
- **Renewal (`renew_leases`)**: for every Attached volume with a live
  lease: renew. `STALE_EPOCH` → self-fence immediately. Unreachable
  witness → keep serving until the last-known deadline, then self-fence
  (W5: the deadline came from the witness, not local clock interpretation).
- **Self-fencing policy** (contract §7 "Witness/quorum lost"): `drbdsetup
  suspend-io` (freezes the data path — the enforcement point a bypassing
  guest cannot escape, only root on the host can, which is the documented
  residual), then attempt demotion; a busy device (kernel refuses demotion
  while open — the P3 rule) stays suspended with the attachment record
  cleared and an `Unhealthy`/fenced event surfaced; reconcile completes
  the demotion once the device closes. Never a silent resume (rule 5).
- **Reconcile**: validates each Attached volume's lease against the
  witness (`inspect`); a superseded epoch self-fences as above. Volumes
  created without a witness (P3-era, epoch 0) reconcile exactly as in P3 —
  the authority layer never weakens existing behavior, it only adds
  enforcement where a witness is configured.

Every new DRBD command form (expected: `drbdsetup suspend-io <res>` /
`resume-io <res>`) is verified against the real drbd-utils 9.29.0 sources
(`/tmp/opencode/du-9290`) before the fake implements it, and the fake emits
the verified shapes verbatim — the P2/P3 lesson, restated.

## 5. Unplanned promotion (ADR-0004 Decision 5, contract §8)

The surviving (peer) host does not hold this volume in its state (P3 peer
model), so failover is an **adopt-and-promote** admin operation:

1. **Adoption verification**: the resource name must match the derived
   `vol-{sanitized-id}-{hash8}` scheme, the local LV must carry the
   matching `volvisor.owner` tag (rule 7 — never adopt foreign state), the
   DRBD role must be Secondary, and the witness must have the volume
   registered with a compatible lineage. Mismatch → typed refusal.
2. **Fencing proof**: fetched from the witness (never from the candidate's
   or the dead host's claims). No proof → `UNSAFE`, promotion refused.
3. **Classification** from observed facts only (the honest table; the
   `SAFE_CURRENT` row's provability hypothesis is verified against the
   drbd sources during implementation and downgraded with an
   `ASSUMPTION(unverified)` marker if the sources cannot confirm it):

   | local disk (observed) | protocol | fencing proof | classification |
   |---|---|---|---|
   | `UpToDate` | C (sync) | proven | `SAFE_CURRENT` * |
   | `UpToDate` | A/B | proven | `POSSIBLE_LOSS { boundary: Unknown }` |
   | `Consistent`/`Outdated` | any | proven | `POSSIBLE_LOSS { boundary: Unknown }` |
   | `Inconsistent`/`Diskless`/`DUnknown`/`Failed`/other | any | any | `UNSAFE` |
   | any | any | unproven | `UNSAFE` |

   \* Hypothesis to verify: under Protocol C every source-ACKed write has
   already reached the peer (ACK waits for peer-durable confirmation), so a
   locally `UpToDate` disk at connection loss holds the full acknowledged
   tail. Under Protocol A/B the tail is *unknowable* — the contract
   explicitly requires reporting unknown rather than pretending precise
   loss bounds. An `Inconsistent` local disk means integrity is unprovable
   (mid-resync loss) — `UNSAFE`, never a "partial" promotion.
4. **Promotion**: `POSSIBLE_LOSS` additionally requires an explicit
   `allow_loss` authorization in the request (recorded with the exposure
   evidence — contract §8's recorded authorization); `SAFE_CURRENT`
   promotes after the §2 quarantine window; `UNSAFE` never promotes. The
   promotion itself takes a **new** epoch from the witness (grant — which
   durably retires the old one), then `drbdadm primary`, then verify.
5. The old source reconnecting later is rejected by construction: its
   epoch is retired (W4), its lease unrenewable, and DRBD refuses a
   secondary→primary transition on a connection whose peer is primary.
   Never dual-primary, never timestamp arbitration (rule 17, contract §8).

## 6. Daemon and API surface

- `volvisord` config: `witness_url`, `witness_token`, `lease_ttl_secs`,
   `renewal_interval_secs`, `fence_quarantine_secs` (validated: quarantine
   ≥ grace + demote budget; token required for non-loopback witness URLs;
   same-failure-domain guard per §3). A background tokio task runs
   `renew_leases` every renewal interval; failures surface as events, not
   crashes.
- `volvisor-api`: `GET /v2/volumes/{id}` (nearline) reports the
   `authority` section; `POST /v2/admin/nearline/{id}/adopt` runs §5 with
   body `{allow_loss: bool}` and responds with the classification and
   resulting volume state (admin token required, like every privileged
   route). Contract text in `volume-api-v2.md` updated to match exactly
   this surface — nothing more is claimed.

## 7. Honesty and verification rules (rules 4, 12)

- `evidence_status: PrototypeOnly` everywhere; no RPO, availability or
  production claims. The dual-write-window analysis (§2) is stated as a
  bounded-not-proven property with its timing assumption documented.
- Operator/root bypass (running `drbdadm` by hand) remains out of scope —
  the same boundary P3 documented; DRBD-native quorum/fencing is the
  recorded hard-fencing follow-up, not a P4a claim.
- Every DRBD command form and every status-shape claim is verified against
  real sources before the fake implements it; unprovable semantics get
  `ASSUMPTION(unverified)` markers and conservative behavior.
- The witness is a third failure domain by deployment, enforced as far as
  configuration can see (§3 guard); volvisor cannot verify physical
  placement — documented.

## 8. Test matrix

- **Witness unit**: W1–W6 as individual tests; crash-restart replay
  (epoch/commit-index monotonicity, torn-tail truncation); idempotent
  grant replay via `operation_id`; forced-revoke authorization recording.
- **Witness server (real loopback HTTP)**: auth fail-closed (no token →
  401; loopback-only without token); request/response round-trips against
  the proto types; journal-backed restart mid-traffic.
- **DRBD behavior (FakeDrbd world + real loopback witness)**: attach
  acquires before promote (ordering asserted); witness refusal → no
  promotion; renewal keeps lease; witness unreachable → serving until
  deadline then self-fence (suspended, attachment cleared, demote
  completed on close); `STALE_EPOCH` → immediate self-fence; detach
  releases; reconcile validates; epoch-0 volumes behave exactly as P3.
- **Promotion**: every row of the §5 table; adoption verification refusals
  (foreign LV, missing tag, wrong name, Primary role, unregistered
  lineage); `POSSIBLE_LOSS` without `allow_loss` → refusal, with → promoted
  and exposure recorded; `UNSAFE` never promotes; registered P3-era volume
  adopts; old source's stale renewal rejected end-to-end.
- **Conformance**: the shared kit stays green (authority is additive).
- **Real-cluster**: env-gated (`VOLVISOR_TEST_DRBD=1`) attach/renew/fence
  over the real peer where the environment provides it.

## 9. P4b preview (recorded, not implemented here)

Engine-neutral `ReplicationProvider` trait (ADR-0007's Create/Inspect/
Seed/Attach/Pause/Promote/Demote/ConfigurePolicy/TrackSync/BeginHandoff/
Recover/Delete surface), the canonical handoff state machine with durable
per-migration state and `IN_DOUBT` recovery, Cloud Hypervisor coordination
(real-CLI-honest `ch-remote` verification), the Volume API v2 mobility
operations, and VM-wide eligibility across all attached writable volumes.
Nothing in P4a may pre-claim or block these (rule 6).
