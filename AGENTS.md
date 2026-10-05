# AGENTS.md

Guidance for code agents working in Volvisor.

## Read first

Normative documents:

1. docs/adr/0001-volvisor-storage-cell-architecture.md
2. docs/adr/0002-replicated-async-local-endpoint-and-migration.md
3. docs/specs/SPEC-0001-storage-cell-v0.md
4. contracts/volvisor-provider-v1.md
5. contracts/storage-class-semantics-v1.md
6. contracts/replicated-async-v1.md

The R&D document does not override a contract.

## Hard rules

1. Never weaken ownership or fencing to make a test pass.
2. Never identify a disk only by /dev/nvmeXnY or PCI BDF.
3. Discovery is read-only. It never implies permission to format or wipe.
4. Foreign or ambiguous storage state fails closed.
5. local-direct has no Storage Cell foreground data path.
6. replicated-async steady-state is not RPO=0.
7. A successful planned replicated-async migration must pass the durable target
   barrier and single-writer transfer.
8. cluster-durable is Ceph-backed; do not implement a new Ceph replacement
   under that class.
9. A device never has two owners.
10. Do not copy or translate third-party storage source code without explicit
    provenance and license review. Public designs may be used as references for
    a clean implementation.

## Evidence honesty

A benchmark is not a durability proof.

A successful happy-path migration is not a fencing proof.

No class is called production-supported until its exact implementation passes
the failure and evidence requirements in SPEC-0001 and the relevant contract.
