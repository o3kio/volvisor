# P4 implementation plan — witness, fencing, writer authority (stage A)

Date: 2026-10-09
Status: implementing (rev 2 — folds in the design review: barrier-evidence
gating for `SAFE_CURRENT`, lease-deadline skew accounting, the
authority-check/proof split, journal roll-forward, peer-side adoption
verification, verified `suspend-io` argv, the `--force` promotion gate)
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
  lease/holder state, `FencingProof`, `AuthorityView`,
  `PromotionClassification` — covering contract §1 durable-state fields
  `writer_epoch`, `authoritative_writer`, `lease_proof`,
  `authority_commit_index`. **All four are persisted** in the volume's
  durable authority block (§4), not merely represented as types.
- A new workspace member `volvisor-witness`: the third-party authority
  service (contract §2: "A third witness may hold authority metadata without
  holding tenant blocks"). Durable, journal-backed epoch/lease registry with
  grant/renew/revoke/register/inspect semantics; an HTTP/JSON server on the
  existing daemon conventions (axum, bearer token, fail-closed auth,
  loopback-only without token); a client used by the storage daemon; a
  `volvisor-witnessd` binary (second `[[bin]]` of the `volvisord` crate,
  with its **own `WitnessConfig` type** — a witness has no volume provider,
  so it must not inherit the provider config shape; only the TOML/validation
  conventions are shared).
- `volvisor-drbd` integration: lease acquisition gates promotion on attach;
  detach releases; per-volume epoch/lease/proof/commit-index persisted in
  state; a `renew_leases` entry point; reconcile and **startup** fail-closed
  self-fencing; adoption + classification + promotion for unplanned
  failover on the surviving host.
- `volvisord`: witness configuration (endpoint, token, lease TTL, renewal
  interval, fence grace, fence quarantine), a background lease-renewal
  task, and admin endpoints for authority observation and adopt/promote.
- `volvisor-api`: the nearline inspect response gains an `authority`
  section (observed epoch/lease state — contract §1 observability); an
  admin adopt/promote endpoint. Volume API v2 contract text updated to
  match exactly what is implemented.
- A `witness register` admin operation: record an existing P3-era volume
  into the witness. The registration attestation captures, in one record,
  the volume's **data lineage** (the DRBD data-generation UUID set, read
  via `drbdsetup show-gi` — argv and output shape verified against the
  real sources before parsing, like every other DRBD form) and **both
  endpoints' backing identities** (each side's LV identity and the
  resource definition facts; the peer's LV is **not** volvisor-created
  and carries no `volvisor.owner` tag — see §5). The witness then
  linearizes all **future** authority for the volume; no historical claims
  are made. This is the migration path for volumes created before P4 —
  without registration they cannot be adopted on a peer (fail-closed typed
  refusal, not a silent gap).

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
- `AuthorityView` (what inspect and the promotion authority check surface):
  current epoch, current holder (`host_id`), lease state (`live` /
  `expired` / `revoked` / `none`), and the witness commit index that last
  changed it.
- `FencingProof`: the witness's **durable statement** that epoch `e` of
  volume `v` was retired at commit index `c`. It is produced by the grant
  of a strictly newer epoch (the grant record retires all older epochs) or
  by an explicit recorded revocation; it is *evidence returned after the
  fact*, never a precondition the caller supplies. Verifiable by
  re-querying the witness; never trusted from the fenced host.
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
  durability properties the witness needs — no new log format. Two
  additional ordering invariants make the reuse sound (rule 8's
  journal-before-ack, applied to responses):
  - **W3a (outcome before response)**: the witness answers a grant/renew/
    revoke only after the operation's **outcome** record is fsynced — not
    merely the intent. A client never learns a lease state the witness
    could forget.
  - **W3b (roll-forward on restart)**: a replayed intent without an
    outcome (crash between append and response) is completed at startup by
    reconstructing the response from the intent and journaled state, then
    appending the outcome. The intent payload carries the **complete
    computed response** (epoch, lease id, deadline duration, proof), so
    the reconstructed outcome is deterministic and byte-identical to what
    the normal path returns — a retry and a roll-forward can never
    disagree. A retried `operation_id` therefore never wedges in
    `in-flight`, and a retry with a fresh `operation_id` is never blocked
    by an orphan lease.
- **W4 (stale renewal rejected)**: renewing a retired epoch is a typed
  `STALE_EPOCH` refusal carrying the current epoch, so a stale writer
  *learns* it is fenced instead of guessing.
- **W5 (bounded-skew deadlines)**: lease expiry is *evaluated* only by the
  witness against its own clock. Writers do not interpret absolute
  timestamps: every renewal/grant response returns the lease deadline as a
  **duration from the response**, and the writer's local deadline is
  `response_received_locally + duration`. The residual skew term is
  therefore bounded by the response latency (network + processing), not by
  free-running clock drift — and that bound is exactly the configurable
  `fence_grace_secs` used in the window analysis below. The documented
  assumption is that response latency stays under that bound (operator
  network responsibility, stated in config docs).
- **W6 (forced revocation is recorded)**: revoking a **live** lease (the
  manual STONITH path against an alive-but-partitioned source) requires an
  explicit authorization record (operator identity + reason) journaled with
  the revocation. It is never silent and never inferred.
- **W7 (grant waits out the fence)**: the witness does not grant a new
  epoch until the previous writer is — under the §2 timing assumption —
  provably suspended. The wait is computed from the revoked lease's
  **recorded end** (which the witness knows exactly: `last grant or renew
  + ttl`), because the writer's own local deadline is exactly that end
  plus the bounded latency term: grants are delayed until `lease end +
  grace + suspend budget`, after expiry and forced revocation alike.
  (Delays keyed to the writer *learning* of a revocation would be wrong:
  W6's own scenario is an alive-but-**partitioned** source that cannot
  reach the witness — it serves until its local deadline no matter how
  often it tries to renew, so only the lease's end bounds it. A
  power-off STONITH attestation may shorten the wait: the forced-revoke
  authorization can record that the source host is provably powered off,
  in which case there is nothing to wait out — recorded, never assumed.)
  A grant requested inside the window is a typed `FENCE_PENDING`
  refusal carrying a retry-after duration — the candidate retries;
  nothing blocks.

### Dual-write window analysis (the honest fence boundary)

Lease + self-fencing bounds — but cannot **prove** — the absence of a
second writer. With duration-from-response deadlines (W5):

- The witness may grant a new epoch at `expiry + quarantine`.
- The fenced writer's last successful renewal was received locally at
  `expiry − ttl + δ` (δ ≤ grace = the response-latency bound), so its
  local deadline is `expiry + δ` and it **suspends I/O** by `expiry +
  grace` (suspend is fast; demotion may be unbounded because the kernel
  refuses demotion of an open device — a suspended-but-primary device is
  already write-frozen, which is the property that matters).

P4a therefore requires **`quarantine ≥ grace + suspend budget`**, making
the dual-write window empty under the documented timing assumption, which
is stated openly — and the wait is enforced **witness-side at grant time**
(W7), never left to the candidate's discipline. Proof-grade elimination
needs data-path epoch enforcement (DRBD-native quorum/fencing — the
recorded follow-up) or STONITH. Two mitigating facts are also documented,
each with its exact scope: DRBD refuses the candidate's unforced promotion
while a connection to a live primary exists — but this gate is worthless
under total partition, which is precisely the case the lease covers (W4 +
self-fence are the protection, not DRBD's gate); and volvisor **never**
runs `primary --force` outside the two justified paths in §5.

## 3. Witness service

- `volvisor-witness`: `core` (registry semantics over `Journal`),
  `proto` (versioned request/response types, `deny_unknown_fields`),
  `server` (axum), `client` (HTTP/JSON, same loopback-friendly conventions
  as `volvisor-api`; hyper-based, no new heavyweight dependency trees).
  Records use the journal's generic `Intent`/`Outcome` payload surface with
  witness-specific `op_kind`s and typed payloads; the idempotency registry
  (same `operation_id` + request hash) plus the W3a/W3b roll-forward
  discipline gives exactly-once grant semantics **as specified in §2** —
  not "for free": the in-flight state is handled explicitly. Concurrent
  requests serialize through a single journal-holding mutex, following the
  `volvisor-api` state convention.
- Operations: `register(volume_id, lineage attestation — both endpoints'
  backing identities and the DRBD data-generation UUID set — optional
  operator-attested barrier)`, `grant(volume_id, host_id, operation_id) →
  {epoch, lease deadline (duration), fencing proof}`, `renew(volume_id,
  epoch, lease_id)`, `revoke(volume_id, epoch, authorization)`,
  `inspect(volume_id) → AuthorityView` where `AuthorityView` carries not
  only the epoch/holder/lease-state/commit-index but also the **full
  registration record** (lineage UUID set, both endpoints' backing
  identities, any recorded barrier) — the adopt flow of §5 reads back
  exactly what it must compare against, through this one operation.
- Deployment: `volvisor-witnessd` on a **third failure domain**, with its
  own config: `listen`, `state_dir`, `auth_token`, `lease_ttl_secs`,
  `lease_grace_secs` (the response-latency bound W7 assumes — the one
  number the correctness argument needs from the deployment; documented
  as an operator network responsibility) and the fixed, documented
  suspend budget used in the W7 wait. The storage daemon refuses a
  witness endpoint whose host equals **either** its own replication
  address **or** the configured peer address — a witness colocated on a
  data node defeats the failure-domain claim from either side. (Name-vs-
  literal resolution is handled conservatively: the guard compares
  against both the configured literal and, where they differ, the
  resolvable address, and refuses on ambiguity.)
- Availability honesty: witness loss blocks **new** grants, renewals past
  deadline (→ self-fencing per policy) and failover — never established
  guest I/O before the lease deadline. This is the contract's conscious
  safety/availability tradeoff, restated in the config docs.

## 4. DRBD provider integration

- **Attach**: after the P3 ownership verification, acquire (or renew) the
  lease **before** `drbdadm primary`; a witness refusal or unreachable
  witness is a typed `INVALID_STATE`/`UNAVAILABLE`-class refusal — a new
  writer is never admitted without authority (contract §2).
- **Persisted authority block** (contract §1): per volume,
  `authority: {epoch, lease_id, lease_proof_ref (the witness commit index
  of the grant), authority_commit_index, acquired_at, deadline_at}` — the
  full recoverable set, saved with the same atomic-write state discipline
  as the rest of the volume record.
- **Detach**: demote (existing rules), then release the lease
  (`revoke` self-initiated, journaled), then save. Crash between demote
  and release leaves an expired lease — harmless (expiry + W1 bound it).
- **Renewal (`renew_leases`)**: for every Attached volume with a live
  lease: renew. `STALE_EPOCH` → self-fence immediately. Unreachable
  witness → keep serving until the **local** deadline computed per W5
  (response-received time + returned duration), then self-fence. The
  writer never guesses expiry from its own clock against an absolute
  witness timestamp.
- **Self-fencing policy** (contract §7 "Witness/quorum lost"):
  `drbdsetup suspend-io <minor>` — the verified argv form is by **minor**
  (or `/dev/drbd<minor>`), which volvisor already tracks; a bare resource
  name is *not* resolvable by `drbdsetup` and is never used (verified
  against the 9.29.0 sources: minor-context commands resolve only
  minor/device-node arguments). Suspension freezes the data path — the
  enforcement point a bypassing guest cannot escape, only root on the
  host can, which is the documented residual. Then demotion is attempted;
  a busy device (kernel refuses demotion while open — the P3 rule) stays
  suspended with the attachment record cleared and an `Unhealthy`/fenced
  event surfaced; reconcile completes the demotion once the device
  closes. Never a silent resume (rule 5).
- **Startup fail-closed**: the provider's startup reconciliation (P3
  already runs one) treats every **witness-managed volume found Primary**
  as **unproven until validated** — keyed on the role and the lease,
  *not* on the attachment record (a crash between `drbdadm primary` and
  the record save, or a zombie promotion, leaves a Primary with no
  attachment; it is still an unvalidated writer). I/O is suspended
  before the API surface starts, the lease is validated via
  `inspect`, and the device is resumed only on a live lease for our
  epoch. An unreachable witness leaves it suspended — a restarted daemon
  never silently resumes a writer it cannot prove (rule 5). This subsumes
  the P3 zombie-primary case safely: the zombie is reported exactly as
  before (never auto-demoted) and additionally suspended when it holds
  no live lease. Epoch-0
  (P3-era, unregistered) volumes keep exactly their P3 behavior.
- **Reconcile**: validates the lease of every witness-managed volume
  found Primary (`inspect`), attachment record or not; a superseded or
  expired lease self-fences as above.

Every new DRBD command form (`suspend-io`/`resume-io` by minor, `show-gi`
for lineage UUIDs) is verified against the real drbd-utils 9.29.0 sources
(`/tmp/opencode/du-9290`) before the fake implements it, and the fake
emits the verified shapes verbatim — the P2/P3 lesson, restated. The
kernel-side gate on unforced promotion against a `DUnknown`/`Outdated`
peer cannot be confirmed from the userspace sources alone; the §5 flow
therefore does not rely on it and carries the verification item.

## 5. Unplanned promotion (ADR-0004 Decision 5, contract §8)

The surviving (peer) host does not hold this volume in its state (P3 peer
model), so failover is an **adopt-and-promote** admin operation:

1. **Adoption verification** (rule 7 — never adopt foreign state). The
   resource name must match the derived `vol-{sanitized-id}-{hash8}`
   scheme, the DRBD role must be Secondary, and the witness must hold a
   registration for the volume whose recorded **lineage UUID set** (from
   `show-gi` at register time) matches what `show-gi` reports now — this
   is the check that closes the recreated-volume hole, since the name
   scheme alone cannot distinguish a same-named volume rebuilt after a
   cluster loss. The backing-LV check has two honest branches:
   - *volvisor-created backing* (the local side of a volume this host
     owned): the LV must carry the matching `volvisor.owner` tag.
   - *operator-provisioned backing* (the P3 peer side): the peer's LV is
     not volvisor-created and carries no tag — here the lineage UUID match
     plus the resource-definition/LV identity captured in the
     registration attestation carries the rule-7 weight. The residual is
     documented: if the witness itself was rebuilt and re-registered, the
     lineage check degrades to operator attestation (single-witness
     residual, stated in §7).
   Mismatch on any branch → typed refusal.
2. **Authority check** (the promotion precondition): `inspect` at the
   witness must show the volume with **no live lease** (the old one
   expired or revoked) — never a proof the caller supplies. A live lease →
   `UNSAFE` refusal (`LEASE_HELD` surfaced). The durable `FencingProof` is
   *created* by step 4's grant (W2), which retires the old epoch.
3. **Classification** from observed facts only:

   | local disk (observed) | protocol | authority check | barrier evidence | classification |
   |---|---|---|---|---|
   | `UpToDate` | C (sync) | no live lease | recorded barrier | `SAFE_CURRENT` |
   | `UpToDate` | C (sync) | no live lease | none | `POSSIBLE_LOSS { Unknown }` |
   | `UpToDate` | A/B | no live lease | — | `POSSIBLE_LOSS { Unknown }` |
   | `Consistent`/`Outdated` | any | no live lease | — | `POSSIBLE_LOSS { Unknown }` |
   | `Inconsistent`/`Diskless`/`DUnknown`/`Failed`/other | any | any | any | `UNSAFE` |
   | any | any | live lease | any | `UNSAFE` (`LEASE_HELD`) |

   The `SAFE_CURRENT` row is deliberately **evidence-gated, not
   protocol-gated**: "Protocol C + locally UpToDate" does **not** prove the
   acknowledged tail, because after a connection drop the source keeps
   ACKing writes locally (degraded mode) while the survivor's disk stays
   `UpToDate` — the tail is unknowable from the survivor's DRBD state
   under *any* protocol. `SAFE_CURRENT` therefore requires a **recorded
   barrier** for the volume: an attestation that the named boundary was
   the **last acknowledged boundary** — source-committed, with
   connection-established evidence at it, **and no writes acknowledged
   past it** (a barrier recorded mid-serving would prove nothing about
   the tail written after it; a volume keeps serving after registration).
   In P4a no automated component writes barriers (P4b's `BARRIER_DURABLE`
   will); the only source is an operator-attested barrier recorded at
   `register` time, and its attestation must explicitly cover the
   last-acknowledged property. Absent that evidence the honest verdict is
   `POSSIBLE_LOSS` with an *unknown* boundary — the contract explicitly
   requires reporting unknown rather than pretending precise loss bounds.
   An `Inconsistent` local disk means integrity is unprovable
   (mid-resync loss) — `UNSAFE`, never a "partial" promotion.
4. **Promotion**: takes a **new** epoch from the witness (`grant` — W2
   durably retires the old one; W7 has already waited out the fence
   window; the grant record returned is the `FencingProof`), then
   `drbdadm primary --force`, then verifies the role and device.
   `--force` is used here because the kernel's unforced promotion gate is
   `ASSUMPTION(unverified)`-expected to refuse promotion against a
   `DUnknown`/`Outdated` peer (an unplanned failover is exactly that
   case; the gate cannot be confirmed from the userspace sources — see
   the real-cluster verification item in §8); this is the second and last
   justified `--force` path in the provider (P3's is seeding a
   provably-fresh resource) and it is gated on the authority check above,
   not on the gate behaving as expected. `POSSIBLE_LOSS`
   additionally requires an explicit `allow_loss` authorization in the
   request (recorded with the exposure evidence — contract §8's recorded
   authorization). `UNSAFE` never promotes.
5. The old source reconnecting later is rejected by construction: its
   epoch is retired (W4), its lease unrenewable, its self-fence already
   taken effect. Never dual-primary, never timestamp arbitration (rule 17,
   contract §8). DRBD's own no-dual-primary-while-connected gate is a
   bonus outside the total-partition case, never a relied-upon fence.

## 6. Daemon and API surface

- `volvisord` config: `witness_url`, `witness_token`,
   `renewal_interval_secs`. The lease TTL, grace and the W7 wait live on
   the **witness** (§3) — they are enforced there, so they are configured
   there; the writer daemon validates `renewal_interval < ttl / 2`
   against the TTL the witness reports in its responses (fail-closed
   startup refusal on violation), rather than duplicating the value.
   Token is required for non-loopback witness URLs, plus the §3
   two-sided failure-domain guard. A background tokio task runs
   `renew_leases` every renewal interval; failures surface as events,
   not crashes.
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
  real sources before the fake implements it; unprovable semantics (the
  kernel promotion gate) get `ASSUMPTION(unverified)` markers and the flow
  does not rely on them. `SAFE_CURRENT` is never claimed without recorded
  barrier evidence.
- The witness is a third failure domain by deployment, enforced as far as
  configuration can see (§3 two-sided guard); volvisor cannot verify
  physical placement — documented. A witness rebuild degrades lineage
  checks to operator attestation — also documented.

## 8. Test matrix

- **Witness unit**: W1–W7 as individual tests; crash-restart replay
  (epoch/commit-index monotonicity, torn-tail truncation); W3b
  roll-forward (kill between intent and outcome → retry with same
  `operation_id` completes, retry with fresh id is not blocked);
  idempotent grant replay; forced-revoke authorization recording; W7
  window enforcement keyed on the **lease's recorded end** (`FENCE_PENDING`
  with retry-after both after expiry and after forced revocation of a
  partitioned writer's live lease — the exact case a
  renewal-interval-based wait would get wrong); a power-off STONITH
  attestation shortening the wait;
  duration-from-response deadline shape; `inspect` returning the full
  registration record (lineage UUIDs, both endpoints' backing identities,
  barrier) for the adopt flow to compare against.
- **Witness server (real loopback HTTP)**: auth fail-closed (no token →
  401; loopback-only without token); request/response round-trips against
  the proto types; journal-backed restart mid-traffic with W3a/W3b
  ordering asserted (a response is never observable before its outcome is
  durable).
- **DRBD behavior (FakeDrbd world + real loopback witness)**: attach
  acquires before promote (ordering asserted); witness refusal → no
  promotion; renewal keeps lease; witness unreachable → serving until the
  W5 local deadline then self-fence (suspended by minor, attachment
  cleared, demote completed on close); `STALE_EPOCH` → immediate
  self-fence; detach releases; reconcile validates; **startup
  fail-closed** (Primary witness-managed volume + unreachable witness →
  stays suspended; a Primary with **no attachment record** — the
  crash-between-promote-and-save and zombie cases — is suspended the
  same way, keyed on role + lease rather than the record); epoch-0
  volumes behave exactly as P3.
- **Promotion**: every row of the §5 table, including the two `UpToDate`/
  Protocol-C rows (with and without recorded barrier evidence); adoption
  verification refusals (foreign LV, missing tag on a volvisor-created
  backing, wrong name, Primary role, unregistered volume, lineage UUID
  mismatch — the recreated-volume hole); `POSSIBLE_LOSS` without
  `allow_loss` → refusal, with → promoted via `--force` and exposure
  recorded; `UNSAFE` never promotes; W7 `FENCE_PENDING` surfaced to the
  adopt caller and cleared on retry; registered
  P3-era volume adopts; old source's stale renewal rejected end-to-end.
- **Conformance**: the shared kit stays green (authority is additive).
- **Real-cluster**: env-gated (`VOLVISOR_TEST_DRBD=1`) attach/renew/fence
  over the real peer where the environment provides it; the kernel
  `--force` gate verification item runs here.

## 9. P4b preview (recorded, not implemented here)

Engine-neutral `ReplicationProvider` trait (ADR-0007's Create/Inspect/
Seed/Attach/Pause/Promote/Demote/ConfigurePolicy/TrackSync/BeginHandoff/
Recover/Delete surface), the canonical handoff state machine with durable
per-migration state and `IN_DOUBT` recovery, Cloud Hypervisor coordination
(real-CLI-honest `ch-remote` verification), the Volume API v2 mobility
operations, VM-wide eligibility across all attached writable volumes, and
the automated source-committed barrier records that unlock `SAFE_CURRENT`
without operator attestation. Nothing in P4a may pre-claim or block these
(rule 6).
