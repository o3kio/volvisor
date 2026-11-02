# Nearline multi-host campaign runbook (#37)

Status: **READY TO RUN — NOT RUN. NO EVIDENCE EXISTS.**

This runbook is the copy-pasteable procedure for the three-physical-host
nearline campaign required by issue #37. It is a procedure, not a result: the
repository currently contains **no** `docs/evidence/nearline/` bundle and no
claim that any scenario below has been executed. Running it is an operator act;
the recorded evidence is what closes the gate, not this document.

Related:
- [Nearline replication contract v2](../../contracts/nearline-replication-v2.md)
- [Volume API v2](../../contracts/volume-api-v2.md)
- [ADR-0007 — DRBD 9 nearline provider](../adr/0007-drbd9-nearline-replication-provider.md)
- [P5 aggressive failure campaign plan](../plans/2026-10-10-p5-aggressive-failure-campaign.md)
- [Witness/fencing authority plan](../plans/2026-10-09-p4-witness-fencing-authority.md)
- [Evidence standards](../evidence/README.md)
- [Preflight script](../../scripts/preflight-nearline.sh) and
  [inventory template](../../scripts/nearline-inventory.example.env)
- [Configuration reference](../configuration.md)

## 0. Scope, claim discipline, and non-goals

**What this campaign may claim after a complete PASS:** that the exact commit
recorded in the bundle exhibited the listed behaviors on the recorded physical
topology, for the recorded versions, with the recorded fault injections. It
does **not** claim production support, universal correctness, or an RPO of zero
for asynchronous replication.

**What it must never do:**
- treat a simulation (Tier S) or real-VMM-only (Tier V) record as this
  campaign's evidence;
- call a successful happy-path migration a split-brain proof;
- report an async failover as lossless because the destination looks healthy;
- resume a source after `SOURCE_REVOKED`;
- use dual-primary, timestamp arbitration, or a forced demotion to make a
  scenario pass;
- silently shrink the topology, skip a scenario, or waive a failure without an
  issue and an explicit `NOT_RUN`/`FAIL` record.

**Out of scope:** performance numbers (separate issue), managed Ceph/Rook
(issue #38 / separate POC), migration of PCI-passthrough devices that have not
been proven migratable, and any "fix" to fencing or error reporting.

## 1. Topology and host roles

The minimum honest topology is **three physical hosts in three distinct
failure domains, plus the witness on a fourth failure domain**:

| Role | Host | Failure domain | What runs there |
|---|---|---|---|
| Data replica 1 / VMM source | `HOST_A` | `FD_A` | `volvisord` (DRBD provider), Cloud Hypervisor source VM, DRBD primary, local backing LV |
| Data replica 2 / VMM destination | `HOST_B` | `FD_B` | `volvisord` (DRBD provider), Cloud Hypervisor destination VM, DRBD secondary, local backing LV |
| Independent campaign/oracle host | `HOST_C` | `FD_C` | HTTP driver, independent workload/oracle controller, off-host evidence capture. **Not a data replica and not the witness.** |
| Writer-authority witness | `WITNESS` | `FD_W` | `volvisor-witnessd` only; no tenant data, no DRBD resource |

`FD_A`, `FD_B`, `FD_C`, `FD_W` must be four distinct chassis/power/network
failure domains. If a real deployment cannot provide four, the run may use a
documented equivalent **only** after recording the exact deviation, its risk,
and the accepting operator in the run's `REPORT.md`; it may not silently
present a three-domain run as the full topology. A witness colocated with A or
B is not a valid equivalent for the witness's purpose.

`HOST_C` is deliberately not a third data copy. DRBD's configured peers are A
and B; a third host must not be counted as a remote replica. Its job is to keep
the driver/oracle and evidence capture independent of the two data hosts, so a
host crash does not take the observer with it.

## 2. Prerequisites

### 2.1 Hardware and authority

- [ ] Three physical hosts A/B/C with separate power and network failure
  domains, plus a witness host W in a fourth.
- [ ] A and B each have a dedicated, sacrificial local NVMe/LV for the
  nearline backing volume. No production data. The backing VG is operator
  provisioned; the daemon never touches foreign LVs.
- [ ] A and B can reach each other on the DRBD port (default `7100`) and both
  can reach W on the witness port (default `9101`). C can reach A/B/W for
  driving and observation.
- [ ] A shared snapshot directory readable and writable by the A and B
  `volvisord` daemons (NFS or equivalent), on a path outside the Ceph/DRBD
  data path. The destination verifies cross-host readability at `PREPARED`;
  a broken share must fail before the cut, not mid-cut.
- [ ] BMC/power control for A and B (or a documented equivalent) so an
  **unplanned** host loss is a real loss, not a graceful shutdown. If only a
  kernel crash (`echo c > /proc/sysrq-trigger`) is available, record it as a
  kernel crash; do not call it a power cut.
- [ ] Operator authorization to force-revoke a witness lease (W6) and to
  promote with `allow_loss` on a `POSSIBLE_LOSS` classification. Record who
  authorized each act and why.
- [ ] Network fault injection capability on A/B (iptables/nftables or a
  controllable switch) for partition and witness-unreachable scenarios.

### 2.2 Software and exact versions

Record these on every host/guest and in every evidence record:

- [ ] repository commit `git rev-parse HEAD`, plus `git status --porcelain`
  (a dirty worktree is recorded as dirty and cannot support a clean-SHA claim);
- [ ] `volvisord --version`, `volvisor-witnessd --version`, binary SHA256;
- [ ] kernel `uname -r`, DRBD kernel module version, `drbdadm --version`,
  `drbd-utils` package version;
- [ ] `cloud-hypervisor --version`, `ch-remote --version`, binary SHA256;
- [ ] guest image digest, guest kernel, `fio --version`;
- [ ] any NFS/network/switch firmware relevant to the shared snapshot path.

The daemons and witness must be built from the exact commit in the bundle. Do
not run a locally modified binary and report the repository SHA.

### 2.3 Configuration

Use the documented shapes in [configuration.md](../configuration.md) and
[`examples/volvisor.toml`](../../examples/volvisor.toml) /
[`examples/witnessd.toml`](../../examples/witnessd.toml). At minimum:

- A and B: `provider = "drbd"`, matching `drbd_node_name`, distinct
  `drbd_local_address`, each other's `drbd_peer_address`, a witness URL on W,
  per-host `witness_host_token`, and `[migration] enabled = true` with the
  shared `snapshot_dir`, peer URL/token, and `[vmm]` binaries.
- W: `lease_ttl_secs`, `lease_grace_secs`, `suspend_budget_secs`, and host
  tokens for A and B. The witness must be a **third/fourth failure domain**;
  the daemon refuses a loopback or colocated witness from its own view, but
  physical placement is the operator's assertion.

Validate without starting a server:

```sh
volvisord --config /etc/volvisor/volvisor.toml --check-config
volvisor-witnessd --config /etc/volvisor/witnessd.toml --check-config
```

## 3. Inventory and preflight

Copy the template outside the repository and fill it in:

```sh
install -d -m 0700 /etc/volvisor
install -m 0600 scripts/nearline-inventory.example.env /etc/volvisor/nearline-inventory.env
$EDITOR /etc/volvisor/nearline-inventory.env
chmod 0600 /etc/volvisor/nearline-inventory.env
```

Never commit the completed inventory. The repository contains only the
placeholder template.

Run the preflight from a checkout at the exact SHA:

```sh
git rev-parse HEAD
git status --porcelain
bash scripts/preflight-nearline.sh /etc/volvisor/nearline-inventory.env \
  | tee /var/lib/volvisor-campaign/preflight-$(date -u +%Y%m%dT%H%M%SZ).log
```

The procedures below use `HOST_A_SSH`, `HOST_B_SSH`, `HOST_C_SSH` and
`WITNESS_SSH`. Read only those keys into the shell; never `source` the
inventory file:

```sh
for key in HOST_A_SSH HOST_B_SSH HOST_C_SSH WITNESS_SSH; do
  value="$(sed -n "s/^${key}=//p" /etc/volvisor/nearline-inventory.env | head -n1)"
  if [[ -z "$value" ]]; then
    printf 'missing %s in inventory; fix it before continuing\n' "$key" >&2
    break
  fi
  export "$key=$value"
done
```

Run the snippets in `bash` on `HOST_C`. The endpoint variables (`API_A`,
`API_B`, `WITNESS`) and the campaign identifiers (`VOL`, `VM`, `MIG`) are
assigned explicitly below; do not copy values from another run.

The preflight is read-only. It validates:

- three distinct SSH targets and three distinct DRBD node identities;
- four distinct failure-domain labels (A/B/C and witness);
- reachability of A/B/C/W and the DRBD/witness ports;
- DRBD 9 tooling/module and `/proc/drbd`, LVM tooling and VG free space;
- clock skew per host against the operator host;
- journal/snapshot/witness-state free space;
- `volvisord`/`volvisor-witnessd --check-config` on the configured paths;
- witness `/healthz`;
- campaign/VMM tooling (`fio`, `jq`, `curl`, `cloud-hypervisor`, `ch-remote`).

A failure is a stop: fix the environment, do not run the campaign. The
preflight produces **no** evidence and does not create
`docs/evidence/nearline/`.

## 4. Evidence workspace and capture discipline

Create a unique run directory **outside the repository** on `HOST_C` (or a
dedicated management host). Only a reviewed, scrubbed copy is later added under
`docs/evidence/nearline/<date>/`.

```sh
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-$(git rev-parse --short HEAD)"
RUN="/var/lib/volvisor-campaign/nearline-$RUN_ID"
umask 077
mkdir -p "$RUN"/{raw,records,oracle,requests,decision}
export RUN RUN_ID
```

Use a capture wrapper for every command. It records UTC time, host, exact
argv, stdout/stderr and exit status. Never run the session with `set -x` and
never put bearer tokens on a command line.

```sh
say() { printf '\n### %s host=%s\n' "$(date -u +%FT%TZ)" "$(uname -n)" | tee -a "$RUN/raw/session.log"; }
cap() {
  local tag="$1"; shift
  say "$tag"
  {
    printf '$'
    printf ' %q' "$@"
    printf '\n'
    "$@"
    printf 'exit=%s\n' "$?"
  } 2>&1 | tee -a "$RUN/raw/session.log"
}
cap_host() {
  local role="$1" target="$2" tag="$3"; shift 3
  say "$tag ($role @ $target)"
  {
    printf '$ ssh %s' "$target"
    printf ' %q' "$@"
    printf '\n'
    ssh -o BatchMode=yes -o ConnectTimeout=5 "$target" "$@"
    printf 'exit=%s\n' "$?"
  } 2>&1 | tee -a "$RUN/raw/session.log"
}
# cap_record additionally keeps the command, output and exit status in a
# per-request record file and propagates the command's status.
cap_record() {
  local rec="$1" tag="$2"; shift 2
  say "$tag"
  (
    printf '$'
    printf ' %q' "$@"
    printf '\n'
    "$@"
    rc=$?
    printf 'exit=%s\n' "$rc"
    exit "$rc"
  ) 2>&1 | tee -a "$RUN/raw/session.log" "$rec"
  return "${PIPESTATUS[0]}"
}
```

Keep tokens in owner-only curl config files, never in the transcript:

```sh
CURL_A="$RUN/requests/curl-a.conf"
CURL_B="$RUN/requests/curl-b.conf"
CURL_W_READ="$RUN/requests/curl-w-read.conf"
CURL_W_A="$RUN/requests/curl-w-a.conf"
CURL_W_B="$RUN/requests/curl-w-b.conf"
umask 077
printf 'header = "Authorization: Bearer %s"\n' "$(cat /etc/volvisor/admin-a.token)" >"$CURL_A"
printf 'header = "Authorization: Bearer %s"\n' "$(cat /etc/volvisor/admin-b.token)" >"$CURL_B"
printf 'header = "Authorization: Bearer %s"\n' "$(cat /etc/volvisor/witness-read.token)" >"$CURL_W_READ"
printf 'header = "Authorization: Bearer %s"\n' "$(cat /etc/volvisor/witness-a.token)" >"$CURL_W_A"
printf 'header = "Authorization: Bearer %s"\n' "$(cat /etc/volvisor/witness-b.token)" >"$CURL_W_B"
```

HTTP helpers (bodies are files in `$RUN/requests/`):

```sh
API_A="http://HOST_A:8787"
API_B="http://HOST_B:8787"
WITNESS="http://WITNESS:9101"

api() { # api <curl-config> <base> <method> <path> [body-file]
  local cfg="$1" base="$2" method="$3" path="$4" body="${5:-}"
  if [[ -n "$body" ]]; then
    curl --fail-with-body --silent --show-error --config "$cfg" \
      --request "$method" --header 'content-type: application/json' \
      --data-binary @"$body" "$base$path"
  else
    curl --fail-with-body --silent --show-error --config "$cfg" \
      --request "$method" "$base$path"
  fi
}
api_a() { api "$CURL_A" "$API_A" "$@"; }
api_b() { api "$CURL_B" "$API_B" "$@"; }
witness_read() { curl --fail-with-body --silent --show-error --config "$CURL_W_READ" "$@"; }
witness_a() { api "$CURL_W_A" "$WITNESS" "$@"; }
witness_b() { api "$CURL_W_B" "$WITNESS" "$@"; }
```

At the end of the run, hash the raw tree and write a manifest. Do this **before**
any curation:

```sh
( cd "$RUN" && find . -type f ! -name MANIFEST.sha256 -print0 \
    | sort -z | xargs -0 sha256sum > MANIFEST.sha256 )
```

Curation rules: raw files are immutable; a correction is a new file/errata; no
secret, kubeconfig, token, private key or unscrubbed Pod environment is
committed; every report claim cites a raw file and line range.

## 5. Baseline bring-up

### 5.1 Start and attest the witness

On W:

```sh
cap_host W "$WITNESS_SSH" "witness version" volvisor-witnessd --version
cap_host W "$WITNESS_SSH" "witness config check" \
  volvisor-witnessd --config /etc/volvisor/witnessd.toml --check-config
cap_host W "$WITNESS_SSH" "start witness" systemctl restart volvisor-witnessd
cap_host W "$WITNESS_SSH" "witness active" systemctl is-active volvisor-witnessd
```

From C, record health and the exact witness journal state:

```sh
cap witness-health curl -fsS "$WITNESS/healthz"
cap_host W "$WITNESS_SSH" "witness journal" ls -l /var/lib/volvisor-witness/state
```

### 5.2 Start A and B and attest DRBD

```sh
for role in A B; do
  target_var="${role}_SSH"; target="${!target_var}"
  cap_host "$role" "$target" "volvisord version" volvisord --version
  cap_host "$role" "$target" "volvisord config check" \
    volvisord --config /etc/volvisor/volvisor.toml --check-config
  cap_host "$role" "$target" "start volvisord" systemctl restart volvisord
  cap_host "$role" "$target" "volvisord active" systemctl is-active volvisord
  cap_host "$role" "$target" "drbd status" drbdadm status
  cap_host "$role" "$target" "drbd version" drbdadm --version
done
```

Record `iptables-save`/`nft list ruleset` on A and B before any fault injection,
so the cleanup baseline is evidence.

### 5.3 Create the nearline volume and attach the source VM

Write `$RUN/requests/create.json`:

```json
{
  "api_version": "volvisor.volume.v2",
  "operation_id": "op-create-VOL",
  "project_id": "campaign",
  "volume_id": "VOL",
  "class": "nearline-replicated",
  "size_bytes": 21474836480,
  "replication": { "engine": "drbd9", "mode": "async", "remote_replicas": 1 }
}
```

```sh
sed -i "s/VOL/$VOL/g" "$RUN/requests/create.json"
cap_record "$RUN/records/create.json" create-volume \
  api_a POST /v2/volumes "$RUN/requests/create.json"
cap_record "$RUN/records/inspect-create.json" inspect-a \
  api_a GET "/v2/volumes/$VOL"
```

The create response gives the volume `generation`. Use it in the attach body
(`$GEN`):

```json
{
  "api_version": "volvisor.volume.v2",
  "operation_id": "op-attach-a-VOL",
  "vm_id": "VM",
  "host_id": "host-a",
  "attachment_id": "att-a-VOL",
  "expected_volume_generation": 1,
  "access_mode": "single_writer",
  "requested_frontend": "virtio-blk",
  "vmm_disk_id": "disk0"
}
```

```sh
sed -i -e "s/VOL/$VOL/g" -e "s/VM/$VM/g" "$RUN/requests/attach-a.json"
cap_record "$RUN/records/attach-a.json" attach-a \
  api_a POST "/v2/volumes/$VOL/attach" "$RUN/requests/attach-a.json"
cap_record "$RUN/records/inspect-attached.json" inspect-attached \
  api_a GET "/v2/volumes/$VOL"
```

Record the authority section, the generated DRBD resource and the exact backing
identities. The resource name is derived by the provider; find the generated
file and correlate its disk path with the campaign LV. Do not guess:

```sh
RESOURCE="$(ssh "$HOST_A_SSH" "grep -l 'disk .*$VOL\|disk .*vol-' /etc/drbd.d/volvisor-*.res | head -n1")"
cap_host A "$HOST_A_SSH" "resource definition" sh -c "ls -l /etc/drbd.d/volvisor-*.res; drbdadm dump '$RESOURCE'"
cap_host A "$HOST_A_SSH" "drbd status" drbdsetup status "$RESOURCE" --verbose
cap_host B "$HOST_B_SSH" "drbd status" drbdsetup status "$RESOURCE" --verbose
cap_host A "$HOST_A_SSH" "lv identity" lvs -o lv_name,vg_name,lv_size,lv_tags
```

### 5.4 Register the volume at the witness

Registration is the explicit, out-of-band provisioning step that linearizes
future authority; it is not a tenant API. Use the exact live lineage, not a
copied value. Extract the peer node id from the generated resource definition
and read the non-zero data-generation UUIDs:

```sh
PEER_NODE_ID="$(ssh "$HOST_A_SSH" "drbdadm dump '$RESOURCE' | awk '/node-id/ {print \$3; exit}'")"
ssh "$HOST_A_SSH" "drbdsetup show-gi '$RESOURCE' '$PEER_NODE_ID' 0" \
  | tee "$RUN/raw/show-gi.txt"
LINEAGE="$(awk -F: 'NF>=16 && $1 ~ /^[0-9A-F]{16}$/ {for(i=1;i<=4;i++) if($i != "0000000000000000") print $i}' \
  "$RUN/raw/show-gi.txt" | sort -u | paste -sd, -)"
```

Write the registration body with both endpoint backing identities (A's
volvisor-created LV carries the owner tag; B's operator-provisioned peer LV does
not):

```json
{
  "protocol_version": 2,
  "operation_id": "op-register-VOL",
  "content": {
    "lineage_uuids": ["LINEAGE1", "LINEAGE2"],
    "endpoints": [
      { "host_id": "host-a", "backing": "A_LV_IDENTITY", "volvisor_created": true },
      { "host_id": "host-b", "backing": "B_LV_IDENTITY", "volvisor_created": false }
    ]
  }
}
```

```sh
# Replace placeholders with the values recorded above; do not invent them.
cap_record "$RUN/records/register.json" witness-register \
  witness_a POST "/v1/volumes/$VOL/register" "$RUN/requests/register.json"
cap_record "$RUN/records/witness-inspect-initial.json" witness-inspect \
  witness_read GET "/v1/volumes/$VOL"
```

The expected initial state is `current_epoch: 0` (pre-authority) until the first
attach/promotion acquires a lease. If it is not, stop and investigate; do not
edit the witness journal.

### 5.5 Start the guest workload

Boot the source VM on A with the attached volume, verify the guest sees the
expected block device and size, and record guest identity:

```sh
cap_host A "$HOST_A_SSH" "source VM" ch-remote --api-socket /run/volvisor/vms/VM.sock info
cap_host A "$HOST_A_SSH" "guest block device" sh -c \
  "ssh guest@VM 'lsblk -o NAME,SERIAL,SIZE,TYPE,FSTYPE,MOUNTPOINTS; blockdev --getsize64 /dev/vdb'"
```

Do not format or mount a production disk. The campaign volume must be
disposable.

## 6. Workload and write-trace oracle

Two independent checks are required; neither may be replaced by the other.

### 6.1 Media integrity: fio verification

Run a tagged, verified workload in the guest. Example (replace device/size):

```sh
cap_host A "$HOST_A_SSH" "fio verify workload" ssh guest@VM \
  "fio --name=nl-verify --filename=/dev/vdb --direct=1 --rw=randwrite --bs=4k \
       --size=8G --iodepth=16 --verify=crc32c --verify_fatal=1 --do_verify=1 \
       --output-format=json --output=/var/tmp/fio-verify.json"
```

Copy the JSON out and keep it in `$RUN/oracle/`. `fio` verification proves the
media/application checksum path; it does not by itself prove which acknowledged
writes were at the destination at the cut.

### 6.2 Acknowledged-write trace: the oracle

Use a monotonic tagged writer that records a sequence **only after the write
call completes**. The trace must live outside the failing host (C), not only in
the guest or on A. A minimal per-block oracle:

```sh
# In the guest, write 4 KiB blocks with a 16-byte header:
#   seq(8, big-endian) | crc32c(payload)(4) | writer_id(4)
# After each successful write (O_DIRECT + fdatasync), print "seq crc" to stdout.
# C captures that stream over ssh into $RUN/oracle/ack-trace.log.
```

The exact oracle implementation must be committed with the run (or its digest
recorded) so a reviewer can reproduce the trace format. The pass conditions:

- every acknowledged sequence present at the destination is crc-correct;
- on a planned migration, every acknowledged pre-barrier sequence is present;
- on an unplanned failover, the missing sequences are **reported as a tail**;
  a non-zero tail is `POSSIBLE_LOSS`, never silently "zero RPO";
- a sequence that is present but corrupt is a distinct failure, not a tail;
- the boundary comes from the data path (the last write that succeeded before
  suspension/role flip), cross-checked against the coordinator's barrier; the
  coordinator's record alone is not the oracle.

Record the oracle inputs, the raw ack trace, the verification output and a
human-readable verdict under `$RUN/oracle/`.

## 7. Scenario matrix

Each scenario is run at the same exact SHA. Run them in order unless a failure
makes the next scenario unsafe; any reordering or skip is recorded.

| ID | Scenario | Required evidence | Pass condition |
|---|---|---|---|
| N0 | Baseline | inspect, authority, DRBD status, witness inspect, oracle baseline | volume Ready/Attached, one primary, one secondary, witness epoch live, oracle baseline verifies |
| N1 | Planned migration | full `ObserveHandoff` state history, cut duration, source/destination inspect, oracle | reaches `COMPLETE`, guest resumes on B, every acknowledged pre-barrier write at B, source fenced/Secondary |
| N2 | Unplanned failover + async tail | power/crash timestamp, witness inspect, adopt classification, oracle tail | old writer fenced; adopt is `POSSIBLE_LOSS` (unless a recorded barrier justifies otherwise); tail honestly measured; no dual primary |
| N3 | Witness unreachable during cut | fault window, `ObserveHandoff`, witness inspect, source/destination state | cut never reports `COMPLETE` over an unproven authority; source is not resumed; recovery is explicit and recorded |
| N4 | Witness unreachable during source resume | fault window, lease deadline, destination state, inspect | destination self-fences at its local deadline or completes under a committed epoch; no stale source write is admitted |
| N5 | Lease expiry / stale epoch | witness inspect before/after, old-epoch renew/attach refusal, DRBD suspended state | old epoch is refused typed; stale writer cannot write; no silent lease resurrection |
| N6 | Split-brain attempt refused on both sides | partition window, adopt refusal, revoke authorization, both-side DRBD status, oracle | at most one writer; B refuses while A's lease is live; after a recorded revoke/fence, A cannot write; no dual-primary |
| N7 | Partition during replication | partition window, `track_sync`/convergence, source I/O, oracle | no caught-up claim over the partition; async tail is honest; heal converges or parks explicitly |
| N8 | Partition during cut | partition window, `ObserveHandoff`, retries, final state | cut refuses/parks; never `COMPLETE` over a partition; source not silently resumed |
| N9 | Crash at each cut step | per-step PID/state, restart logs, `ObserveHandoff`, oracle | reconcile drives forward only; no source resume over an unvoided barrier; terminal state is `COMPLETE`, `ABORTED` (pre-cut) or `IN_DOUBT` as the contract allows |
| N10 | Peer rejoin/resync | pre/post DRBD status, resync progress, oracle | peer rejoins with matching lineage; resync completes or is honestly in progress; no foreign/divergent data is promoted |
| N11 | 24h soak | continuous fio/oracle, periodic migration/failover, all logs | no dual writer, no silent corruption, no unreported failure; every injected event has a record |

## 8. Common operations and observations

### 8.1 Observe a migration

```sh
observe() { api_a GET "/v2/migrations/$MIG" | tee "$RUN/records/migration-$MIG-$(date -u +%H%M%S).json"; }
wait_state() {
  local want="$1" timeout="${2:-300}" start now state
  start="$(date +%s)"
  while :; do
    state="$(api_a GET "/v2/migrations/$MIG" | jq -r .state)"
    printf '%s state=%s\n' "$(date -u +%FT%TZ)" "$state" | tee -a "$RUN/raw/state.log"
    [[ "$state" == "$want" ]] && return 0
    now="$(date +%s)"
    (( now - start >= timeout )) && { echo "TIMEOUT waiting for $want (last: $state)" >&2; return 1; }
    sleep 1
  done
}
```

Keep every observation file; do not overwrite a state history with a summary.

### 8.2 Check mobility and prepare a planned migration

```sh
cat > "$RUN/requests/check-mobility.json" <<EOF
{ "target_host": "host-b" }
EOF
cap_record "$RUN/records/check-mobility.json" check-mobility \
  api_a POST "/v2/vms/$VM/check-mobility" "$RUN/requests/check-mobility.json"

cat > "$RUN/requests/migration.json" <<EOF
{
  "migration_id": "$MIG",
  "vm_id": "$VM",
  "target_host": "host-b",
  "volume_ids": ["$VOL"],
  "expected_generations": [1]
}
EOF
cap_record "$RUN/records/migration-prepare.json" prepare \
  api_a POST /v2/migrations "$RUN/requests/migration.json"
cap_record "$RUN/records/inspect-prepared.json" inspect-prepared \
  api_a GET "/v2/volumes/$VOL"
```

### 8.3 Transfer (cut) and observe

```sh
cat > "$RUN/requests/transfer.json" <<'EOF'
{ "vm_paused_and_io_drained_proof": { "kind": "operator", "observed_by": "campaign" } }
EOF
cap_record "$RUN/records/migration-transfer.json" transfer \
  api_a POST "/v2/migrations/$MIG/transfer" "$RUN/requests/transfer.json"
```

The proof is corroboration only; Volvisor performs and verifies its own pause.
Poll `observe` until a terminal state. A `202` is not a success.

### 8.4 Witness inspect and host-side state

```sh
cap_record "$RUN/records/witness-inspect-$(date -u +%H%M%S).json" witness-inspect \
  witness_read GET "/v1/volumes/$VOL"
cap_host A "$HOST_A_SSH" "drbd state" drbdsetup status "$RESOURCE" --verbose
cap_host B "$HOST_B_SSH" "drbd state" drbdsetup status "$RESOURCE" --verbose
cap_host A "$HOST_A_SSH" "kernel drbd" cat /proc/drbd
cap_host B "$HOST_B_SSH" "kernel drbd" cat /proc/drbd
```

Also capture daemon logs and journal excerpts per host:

```sh
cap_host A "$HOST_A_SSH" "volvisord journal" \
  journalctl -u volvisord --since "$RUN_START_UTC" --no-pager -o short-iso
cap_host A "$HOST_A_SSH" "volvisor journal files" \
  sh -c "ls -l /var/lib/volvisor/journal; cp /var/lib/volvisor/journal/journal.log /tmp/campaign-journal-a.log"
```

Copy, do not edit, `/var/lib/volvisor/journal/journal.log`, `drbd-state.json`,
migration records and witness `journal.log`. Redact secrets only in the curated
bundle, never in the raw evidence.

## 9. Scenario procedures

### N0 — Baseline

1. Verify `GET /v2/volumes/$VOL` shows one attachment, one writer, the expected
   generation and a witness-managed authority section.
2. `drbdsetup status` on A shows `primary`, B `secondary`, connected and
   `UpToDate`; `drbdsetup show-gi` is recorded.
3. Witness inspect shows a live lease for A, epoch 1 (or the first granted
   epoch), commit index recorded.
4. Run the oracle baseline and `fio` verification; both must pass before any
   cut.

If baseline health is `Unknown`/`Degraded`, record it and stop; do not proceed
on an unproven baseline.

### N1 — Planned migration happy path

1. Ensure the VM is running on A and the shared snapshot dir is visible from B.
2. `check-mobility`; record `eligible` and `participants`.
3. Prepare and transfer as in §8.2–8.3.
4. Poll `ObserveHandoff`; capture every state and the `cut_duration_secs`.
5. On `COMPLETE`, verify:
   - guest is resumed on B and has the same disk identity;
   - B's DRBD role is `primary`, A is `secondary`/fenced;
   - source authority epoch is retired; destination holds a strictly newer
     epoch;
   - the oracle has every acknowledged pre-barrier write at B, crc-correct;
   - no source VM is running on A.
6. Record the final inspect from both A and B and the witness inspect.

A `COMPLETE` with a missing acknowledged write is a **failure**, not a tail.

### N2 — Unplanned failover with measured async tail

1. Run the oracle continuously; record the last acknowledged sequence and the
   wall-clock time at the fault.
2. Kill A for real (BMC power off preferred; record exact mechanism and time).
   Do not use a graceful shutdown for this scenario.
3. On B, observe the DRBD peer state and wait for the witness's authority
   window. Attempt adopt:

```sh
cat > "$RUN/requests/adopt.json" <<EOF
{
  "api_version": "volvisor.volume.v2",
  "operation_id": "op-adopt-$RUN_ID",
  "allow_loss": true
}
EOF
cap_record "$RUN/records/adopt.json" adopt \
  api_b POST "/v2/admin/nearline/$VOL/adopt" "$RUN/requests/adopt.json"
```

4. Record the classification. For async replication without a recorded
   last-acknowledged barrier, the honest classification is `POSSIBLE_LOSS`
   with an unknown boundary. `SAFE_CURRENT` requires the evidence the contract
   defines; do not accept a protocol-only claim.
5. After promotion, run the oracle verification against B and compute the tail:
   acknowledged sequences not present at B. Record the count and sequence
   ranges; do not describe an unknown tail as zero.
6. Bring A back if the scenario requires rejoin (N10). Do not let A write.

If B promotes while A may still write, or if two primaries exist, **stop the
campaign**: this is a fencing failure, not a test failure to work around.

### N3 — Witness unreachable during cut

1. Record witness health and the current lease/epoch.
2. On A and B, block the witness port (and only that port). Example using a
   dedicated iptables chain; record `iptables-save` before/after:

```sh
for target in "$HOST_A_SSH" "$HOST_B_SSH"; do
  ssh "$target" "iptables -w -I OUTPUT -p tcp --dport 9101 -j DROP; \
                 iptables -w -I INPUT  -p tcp --sport 9101 -j DROP"
done
```

3. Drive or continue a cut (`transfer`, or start one first if the scenario is
   "during cut"). Poll `ObserveHandoff` through at least
   `2 × lease_ttl_secs + lease_grace_secs + suspend_budget_secs`.
4. Record every state. A `COMPLETE` while the witness is unreachable and no
   committed authority proves the destination is a **fencing failure**. The
   expected honest outcomes are a typed refusal, a park, or `IN_DOUBT`; the
   source must not resume over an unvoided barrier.
5. Remove the block and record recovery. If the migration parked, follow the
   contract's recovery verbs; do not "fix" it by resuming the source or
   force-promoting.

Cleanup:

```sh
for target in "$HOST_A_SSH" "$HOST_B_SSH"; do
  ssh "$target" "iptables -w -D OUTPUT -p tcp --dport 9101 -j DROP; \
                 iptables -w -D INPUT  -p tcp --sport 9101 -j DROP"
done
```

### N4 — Witness unreachable during source resume / destination operation

1. Start a migration and wait for `DESTINATION_AUTHORIZED` (or the latest
   pre-`VM_RESUMED` state) using `wait_state`.
2. Block W from the destination (and source, if the scenario is a full witness
   outage).
3. Observe the destination's lease renewal failure and its local deadline. The
   destination must self-fence at the W5 deadline rather than keep serving on a
   fabricated lease.
4. Record whether the VM was resumed before the fence. If the destination
   resumed and then lost authority, that is a stop condition; do not continue
   as if the migration passed.
5. Unblock W. Record the recovery to `COMPLETE`, `IN_DOUBT`, or a typed
   refusal. Never resume the old source epoch.

### N5 — Lease expiry and stale-epoch refusal

1. Record `witness inspect`, the lease id, epoch and remaining seconds.
2. Stop the witness (or block W from the writer) for longer than
   `lease_ttl_secs + lease_grace_secs + suspend_budget_secs`.
3. On the old writer, attempt an old-epoch renewal directly and an attach:

```sh
# Use the recorded epoch and lease id; the body must be the real values.
cat > "$RUN/requests/renew-old.json" <<EOF
{
  "protocol_version": 2,
  "operation_id": "op-renew-old-$RUN_ID",
  "host_id": "host-a",
  "epoch": OLD_EPOCH,
  "lease_id": OLD_LEASE_ID
}
EOF
cap_record "$RUN/records/renew-old.json" renew-old \
  witness_a POST "/v1/volumes/$VOL/renew" "$RUN/requests/renew-old.json"
```

Record the typed result (`STALE_EPOCH` expected; `LEASE_HELD` is also an
authority refusal). Inspect `drbdsetup status` and `/proc/drbd` for the
self-fenced/suspended state. Attempt an old-source guest write only through the
oracle, never by manually `dd`-ing around Volvisor's enforcement; record
whether it is refused or blocked.

4. Restart/restore the witness and verify epochs/commit indices did not shrink.
   A stale writer must not resume without a fresh, correctly fenced promotion.

### N6 — Split-brain attempt refused on both sides

1. Establish a healthy A-primary/B-secondary baseline.
2. Partition A from B on the replication port only, while both may still reach
   W (or document which side can reach W). Record the exact rules.
3. On B, attempt `POST /v2/admin/nearline/$VOL/adopt` with `allow_loss: true`.
   With A's lease still live, B must refuse (`UNSAFE`/`LEASE_HELD`); record the
   refusal and B's unchanged state.
4. From the campaign controller (holding A's witness credential), issue the
   operator-authorized W6 revocation of A's live lease, with the recorded
   authorization:

```sh
cat > "$RUN/requests/revoke-a.json" <<EOF
{
  "protocol_version": 2,
  "operation_id": "op-revoke-a-$RUN_ID",
  "host_id": "host-a",
  "epoch": OLD_EPOCH,
  "authorization": {
    "operator": "OPERATOR_ID",
    "reason": "campaign N6 forced revocation; source may be partitioned from the destination"
  }
}
EOF
cap_record "$RUN/records/revoke-a.json" revoke-a \
  witness_a POST "/v1/volumes/$VOL/revoke" "$RUN/requests/revoke-a.json"
```

5. Wait for the witness's W7 fence window. B's adopt may return
   `FENCE_PENDING`; retry after the stated retry-after. Record the grant and the
   fencing proof.
6. Verify A cannot write: its renewal is stale, its device is suspended or
   demoted, and a source-side oracle write is refused by the enforcement point.
   Bring B up as the only primary and verify the oracle.
7. If A and B are ever both primary/writable, **stop immediately** and preserve
   the evidence; that is a split-brain violation, not a scenario to salvage.

### N7 — Network partition during replication

1. With a healthy baseline and foreground I/O, partition A↔B on the DRBD port,
   leaving both reachable to W.
2. Record `drbdsetup status` (`StandAlone`/`Disconnected`/`WFConnection` as
   observed), the convergence query and the source's continued async ACKs.
3. Verify the system does not claim the peer is caught up while partitioned; a
   `COMPLETE`/`UpToDate` claim over the partition is a failure.
4. Run foreground I/O long enough to create a real tail; record the ack trace.
5. Heal the partition. Record resync progress and convergence. If it does not
   converge, record the park/refusal and the exact state; do not delete the
   resource to "start clean".

### N8 — Network partition during cut

1. Prepare a migration.
2. Partition A↔B after `PREPARED` (and, for a second run, at
   `BARRIER_DURABLE`/`QUIESCED`) using the same scoped rules.
3. Transfer and poll. The cut must refuse or park; it must not cross
   `SOURCE_REVOKED`/`DESTINATION_AUTHORIZED` on an unproven link or report
   `COMPLETE` over the partition.
4. Record the final state (`ABORTED` pre-cut, `IN_DOUBT` post-cut, or the
   canonical state plus stall detail). Heal and follow the documented recovery;
   never resume the source over the barrier.

### N9 — Crash at each cut step

Crash the daemon (real `kill -9`/SIGKILL, not a graceful stop) at each
observable step. Use a fresh migration id for each cell. On systemd, record
whether `Restart=on-failure` restarted the daemon; both the kill and the restart
are evidence.

| Cell | Target observation | Kill |
|---|---|---|
| N9.1 | `PREPARED` first observed | source `volvisord` |
| N9.2 | immediately after `transfer` accepted (`202`, before `QUIESCED`) | source `volvisord` |
| N9.3 | `QUIESCED` first observed | source `volvisord` |
| N9.4 | `BARRIER_DURABLE` first observed | source `volvisord` |
| N9.5 | `SOURCE_REVOKED` first observed | source `volvisord` |
| N9.6 | `DESTINATION_AUTHORIZED` first observed | destination `volvisord` |
| N9.7 | `VM_RESUMED` first observed (or immediately after resume) | destination `volvisord` |

Procedure per cell:

```sh
# Wait for the target state, then kill the daemon on the chosen side.
wait_state TARGET_STATE 300
cap_host SIDE "$SIDE_SSH" "kill volvisord" systemctl kill --signal=KILL --kill-who=main volvisord
cap_host SIDE "$SIDE_SSH" "post-kill status" systemctl is-active volvisord || true
# Restart if the service did not auto-restart; record the choice.
cap_host SIDE "$SIDE_SSH" "restart volvisord" systemctl restart volvisord
```

Then poll `ObserveHandoff` to a terminal state and assert:

- no source resume over an unvoided barrier (G5/W9);
- the re-driven operation either completes or refuses typed, never
  double-executes;
- the oracle's acknowledged prefix property holds;
- the cut marker/fence residue is resolved or durably recorded for the
  operator.

Record `journal.log`, migration records, `drbd-state.json`, witness inspect and
the daemon journal after every cell.

### N10 — Peer rejoin and resync

1. After a partition/crash scenario, restart the peer and record
   `drbdsetup status`, `drbdsetup show-gi`, and the resource definitions.
2. Verify the rejoining peer presents the same lineage and does not adopt
   foreign state. If it is `Inconsistent`/`Outdated`/`DUnknown`, record the
   resync baseline and progress; do not promote it or declare it current.
3. Let resync complete under foreground load. Record the duration, any
   reconnects, and the oracle verification.
4. If the peer cannot rejoin without a full wipe, that is a result: record it
   and file an issue. Do not erase the resource to make the run continue.

### N11 — 24-hour soak

Run one continuous fio/oracle workload while injecting the scheduled events.
A defensible schedule (adjust and record before starting):

- `fio` continuous random write + periodic verify for 24h;
- planned migration every 60 minutes;
- witness unreachable for 60 seconds every 2 hours;
- replication partition for 60 seconds every 3 hours;
- unplanned failover on a sacrificial volume every 4 hours (or fewer if the
  topology cannot sustain it; record the deviation);
- full state/inspect/DRBD snapshots at least every 5 minutes and after every
  event.

```sh
cap soak-start date -u +%FT%TZ
cap_host A "$HOST_A_SSH" "soak workload" ssh guest@VM \
  "fio --name=nl-soak --filename=/dev/vdb --direct=1 --rw=randwrite --bs=4k \
       --time_based --runtime=24h --iodepth=32 --verify=crc32c --verify_fatal=1 \
       --output-format=json --output=/var/tmp/fio-soak.json"
cap soak-end date -u +%FT%TZ
```

The soak passes only if there was no dual writer, no silent corruption, no
unreported failure, every scheduled event has a record, and the final state is
explained. Missing a scheduled event is `NOT_RUN`, not pass.

## 10. Stop conditions

Stop the campaign immediately and preserve the evidence if any of these occurs:

1. Two writable authorities for one volume, or a second primary is accepted.
2. A planned migration reports `COMPLETE` with an acknowledged pre-barrier
   write missing or corrupt at the destination.
3. A source resumes after `SOURCE_REVOKED` without fenced reconciliation.
4. A stale epoch is accepted, or a writer keeps serving past its local deadline
   without self-fencing.
5. A cut reports `COMPLETE` over a partition, an un-drained apply queue, an
   unreachable witness without committed authority, or foreign-lineage data.
6. Witness state (epoch/commit index/lease) goes backwards across restart, or a
   stale witness view is used to mint authority.
7. A host/disk is mutated outside the claimed device/volume, or a foreign
   device/LV is adopted.
8. The evidence stream cannot be trusted (missing timestamps/commands,
   overwritten raw files, secrets in the bundle, host clock skew beyond the
   documented bound).
9. Any failure is waived to make the scenario pass.

A stop is a result: record it, file an issue, and do not continue the matrix on
an unsafe system.

## 11. Verdict template

Copy this into `REPORT.md` and fill every row. Use `NOT_RUN` rather than a blank.
The overall verdict is the weakest required row, not an average.

```markdown
# Nearline multi-host campaign — REPORT

- Campaign: #37
- Exact commit: <sha> (dirty: yes/no)
- Run window (UTC): <start> .. <end>
- Topology: A=<fd>, B=<fd>, C=<fd>, witness=<fd>
  (document any equivalent/deviations)
- Versions: volvisord=<>, witnessd=<>, kernel=<>, drbd=<>, cloud-hypervisor=<>, ch-remote=<>, fio=<>
- Raw bundle: <path> (MANIFEST.sha256 <digest>)
- Operator(s): <>

## Claim discipline
This bundle records an executed campaign on the topology above. It does not
claim production support or zero RPO for async replication. / (adjust only if
the run truly supports no stronger statement)

## Scenario results

| ID | Verdict | Oracle (acked / verified / missing / corrupt / tail) | Key observation | Evidence |
|---|---|---|---|---|
| N0 | PASS/FAIL/NOT_RUN/IN_DOUBT | | | raw/... |
| N1 | | | | |
| N2 | | | | |
| N3 | | | | |
| N4 | | | | |
| N5 | | | | |
| N6 | | | | |
| N7 | | | | |
| N8 | | | | |
| N9.1..N9.7 | | | | |
| N10 | | | | |
| N11 | | | | |

## Failures and follow-ups
- <scenario>: <observed vs required>; issue <link>; not waived.

## Final authority/inspect state
<final inspect JSON references; witness epoch/commit; DRBD role/state>

## Overall verdict
PASS / FAIL / INCOMPLETE. Required scenarios not run: ...

No production-support claim is made.
```

## 12. Closeout

1. Hash the raw bundle before curation (§4).
2. File an issue for every failure or unexpected limitation; link it in the
   report. Do not use "known flake" as a waiver.
3. Curate a scrubbed bundle under `docs/evidence/nearline/<date>/` only when
   the run actually happened and the report is ready for review. The bundle
   must follow [the evidence standards](../evidence/README.md).
4. Do not change ADR-0007 or SPEC-0002 to a production-accepted status based on
   this campaign alone. Record what the evidence supports; acceptance remains a
   separate decision.
5. If a scenario was `NOT_RUN`, say so in the report and in issue #37. Do not
   check the issue's evidence boxes from this runbook.
