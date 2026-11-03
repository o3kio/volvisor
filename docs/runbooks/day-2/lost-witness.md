# Lost or unreachable witness — day-2 runbook

**Status: PARTIAL.**

- **DRILLED:** witness service outage/unreachability, journal-backed recovery
  of registration/epoch/holder/lease/commit index, live-lease refusal,
  self-release, fence-window wait and a successor epoch. Evidence:
  [D3 raw log](../../evidence/day2/2026-11-03-devhost/raw/03-lost-witness.log).
- **NOT RUN — REQUIRES a DRBD 9 kernel module and a real peered resource on a
  second host** (`drbdadm` and `/dev/drbd*` are absent on this host). The
  storage-side question — does a writer actually freeze/self-fence after
  losing the witness, and what is the data-loss window? — is **not** proven.
  **Do not cite this page as a split-brain proof.**

Read [configuration.md](../../configuration.md) for the witness and DRBD fields,
and the [P4a witness/fencing plan](../../plans/2026-10-09-p4-witness-fencing-authority.md)
for the authority model.

## Scope and non-goals

The witness is the third-party writer-authority service. It grants epochs and
leases; it records fencing proofs and retirements. It does **not** carry tenant
data and it is not a data replica.

This page covers:

- detecting an unreachable witness;
- what to do (and not do) while it is unreachable;
- restarting the witness and verifying journal recovery;
- a planned, drained handover with a strictly newer epoch;
- operator-authorized forced revocation, at the procedure level (not drilled).

It does not prove DRBD data safety, split-brain prevention on the storage
path, or a real host-loss failover. Those are D8 and the nearline campaign.

## RPO / RTO

- **No new authority** can be minted while the witness is unreachable. An
  existing lease is time-bounded; it is not silently renewed.
- **Witness journal state:** no loss observed in D3; registration, epoch,
  holder, lease and commit index all survived a restart.
- **RTO** for the witness service itself: restart to health; D3 did not
  separately time it (the harness waits up to 30 s).
- **Storage-side RPO/RTO: unknown.** Not measured without DRBD.

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

- The witness's `state_dir` is on durable local storage on a failure domain
  independent of both data hosts. A loopback or colocated witness is refused
  by validation; physical placement is your assertion.
- You have the read/admin token (`auth_token`) for `inspect` and `healthz`,
  and each host has its own `witness_host_token` for mutations. On the v2
  protocol the shared `auth_token` is **read-only**; a mutation must resolve to
  the asserted `host_id`.
- You know `lease_ttl_secs`, `lease_grace_secs` and `suspend_budget_secs`.
- You have authority to stop/drain the writer before a planned handover, and
  named operator authority for a forced revocation. Record the operator and
  reason.

## A. Detect and handle an unreachable witness

1. Check health. A transport failure is the expected failure shape:

```sh
WITNESS=https://witness.example.internal:9101
curl -sS --max-time 3 "$WITNESS/healthz"
```

Expected when the service is down:

```text
curl: (7) Failed to connect to witness.example.internal port 9101 ...
```

Exit status 7, no JSON. D3 observed exactly this on loopback.

2. **Do not** try to force a new epoch by editing or deleting the witness
   state directory. **Do not** start a second witness on the same
   `state_dir`; it must refuse with a single-writer lock error. **Do not**
   treat a failed grant as permission to promote the other host.

3. Protect the data path. For a writer whose lease cannot be renewed, follow
   the storage-specific drain/fence procedure for your provider. That
   procedure is **not** in this drilled runbook. If you cannot fence the old
   writer, do not promote a new one.

4. Restore the witness service and verify journal recovery before minting any
   new authority:

```sh
systemctl start volvisor-witnessd.service
curl -fsS "$WITNESS/healthz"
curl -fsS --config ~/.volvisor-witness-read.curl \
  "$WITNESS/v1/volumes/$VOL"
```

Expected: `{"status":"ok"}` and the pre-outage authority, for example (D3):

```json
{
  "volume_id": "vol-d3-witness",
  "current_epoch": 1,
  "holder": "host-a",
  "lease_state": "live",
  "lease_id": 1,
  "commit_index": 2,
  "registration": {
    "endpoints": [
      {"host_id": "host-a", "backing": "vg-near/vol-d3-00000004"},
      {"host_id": "host-b", "backing": "vg-near/vol-d3-00000004"}
    ]
  }
}
```

Compare every field with the pre-outage inspect you recorded. A different
epoch, holder or commit index means the writer's view may already be stale:
stop and investigate before serving.

5. A competing grant while the old lease is live must be refused:

```sh
curl -sS -o /tmp/grant-b.json -w '%{http_code}\n' \
  --config ~/.volvisor-host-b.curl -H 'content-type: application/json' \
  -d '{"protocol_version":2,"operation_id":"op-grant-b","host_id":"host-b"}' \
  "$WITNESS/v1/volumes/$VOL/grant"
```

Expected: `409`, body code `LEASE_HELD`, message
`witness lease held; current_epoch=1`. Do not retry in a loop as a way to
force a promotion.

## B. Planned handover (drained writer)

1. Stop and drain the old writer. Do not revoke while it can still write.
2. Self-release its lease with the epoch it holds:

```sh
curl -fsS --config ~/.volvisor-host-a.curl -H 'content-type: application/json' \
  -d '{"protocol_version":2,"operation_id":"op-revoke-a","host_id":"host-a","epoch":1}' \
  "$WITNESS/v1/volumes/$VOL/revoke" | jq .
```

Expected (D3):

```json
{
  "fencing_proof": {
    "volume_id": "vol-d3-witness",
    "retired_epoch": 1,
    "commit_index": 3
  }
}
```

3. Wait at least `lease_grace_secs + suspend_budget_secs` before granting the
   successor. D3 used 2 s + 2 s and waited 5 s.

```sh
sleep $((LEASE_GRACE_SECS + SUSPEND_BUDGET_SECS))
```

4. Grant the successor and verify the epoch is strictly newer:

```sh
curl -fsS --config ~/.volvisor-host-b.curl -H 'content-type: application/json' \
  -d '{"protocol_version":2,"operation_id":"op-grant-b","host_id":"host-b"}' \
  "$WITNESS/v1/volumes/$VOL/grant" | jq .
```

Expected (D3):

```json
{
  "epoch": 2,
  "lease_id": 2,
  "lease_ttl_secs": 120,
  "fencing_proof": {
    "volume_id": "vol-d3-witness",
    "retired_epoch": 1,
    "commit_index": 4
  }
}
```

5. Inspect the final authority and confirm the retirement history:

```sh
curl -fsS --config ~/.volvisor-witness-read.curl \
  "$WITNESS/v1/volumes/$VOL" | jq '{current_epoch,holder,lease_state,lease_id,commit_index,retirements}'
```

Expected shape: holder `host-b`, `current_epoch=2`, `lease_id=2`, and a
retirement for epoch 1.

## C. Forced revocation and power-off attestation

`revoke` accepts an optional operator authorization for revoking a live lease
held by another host, and an optional power-off attestation. These are trust
decisions, not a convenience override:

- the authorization records `operator` and a non-empty `reason` verbatim;
- a power-off attestation needs positive evidence distinct from the
  authorization, and a false attestation re-opens the dual-write window in the
  alive-but-partitioned case;
- **neither forced revocation nor power-off attestation was drilled here.**
  There is no DRBD peer, no BMC and no real partition. Use them only with a
  storage-side fencing procedure that was actually tested.

If the witness state directory is lost (not just the service), there is no
automatic recovery in this runbook. Treat it as an authority-loss incident:
stop writers, restore the witness state from a verified backup on the same
failure domain, or rebuild the authority with the storage-side fencing review.
Do not hand-create epochs.

## Pass condition

- Health/traffic failure is a transport error, not a silent stale success.
- No new authority is minted while the witness is unreachable.
- On restart, registration/epoch/holder/lease/commit index match the
  pre-outage record.
- A live-lease grant is refused with `LEASE_HELD`.
- A drained handover gets a strictly newer epoch and a fencing proof that
  retires the old epoch.
- The storage-side writer is frozen before the successor serves. **This last
  item was not drilled; without it the pass condition is incomplete.**

## What was drilled

D3 started a local `volvisor-witnessd`, registered a volume with host-a and
host-b endpoints, granted host-a epoch 1/lease 1, stopped the witness, observed
`curl` exit 7 for health and grant, restarted it, and recovered epoch/holder/
lease/commit/registration. It then showed a live-lease 409, self-released
host-a, waited out the fence window, and granted host-b epoch 2 with a fencing
proof. Evidence:
[raw/03-lost-witness.log](../../evidence/day2/2026-11-03-devhost/raw/03-lost-witness.log)
and [journal/d3-witness-journal.log](../../evidence/day2/2026-11-03-devhost/journal/d3-witness-journal.log).

**Limits:** single host; witness service only; no DRBD 9 module or peer; no
BMC/fencing; no network partition; no split-brain test; no measured RPO/RTO
for the storage half.

**NOT RUN — REQUIRES** a real DRBD 9 peer (or Ceph) and a second host with an
independent failure domain to prove that the old writer actually stops writing
after losing its lease. That is part of D8 and the
[nearline campaign](../nearline-multihost-campaign.md).
