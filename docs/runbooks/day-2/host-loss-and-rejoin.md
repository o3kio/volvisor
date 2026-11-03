# Total loss of one host and re-admission — day-2 runbook

**Status: NOT RUN — REQUIRES a real multi-host lab, a third failure domain and
BMC/fencing.** No command in this page was executed for evidence. There is no
raw record, no timestamp, no observed output, and no verdict. **Do not treat
this as a tested procedure.**

Why it could not run here:

- one physical host; every process on `127.0.0.1`;
- no second host, no independent failure domain, no witness host;
- no DRBD 9 kernel module or peer (`drbdadm` absent, no `/dev/drbd*`);
- no Ceph cluster (`rbd`/`ceph` absent);
- no BMC/power control, so a "host loss" would be a graceful process stop;
- no independent observer/oracle host for a write-trace verdict.

The nearest executable evidence is
[D3 (witness service only)](lost-witness.md) and the
[nearline multi-host campaign](../nearline-multihost-campaign.md). D3 does not
prove storage-side fencing and does not survive a host loss.

## Scope and non-goals

This is the procedure for **unplanned loss of one data host** in a
nearline-replicated deployment, followed by **re-admission** of a rebuilt
host. It is a procedure template, not a result.

It does not cover:

- native-local volumes: with no remote replica, a total host loss is a
  restore-from-backup event; the surviving control-plane state cannot
  recreate the data;
- ceph-rbd: Ceph owns the data durability; host loss/re-admission is a Ceph
  cluster procedure, not this DRBD-shaped one;
- a graceful reboot, a daemon restart, or a planned migration;
- split-brain arbitration by timestamp or forced dual-primary.

For the contract and topology, read
[ADR-0007](../../adr/0007-drbd9-nearline-replication-provider.md),
[nearline replication contract v2../../../contracts/nearline-replication-v2.md),
and [N1/N2/N3/N10 in the nearline campaign](../nearline-multihost-campaign.md).

## Required topology

| Role | What it must be |
|---|---|
| `HOST_A` | data replica 1 / failed host; independent power and network |
| `HOST_B` | data replica 2 / survivor; independent power and network |
| `WITNESS` | third (or fourth) failure domain; authority only, no tenant data |
| `HOST_C` | independent driver/oracle/evidence host; not a data replica |
| `BMC_A` | power control for A (or a documented equivalent) |
| `VM_A`, `VM_B` | real guests or a real write workload, with an oracle |

A witness colocated with A or B is not a valid equivalent. A two-host run
without a third failure domain is a documented deviation with a recorded risk
and accepting operator, not a silent shortcut.

## RPO / RTO

**Unknown and not claimed.** Nearline replication is asynchronous; an
unplanned loss can lose the tail that had not reached B. The run must measure
the last acknowledged write and the missing sequence range at B; without a
recorded barrier the honest classification is `POSSIBLE_LOSS`, not zero-RPO.
RTO is whatever the run measures from the fault to a verified serving B plus
a resynced/ rejoined A; no number is available today.

## Preconditions

API examples use owner-only curl configs for the witness read token and each
host token. Do not put a bearer token on the command line.

```sh
umask 077
printf 'header = "Authorization: Bearer %s"\n' "$WITNESS_READ_TOKEN" > ~/.volvisor-witness-read.curl
printf 'header = "Authorization: Bearer %s"\n' "$HOST_A_TOKEN" > ~/.volvisor-host-a.curl
printf 'header = "Authorization: Bearer %s"\n' "$HOST_B_TOKEN" > ~/.volvisor-host-b.curl
chmod 600 ~/.volvisor-witness-read.curl ~/.volvisor-host-a.curl ~/.volvisor-host-b.curl
```

- [ ] Three physical hosts in distinct failure domains, plus a witness on a
      third/fourth domain and an independent observer.
- [ ] A real DRBD 9 module/peer (or a real Ceph cluster) and a
      volvisor-provisioned nearline volume with a known lineage.
- [ ] Witness configured with `lease_ttl_secs`, `lease_grace_secs`,
      `suspend_budget_secs`, per-host `witness_host_token`s, and a witness on
      an independent failure domain. A loopback/colocated witness must be
      refused by validation; do not weaken it.
- [ ] BMC/power control for A, and authority to power it off. Record the exact
      mechanism and UTC time.
- [ ] A continuous write workload plus an independent oracle that records the
      acknowledged sequence prefix and can verify blocks at B.
- [ ] Named operator authority for `allow_loss`/forced revocation and for the
      re-admission decision. Record the identity and reason.
- [ ] A recent, verified backup of A's control-plane state, even though it is
      not the durability mechanism.

## Procedure (template — unexecuted)

### Phase 0 — Baseline and write trace

1. Record versions, binary digests, topology and failure-domain labels on
   every host.
2. Start the oracle/workload; record the first acknowledged sequence and UTC
   time.
3. Record the baseline authority and replication state:

```sh
# on the observer, against the witness
curl -fsS --config ~/.volvisor-witness-read.curl \
  "$WITNESS/v1/volumes/$VOL" | tee baseline-witness.json

# on B (and A before the fault)
drbdadm status "$RESOURCE" | tee baseline-drbd-status.txt
drbdadm show-gi "$RESOURCE" | tee baseline-drbd-gi.txt
```

4. Record the last acknowledged write and the wall-clock fault boundary.
   **Required evidence, never observed here.**

### Phase 1 — Lose A for real and fence it

1. Power off A via BMC (preferred), or use the documented equivalent. Record
   the mechanism and UTC time. Do not use `systemctl stop` and call it a host
   loss.
2. Confirm A is not writing: BMC power state, network unreachable, no guest
   process, no DRBD connection from A.
3. Inspect the witness and retire A's epoch. A self-release is impossible if A
   is dead; a forced revocation needs the operator authorization (and, if
   claimed, positive power-off evidence):

```sh
# Read the current authority first.
curl -fsS --config ~/.volvisor-witness-read.curl \
  "$WITNESS/v1/volumes/$VOL" | tee pre-fence-witness.json

# Forced revoke shape (requires authority; NOT DRILLED; validate fields):
curl -fsS --config ~/.volvisor-host-b.curl -H 'content-type: application/json' \
  -d '{"protocol_version":2,"operation_id":"op-fence-a","host_id":"host-b",
       "epoch":'"$OLD_EPOCH"',
       "authorization":{"operator":"'"$OPERATOR"'","reason":"host A powered off at ..."},
       "power_off":{"evidence":"BMC power state off at ..."}}' \
  "$WITNESS/v1/volumes/$VOL/revoke" | tee fence-a.json
```

4. Do not grant B until the old epoch cannot admit writes and the fence
   window/authority rules are satisfied. If B promotes while A may still write,
   **stop the run**: that is a fencing failure, not a scenario to work around.

   The forced-revoke body above is a **shape only, not an executed request**.
   Validate the requesting `host_id`, the target `epoch`, and the
   `authorization`/`power_off` fields against the running witness version and
   the contract before using it. A forced revoke without positive power-off
   evidence is a trust decision with a documented dual-write risk.

### Phase 2 — Promote the survivor and measure the tail

1. Grant B a strictly newer epoch and record the fencing proof:

```sh
curl -fsS --config ~/.volvisor-host-b.curl -H 'content-type: application/json' \
  -d '{"protocol_version":2,"operation_id":"op-grant-b","host_id":"host-b"}' \
  "$WITNESS/v1/volumes/$VOL/grant" | tee grant-b.json
```

2. Adopt the nearline volume on B only with the recorded authority and the
   contract's classification. The campaign's shape is:

```sh
cat > adopt.json <<EOF
{"api_version":"volvisor.volume.v2","operation_id":"op-adopt-$RUN_ID","allow_loss":true}
EOF
curl -fsS --config ~/.volvisor-host-b.curl -H 'content-type: application/json' \
  --data-binary @adopt.json \
  "http://$HOST_B:8080/v2/admin/nearline/$VOL/adopt" | tee adopt.json
```

3. Record the classification. Without a recorded last-acknowledged barrier,
   the honest result is `POSSIBLE_LOSS` with an unknown boundary;
   `SAFE_CURRENT` requires the contract's evidence. Do not accept a
   protocol-only claim.
4. Run the oracle at B and compute the missing acknowledged sequence range.
   **The measured tail is the RPO result.** Do not report zero because B looks
   healthy.
5. Record B's post-promotion `drbdadm status`, inspect output, guest I/O
   verification, witness inspect and logs.

### Phase 3 — Re-admit a rebuilt A

1. Rebuild A from a trusted image. Do not restore A's old control-plane state
   over B's newer state. If A's old state is used at all, it must be treated
   as stale and reconciled; the surviving authority is B.
2. Give A a distinct/validated host identity. Do not blindly reuse an identity
   that could collide with a stale witness/DRBD record; reconcile the
   HostId/lineage per the contract.
3. Join A as a **secondary** with the same resource lineage and matching
   configuration. Do not make it primary and do not let it write.

```sh
drbdadm status "$RESOURCE"
drbdadm show-gi "$RESOURCE"
```

4. Record whether A is `Inconsistent`/`Outdated`/`DUnknown`, the resync
   baseline and progress. Let resync complete under foreground load. Do not
   declare A current because the process is up.
5. Verify the oracle: every acknowledged post-fence write is present at B and
   A, and the resync did not introduce foreign/divergent data. If A cannot
   rejoin without a full wipe, record that as a result and file an issue; do
   not erase the resource to make the run continue.
6. Only then allow A to serve again, under a new authority handover if
   required.

## Expected output (required evidence, not observed)

No output below was produced by this repository. A real run must capture:

- exact UTC start/end and the fault mechanism for A;
- `drbdadm status` / `show-gi` / resource definitions on A and B before and
  after;
- witness `inspect` before the fault, after the fence, after B's grant, and
  after re-admission, with epoch/lease/holder/commit/retirements;
- the adopt response and its loss classification;
- oracle acknowledged prefix at the fault, verified-at-B prefix, missing
  sequence ranges and block verification results;
- the measured RPO tail and measured RTO;
- resync baseline/progress/duration and the final A/B consistency;
- every failure filed as an issue.

A shape such as `"classification":"POSSIBLE_LOSS"` with a measured missing
range would be a valid result; `"SAFE_CURRENT"` without the contract's barrier
evidence would not.

## Pass condition

- A was lost unplanned and is provably not writing.
- The old epoch is retired with a recorded fencing proof; B is granted a
  strictly newer epoch after the fence requirements.
- B's promotion classification is honest, and the measured acknowledged tail
  is recorded (zero only if the oracle proves it).
- No dual-primary or stale-source write is observed.
- A rejoins as a secondary with matching lineage, resync completes or is
  honestly recorded, and the oracle verifies the data.
- Every command, timestamp, output and failure is in an evidence bundle at one
  exact SHA.

## What would change the verdict

A real run on the topology above, with raw records and a measured tail, could
turn this page into **DRILLED** (or **FAIL** with a filed issue). Until then it
stays **NOT RUN — REQUIRES** the listed hardware/authority. A single-host
`SIGKILL`, a witness restart, or a simulation is not a substitute.

## References

- [Nearline multi-host campaign (N1/N2/N3/N10)](../nearline-multihost-campaign.md)
- [Nearline replication contract v2../../../contracts/nearline-replication-v2.md)
- [ADR-0007 — DRBD 9 nearline provider](../../adr/0007-drbd9-nearline-replication-provider.md)
- [Evidence standards](../../evidence/README.md)
- [Lost-witness runbook](lost-witness.md) — the service half that *was* drilled
- [Issue #37](https://github.com/o3kio/volvisor/issues/37) — the multi-host
  campaign prerequisite
