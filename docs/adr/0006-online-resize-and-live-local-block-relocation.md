# ADR-0006 — Online block-volume growth and live backing-store relocation

Status: Accepted (first slice: grow-notification + same-VG pvmove evacuation); the QSD mirror/pivot path (Option B) remains R&D-gated
Decision-accepted: 2026-10-10 (PR #18, per the readiness plan D4)
Date: 2026-10-09
Related: [ADR-0003](0003-tiered-volume-virtualization.md), [SPEC-0002](../specs/SPEC-0002-volvisor-volume-virtualization.md), [Volume API v2](../../contracts/volume-api-v2.md)

## Context and precise distinction

Volvisor should match Proxmox's two useful VM disk behaviors without assuming QEMU must run the guest:
1. **Online grow**: enlarge the same virtual disk capacity without copying data.
2. **Online storage relocation**: copy a running virtual disk to another host-local backing device/pool, capture concurrent writes and switch the backing store without changing the VM's guest-visible device identity.

These are **not the same as cross-host live VM migration**. A VM can remain pinned to host A while its disk moves from NVMe A1 to A2. The v2 `native-local` VM migration restriction remains valid; online same-host disk relocation is a separate, potentially supported capability.

QEMU provides `block_resize` and live block `blockdev-mirror`/job completion. The **QEMU Storage Daemon (QSD)** is a standalone process with QMP block graph/jobs and `vhost-user-blk` exports, not the QEMU guest VMM. Cloud Hypervisor supports `vhost-user-blk`; consequently QSD is a serious candidate for a *separate* optional advanced-disk attachment provider, but end-to-end live pivots, flush, resize notifications, reboot/reconnect, live VM migration and lock ownership must be demonstrated on pinned releases rather than assumed.

References:
- https://www.qemu.org/docs/master/interop/live-block-operations
- https://www.qemu.org/docs/master/tools/qemu-storage-daemon.html
- https://www.qemu.org/docs/master/interop/qemu-storage-daemon-qmp-ref.html
- https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/device_model.md
- https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/api.md
- https://github.com/cloud-hypervisor/cloud-hypervisor/pull/7948
- https://man7.org/linux/man-pages/man8/pvmove.8.html

## Decision: do not force all native volumes through an extra daemon

Default `native-local` foreground I/O should remain:

```text
VM virtio-blk -> Cloud Hypervisor native block frontend -> /dev/mapper/volvisor-V -> LVM/dm -> local NVMe
```

A dm/LVM kernel mapping **is** a block translation layer, but avoids a mandatory separate userspace block server. It supports online growth and can support certain same-volume-group physical-extent moves. An online resize is provisioner growth + VMM capacity notification + guest partition/filesystem growth, not a migration.

Optional advanced I/O path:

```text
VM virtio-blk -> Cloud Hypervisor vhost-user-blk -> QEMU Storage Daemon (QMP)
                                                 -> QEMU block graph
                                                    |           |
                                             Source LV      Target LV
```

QSD owns the **exclusive active block-device writer access** and the block mirror job. Cloud Hypervisor must not simultaneously open the source LV natively. QMP job `READY` then verified `job-complete` may redirect the export-backed I/O path to the destination while the VM continues. The route must preserve the same guest-visible device and single-writer attachment identity.

QSD brings extra process, packaging, failure domain and potential latency/CPU costs; it is **optional**, not the universal native datapath. Prove restart/reconciliation and guest write safety before production use. Treat QSD block jobs as transient *intra-host mobility* primitives, not a replacement for Volvisor's distributed nearline fencing and durable handoff protocol.

## Online capacity growth for native logical LV

1. Consumer requests `GrowVolume` with `expected_generation` and new size. Grow-only in v0, and ensure quota, reserve, layout and thin metadata headroom.
2. Volvisor journal intent and grows backing LV using LVM/dm (e.g. `lvextend -L +50G vg/vol` for a qualified thick LV; thin virtual-size changes use their specific LVM operations).
3. Verify actual mapped block size with `blockdev --getsize64`. Do not infer from command exit code alone.
4. Call Cloud Hypervisor `PUT /api/v1/vm.resize-disk` using its configured disk ID and actual desired bytes, *on a pinned version proven to support host block devices*.
5. Verify guest capacity change; guest partition and filesystem growth are independent and may need guest action.
6. Persist effective size and outcome, including partial case: backend grew but guest notification failed. Retry **notification**, never shrink the LV to 'undo' a successful expansion.

Cloud Hypervisor's native `/vm.resize-disk` API exists; its failure on externally grown host block devices was fixed by upstream PR #7948 (2026-04-17). The installed version and path must be verified. Resize of **vhost-user-blk** may require a backend-to-frontend virtio configuration-change notification and is **not** proven by the native block-device fix; test separately.

## Online same-host backing-store relocation

### Option A: LVM pvmove (simplest where it applies)

When source and destination PVs are in the **same VG**, `pvmove` can relocate allocated extents while the LV remains in use. The LV's dm identity stays stable and Cloud Hypervisor does not have to pivot to a new path. It supports crash restart via LVM checkpoints. Validate supported LV/segment types and whether an LVM-thin operation relocates **the pool** rather than one individual thin volume: do not promise single-thin-LV pvmove isolation.

This is a great candidate for physical disk evacuation/replacement **within the same VG**, not a general arbitrary `VolumeId` migration across VGs, thin pools or Ceph.

### Option B: QSD live block mirror + pivot

For relocating an entire **logical volume** between compatible LVs/pools with the VM still on the **same host**:
1. Confirm QSD is the exclusive owner of the current backing block node, all frontend features supported and no competing storage operation.
2. Allocate target LV and require equal or larger virtual capacity with compatible flush, block-size, alignment, discard and encryption policy. Journal source/target identities and operation.
3. Connect new QSD target block node and run QMP `blockdev-mirror` (full copy), with monitored dirty/change tracking and bounded bandwidth.
4. Verify mirror ready and no I/O error. Arrange QMP job completion/pivot through the *same vhost-user-blk export*, including block graph replacement where required by QSD.
5. Validate target is current, that write/flush ordering and guest-visible identity are intact, and recover QSD/Volvisor authority after crashes.
6. Persist destination backend mapping; **only then** remove source under scoped authorization.

QSD command availability is **not sufficient** evidence that a particular exported block graph can pivot while attached. A proof-of-concept must show uninterrupted fio with read/write verification, lossless clean cutover and safe handling of power-loss at each stage.

### Option C: custom dm-mirror or new userspace mover

Do not write a generic block copier/mirror or manipulate dm tables during live guest writes just to avoid QSD. Correctly handling concurrent writes, dirty tracking, flush, discard and atomic cutover is a major correctness responsibility. Use this only if LVM pvmove and QSD are unsuitable and there is a clear performance/operational reason.

**Do not suggest dm-clone as a drop-in live-writable-source copy:** its documented source side is read-only.

## Capability matrix

| Operation | Native CHV + LV/dm | CHV + QSD vhost-user-blk | Nearline | Ceph RBD |
|---|---|---|---|---|
| Online grow capacity | supported mechanism; exact version/test gate | requires QSD/virtio notification proof | per-engine + frontend proof | backend RBD resize + guest notification proof |
| Move physical extents within one VG | LVM `pvmove` | may use LVM when safe; redundant QSD dependency | not an automatic replica migration | not applicable |
| Move whole live volume to a different local VG/pool | **not inherently supported** by native VMM | **candidate**, QSD mirror/pivot test required | replica relocation engine | Ceph images have separate facilities |
| Move VM to another compute host | no, not on local native backend | **no**, not conferred by a same-host pivot | only with distributed durable barrier + fencing | conditioned on Ceph/VMM integration |

## Safety contract for source deletion

A storage move is a *persisted ownership transition*, never just background `dd`. Source is not disposable until verified target authority is durable and the old backend cannot accept new foreground writes. Failures **before** pivot retain source as authoritative; failures **after** pivot must reconcile target state and avoid automatic source rollback. Unknown outcome: `IN_DOUBT`, no destructive cleanup.

Online operation acceptance suite: growing attached thick and thin LVs, host block-device CHV notification, guest re-read of partition geometry, same-VG extents move under I/O, QSD vhost-user-blk connection and export, 4K random writes during mirror, flush/FUA/discard, target out-of-space, source SSD failure, QSD SIGKILL at every phase, stale attachment fencing, restart and idempotency, load at p99/p999 and throughput against native device.

## First implementation slice (P6)

Per the [readiness plan](../plans/2026-10-10-post-p5-readiness-questions.md)
decision D4, exactly one slice of this ADR is accepted for
implementation; everything else keeps its existing gate. The scope of
the acceptance (points 1–2 are the accepted work; points 3–4 record
what remains closed):

1. **Grow-notification (Option "online grow", step 4 above).**
   `GrowVolume` on an attached `native-local` volume completes the VMM
   capacity-notification step through the existing
   `ChRemoteVmm` adapter (`PUT /api/v1/vm.resize-disk`) on a **pinned,
   startup-verified Cloud Hypervisor version** — upstream PR #7948 is
   required for externally grown host block devices, so a version that
   is not proven at startup refuses the grow typed instead of growing
   without notification. Resize of **vhost-user-blk** frontends remains
   unproven and out of scope. Partial-failure semantics are the ADR's
   existing rule: the backend may grow before the VMM/guest is
   notified; the provider **retries the notification, never shrinks
   the LV to undo** a successful expansion
   (`guest_notification_status: retry_required`).
2. **Same-VG extent evacuation (Option A).** Implemented over the
   **already-contracted** surface — [Volume API
   v2](../../contracts/volume-api-v2.md) §4A
   `MoveVolumeBackingOnline` with the `same_vg_extent_move`
   capability — not a new operation name. Journal-before-mutate;
   source extents are freed only after verified relocation and
   ownership reconciliation; the contract's never-generic-`FAILED` /
   `IN_DOUBT` rule applies verbatim.
3. **`same_host_live_backing_move` (Option B, QSD mirror/pivot) is
   advertised nowhere** until its acceptance suite passes (the
   uninterrupted-fio/power-loss/restart suite above). This is the
   ADR's existing gate, unchanged by this acceptance.
4. **Option C (custom mover) stays rejected.** Accepting the first
   slice does not reopen it.

Out of the slice's scope by construction: cross-VG, cross-pool and
cross-class moves (typed refusals, see the volume contract's
`MOVE_UNSUPPORTED_SCOPE`), thin-pool-wide relocations presented as
single-thin-LV moves, and any claim that same-VG evacuation confers
cross-pool mobility.

## Conclusion

The right abstraction is **an optional movable block backend**, not a mandatory middleware process for every native LV. Implement cheap native LV online growth and qualifying same-VG LVM evacuation first. Evaluate QSD + vhost-user-blk for general live per-volume pool-to-pool moves next. Keep cross-host VM live migration and nearline replication as distinctly stronger contracts.
