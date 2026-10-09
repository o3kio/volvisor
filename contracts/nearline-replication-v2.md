# Nearline replication and migration contract v2

Status: Proposed / not implemented
Version: 2
Date: 2026-10-09
Related: [ADR-0004](../docs/adr/0004-nearline-replication-and-mobility.md), [SPEC-0002](../docs/specs/SPEC-0002-volvisor-volume-virtualization.md), [Volume API v2](volume-api-v2.md)

This version strengthens the draft v1 replicated-async contract. It **does not** claim that DRBD, Mayastor, SPDK or a custom engine already meets it.

## 1. Required durable state

A nearline volume must be able to recover at least:

```text
volume_id, data_lineage_id, volume_generation, migration_id?,
writer_epoch, authoritative_writer, lease_proof, authority_commit_index,
local_committed_prefix, local_acknowledged_boundary,
replicas[] {
  replica_id, host_id, local_protection_health, data_lineage_id,
  received_prefix, applied_prefix, durable_prefix,
  resync_baseline, dirty_ranges_or_log_cursor, health
}
handoff {state, source_epoch, destination_epoch, barrier, outcome}
```

Sequence numbers without a lineage/epoch are not enough. `local_protection_health` is the runtime health of that replica's `local_protection` policy (section 3). A replica's `durable_prefix` must be a **contiguous exact-lineage prefix**; holes and reordered writes may never be declared durable merely because a later sequence reached disk. Durable metadata and payload must survive loss of the processes or host according to the published ACK contract.

## 2. Writer authority / quorum

One writable authority per volume. Epochs increase monotonically, including after crash, promotion, cancellation and failback. A stale writer's I/O must be rejected at an enforcement point it cannot bypass (including stale host-native device mappings), not merely by a control-plane status flag.

Quorum or independently proved external fencing is required for unsafe authority transfers. A two-data-node partition **cannot** elect two primaries. A third witness may hold authority metadata without holding tenant blocks. Expiring leases only work if every write path self-fences at expiry and renewals come from a correct authoritative service.

Consumer O3K API unavailability alone should not kill established I/O, but loss of runtime lease quorum may force write rejection. This is a conscious availability/safety tradeoff, not a bug to paper over.

## 3. Local and remote protection

`local_protection` is an independent requested/effective policy per replica:
- `none`: local SSD loss may destroy that copy;
- `mirror`: explicitly qualified multi-leg local mirror, with number of healthy legs and rebuild progress;
- additional mechanisms only by separately versioned capability.

`remote_replicas` live in distinct **host failure domains**. Two local mirrored legs do not count as remote replicas. A peer on the same physical host cannot satisfy inter-host protection.

A local mirror leg failure should not trigger a cross-host writer election if the active local copy remains valid. The provider must expose degraded local redundancy and protect against a second failure during rebuild.

## 4. Steady-state write ordering

For every guest write/zero/discard/barrier:
1. Validate current writer authority and volume/attachment generation.
2. Establish a recoverable ordering/record of the affected logical bytes.
3. Commit data and metadata necessary to recover that operation locally, honoring cache mode, FUA and flush ordering.
4. Advance crash-recoverable local committed/ACK prefix.
5. ACK without requiring remote durable media for `replication.mode=async`.
6. Send ordered changes to peer(s) and persist confirmed peer durable progress.

An implementation may pipeline the steps but cannot acknowledge before its own local-durable contract is satisfied. Never replay a discard out of order or silently interpret it as a zero write. Dirty-range metadata must be updated/committed atomically enough to avoid declaring unsynchronized bytes clean after a crash.

A crash during in-place copying must not leave a target marked current with a mixture of old and new bytes. Use snapshots, versioned extents, copy-on-write or another proof of consistent delta capture.

## 5. Target progress and seeding

`received_prefix` != `applied_prefix` != `durable_prefix`. A candidate target is current through B iff:
- data lineage and writer epoch match the authoritative source barrier;
- all operations up to B applied without holes in source order;
- payload, metadata and required cache flush are durable;
- integrity checks show no divergent chunks.

After disconnect, resync can use retained ordered log or provably versioned dirty extents; otherwise do a full copy. A target never becomes eligible merely because a socket connects or a counter appears numerically larger.

## 6. Planned live migration

The migration coordinator must prove that Cloud Hypervisor can stop new guest writes, drain outstanding I/O, preserve all disk/device state and delay destination execution until Volvisor gives permission.

Sequence:
```text
PREPARED (destination replica and readonly endpoint)
 -> PRECOPY (source writer; replica catch-up)
 -> QUIESCED (VM guest I/O and host frontend drained)
 -> BARRIER_DURABLE (source B; target exact-prefix durable >= B)
 -> SOURCE_REVOKED (old epoch cannot admit writes)
 -> DESTINATION_AUTHORIZED (strictly newer epoch)
 -> VM_RESUMED
 -> COMPLETE
```

This is the **canonical** migration state vocabulary. SPEC-0002 section 7 and the Volume API v2 `ObserveHandoff` use these canonical states; the API additionally exposes the terminal `IN_DOUBT` and `ABORTED` outcomes. `IN_DOUBT` is a legitimate fail-closed state reachable after `SOURCE_REVOKED` and before `DESTINATION_AUTHORIZED`; it must never be reported as a generic `ABORTED`. ADR-0004 Decision 3 phase names map onto these states as follows:

| ADR-0004 phase | Canonical state |
|---|---|
| PREPARE | `PREPARED` |
| PRECOPY | `PRECOPY` |
| CONVERGE | `PRECOPY` (rate-control policy inside pre-copy) |
| QUIESCE | `QUIESCED` |
| BARRIER | `BARRIER_DURABLE` |
| FENCE/COMMIT | `SOURCE_REVOKED` then `DESTINATION_AUTHORIZED` |
| RESUME | `VM_RESUMED`, then `COMPLETE` once data and VMM completion are both proven |

Implementations must not collapse `SOURCE_REVOKED -> DESTINATION_AUTHORIZED` into a single atomic status: the gap between them is exactly where `IN_DOUBT` is observable.

The `SOURCE_REVOKED -> DESTINATION_AUTHORIZED` transaction needs authoritative durable linearization. If split across actions, `IN_DOUBT` is a legitimate fail-closed state. Timeout after source revocation does **not** permit automatic source resume.

A migration with multiple writable nearline volumes must coordinate a **single VM I/O cut** and prove all target barriers and fencing actions before destination resume. If atomic multi-volume authorization is unavailable, the VM migration feature must remain disabled.

Rate control: isolate three streams—foreground guest writes, replica catch-up, and VMM memory copying (plus optional local mirror rebuild). Lowering all replication traffic is not a migration strategy. The coordinator must estimate dirty-rate/catch-up, bound downtime, prioritize convergence and abort before destructive cutover if deadlines cannot be met.

## 7. Failure behavior

| Failure point | Required action |
|---|---|
| Target seed fails before quiesce | cancel attempt; source remains writer |
| VMM precopy fails while source owns epoch | abort target; source continues if authority proved |
| Barrier not reached | restore source writes only with proven old authority |
| Source died with unreplicated ACKs | fence old writer; classify potential data loss; never call it migration success |
| Source revoked, target not yet granted | `IN_DOUBT`; resolve authoritative log, no blind rollback |
| Target writer committed then target VM fails | recover from committed epoch; no rollback to source epoch |
| Old source reconnects after promotion | stale epoch rejected at data path |
| Witness/quorum lost | use documented lease-fail-closed policy |
| Replica data checksum diverges | quarantine/differential repair from proven good copy, not timestamp arbitration |

## 8. Unplanned promotion

Promotion requires separate **writer fencing** and **data recovery** proofs.

`SAFE_CURRENT`: old authority irrevocably fenced **and** all acknowledged writes proved durably present at destination.

`POSSIBLE_LOSS`: old authority fenced but source ACK tail not fully proved on target. Must require explicit tenant/operator policy or recorded authorization and preserve RPO exposure evidence; current tail can be unknowable, so report unknown rather than pretend precise loss bounds.

`UNSAFE`: old authority may still write, replica lineage conflicts, or integrity unknown; reject promotion. Never use dual-primary, timestamps or last-writer-wins merge to escape split brain.

A successful planned migration is lossless **only for source writes accepted before its barrier** and only after documented final cutover. It makes no claim about arbitrary unplanned failover.

## 9. Read visibility and host-local endpoint

The serving workload host consumes the active local replica via an isolated frontend mapped to a stable canonical `VolumeId`. Inactive endpoints must be read-only or closed. When source placement changes, destination endpoint activation must be tied to new writer epoch, not mere control-plane creation. Endpoints must not be tenant-network reachable.

## 10. Required evidence

- Continuous write-trace oracle with acknowledged-write verification across planned migration; same under power loss to quantify tail.
- SIGKILL/kill -9 during ACK, WAL/meta commit, dirty bitmap, local mirror leg repair, peer stream and replay.
- Separate SSD loss, Storage Cell crash, host power cut, storage network disconnect, VMM crash, O3K control-plane disconnect and quorum loss.
- Concurrent multi-disk VM final cut, source-after-fence stale writes, wrong epoch data injection, target-after-commit failure and `IN_DOUBT` deterministic recovery.
- Saturated disk/network, multi-TB seed, growing dirty rate, repeated migration aborts, resync while foreground continues and mirror rebuild.
- Benchmark DRBD Protocol A (where its actual ACK/cache semantics match), qualified Mayastor and Ceph reference with comparable VMM/frontend topology; record semantic mismatches.
- Exact source commit/version, independent fault harness and full logs. No claim of production support based on simulation or successful happy path alone.
