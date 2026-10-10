# The `grant_set` wedge: diagnosis

**Status**: diagnosed 2026-10-10 (P6-A part 3). The defect of record
lives in the P5 campaign's findings (the `grant_set` wedge) and the
readiness plan §1; this document is the precise mechanics — where the
failure journals, why it replays forever, and what a re-issue must
reconcile with. The fix and its safety proofs are PR #23's body; the
campaign rows that reused the park as an injection window are
re-derived there.

## The defect in one paragraph

A witness kill inside the grant commit parks the migration
**permanently** at `destination_authorized` (observed with a stall
detail): the destination's peer-grant operation journaled its failure,
and the ops pipeline replays recorded outcomes — success or failure —
byte-for-byte forever. The park is safe (fail-closed: no dual writer,
the source stays fenced, the destination never promotes) but recovery
requires operator action: the post-revoke record has no rollback
(abort refuses cut-or-later, G1/D1a) and no consumer recourse (the
derived operation id is deterministic per migration, so every re-drive
of the promote step hits the same recorded failure).

## The mechanics, step by step

1. **Where the failure journals.** The source's drive reaches
   `drive_authorize`: the target is not granted, so it calls the
   destination's `POST /v2/internal/peer/grant` (the `grant_set`
   step). The route runs through the ops pipeline
   (`execute_resolvable`): the intent is journaled (the derived id
   `mig-api-peer-grant-…`), then `grant_act` executes — the witness
   batch grant under its deterministic operation id, then the
   per-participant promotes with their device paths recorded durably
   in the preparation. A witness kill inside the batch commit kills
   B's witness connection mid-call: `grant_act` returns the transport
   error, and the pipeline's failure tail (`finish_outcome` →
   `append_outcome(success=false, error_body)`) durably records that
   failure under the same derived id. B survives; the failure is on
   record.

2. **Why it replays forever.** The replay rule
   (`execute_resolvable` step 2): a recorded outcome for the same
   operation id and request hash is replayed byte-for-byte *without
   executing* — with no distinction between success and failure. The
   idempotency-registry semantics are deliberate (a failed act is
   never blindly re-executed), but for an act whose failure was
   *transient* — the witness came back — the recorded answer is a
   fact about the past attempt, not about the world. The source's
   retry task re-drives every 5 s tick: the fold lands the record at
   `destination_authorized` (the witness's own journal replay
   re-derived the grant from its durable intent — W3), the drive's
   promote step re-calls B's grant route for the promoted device
   paths, and the route re-serves the recorded 500. Forever. The
   retry is a bounded spin (never a silent resume) — the honest
   reading is a permanent park.

3. **What actually landed (the partially-landed state a re-issue
   must reconcile with).** The world is *almost* converged at the
   moment of the park:
   - **Witness**: the grant **did** land — the witness's journal
     replay re-derives it from the durable intent, so epoch 2 is live
     and held by the destination. (The two armed kill windows both
     leave the intent durable; the row-7 cells prove the replay
     lands the grant exactly once.)
   - **Source**: fully fenced — the cut is durable (the VM destroyed,
     every participant demoted, the leases revoked); the fold proves
     it into the record. Nothing on the source side re-acquires
     authority through any peer route.
   - **Destination (B)**: the *missing* pieces are exactly the parts
     the failed act never reached: the per-participant promotes
     (idempotent per migration — a completed promote replays its
     recorded attachment response) and the promoted device paths in
     the durable preparation (the restore's disk-mapping verification
     needs them). A re-issued grant act must therefore reconcile
     with: a live lease it did not itself mint (the witness batch
     under the same deterministic operation id replays the recorded
     outcome — it cannot mint a second epoch), promotes that may have
     partially landed (idempotent, same attachment identity), and a
     preparation whose device paths may be absent (not provable
     landed → re-execute; recorded → serve).

## Why the remedy is "re-resolvable peer acts"

The readiness plan's remedy direction names two options: re-resolvable
peer acts, or a promote path that does not route through the failed
grant op. The first is the right one: the grant route *already* has
the machinery — `grant_inspect`, the total landed-ness inspection
that resolves the **in-flight** case (an intent without an outcome) by
proving the act landed or did not from durable world state (the
witness views plus the preparation's device paths). The recorded
*failure* case is the same question with one more fact on record: an
attempt that failed. Re-issuing the act through the same inspection
is safe exactly because the act's safety shape holds (the proofs are
PR #23's body): a deterministic derived id, a total inspection, an
idempotent re-execution at every layer (the witness batch under its
op id, the promote per migration, the attach identity deterministic),
and world-derived refusals that reproduce identically on
re-execution — no operator judgment is bypassed.

A promote path that bypasses the grant op would *duplicate* the
inspection's job (proving the grant landed before promoting) in a
second code path, and would leave the recorded failure sitting in the
journal as a landmine for the next same-id caller. Re-resolvability
fixes the class, not the symptom.

## The convergence horizon (the live-lease assumption)

The re-issue's convergence relies on one world fact: the witness-side
grant remaining live. The minted grant-hold leases carry the 60 s TTL
(`DEFAULT_LEASE_TTL_SECS`), and an unpromoted grant is never renewed —
the destination's renewal pass (`renew_leases`) iterates only its own
volumes (the ones with authority blocks), and a grant-hold lease for a
volume the destination never promoted has none. A park that outlives
the TTL therefore has no re-mint path: the deterministic operation id
replays the expired grant forever (the witness journal re-serves the
recorded grant outcome), and `promote_target` refuses over the
non-live lease — the spin stays honest and fail-closed, but the
migration no longer self-resolves. The remedy directions — renewing
unpromoted grant-hold leases while a park is being retried, or minting
a fresh-grant epoch when the recorded grant's lease is provably dead —
are a recorded follow-up (readiness plan §1), not shipped in this fix.

## Review notes (PR #23's review)

Three properties the review asked to have named in this record:

- **Response divergence across callers of one operation id.** The same
  peer-act operation id can serve `INTERNAL` to its first caller and
  `200` to a later retry (the supersede path journals and serves the
  proven outcome); safe today because the only consumer — the source
  daemon's drive — treats peer-act failure as retryable, but a future
  consumer treating peer-act failure as terminal would diverge from
  the later state.
- **The restore act is not internally serialized at the VMM layer**,
  unlike the witness layer's single-guard serialization (the batch
  replays under one operation id). Concurrent restore re-issues
  converge to exactly one fully-restored VM by construction: the
  destroy arm targets only the non-serving `Created`/`Paused` shapes
  (this migration's own half-restore, bounded by the prepare act's
  emptiness proof — the socket was proven empty before the source's
  cut), never a serving VM.
- **The discard advisory field diverges on re-issue**: an
  already-landed discard re-issued serves `discarded:false` (the
  inspection's proven answer — the preparation is absent) where the
  original call served `discarded:true` (it removed a present
  preparation). Idempotent semantics — the preparation is gone either
  way — and metadata-only: the divergence is in the advisory field,
  not in any state the caller acts on.
