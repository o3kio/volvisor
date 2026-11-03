# Day-2 drill report — 2026-11-03 single-host lab

**Overall verdict: partial.** Seven executable drills ran on one host and
passed their pre-stated assertions. Total host loss and real cross-host
re-admission (**D8**) did not run. The schema drill does **not** include a
live old-binary → new-binary upgrade or a cross-version rollback. The witness
drill covers the witness service, not storage-side DRBD lease enforcement.

| Area | Verdict | Why |
|---|---|---|
| Backup/restore of control-plane state | **DRILLED** | D1; single-host stopped-state restore, attachment state/revision/tombstones proven |
| Upgrade/rollback across schema versions | **PARTIAL** | D2 proves the schema policy, upgrade-on-write and future-version refusal; live binary upgrade/rollback **NOT RUN** (no second binary/tag) |
| Lost/unreachable witness | **PARTIAL** | D3 proves witness service outage/restart and lease/epoch recovery; storage-side DRBD fencing **NOT RUN** (no DRBD 9 module/peer) |
| Torn/corrupt journal tail and corrupt state file | **DRILLED** | D4; garbage tail, half frame, future frame, corrupt state file |
| ENOSPC during journal/state write | **DRILLED**, with finding | D5; journal is fail-closed; state-save path leaks an LV and a temporary in-memory volume — [#58](https://github.com/o3kio/volvisor/issues/58) |
| Crash loop and single-writer lock contention | **DRILLED** | D6; five SIGKILLs and competing daemon/witness refusals |
| Credential/certificate rotation | **DRILLED** | D7; restart-based token and same-host TLS rotation, no hot reload |
| Total host loss and re-admission | **NOT RUN — REQUIRES** a real multi-host DRBD/Ceph lab, a third failure domain and BMC/fencing | D8; no second host, no DRBD/Ceph, no power control |

## Identity and provenance

- **Source commit:** `e551d26af7cdedb468797ff8049e0b91b9ab416d`
  (`git describe --tags --always` = `e551d26`).
- **Worktree at run time:** every raw drill header records
  `# worktree_dirty: no` and the same source SHA. The flag comes from
  `git status --porcelain`, which includes untracked, non-ignored files.
  **The harness scripts are absent from `e551d26`'s tree**, and the flag
  would have read `yes` had they been present untracked in the recorded
  worktree. The raw logs therefore place the harness outside the recorded
  worktree and do not bind its bytes; its first tracked revision is
  `d5af71f`, committed after the run window. [SCRIPTS.txt](SCRIPTS.txt)
  records the SHA256 of each script at that revision and states that no
  byte-level capture proves those exact bytes executed. The clean-worktree
  claim is about the recorded product worktree, not the harness.
- **Bundle commit:** the commit that contains this directory. The source SHA
  and the bundle commit are different by design: the drills ran at
  `e551d26`; the bundle is a docs-only addition made afterwards. This bundle
  is evidence for code at `e551d26`, not for any later code change.
- **Run window:** 2026-11-03T11:23:29Z .. 2026-11-03T11:23:49Z.
- **Host:** `dev`, kernel `6.8.0-142-generic`, uid 0, 16 CPUs, 31 GiB.
- **Binaries:** `volvisord` SHA256
  `75710c05ae637c66035d1abc2f99cb4c35af2f858f974cfec13b57ea3c79491f`,
  `volvisor-witnessd` SHA256
  `745869ea0b36b472fd64bfceef311da942b49429cc8ef585820ccac5629e9436`;
  both report `--version` = `e551d26`. See [BINARIES.txt](BINARIES.txt).
  The raw drill headers do not record either hash or the version stamp, so
  these values are authored corroboration, not an anchor captured inside the
  raw logs.
- **Harness:** [`docs/runbooks/day-2/scripts/`](../../../runbooks/day-2/scripts/),
  started by `run-all.sh`; each drill writes its own `raw/<name>.log` with
  UTC timestamps, exact argv, stdout, stderr and exit status.
- **Environment and absent capabilities:** [ENVIRONMENT.txt](ENVIRONMENT.txt).

Every executed drill ended with `ASSERT SUMMARY: 0 failures`. Assertions are
in the scripts and in the raw logs; a value that was printed but not asserted
is described as observed, not as proven.

## D1 — Backup and restore of the control-plane state

**Pass condition (pre-stated):** with the daemon stopped, back up the journal
directory; perform a destructive drift; restore; the attachment state,
`state_revision`, evidence and retired-attachment tombstones must survive,
post-backup operations must not reappear, and a retired attachment id must
still be refused.

**What ran:** created `vol-d1-a`, `vol-d1-b`, `vol-d1-c`; attached A and B;
reported A `active`/`guest_confirmed`; detached B (creating tombstone
`att-d1-b`). Stopped `volvisord`, verified `flock` was free, recorded
checksums, created `state.tar` with `tar --xattrs --acls` over the journal
directory, restarted, then created `vol-d1-c` (post-backup drift). Stopped
again, moved the journal directory aside, restored the tar, verified
checksums, restarted and re-inspected.

**Observed:**

- pre-backup A: `state=active`, `state_revision=2`, `evidence=guest_confirmed`;
- backup tar SHA256 `eebc53f97af22a5657c775283259e01cd5197672243d81dab21184ebb89f6700`;
  backup duration 0.028 s;
- post-restore `journal.log` and `lvm-state.json` hashes are byte-identical
  to the backup source;
- post-restore A is still `active`/revision 2/`guest_confirmed`; tombstone
  `att-d1-b` survived;
- reattaching retired `att-d1-b` returns HTTP 409
  `IDEMPOTENCY_CONFLICT`;
- post-backup `vol-d1-c` returns HTTP 404 after restore;
- restore-to-health took 0.194 s on this host;
- the backing LV for `vol-d1-c` still exists in the VG after restore. The
  restore deliberately does not delete backend resources; the operator must
  reconcile them (see the runbook's consistency step).

**Evidence:** [raw/01-backup-restore.log](raw/01-backup-restore.log),
[records/d1-inspect-a-after.json](records/d1-inspect-a-after.json),
[records/d1-reattach-b-after.json](records/d1-reattach-b-after.json).

**Verdict: DRILLED** for the control-plane state directory on one host.
This is not a VM-disk backup: volume data and LVM metadata are outside the
scope. **RPO:** every control-plane mutation after the stopped backup is
lost (demonstrated with `vol-d1-c`). **RTO:** 0.194 s from restore start to a
healthy API in this lab; production RTO also includes the operator's
backend-resource reconciliation and data-path validation.

## D2 — Schema compatibility, upgrade on write, future-version refusal

**Pass condition (pre-stated):** the populated-directory compatibility test
must pass; a pre-versioning state file must be read and upgraded on the first
successful write without losing attachment state/revision/tombstones; a
file declaring a newer schema must be refused with a typed error and left
byte-identical; the same binary must reopen the supported file.

**What ran:** `cargo test --locked -p volvisord --test schema_upgrade`;
assembled a pre-versioning `lvm-state.json` from the golden N-1 attachment
fixture plus the tombstone fixture and the lab device claim; created the
matching LVs; started the daemon, created one new volume (first write);
stopped; inspected; set `schema_version` to 2 and tried to start; restored
version 1 and started again.

**Observed:**

- `schema_upgrade`: 2 tests passed, 0 failed;
- before startup the state file had **no** `schema_version`;
- after the first write `schema_version=1`;
- attachment survived with `state=active`, `state_revision=3`,
  `evidence=guest_confirmed`;
- tombstones survived as `att-retired-oldest,att-retired-newer`;
- future-schema startup exited 1 with
  `INTERNAL: unsupported durable-state schema version: ... declares
  schema_version 2, but this build supports at most 1 ...`;
- the future-schema file's SHA256 was unchanged;
- restoring version 1 let the same binary start and serve the volume.

**Evidence:** [raw/02-schema-upgrade.log](raw/02-schema-upgrade.log),
[records/d2-state-v1.json](records/d2-state-v1.json),
[records/d2-state-v2.json](records/d2-state-v2.json).

**Verdict: PARTIAL.**

- **DRILLED:** schema compatibility test, upgrade-on-write, future-version
  refusal, same-binary reopen.
- **NOT RUN — REQUIRES a second Volvisor binary/tag built from the other
  schema-version commit.** No older binary exists on this host and no tag has
  been released. A single binary reading its own prior format is not a live
  binary upgrade, and it is not a rollback drill.
- **ROLLBACK policy, not drilled:** there is no in-place downgrade. Rollback
  means stop, restore the pre-upgrade state directory, and start the matching
  binary. That loses every operation committed after the backup; backend
  resources must be reconciled. **RPO** for rollback = the backup point.
  **RTO** = restore time plus backend reconciliation, not measured for a
  real cross-version deployment.

## D3 — Lost or unreachable witness (service level)

**Pass condition (pre-stated):** a witness grant must create epoch 1/lease 1;
stopping the witness must make health and grant fail at the transport layer;
restarting it must recover the registration, epoch, holder, lease and commit
index from its journal; a competing grant during a live lease must return 409
`LEASE_HELD`; a self-release plus the fence window must let host-b acquire a
strictly newer epoch with a fencing proof.

**What ran:** started a local `volvisor-witnessd`; registered
`vol-d3-witness` with endpoints for host-a and host-b; granted host-a;
inspected; sent `SIGTERM` to the witness; tried health and a host-b grant;
restarted; inspected; attempted a live host-b grant; had host-a self-revoke;
waited 5 s (grace 2 s + suspend budget 2 s); granted host-b.

**Observed:**

- initial grant: `epoch=1`, `lease_id=1`, `lease_ttl_secs=120`;
- before outage: `current_epoch=1`, `holder=host-a`, `lease_state=live`,
  `lease_id=1`, `commit_index=2`;
- during outage: health and grant both `curl` exit 7 (connection refused);
  no epoch was minted;
- after restart: epoch 1, holder `host-a`, lease 1, commit index 2, both
  registration endpoints recovered;
- live host-b grant: HTTP 409 `LEASE_HELD`, `current_epoch=1`;
- host-a self-revoke returned fencing proof `retired_epoch=1`,
  `commit_index=3`;
- after the fence window, host-b grant: `epoch=2`, `lease_id=2`, fencing
  proof `retired_epoch=1`, `commit_index=4`;
- final view: `current_epoch=2`, holder `host-b`, two retirements.

**Evidence:** [raw/03-lost-witness.log](raw/03-lost-witness.log),
[records/d3-view-after.json](records/d3-view-after.json),
[records/d3-grant-b.resp.json](records/d3-grant-b.resp.json),
[journal/d3-witness-journal.log](journal/d3-witness-journal.log).

**Verdict: PARTIAL.**

- **DRILLED:** witness service loss/unreachability, journal-backed recovery,
  lease-holder refusal, self-release and successor epoch minting.
- **NOT RUN — REQUIRES a DRBD 9 kernel module and a real peered resource on
  a second host** (`drbdadm` and `/dev/drbd*` are absent here). This drill
  does not prove that a writer actually freezes or self-fences after losing
  the witness, and it does not prove storage-side fencing or data safety
  during a partition. **RPO for that unexecuted half is unknown.**
- **Witness availability:** no new authority can be minted while the witness
  is unreachable; an existing lease is not silently renewed. **RTO** for the
  witness service itself was not separately measured (the harness only waits
  up to 30 s for health).

## D4 — Torn/corrupt journal tail and corrupt state file

**Pass condition (pre-stated):** a structurally torn tail must be truncated
to the last complete frame, leaving the valid prefix byte-identical; replay
must still be idempotent and new appends must work; a complete CRC-valid
frame declaring an unsupported record version must be refused and **not**
truncated; a corrupt provider state file must be refused without rewriting
it.

**What ran:** created two volumes against a fake provider; recorded the
4-frame, 2218-byte journal; appended 64 garbage bytes; restarted; replayed a
create; stopped; cut the last frame to `start+8`; restarted; replayed and
appended; stopped; appended a complete CRC-valid `record_version=2` frame;
tried to start; restored the pre-future journal; then truncated a loop-LVM
`lvm-state.json` mid-JSON and tried to start the LVM provider.

**Observed:**

- garbage tail: 2218 → 2282 bytes, restart truncated back to 2218; SHA256
  identical to the baseline; replay returned `vol-d4-keep`;
- half frame: 1704 → 1696 bytes; recovered prefix hash matched the baseline
  prefix; a new append succeeded (journal 2803 bytes, 5 frames);
- future frame: 2803 → 2922 bytes; startup exit 1 with
  `unsupported durable-state schema version ... journal record state ...
  declares schema_version 2`; the file stayed at 2922 bytes and the prefix
  before the future frame was untouched;
- corrupt state: startup exit 1 with
  `lvm provider construction failed: ... failed to parse provider state ...
  EOF while parsing a value`; the corrupt file's SHA256 was unchanged.

**Evidence:** [raw/04-torn-journal-and-corrupt-state.log](raw/04-torn-journal-and-corrupt-state.log),
[records/d4-frames.py](records/d4-frames.py),
[records/d4-journal-baseline.log](records/d4-journal-baseline.log),
[journal/d4-journal.log](journal/d4-journal.log).

**Verdict: DRILLED** for the injected torn-tail and corrupt-file shapes.
This is not media bit-rot, filesystem corruption, or a partial page write
inside a complete frame; those need a different fault injector. The drill
proves the daemon never silently rewrites a **complete** record it cannot
understand, and never guesses at an unparseable state file. **RPO:** a torn
tail can drop only an incomplete last record; an acknowledged prior record
was not lost in this drill. **RTO:** restart to health on this host; a
corrupt state file requires operator restore, so it is backup-RPO bound.

## D5 — ENOSPC on journal and provider-state writes

**Pass condition (pre-stated):** with the journal filesystem full, a create
must fail closed (HTTP 500, no volume, no divergent in-memory state) and must
succeed after space is freed, with the torn append truncated on restart. With
the state filesystem full, a create must return a typed 500, leave the old
durable state intact, and the post-restart API must not invent the volume.
The drill records any divergence honestly rather than asserting it away.

**What ran, part A (journal):** 256 KiB tmpfs for the journal directory;
created a baseline volume; padded with real creates until the last-page slack
was 228 bytes; filled the tmpfs with a filler to ENOSPC; attempted
`vol-d5-full`; checked it was absent; stopped; removed the filler; restarted;
retried the same create.

**What ran, part B (state):** loop-backed LVM VG with the journal on ordinary
storage and the provider state on a separate 256 KiB tmpfs; created a
baseline volume; filled the state filesystem; attempted `vol-d5-lvm-full`;
checked the old state hash, `lvs`, the in-memory GET, restart, GET again, and
a subsequent create.

**Observed, part A:** journal size 12060 with 228 bytes of last-page slack;
after the failed append the size was 12288 and the API returned HTTP 500
`failed to write journal record: No space left on device (os error 28)`;
the volume was absent (404); after freeing space and restarting, the journal
was truncated back to 12060 with the same SHA256; the retry returned HTTP 200
and `vol-d5-full` was durable (24 frames, valid prefix 13166).

**Observed, part B — the finding:** baseline state 1023 bytes, SHA256
`687daef0...e15a4b2`; the create returned HTTP 500
`failed to write .../lvm-state.json.tmp: No space left on device (os error 28)`;
the old state file was byte-identical; **but** `lvs` already contained
`vol-vol-d5-lvm-full-58c0134e`, `GET /v2/volumes/vol-d5-lvm-full` returned
HTTP 200 from memory, and after a restart the GET returned HTTP 404 while
the LV remained. A new create after freeing space returned HTTP 200. The
daemon was not wedged and the old state was not corrupted, but the API
result, in-memory state and backend resource did not converge.

**Follow-up:** [#58](https://github.com/o3kio/volvisor/issues/58) — LVM create
returns 500 on state-save ENOSPC but leaves the LV and serves it from memory.

**Evidence:** [raw/05-enospc.log](raw/05-enospc.log),
[records/d5-create-full.resp.json](records/d5-create-full.resp.json),
[records/d5-lvm-full.resp.json](records/d5-lvm-full.resp.json),
[records/d5-lvm-full-inmem.json](records/d5-lvm-full-inmem.json),
[records/d5-lvm-full-after.json](records/d5-lvm-full-after.json).

**Verdict: DRILLED**, with the state-save finding above. **Journal ENOSPC:
fail-closed, no acknowledged state lost, no volume invented; RTO = free space
plus restart. State ENOSPC: the 500 is honest, but the leaked LV and
temporary phantom volume are an operator reconciliation hazard (and a
capacity leak); RTO = free space plus restart plus `lvremove`/reconcile.**
The tmpfs methodology (padding with real creates until the last page had
<500 bytes free) is recorded in the runbook; without that padding the
filesystem absorbs the small append and ENOSPC is not reached.

## D6 — Crash loop and single-writer lock contention

**Pass condition (pre-stated):** after five `SIGKILL`s, each while a create
is in flight, the journal must open with no trailing garbage and the durable
attached volume must still be served with the same attachment state,
revision and evidence. A second daemon and a second witness on the same state
directory must refuse to start; the first services must stay healthy.

**What ran:** created `vol-d6-base`, attached it (`att-d6-base`), reported it
`active`/`guest_confirmed`. For five iterations, started a create request in
the background, slept 20 ms, `kill -9` the daemon, recorded the client result,
restarted, parsed the journal, and re-inspected the volume. Then started a
second `volvisord` on the same journal directory and a second witness on the
same witness state directory.

**Observed:**

- every in-flight create died with `curl` exit 52 (`Empty reply from
  server`), HTTP status `000`; none is claimed to have completed;
- every restart parsed the whole journal with `valid_prefix == file_size`
  (7, 8, 9, 10, 11 frames across the five restarts);
- after every restart the volume was served with `state=active`,
  `state_revision=2`, `evidence=guest_confirmed`;
- second daemon: exit 1,
  `journal directory ... is already locked by another writer
  (single-writer enforcement): Resource temporarily unavailable`;
- the first daemon still returned HTTP 200;
- second witness: exit 1 with the same typed lock refusal; the first witness
  stayed healthy.

**Evidence:** [raw/06-crash-loop-and-lock.log](raw/06-crash-loop-and-lock.log),
[records/d6-base-after-5.json](records/d6-base-after-5.json),
[raw/d6-second.daemon.log](raw/d6-second.daemon.log),
[raw/d6-witness-second.log](raw/d6-witness-second.log).

**Verdict: DRILLED** for a same-host crash loop and local lock contention.
**Not drilled:** the killed creates were not retried with the same
`operation_id`, and backend-orphan reconciliation was not exercised; those
remain procedure-only pass items.
**RPO:** an in-flight request may fail; this drill makes no exactly-once
claim, and the client must retry with its operation id. Durable prior state
survived every kill. **RTO:** startup replay + health on this host (observed
within the harness's 30 s wait; not separately timed). The lock is a
same-directory guard, not a distributed consensus protocol; a second host
mounting the same directory is not tested.

## D7 — Credential and certificate rotation

**Pass condition (pre-stated):** a token not yet issued must be rejected; the
old token must work before rotation; both tokens must work during the overlap;
the old token must be rejected after its entry is removed; the new token must
still work. For TLS, the served certificate must match cert 1, replacing the
PEM bytes without a restart must **not** change the served certificate (no
hot reload), and a restart must serve cert 2 with a verifying HTTPS request.

**What ran, part A:** phase 1 with one admin and one operator; phase 2 with
old + new admin entries; phase 3 with only the new admin. Each phase used
`--check-config` and a restart. Part B: created a throwaway CA and two server
certificates with `subjectAltName=DNS:localhost,IP:127.0.0.1`; started the
daemon with stable `server.crt`/`server.key` paths over TLS; checked health
and the served fingerprint; replaced the PEM bytes without restarting;
checked again; restarted; checked again and created a volume over HTTPS.

**Observed, part A:** old token 200; operator token 200; new token 401 before
issue; during overlap old and new both 200 and both could list; after removal
old token 401 for reads and 401 for a mutation; new token 200. No fallback
for a removed token.

**Observed, part B:** cert1 fingerprint
`7F:54:F9:...:D3:64`; cert2 fingerprint `71:9E:40:...:17:35`; before rotation
the server served cert1; after the file swap without restart it **still**
served cert1 (no hot reload); after restart it served cert2; `openssl
s_client` reported `Verification: OK` and a TLSv1.3 session; an admin create
over HTTPS returned 200.

**Evidence:** [raw/07-credential-and-cert-rotation.log](raw/07-credential-and-cert-rotation.log),
[records/d7-old-removed.json](records/d7-old-removed.json),
[records/d7-cred-new-final.json](records/d7-cred-new-final.json).

**Verdict: DRILLED** for restart-based rotation on one host. **There is no
hot reload**; both token and certificate rotation require a controlled
restart. **RPO:** none for control-plane state (no state is changed by
rotation). **RTO:** two restarts in the overlap procedure (add + restart,
remove + restart) plus client rollout; not a zero-downtime rotation. Cross-host
TLS, mTLS client certificates, HSM/TPM key storage and external terminators
were not tested.

## D8 — Total host loss and re-admission — NOT RUN

**No command was executed for D8. No result exists.** A total-loss drill on
this host would be a graceful process stop, not a host loss, and there is no
second host to re-admit.

**NOT RUN — REQUIRES all of the following:**

- at least two real data hosts with independent power/network failure
  domains, plus a witness on a third (or fourth) failure domain;
- a real DRBD 9 module and peered resource (or a real Ceph cluster) carrying
  tenant data, so the surviving copy and the lost host's last acknowledged
  write can be measured;
- BMC/power control (or a documented equivalent) to make the loss
  **unplanned**, not a `systemctl stop`;
- a fencing/authority path that can revoke or expire the lost host's lease;
- an independent observer/oracle host for the write-trace verdict.

The planned procedure and the evidence that would be required are in the
[host-loss runbook](../../../runbooks/day-2/host-loss-and-rejoin.md). The
absence of a result is not a pass. No RPO or RTO is claimed for host loss.

## Cross-cutting limits

- **One host, one kernel, one clock.** No network partition, clock skew,
  cross-host witness hop or independent failure domain was exercised.
- **Loop-device LVM.** The LVM provider skips loop devices during discovery;
  the harness seeds an operator-provisioned `native_pool` claim. That is a
  lab scaffold and is documented in the runbook.
- **Debug binaries.** The evidence is from debug builds at `e551d26`; a
  release build at the same commit is not claimed to behave identically.
- **No production support.** This bundle does not change the support matrix,
  does not claim data durability, and does not close any release gate.
