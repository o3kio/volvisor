# Volvisor replicated-async contract v1

Status: Proposed  
Version: 1  
Implementation engine: deliberately unresolved / R&D-gated

Related:

- [ADR-0002](../docs/adr/0002-replicated-async-local-endpoint-and-migration.md)
- [Replicated-async R&D](../docs/research/replicated-async-rnd.md)

This contract fixes correctness semantics independently of whether the final
engine uses DRBD, SPDK, kernel NVMe-oF, io_uring or another implementation.

## 1. Required volume state

Each volume has authoritative state equivalent to:

```text
volume_id
volume_generation
writer_generation
primary_cell_id
local_committed_seq
replicas[] {
    replica_id
    cell_id
    received_seq
    durable_seq
    health
}
```

The exact representation may differ but it must support equivalent proofs.

## 2. Single-writer invariant

At most one writer generation is active.

A write from a stale writer generation must be rejected before it can make the
volume diverge.

Network partitions must not create two writable primaries.

If writer authority is ambiguous, writes fail closed.

## 3. Write acknowledgement

A normal foreground write may be acknowledged after:

1. current writer authority is validated;
2. data and ordering metadata required by the local durability contract are
   durably committed on the primary;
3. a monotonically ordered replication position has been assigned.

The implementation does not have to wait for remote persistent storage before
the normal ACK.

If the implementation chooses a stronger ACK for a period, that does not
change the class name.

## 4. Flush/FUA

Guest flush and FUA semantics must not be discarded.

A completed flush establishes a local durable ordering boundary.

A migration barrier additionally requires the selected destination to have
durably applied every write through the barrier sequence.

## 5. Replica progress

`received_seq` and `durable_seq` are distinct unless the implementation can
prove they are equivalent.

Receiving bytes into remote volatile memory does not permit the system to claim
remote durable completion.

Progress must survive the process failures needed to support correct replay, or
the replica must conservatively roll back its advertised progress after
restart.

## 6. Resynchronization

A disconnected replica is marked stale.

On reconnect, the engine must either:

- replay a retained ordered log;
- transfer tracked dirty ranges/blocks;
- or perform a conservative full copy.

It must not infer equality from equal volume size, timestamps or connection
success.

The replica becomes current only after an explicit convergence proof.

## 7. Planned migration barrier

Given source S and destination D:

1. S remains the sole writer during preparation.
2. D must have a compatible replica.
3. Replication continues while VM memory pre-copy proceeds.
4. At final handoff, VM/block writes are quiesced.
5. S flushes all accepted writes.
6. S records barrier sequence `B = local_committed_seq`.
7. D must prove `durable_seq >= B`.
8. S writer generation is fenced/revoked.
9. A strictly newer writer generation is granted to D.
10. D's local endpoint is activated for that writer generation.
11. Only then may the VM resume at D.

The ordering of steps 8–10 may be implemented through one atomic fenced
transition, but no interval may permit both sides to accept writes.

## 8. Migration abort

If any barrier/fencing step fails:

- D must not become writer;
- S remains or is restored as the only writer when this can be proven;
- the VM remains/resumes on S;
- the failure is observable;
- no success event is emitted.

A timeout is a failed migration, not permission to skip the barrier.

## 9. Unplanned primary loss

A surviving replica may have `durable_seq < last acknowledged source seq`.

The system must classify promotion:

- `safe`: target proven to contain the authoritative committed boundary;
- `possible-loss`: target is the best known copy but acknowledged tail may
  be absent;
- `unsafe`: writer fencing or ordering cannot be established.

`unsafe` promotion is forbidden.

`possible-loss` promotion requires an explicit policy/administrative action.

## 10. Local endpoint invariant

Each host presents an attachment for a volume through a host-local path.

The exact transport is not normative.

The endpoint identity must be stable enough that O3K/CellHV can prepare the
same `VolumeId` on a migration target without embedding the old primary's
network address in the workload contract.

A host-local endpoint must not bypass writer fencing.

## 11. Replica placement

A replica that is intended to protect against host failure must reside on a
different failure domain from the primary.

Two NVMe devices in the same host do not count as host-failure replication.

Placement policy must expose the actual failure domain.

## 12. Checksums and corruption

Replication must include end-to-end data integrity sufficient to detect
transport or storage corruption at the engine's block/chunk granularity.

A mismatching replica is not automatically authoritative because it is newer.

Corruption handling must fail closed or recover from a separately proven good
copy.

## 13. Security

Replication peers authenticate each other and authorize by volume/generation.

A peer may not request arbitrary raw device ranges outside its assigned volume.

Control messages that transfer writer authority require replay protection.

## 14. Evidence

No implementation satisfies this contract until real-host tests prove:

- stale writer rejection after partition;
- no dual writer;
- local process crash replay;
- source Storage Cell crash;
- source host power loss;
- peer loss/rejoin and delta resync;
- migration barrier success with continuous write verification;
- target failure during barrier;
- source recovery after aborted migration;
- exact classification of possible-loss failover;
- integrity detection;
- sustained performance under resync.

## 15. Implementation freedom

The engine may use DRBD, SPDK, kernel block/NVMe facilities or a clean Volvisor
implementation, but no engine-specific convenience may weaken the contract.
