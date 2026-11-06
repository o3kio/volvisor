# Default integration scenarios for durable semantics

Status: normative catalog for the provider/journal durable-semantics
surface. Enforced on every change by `scripts/test-default-scenarios.sh`
in the `.github/workflows/ci.yaml` `check` job.

Related: [ADR-0013](../adr/0013-durable-attachment-generation-allocator.md)
(generation lifetime),
[ADR-0014](../adr/0014-journal-bounds-and-exhaustion-contract.md)
(journal bounds),
[ADR-0015](../adr/0015-write-path-critical-section-invariant.md)
(write-path critical sections),
[ADR-0016](../adr/0016-semantic-change-and-default-scenario-gate.md)
(the change gate),
[schema compatibility](../schema-compatibility.md),
[Phase F plan](../plans/2026-11-06-phase-f-write-path-remainder.md),
[Phase F evidence](../benchmarks/2026-11-06-devhost-phase-f-write-path-remainder/REPORT.md).

## What "default" means here

These are the default **regression** scenarios for the semantics that decide
whether an existing daemon keeps running and whether existing durable state
is interpreted correctly. They are a pinned subset of the workspace test
suite: the same tests already run under `cargo test --workspace --locked`,
but the catalog additionally guarantees that each exact test still exists
with its exact name and is neither removed, renamed nor `#[ignore]`d.

The catalog is not a production-support claim, an SLA, a durability proof, a
data-path benchmark, a real-backend qualification, a 24-hour soak, or a
multi-host/hardware result. A green catalog says the control-plane
regression semantics below still hold on the test host.

## Acceptance time budget

The gate is deliberately fast so it can run on every acceptance:

- **60 s for any single scenario** (`--per-test-timeout`, default `60`).
- **300 s for the whole warm execution** (`--budget`, default `300`).
  Compilation and test-binary listing happen before the timer starts and are
  outside the budget, so a cold build does not consume it.
- A test that exceeds either bound fails the gate with a typed message.

Indicative warm baseline for the initial 54 scenarios on the development
host: **9.0 s total, slowest 1.4 s**. Treat that as a measured baseline, not
a CI SLA.

Long-running work is **not admitted** to this catalog: the shortened or
24-hour soak, the real LVM/Ceph/DRBD suites, campaign Tier R/V, fault
campaigns, fuzzing and evidence benchmarks stay in their scheduled or
evidence lanes. If a semantic needs a long run to prove, add a fast
deterministic scenario for acceptance and record the long proof separately.

## How to run

```sh
# Run every catalogued test, exactly, under the default 60 s/300 s budget.
scripts/test-default-scenarios.sh

# Fast check: verify each test still exists and each scenario is documented,
# without executing it.
scripts/test-default-scenarios.sh --list-only

# Tighten the budget for a constrained acceptance host.
scripts/test-default-scenarios.sh --per-test-timeout 30 --budget 120
```

The runner reads
[`default-integration-scenarios.tsv`](default-integration-scenarios.tsv),
which is the machine-readable list. It caches `cargo test -- --list` once per
package/test-binary pair, asserts the exact test name is present, then runs
it with `--exact --test-threads=1`. It fails if the test is missing,
filtered out, ignored, duplicated, undocumented, or not exactly one passing
test. CI runs the full form after `cargo test --workspace --locked`.

## Adding or changing a scenario

Any change that alters durable semantics must add or update a scenario here
before merge (see ADR-0016). The change set must include:

1. the ADR or ADR amendment that records the decision;
2. the compatibility and migration statement, including the N-1 fixture for
   a schema bump;
3. the fail-closed refusal path;
4. a new row in the TSV and a row in this document with the exact test name;
5. rollback/replay evidence or an explicit recorded gap.

The scenario must fit the acceptance budget (60 s for that test, 300 s for
the whole warm catalog). A test that needs a long run, a real backend, root
or a soak is not a default scenario; pair a fast deterministic scenario with
a separately recorded long proof.

Removing, renaming or ignoring a catalogued test is a semantic change, not
test cleanup. It requires the same review as the behavior it protects.

## WP — write-path critical sections (ADR-0015)

Each scenario parks an external command and proves that the provider state
lock is not held across it, or that the epoch/revalidation protocol refuses
a stale decision. The failure mode is a silent global stall or a destructive
command authorized by a stale in-memory view.

| ID | Invariant | Exact test (`package`, target) |
|---|---|---|
| WP-01 | Create persists its intent, then runs `lvcreate` without the state lock. | `create_releases_the_state_lock_across_lvcreate` (`volvisor-lvm`, `--test lvm_tests`) |
| WP-02 | Create samples `vgs` outside the lock or by a bounded epoch retry. | `create_releases_the_state_lock_across_vgs_pool_sample` (`volvisor-lvm`, `--test lvm_tests`) |
| WP-03 | A capacity sample is retried or refused when the allocation epoch moves. | `a_create_re_samples_when_the_allocation_epoch_moves` (`volvisor-lvm`, `--test lvm_tests`) |
| WP-04 | Failed-create compensation runs `lvremove` without the state lock. | `create_compensation_releases_the_state_lock_across_lvremove` (`volvisor-lvm`, `--test lvm_tests`) |
| WP-05 | Delete runs its `lvs` existence check outside the lock. | `delete_releases_the_state_lock_across_lvs_existence_check` (`volvisor-lvm`, `--test lvm_tests`) |
| WP-06 | Delete runs `lvremove` without the state lock. | `delete_releases_the_state_lock_across_lvremove` (`volvisor-lvm`, `--test lvm_tests`) |
| WP-07 | Delete refuses to act when the volume changed during the unlocked check. | `delete_refuses_a_volume_that_changed_during_the_lvs_check` (`volvisor-lvm`, `--test lvm_tests`) |
| WP-08 | Move's `lvs` observation does not hold the state lock. | `move_releases_the_state_lock_across_lvs_observation` (`volvisor-lvm`, `--test lvm_tests`) |
| WP-09 | Move's `pvmove` start does not hold the state lock. | `move_releases_the_state_lock_across_pvmove_start` (`volvisor-lvm`, `--test lvm_tests`) |
| WP-10 | Grow's `lvextend` does not hold the state lock. | `grow_releases_the_state_lock_across_lvextend` (`volvisor-lvm`, `--test lvm_tests`) |
| WP-11 | Grow samples capacity outside the lock under the epoch protocol. | `grow_samples_capacity_without_holding_the_state_lock` (`volvisor-lvm`, `--test lvm_tests`) |

Accepted exceptions (documented in ADR-0015, not separate scenarios): the
failure-only durable re-read that classifies a failed save stays under the
lock, startup recovery may run destructive commands under the single-threaded
startup lock, and move's synchronous CLI calls remain inline on async workers
as a tracked worker-handoff residual.

## FC — fail-closed create/delete and idempotency

Each scenario injects a save failure, an unreadable durable file, a stale
intent or a replay and proves the provider keeps or clears the durable
authority truthfully. The failure mode is an orphaned LV reported as absent,
a deleted record without a durable intent, or a second execution of a
mutation.

| ID | Invariant | Exact test (`package`, target) |
|---|---|---|
| FC-01 | A failed create save rolls back and converges on restart. | `create_save_failure_rolls_back_lv_and_memory_and_converges_across_restart` (`volvisor-lvm`, `--test lvm_tests`) |
| FC-02 | A failed compensating `lvremove` surfaces the orphan instead of claiming success. | `create_save_failure_with_a_failing_lvremove_surfaces_the_orphan` (`volvisor-lvm`, `--test lvm_tests`) |
| FC-03 | A create whose rename landed is honored, not compensated away. | `create_save_failure_after_rename_is_honored_not_compensated` (`volvisor-lvm`, `--test lvm_tests`) |
| FC-04 | An unreadable durable file still fails closed and compensates. | `create_save_failure_with_an_unreadable_durable_file_still_compensates` (`volvisor-lvm`, `--test lvm_tests`) |
| FC-05 | A failed compensation clear keeps the intent for recovery. | `a_create_compensation_clear_failure_keeps_the_intent_in_memory` (`volvisor-lvm`, `--test lvm_tests`) |
| FC-06 | A retry adopts only a verified LV left by a kill window. | `create_intent_adopts_a_verified_lv_left_by_a_kill_window` (`volvisor-lvm`, `--test lvm_tests`) |
| FC-07 | A failed delete keeps its durable authorization and fences new authority. | `delete_command_failure_keeps_the_pending_authorization_and_fences_new_authority` (`volvisor-lvm`, `--test lvm_tests`) |
| FC-08 | A stale delete intent is dropped without deleting the volume. | `delete_intent_with_a_stale_generation_is_dropped_without_deleting` (`volvisor-lvm`, `--test lvm_tests`) |
| FC-09 | A failed delete completion is finished at startup. | `delete_final_save_failure_is_completed_at_startup` (`volvisor-lvm`, `--test lvm_tests`) |
| FC-10 | A failed delete-intent save leaves the volume untouched. | `delete_intent_save_failure_leaves_the_volume_untouched` (`volvisor-lvm`, `--test lvm_tests`) |
| FC-11 | Create replay is idempotent and payload-exact. | `provider_conformance::idempotent_create` (`volvisor-lvm`, `--test conformance`) |

## GF — attachment generation and fence

These scenarios protect ADR-0012's report/detach fence and the generation
source to be revised by ADR-0013. The failure mode is a delayed report or
detach from a retired instance being accepted against a live attachment, or
a replay fabricating a second attachment.

| ID | Invariant | Exact test (`package`, target) |
|---|---|---|
| GF-01 | A consumed attachment id cannot be reattached under a stale generation. | `reattaching_a_consumed_id_fences_the_retired_instance_by_generation` (`volvisor-lvm`, `--test lvm_tests`) |
| GF-02 | Restart preserves attachment state and never moves the generation. | `report_state_survives_a_provider_restart_and_never_moves_the_generation` (`volvisor-lvm`, `--test lvm_tests`) |
| GF-03 | Attachment replay survives a provider restart. | `attach_replay_survives_a_provider_restart` (`volvisor-lvm`, `--test lvm_tests`) |
| GF-04 | Attachment replay with a different disk id is a typed conflict. | `attach_replay_conflicts_on_a_different_vmm_disk_id` (`volvisor-lvm`, `--test lvm_tests`) |
| GF-05 | Attachment replay never fabricates a second attachment. | `attach_replay_never_fabricates_a_second_attachment` (`volvisor-lvm`, `--test lvm_tests`) |
| GF-06 | Generation fences are distinct typed refusals. | `semantics::tests::generation_fences_are_typed_and_separate` (`volvisor-provider`, `--lib`) |
| GF-07 | Reports advance monotonically without changing the generation. | `semantics::tests::reports_advance_monotonically_without_moving_the_generation` (`volvisor-provider`, `--lib`) |
| GF-08 | Attachment replay is identity-exact. | `semantics::tests::attach_replay_is_identity_exact` (`volvisor-provider`, `--lib`) |
| GF-09 | A stale expected generation is refused. | `provider_conformance::attach_stale_generation` (`volvisor-lvm`, `--test conformance`) |
| GF-10 | Attach replay is idempotent. | `provider_conformance::idempotent_attach` (`volvisor-lvm`, `--test conformance`) |
| GF-11 | Single-writer attachment admission is preserved. | `provider_conformance::attach_single_writer` (`volvisor-lvm`, `--test conformance`) |

ADR-0013's delete + same-id recreate scenario is **not yet implemented**;
when it lands it is added here as `GF-12`. Until then the residual is
recorded in the readiness report and `#63` stays open.

## SC — durable schema compatibility

These scenarios protect the fail-closed decode rules in
[`docs/schema-compatibility.md`](../schema-compatibility.md). The failure
mode is silently dropping legacy data, accepting a malformed filter,
rewriting a future-version file, or starting on an unknown schema.

| ID | Invariant | Exact test (`package`, target) |
|---|---|---|
| SC-01 | The v3 N-1 fixture imports legacy ids and upgrades without loss. | `state::tests::v3_n_minus_one_fixture_imports_legacy_ids_and_upgrades_to_v3` (`volvisor-lvm`, `--lib`) |
| SC-02 | The legacy tombstone fixture decodes without loss. | `state::tests::tombstone_fixture_preserves_retired_attachment_ids` (`volvisor-lvm`, `--lib`) |
| SC-03 | The retired-id filter has no false negatives and is bounded. | `state::tests::retired_attachment_filter_has_no_false_negatives_and_is_bounded` (`volvisor-lvm`, `--lib`) |
| SC-04 | A malformed filter is refused typed. | `state::tests::malformed_retired_attachment_filter_is_rejected` (`volvisor-lvm`, `--lib`) |
| SC-05 | A legacy vector merges into an existing filter without loss. | `state::tests::a_legacy_vector_is_merged_into_an_existing_filter_without_false_negatives` (`volvisor-lvm`, `--lib`) |
| SC-06 | A noncanonical filter with set bits is not treated as empty. | `state::tests::a_noncanonical_filter_with_set_bits_is_never_treated_as_empty` (`volvisor-lvm`, `--lib`) |
| SC-07 | A future schema version is refused without rewriting the file. | `state::tests::future_schema_version_is_refused_typed_without_rewriting` (`volvisor-lvm`, `--lib`) |
| SC-08 | A zero schema version is refused typed. | `state::tests::zero_schema_version_is_refused_typed` (`volvisor-lvm`, `--lib`) |

## JR — journal retention, replay and idempotency

These scenarios protect ADR-0014's current implementation: terminal
retention is bounded, in-flight intents are pinned, replay evicts and
compacts before serving, and a failed compaction never fails a durable
append. The failure mode is unbounded growth, eviction of recovery state, a
served request before replay completes, or a lost append acknowledgement.

| ID | Invariant | Exact test (`package`, target) |
|---|---|---|
| JR-01 | Compaction bounds the log and expires evicted operations. | `journal::tests::compaction_bounds_the_log_and_expires_evicted_operations` (`volvisor-journal`, `--lib`) |
| JR-02 | The tombstone window is bounded and overflow has the documented answer. | `journal::tests::the_tombstone_window_is_bounded_and_its_overflow_is_the_documented_contract` (`volvisor-journal`, `--lib`) |
| JR-03 | Replay evicts and compacts before serving. | `journal::tests::bounded_replay_evicts_and_compacts_before_serving` (`volvisor-journal`, `--lib`) |
| JR-04 | A failed compaction never fails a durable append and is retried. | `journal::tests::a_failed_compaction_never_fails_the_durable_append_and_is_retried` (`volvisor-journal`, `--lib`) |
| JR-05 | A persistent compaction failure backs off and recovers after growth. | `journal::tests::a_persistently_failing_compaction_backs_off_and_recovers_after_growth` (`volvisor-journal`, `--lib`) |
| JR-06 | Compaction evicts to fit one frame instead of failing forever. | `journal::tests::compaction_evicts_to_fit_one_frame_instead_of_failing_forever` (`volvisor-journal`, `--lib`) |
| JR-07 | A post-snapshot record for a snapshot tombstone is still refused. | `journal::tests::a_post_snapshot_record_for_a_snapshot_tombstone_is_still_refused` (`volvisor-journal`, `--lib`) |
| JR-08 | A pinned in-flight operation is never evicted. | `journal::tests::a_pinned_operation_is_never_evicted_while_its_retry_is_executing` (`volvisor-journal`, `--lib`) |
| JR-09 | A pin interleaved with compaction preserves the outcome across reopen. | `journal::tests::a_pin_interleaved_with_compaction_preserves_the_replacement_outcome_across_reopen` (`volvisor-journal`, `--lib`) |
| JR-10 | A redriven outcome after window eviction reopens instead of wedging. | `journal::tests::a_redriven_outcome_after_a_replay_window_eviction_reopens_instead_of_wedging` (`volvisor-journal`, `--lib`) |

ADR-0014's hard-ceiling refusal (`JOURNAL_FULL`) and its metrics are **not
yet implemented**; when they land they are added here as `JR-11` and
`JR-12`. The Phase F report records the current soft-bound residual.

## CF — durable configuration bounds

These scenarios protect the configuration values that feed durable retention.
The failure mode is an unbounded or zero bound silently accepted, or the
byte trigger disabled along with the record trigger.

| ID | Invariant | Exact test (`package`, target) |
|---|---|---|
| CF-01 | Journal retention defaults are bounded and mapped to the runtime. | `config::tests::journal_retention_defaults_are_bounded_and_mapped` (`volvisord`, `--lib`) |
| CF-02 | Retention parses and zero bytes disables only the byte trigger. | `config::tests::journal_retention_is_parsed_and_zero_bytes_disables_only_the_byte_trigger` (`volvisord`, `--lib`) |
| CF-03 | Invalid retention bounds and unknown keys are refused. | `config::tests::journal_retention_rejects_zero_bounds_and_unknown_keys` (`volvisord`, `--lib`) |

## Non-claims

- Passing this catalog does not make any readiness gate met. Gates 1/2/5
  remain **NOT MET**, gates 3/4 remain **PARTIAL**, and the overall status
  remains **NOT PRE-PRODUCTION READY**.
- It does not prove real-backend behavior, a 24-hour soak, data-path
  durability, multi-host fencing, crash consistency on real media, or
  production capacity.
- It does not implement ADR-0013's durable allocator or ADR-0014's hard
  ceiling. ADR-0012/0013/0014/0015/0016 are **Accepted** (2026-11-06,
  zoorpha); acceptance does not add the scenarios still owed.
- `#62`, `#63` and `#64` remain open.
