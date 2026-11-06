# ADR-0015 — The provider state lock never spans an external command on a request path

Status: Proposed (decision recorded; enforcement via the default scenario
catalog pending acceptance)
Decision-accepted: pending (record acceptance date and accepting authority here)
Date: 2026-11-06
Related: [ADR-0012](0012-monotonic-attachment-generation-fence.md),
[ADR-0013](0013-durable-attachment-generation-allocator.md),
[ADR-0014](0014-journal-bounds-and-exhaustion-contract.md),
[SPEC-0002](../specs/SPEC-0002-volvisor-volume-virtualization.md),
[Phase F plan](../plans/2026-11-06-phase-f-write-path-remainder.md),
[Phase F evidence](../benchmarks/2026-11-06-devhost-phase-f-write-path-remainder/REPORT.md),
issue [#62](https://github.com/o3kio/volvisor/issues/62)

## Context

Every LVM provider mutation is serialized by one in-process state mutex. The
mutex protects the in-memory `LvmState`, the allocation epoch, the structural
drive registry and the durable save. It is correct to hold it while deciding,
reserving and persisting. It is not correct to hold it while waiting for an
external command (`lvcreate`, `lvremove`, `lvs`, `vgs`, `pvmove`) to return.

Phase E moved the create, delete and grow calls out of the guarded
regions. Phase F added the deterministic gate tests for those paths and for
the move drive, whose request/supervision path was already narrow. The write
path follows a durable-intent pattern:

1. decide and validate under the lock;
2. persist the intent durably under the lock (journal-before-mutate);
3. release the lock;
4. run the external command;
5. reacquire the lock, revalidate the world against the intent, then commit
   or compensate.

The pattern is easy to regress. Nothing about `let mut state =
self.lock_state_stable()?;` prevents a later edit from calling `lvs` inside
the guard, and the resulting stall is invisible in ordinary tests: the
mutation still succeeds, only every other request waits. Phase F added
deterministic gate tests that park a CLI call and assert an unrelated volume
mutation completes, but until now the invariant was implicit in code review.

## Decision

**On any request path, a `tokio` worker must not hold the provider state
mutex across an external command. The durable intent, not the lock, is the
cross-command fence.**

- The lock may span: request validation, replay/admission decisions,
  capacity reservation, allocation-epoch reads, intent construction,
  durable save, in-memory commit, and compensation classification.
- The lock must not span: `lvcreate`, `lvremove`, `blkdiscard`, `lvs`,
  `vgs`, `pvs`, `pvmove`, `lvextend`, or any other external process wait.
- Before releasing the lock, the provider persists enough durable state for
  recovery to finish or refuse the operation. A command is never authorized
  by an in-memory-only decision.
- After reacquiring the lock, the provider revalidates the facts its
  decision depended on — volume generation, lifecycle, matching intent,
  allocation epoch, or a concurrent supervisor outcome, as applicable. A
  mismatch is refused typed; it never authorizes a destructive command under
  stale authority.
- Capacity samples are taken outside the lock under the allocation-epoch
  protocol: read the epoch, sample, reacquire, and retry while the epoch
  moved. Retries are bounded (`CREATE_ADMISSION_ATTEMPTS`); exhaustion is a
  typed `OVERLOADED` refusal, never allocation against a stale sample.
- A failed final save is classified against the durable file while the lock
  is still held, so in-memory rollback is atomic with the classification.
  This failure-only re-read is bounded and is an accepted exception; it is
  not a general licence to do I/O under the lock.
- Startup recovery may run destructive recovery commands under the startup
  lock because startup is single-threaded and has no concurrent request to
  stall. This is an accepted exception, not a request-path pattern.
- Synchronous CLI calls that remain inline on an async worker (the move
  drive) are a worker-handoff residual. They must not additionally hold the
  state lock, and the residual is tracked separately.

## Required evidence

The default scenario catalog (`docs/testing/default-integration-scenarios.md`)
contains the lock-scope scenarios:

- move's `lvs` observation and `pvmove` start;
- delete's `lvs` existence check and `lvremove`;
- create's `vgs` sample, `lvcreate`, and failed-create compensating
  `lvremove`;
- delete's revalidation refusal when the volume changed during the outside
  check;
- create's allocation-epoch re-sample.

Each parks the external call and asserts an unrelated volume's mutation
completes. A new external call inside the guard must add a matching scenario
or the change fails review.

## Consequences

- Concurrency is bounded by the commands themselves, not by unrelated
  volumes' commands.
- Fail-closed correctness depends on the durable intent plus revalidation;
  the lock is not the inter-command fence. Any new path must design its
  intent and revalidation, not just drop the guard.
- A bug in revalidation is a correctness bug, not a performance trade-off.
  The generation and lifecycle rechecks are mandatory.
- This is a control-plane concurrency invariant. It is not a throughput,
  latency, capacity, SLA or production-support claim, and the Phase F
  single-host measurement does not attribute any sweep or soak delta to it.

## Alternatives rejected

1. **Hold the lock across CLI calls for simplicity.** Rejected: one slow
   `lvcreate` stalls every volume, and the Phase E/F measurement exists to
   remove that coupling.
2. **Replace the mutex with per-volume locks.** A larger redesign; the
   allocator, epoch and durable save are global. Not required to obtain the
   invariant, and it would complicate fail-closed reasoning.
3. **Rely on code review without executable evidence.** Rejected: the
   regression is silent in functional tests, so a parked-CLI scenario is
   required.
4. **Move the failure-only durable re-read outside the lock.** Rejected: it
   opens a window between classification and in-memory restore where a
   concurrent supervisor can complete the operation.
