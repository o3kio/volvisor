# Volvisor storage-class semantics v1

Status: Proposed  
Version: 1

This contract defines what the three v0 storage classes mean to O3K/CellHV
callers. Performance numbers are not part of the class identity; failure and
acknowledgement semantics are.

## 1. Summary

| Property | local-direct | replicated-async | cluster-durable |
|---|---|---|---|
| Primary data path | workload -> physical NVMe | host-local endpoint -> local NVMe | Ceph |
| Volvisor foreground proxy | no | yes/provider path | Ceph/provider path |
| Steady-state remote ACK required | no | no | according to Ceph policy |
| Volvisor replication | none | asynchronous | none; Ceph owns it |
| Host-loss RPO | entire local volume possible | non-zero tail possible | according to Ceph durability |
| Planned migration v0 | unsupported while attached | supported only with sync barrier | supported subject to CHV/Ceph integration |
| Multi-writer | no | no | only if chosen Ceph/frontend semantics allow it |
| Typical AI use | KV/scratch/hot cache | model/warm cache/intermediate checkpoint | persistent disks/critical data |

## 2. local-direct

### 2.1 Meaning

`local-direct` means the workload receives exclusive direct access to a
PCI-addressable NVMe controller/function assigned through VFIO. Ordinary
namespaces sharing one controller are not separate VFIO isolation boundaries.

It does **not** mean:

- a fast virtual disk;
- a cached Ceph volume;
- an NVMe-oF namespace;
- a Storage Cell proxied device.

### 2.2 Failure semantics

Loss of the device or host may lose all data on the allocation.

Volvisor provides no replica.

The caller must classify data as reconstructable or accept the device-local
failure domain.

### 2.3 Migration

A VM with an attached local-direct device is storage-pinned in v0.

The scheduler/migration API must reject transparent cross-host migration unless
the direct device has first been detached under an explicit workflow.

## 3. replicated-async

### 3.1 Meaning

`replicated-async` means foreground writes are durably committed on the
current local replica before acknowledgement and are then copied to peer
replicas outside the steady-state foreground ACK path.

### 3.2 RPO

Unplanned loss of the current host can lose an acknowledged tail that had not
yet become durable on another replica.

The implementation must not expose RPO=0 for this class.

Replication lag must be visible.

### 3.3 Planned migration

A planned migration can be lossless only if the replicated-async migration
barrier succeeds.

The target must be durable through the source barrier sequence before writer
authority transfers.

A migration that cannot prove this must abort.

### 3.4 Failover

Failover is not equivalent to migration.

If the best surviving replica is known stale, promotion may imply data loss.
The API must expose this before promotion.

### 3.5 Availability

A volume may continue local I/O while a replica is unavailable if policy allows
degraded operation, but the volume must report degraded state and growing
replication exposure.

## 4. cluster-durable

### 4.1 Meaning

`cluster-durable` is backed by the configured Ceph durable provider.

The name describes a failure contract, not an expectation that it is always
slow.

### 4.2 Durability

Acknowledgement/durability follows the selected Ceph/RBD policy and healthy
cluster state.

Volvisor must surface Ceph degradation honestly and must not substitute weaker
replicated-async behavior under the same class name.

### 4.3 Migration

A compute host is not the data owner. Cross-host VM migration is therefore
compatible with the class in principle, subject to the exact Cloud Hypervisor
storage frontend and migration integration.

## 5. Naming rule

External APIs should prefer semantic class names:

- `local-direct`
- `replicated-async`
- `cluster-durable`

Aliases such as "fast", "premium" or "slow" may exist in UI/product policy but
must resolve to a semantic class and must not replace the contract name.

## 6. No automatic semantic downgrade

Volvisor must never silently change:

- cluster-durable -> replicated-async;
- replicated-async -> local-direct;
- any protected class -> an unreplicated class.

A requested degraded mode requires an explicit policy transition visible to the
caller.
