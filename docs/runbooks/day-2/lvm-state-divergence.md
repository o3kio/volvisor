# LVM state divergence and persist-failure recovery — day-2 runbook

**Status: implemented and deterministically test-covered for
[#70](https://github.com/o3kio/volvisor/issues/70); NOT drilled against real
LVM or hardware.** This page documents the crash/persist contracts for the
native-local LVM provider and the operator procedure when a mutation's durable
state and the observed LVM world may disagree. The deterministic tests use the
in-tree LVM simulation plus injected store-save failures; there is no kill-9
rig, real-disk fault injection or multi-host run behind this page, and none of
it changes the pre-production verdict.

This page is the companion to [enospc.md](enospc.md): ENOSPC is one cause of a
failed state save, but the contracts below hold for any save error (I/O error,
filesystem failure, injected fault) and for a process kill between a world
mutation and its persist.

## Scope and non-goals

Covered:

- `create`, `grow`, `delete` and online `move` on `native-local` LVM;
- a state save that fails **before** its rename (old durable file survives) or
  **after** its rename but before the directory fsync (new durable file
  survives — the real post-rename window);
- a kill-9 between `lvcreate`/`lvextend`/`blkdiscard`/`lvremove`/`pvmove` and
  the persist that records it;
- startup reconciliation of the resulting durable intents.

Not covered:

- a genuinely corrupt state file (see
  [journal-and-state-corruption.md](journal-and-state-corruption.md));
- filesystem ENOSPC specifics (see [enospc.md](enospc.md));
- foreign or operator-created LVs — recovery never deletes them (AGENTS
  rule 7);
- real Ceph/DRBD, multi-host fencing, or a kill-9 campaign on physical
  hardware. Those remain the open evidence gaps in
  [`pre-production-readiness.md`](../../releases/pre-production-readiness.md).

## The durable-first model

Every privileged mutation writes a **durable intent** and fsyncs it before the
allocating or destructive step, then commits the resulting volume record in a
second atomic save. A save is `write .tmp` → `fsync file` → `rename` →
`fsync directory`; an error can therefore land on either side of the rename:

- **pre-rename:** the old durable file is intact, and the world step may or may
  not have run;
- **post-rename:** the new durable file is in place, and only the directory
  fsync failed. The durable file already contains the record; the provider
  reports the error but keeps the committed shape.

A 5xx alone never proves "nothing happened". Startup reconciliation is the
authority: it re-reads the durable intents and classifies them against `lvs`.

The intent maps (`pending_creates`, `pending_grows`, `pending_deletes`) are
defaulted and omitted when empty. Adding them is a durable-shape change, so
`LVM_STATE_SCHEMA_VERSION` is **2**: v1 files load with empty maps and are
upgraded by the first successful save, while an older binary refuses a v2
file with the typed unsupported-version error instead of failing on an
unknown field. A state with a pending intent is therefore a supported v2
file; see the
[durable-state schema compatibility policy](../../schema-compatibility.md).
The v1 N-1 fixture is `tests/fixtures/schema/lvm-state-v2-n1.json`.

## Contract per mutation

`skip` in the test seam (`StoreCrashHooks::arm_save_failure`) counts matching
save consults; the same point on a different save does not consume the arm. The
tests in `crates/volvisor-lvm/tests/lvm_tests.rs` and `move_tests.rs` use it to
target each boundary exactly.

| Mutation | Durable anchor written before the world step | World step | Pre-rename intent-save failure | Final-save failure, pre-rename | Final-save failure, post-rename (rename landed) | Startup recovery |
|---|---|---|---|---|---|---|
| **create** | `CreateIntent` (VG, id-derived LV name, requested size, project, block size, canonical payload) | `lvcreate` | No LV is created. If the rename did land, the durable intent remains and is dropped at startup (no intent-owned LV). | The LV exists but the volume record is not durable. The provider compensates with a **checked** `lvremove`, drops the in-memory entry and clears the intent. A failing `lvremove` names `manual lvremove <vg>/<lv>`. An unreadable durable re-read is treated as *not committed* (fail-closed) and the file is left for the operator. | The durable file contains this volume's record: keep the LV and the in-memory entry, do not compensate; the error says the record reached the state file. | If a volume record exists, the stale intent is dropped. Otherwise an intent-owned LV that is present and **at least** the requested size is adopted as generation 1, `Ready`. A smaller LV, a mismatched LV name, or an unobservable size **fails startup** rather than adopting the wrong geometry. An absent LV drops the intent. |
| **grow** | `GrowIntent` (operation, from-size, requested size, expected generation) | `lvextend` | No `lvextend` runs. If the rename did land, the durable intent remains and startup drops it after observing the LV still at the from-size. | The LV is larger; the durable volume record still shows the old size. The in-memory view keeps the observed new size and bumped generation, and the durable grow intent stays for restart. **The LV is never shrunk** (data may already live in the new extents). | The durable file already contains the completed record and generation bump: keep it and report the error. | If the generation/from-size is stale, or the LV is absent, the intent is dropped (an absent LV is marked `Failed` by the volume pass). At or above the requested size: finalize from the observed size, bump the generation exactly once, drop the intent. Still at the from-size: drop the intent (the grow never landed). Any size strictly between: **fail startup** rather than guess. |
| **delete** | `DeleteIntent` (operation, expected generation, erasure policy) | `blkdiscard` (ZeroDiscard) then `lvremove`; refused while an unsettled move record exists | No destructive command runs. If the rename did land, the durable intent remains; a retry or restart completes the authorized delete. | If the destructive command fails, the durable intent is **kept**: the volume stays fenced against attach/grow/move (a `blkdiscard` may have partially erased or `lvremove` may have raced a move), and a retry or restart completes it. If the final save fails, the LV may already be gone but the record is not removed; the in-memory record, delete intent and move record are restored; a retry completes it. | The durable file already contains the deletion (volume record, intent and move record removed): report the error, keep the in-memory result, and do not re-delete. | A stale-generation intent is dropped **without deleting** (never delete on a stale authorization). A volume already absent from the record drops the intent. A matching intent runs the recorded policy (`blkdiscard` when `ZeroDiscard`, then `lvremove`) and removes the record; if the LV is already absent, no destructive command runs and the observed absence is accepted as the terminal state (the record is removed, but the provider does not claim the erase command ran). Delete completion also clears any stale create/grow intent **and the settled move record** for the same id. The delete does **not** become a `Failed` volume. An attached or non-`Ready`/`Failed` volume with a pending delete **fails startup**, and a pending delete with an unsettled move **also fails startup**, rather than removing an LV a consumer may still use. |
| **move — `PREPARING`** | `MoveRecord { state: Preparing }` | `pvmove` | No `pvmove` starts; the PREPARING record is the sole anchor and nothing durable exists to reconcile. The failure does not wedge the volume. | — (the same save is the anchor) | The durable file contains PREPARING; startup classifies it against the observed world. | A PREPARING record whose `pvmove` verifiably never started is re-driven by the retry reconcile; a started/relocated world is classified and completed only on verified relocation. |
| **move — `COPYING`/`COMPLETE`** | Prior durable record (`Preparing`/`Copying`) | `pvmove` completion | — | The prior durable record survives. The in-memory completion is rolled back to the prior record (never a completion against a prior durable file); startup classifies the world and completes on verified relocation with exactly one generation bump. | The durable file already holds the completed record and the new generation: keep the completion and report the error. | The completed record is not bumped again; a verified relocation completes a `Copying`/`IN_DOUBT` record; an unverifiable world parks `IN_DOUBT` instead of guessing. |

Three cross-cutting rules:

- **No new authority over a pending delete.** While a durable delete intent
  exists, a create replay, attach, grow and move are refused with
  `INVALID_STATE` naming the pending delete. Delete retries are allowed — they
  are the completion path.
- **No delete over an unsettled move.** A `Preparing`, `Copying`,
  `MIRROR_READY`, `PIVOTED` or `IN_DOUBT` move record refuses a live delete
  before any destructive command, and fails startup if it coexists with a
  durable delete intent. `Complete`/`Failed` records are settled and are
  removed with the volume.
- **Never delete foreign LVs.** Recovery acts only on the volume recorded by
  its own intent. It never enumerates and removes LVs it does not own.
- **A delete clears stale state for the same volume id.** Completing a delete
  removes any create/grow intent and the settled move record left by earlier
  mutations in the same atomic save, so a later create with that id cannot
  inherit an old authorization (or a false `COMPLETE`). If that save fails
  before its rename, the volume, delete intent, stale intents and move record
  are all restored in memory to match the durable file.

## A. Detect

A failed persist surfaces as an `INTERNAL` 500 whose detail names the state
path/`.tmp` write, the failed rename/fsync, or an injected test fault. Look for
the mutation's own wording:

- create intent: `the create intent ... could not be made durable`
- create final save: the compensation text naming the checked `lvremove`
  (`manual lvremove` / `could not be verified`) or the carve-out text
  `the durable state already contains`
- grow: `the grow intent ... could not be made durable` or the final-save text
  naming the durable grow intent
- delete: `the delete intent ... could not be made durable`, the
  post-rename text saying a retry/restart will complete the delete, or the
  command-failure text `remains authorized and pending`
- move: `the move's PREPARING record could not be saved`, the `COPYING`
  prior-anchor text, or the post-rename `COPYING anchor remains` /
  `durable state already records the completed move` carve-out text
- startup adoption save failure: `... so the LV ... is left in place`

Do not infer the outcome from the status code. Read the durable intents and the
world:

```sh
STATE=/var/lib/volvisor/journal/lvm-state.json
sha256sum "$STATE"
stat -c '%s %n' "$STATE"
jq '{pending_creates, pending_grows, pending_deletes, moves}' "$STATE"
lvs --noheadings -o lv_name,vg_name,lv_size
curl -sS -o /dev/null -w '%{http_code}\n' \
  --config ~/.volvisor-admin.curl \
  http://127.0.0.1:8080/v2/volumes/$VOLUME_ID
```

If the daemon cannot read the state file it will not start; go to
[journal-and-state-corruption.md](journal-and-state-corruption.md) instead of
patching JSON.

## B. Recover

1. **Stop the daemon before inspecting or changing state.** `flock` is the
   single-writer guard and recovery only runs at startup or on the retry
   reconcile.

```sh
systemctl stop volvisord.service
```

2. **Free space or clear the I/O fault.** Do not delete or hand-edit the state
   file and do not remove an LV whose ownership the file does not prove.

3. **Restart and let reconciliation resolve the intents.**

```sh
systemctl start volvisord.service
curl -fsS http://127.0.0.1:8080/healthz
jq '{pending_creates, pending_grows, pending_deletes, moves}' "$STATE"
lvs --noheadings -o lv_name,vg_name,lv_size
```

4. **Use the per-mutation tails below.** Anything that does not match the
   expected outcome is a divergence: keep the LV, stop, and escalate with the
   state hash, the intent maps and `lvs` output.

### Create tail

- **Normal / compensated:** `GET` is 404, no LV named for the failed id, and
  no `pending_creates` entry. Retry with a **new `operation_id`** and the same
  payload.
- **Cleanup failure:** `GET` is 404, the LV is present but absent from the
  state, and the 500 names `manual lvremove`. Confirm the LV is not referenced
  by the durable state or a consumer, then remove it; retry with a new
  `operation_id`.
- **Carve-out (rename landed):** `GET` is 200, the durable record, memory and
  LV agree. The volume is served; a new `operation_id` with the same payload
  returns it idempotently. Do not remove the LV.
- **Unreadable durable re-read:** treat as indeterminate. The provider
  attempted the checked `lvremove` and left the file for the operator. If the
  file later reads, follow the normal or carve-out branch it proves. Never
  retry or delete until the file is readable.

### Grow tail

- **Normal / rolled back:** the volume is still at its old size and generation,
  with no `pending_grows` entry. Retry the grow.
- **Final-save failure (pre-rename):** the LV is already larger; the durable
  volume record may still show the old size and a `pending_grows` entry. The
  current API may already report the observed size (memory) — restart to roll
  it forward. Do **not** shrink the LV.
- **Roll-forward:** after restart the volume reports the observed size, the
  generation is bumped exactly once, and the intent is gone. Retry the grow
  only for a further size increase.
- **Ambiguous geometry at startup:** startup refuses rather than guessing.
  Escalate with the intent's from-size/requested size and the observed
  `lv_size`; do not hand-edit the intent to force a start.

### Delete tail

- **Intent-save failure (pre-rename):** no destructive command ran. The volume
  is still `Ready`/`Failed`, the LV is intact, and no intent is durable. A
  retry with a new `operation_id` (or a restart) re-authorizes it.
- **Intent-save failure (post-rename):** the durable intent exists and the
  pending-delete fence stays in force. A retry or restart completes the
  authorized delete.
- **Destructive command failure:** the durable intent is kept on purpose. The
  `lvremove` did not complete, but a `blkdiscard` may have partially erased or
  the command may have raced a move; the volume remains fenced against
  attach/grow/move. Inspect the LV, then retry the delete (or restart) to
  complete it. Do not force an attach or hand-remove a record.
- **Final-save failure:** the LV may already be gone. Retry the delete; do not
  create a new `Failed` volume by hand. Restart completes a matching intent and
  removes the record, including its settled move record.
- **Post-rename (carve-out):** the durable state already records the deletion;
  `GET` is 404, `pending_deletes` and `moves` are empty for the id, and the LV
  is gone. Nothing to do.
- **Stale generation:** a restart drops the intent without deleting anything.
  The volume is still served; re-authorize the delete if it was intended.
- **Attached/non-stable volume with a pending delete:** startup fails closed.
  Detach the volume through the API first (if the delete is still wanted), then
  restart; do not remove an attached LV during startup.
- **Unsettled move with a pending delete:** the live delete is refused before
  any destructive command, and startup refuses to complete the delete. Let the
  move reconcile to `Complete`/`Failed` (or investigate an `IN_DOUBT` record)
  before retrying the delete.

### Move tail

- **PREPARING save failure:** no `pvmove` started, no move record, source
  placement intact. A normal move can be retried.
- **COPYING save failure, pre-rename:** the pvmove may be running and the
  durable record is the prior `Preparing` record (the anchor). Restart.
  Reconciliation observes the world: a verified relocation completes with one
  bump; anything else stays classified/parked rather than restarted blind.
- **COPYING save failure, post-rename:** the durable record is `Copying` and
  the in-memory record is kept at `Copying` too. A retry re-attaches to the
  live move; restart likewise classifies it. There is no second durable state
  to guess from.
- **COMPLETE save failure, pre-rename:** the world is already on the target but
  the durable record is the prior one. The API serves the prior shape until
  restart; restart completes it with exactly one bump.
- **COMPLETE save failure, post-rename:** the durable record already says
  `Complete` with the new generation; keep it. A restart must not bump again.
- Never free the source or re-issue `pvmove` by hand; let the reconcile task
  classify.

## C. Verification

- The state file hash changed only if a durable roll landed; a failed
  pre-rename save leaves the prior bytes.
- `pending_creates`, `pending_grows` and `pending_deletes` are empty, or an
  entry is explained by the tail above.
- Each remaining volume has exactly one LV, and each remaining LV belongs to a
  durable record (except an operator-confirmed cleanup-failure orphan).
- No volume with a durable pending delete grants attach/grow/move authority.
- `GET /v2/volumes` agrees with `jq '.volumes'` and with `lvs` for every
  affected id.
- A move record is `Complete` only after a verified relocation; otherwise it
  stays `Copying`/`IN_DOUBT` with an honest detail.

## D. Deterministic test references

Production code: `crates/volvisor-lvm/src/provider.rs` (recovery passes,
mutation paths), `crates/volvisor-lvm/src/state.rs` (intent schema and atomic
save), `crates/volvisor-types/src/crash.rs` (the test-only
`AfterRenameBeforeDirFsync` / `arm_save_failure` seam).

| Contract | Test |
|---|---|
| create final-save compensation, cleanup failure, unreadable re-read | `create_save_failure_rolls_back_lv_and_memory_and_converges_across_restart`, `create_save_failure_with_a_failing_lvremove_surfaces_the_orphan`, `create_save_failure_with_an_unreadable_durable_file_still_compensates` |
| create post-rename carve-out (real dir-fsync seam) | `create_save_failure_after_rename_is_honored_not_compensated` |
| create intent-save failure (pre- and post-rename) | `create_intent_save_failure_runs_no_lvcreate`, `create_intent_save_failure_after_rename_is_dropped_on_restart` |
| create startup adoption / absent LV / undersized LV | `create_intent_adopts_a_verified_lv_left_by_a_kill_window`, `create_intent_is_dropped_when_the_intent_owned_lv_is_absent`, `create_intent_with_an_undersized_lv_fails_startup` |
| grow intent-save failure / final-save roll-forward / seeded roll-forward / drop / ambiguous / stale generation | `grow_intent_save_failure_means_no_lvextend`, `grow_final_save_failure_rolls_forward_on_restart`, `grow_intent_roll_forward_at_startup`, `grow_intent_is_dropped_when_the_lv_did_not_grow`, `grow_intent_with_ambiguous_geometry_fails_startup`, `grow_intent_with_a_stale_generation_is_dropped_at_startup` |
| grow intent/final save post-rename | `grow_intent_save_failure_after_rename_is_dropped_on_restart`, `grow_final_save_failure_after_rename_is_the_carve_out` |
| delete intent-save failure / final-save + pending-delete guards / startup completion / policy / stale generation / attached refusal / post-rename carve-out | `delete_intent_save_failure_leaves_the_volume_untouched`, `delete_intent_save_failure_after_rename_keeps_the_fence_and_completes_on_restart`, `delete_final_save_failure_blocks_new_authority_and_completes_on_retry`, `delete_final_save_failure_is_completed_at_startup`, `delete_startup_completion_runs_the_recorded_policy`, `delete_intent_with_a_stale_generation_is_dropped_without_deleting`, `delete_intent_with_an_attached_volume_fails_startup`, `delete_post_rename_failure_is_the_carve_out` |
| delete command failure keeps the pending authorization | `delete_command_failure_keeps_the_pending_authorization_and_fences_new_authority` |
| delete ↔ move-record boundary | `delete_refuses_an_unsettled_move_before_any_destructive_command`, `delete_with_a_settled_move_clears_the_move_record_and_prevents_a_false_complete`, `a_delete_completion_save_failure_restores_the_move_record`, `startup_refuses_to_complete_a_delete_with_an_unsettled_move` |
| move PREPARING / COPYING / COPYING post-rename / COMPLETE / COMPLETE post-rename | `a_preparing_save_failure_starts_no_pvmove`, `a_preparing_save_failure_after_rename_keeps_the_anchor_and_starts_no_pvmove`, `a_copying_save_failure_keeps_the_anchor_and_startup_completes`, `a_copying_save_failure_after_rename_keeps_the_anchor_and_startup_completes`, `a_complete_save_failure_recovers_with_one_bump`, `a_complete_save_failure_after_rename_is_the_carve_out` |

Run them with:

```sh
cargo test -p volvisor-lvm --test lvm_tests
cargo test -p volvisor-lvm --test move_tests
```

## E. Limits (read before quoting this page)

- **Simulation, not hardware.** These tests drive the provider through
  `FakeLvm` and the in-process runner. They prove ordering, fail-closed
  decisions and idempotence; they do **not** prove real `lvcreate`/`pvmove`
  crash behavior, filesystem ordering on a real disk, or a kill-9 at every
  instruction.
- **No real post-rename device.** The `AfterRenameBeforeDirFsync` seam returns
  a typed error at the real save path; it does not power-cut a physical
  directory entry.
- **Adoption is name+size based.** A durable create intent proves the
  provider intended to create the id-derived LV at the requested size; the
  provider does not read an on-LV ownership tag. A pre-existing LV that
  already used the exact derived `vol-…` name and is at least the requested
  size would be adopted. Do not create operator/foreign LVs in Volvisor's
  claimed VG with derived names; recovery never deletes such an LV.
- **Single host, single writer.** No concurrent daemon, multi-host relocation,
  witness/fencing or DRBD interaction is exercised here.
- **No production claim.** Nothing here is an SLA, a zero-RPO statement, a
  release qualification or evidence for the five readiness gates. The overall
  verdict remains **NOT PRE-PRODUCTION READY**; see
  [`pre-production-readiness.md`](../../releases/pre-production-readiness.md).
