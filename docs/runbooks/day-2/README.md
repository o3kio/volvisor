# Day-2 operations runbooks

Status: **five areas fully drilled and two partially drilled on one host; total host loss NOT RUN.** Read the
verdict on each page before using it. A runbook is a procedure; only the
evidence bundle is a result.

These are the recovery and maintenance procedures required by issue
[#46](https://github.com/o3kio/volvisor/issues/46). Every executable drill was
run at source commit `e551d26af7cdedb468797ff8049e0b91b9ab416d` with a clean
worktree on the single development host `dev`. The committed bundle is
[`docs/evidence/day2/2026-11-03-devhost/`](../../evidence/day2/2026-11-03-devhost/).
The scripts that produced it are in [`scripts/`](scripts/).

This page is the index. Each area page states **DRILLED**, **PARTIAL** or
**NOT RUN — REQUIRES ...** at the top. Where an area has a drilled part and an
undrilled part, both are named; the overall verdict is the weaker one.

## Verdict summary

| Area | Verdict | Procedure | Evidence |
|---|---|---|---|
| Backup and restore of control-plane state, with a consistency procedure | **DRILLED** | [backup-and-restore.md](backup-and-restore.md) | [D1](../../evidence/day2/2026-11-03-devhost/raw/01-backup-restore.log) |
| Upgrade and rollback across schema versions | **PARTIAL** — schema policy drilled; live binary upgrade/rollback **NOT RUN** | [upgrade-and-rollback.md](upgrade-and-rollback.md) | [D2](../../evidence/day2/2026-11-03-devhost/raw/02-schema-upgrade.log) |
| Lost or unreachable witness | **PARTIAL** — witness service drilled; storage-side fencing **NOT RUN** | [lost-witness.md](lost-witness.md) | [D3](../../evidence/day2/2026-11-03-devhost/raw/03-lost-witness.log) |
| Torn/corrupt journal tail and corrupt state file | **DRILLED** | [journal-and-state-corruption.md](journal-and-state-corruption.md) | [D4](../../evidence/day2/2026-11-03-devhost/raw/04-torn-journal-and-corrupt-state.log) |
| Disk full / ENOSPC during journal or state write | **DRILLED**, with a state-save finding ([#58](https://github.com/o3kio/volvisor/issues/58)) | [enospc.md](enospc.md) | [D5](../../evidence/day2/2026-11-03-devhost/raw/05-enospc.log) |
| Daemon crash loop and single-writer lock contention | **DRILLED** | [crash-loop-and-lock.md](crash-loop-and-lock.md) | [D6](../../evidence/day2/2026-11-03-devhost/raw/06-crash-loop-and-lock.log) |
| Credential and certificate rotation | **DRILLED** — restart-based; no hot reload | [credential-and-cert-rotation.md](credential-and-cert-rotation.md) | [D7](../../evidence/day2/2026-11-03-devhost/raw/07-credential-and-cert-rotation.log) |
| Total loss of one host and re-admission | **NOT RUN — REQUIRES** a real multi-host lab, a third failure domain and BMC/fencing | [host-loss-and-rejoin.md](host-loss-and-rejoin.md) | none |

## RPO / RTO expectations (honest, from the Drills)

These are **single-host lab observations**, not SLAs. "Not measured" means
exactly that; it is not zero.

| Scenario | RPO | RTO | Basis |
|---|---|---|---|
| Restore a stopped control-plane backup | Every mutation after the backup (demonstrated: one post-backup volume lost) | 0.194 s to a healthy API in the lab, plus backend reconciliation and data-path validation | D1 |
| Upgrade on write / future-version refusal | No loss in the supported-version path | Startup/restart time; not separately measured | D2 |
| Roll back to an older binary | The pre-upgrade backup point; later operations are discarded | Restore time + backend reconciliation; not measured | D2 policy, not drilled across binaries |
| Witness service outage | No state loss in the witness journal; no new authority is minted while unreachable | Witness restart to health; not separately measured | D3 |
| Storage-side lost-witness fencing | **Unknown** — not tested | **Unknown** — not tested | D3 limitation |
| Torn journal tail | Only the incomplete last record; acknowledged prior records survived | Restart to health | D4 |
| Corrupt state file | Backup point; the daemon refuses to guess | Restore + reconciliation; not measured | D4 |
| Journal ENOSPC | No acknowledged state lost; the failed create is absent | Free space + restart; measured in seconds in the lab | D5 part A |
| Provider-state ENOSPC | The failed create is not durable; its LV **is** leaked | Free space + restart + orphan-LV reconciliation | D5 part B, [#58](https://github.com/o3kio/volvisor/issues/58) |
| Crash loop with in-flight requests | The in-flight request may fail; prior durable state survived every SIGKILL | Restart/replay to health; not separately measured | D6 |
| Token/certificate rotation | No control-plane state loss | Two controlled restarts (overlap + removal), plus client rollout | D7 |
| Total host loss | **Unknown**; async replication can lose its tail | **Unknown** | D8 NOT RUN |

## How to read and run these pages

Every page has the same shape:

1. **Status** — the verdict and the exact limitation, with the evidence link.
2. **Scope and non-goals** — what the procedure does and does not prove.
3. **Preconditions** — stopped/healthy state, backups, authority, tools.
4. **Procedure** — copy-pasteable commands with the expected result.
5. **Verification** — the pass condition and the state to inspect afterwards.
6. **If it fails** — fail-closed handling; never hand-edit durable state.
7. **What was drilled** — the exact observation behind the verdict.

### Lab harness versus packaged units

The evidence was produced by [`scripts/run-all.sh`](scripts/run-all.sh), which
starts `volvisord` and `volvisor-witnessd` **directly** (no systemd) and uses
disposable loop-backed volume groups. That is deliberate: the drills need
`SIGKILL`, direct port allocation and local tmpfs mounts.

The procedures below show the daemon-level commands. On a packaged host,
replace "start the daemon" with `systemctl start volvisord` and "stop the
daemon" with `systemctl stop volvisord` (and the witness equivalent). The
journal lock, replay and fail-closed rules are the same. **Unit start under the
packaged install is not part of this evidence**; see
[`packaging/README.md`](../../../packaging/README.md).

### Build and run the executable drills

```sh
cd /path/to/volvisor
git rev-parse HEAD
git status --porcelain
cargo build
sudo RUN=/tmp/opencode/volvisor-day2-$(date -u +%Y%m%dT%H%M%SZ) \
  bash docs/runbooks/day-2/scripts/run-all.sh
```

Expected final lines: `all executable drills completed: <RUN>` and seven
`ASSERT SUMMARY: 0 failures` lines in `$RUN/raw/`. A dirty worktree is recorded
as `worktree_dirty: yes` and cannot support a clean-SHA claim.

Run one drill only:

```sh
sudo RUN=/tmp/opencode/volvisor-day2-d4 \
  bash docs/runbooks/day-2/scripts/d4.sh
```

The harness is destructive to the loop devices and volume groups it creates.
It does not touch pre-existing VGs. Do not run it on a production host.

## Cross-cutting safety rules

- **Stop the writer before backing up or restoring state.** `flock` is the
  single-writer guard; a running daemon owns it.
- **Never hand-edit `schema_version`, truncate a complete journal frame, or
  patch JSON to make a binary start.** The typed refusal is the safety
  property, not an obstacle.
- **There is no in-place downgrade.** Rollback is restore-then-old-binary.
- **A failed mutation may still have changed the backend.** Check `lvs` and the
  durable state after any 5xx; see [enospc.md](enospc.md) and
  [crash-loop-and-lock.md](crash-loop-and-lock.md).
- **Keep the old state directory.** Rename it aside; never delete the only
  copy before the restored daemon is verified.
- **Secrets never go on a command line.** Use an owner-only curl config,
  `umask 077`, and a controlled restart. See
  [credential-and-cert-rotation.md](credential-and-cert-rotation.md).
- **No output here is a production-support claim.** The support posture stays
  in [`SUPPORT_MATRIX.md`](../../../SUPPORT_MATRIX.md).

## Normative references

- [Durable-state schema compatibility and upgrade policy](../../schema-compatibility.md)
- [Configuration reference](../../configuration.md)
- [Security deployment: credentials and TLS rotation](../../security/deployment.md)
- [Evidence standards](../../evidence/README.md)
- [Nearline multi-host campaign](../nearline-multihost-campaign.md) — the multi-host run that D8 still needs
- [Volume API v2](../../../contracts/volume-api-v2.md)
- [Nearline replication contract v2](../../../contracts/nearline-replication-v2.md)
- [Witness/fencing authority plan](../../plans/2026-10-09-p4-witness-fencing-authority.md)

## Known gaps filed as issues

- [#58](https://github.com/o3kio/volvisor/issues/58) — LVM create returns 500
  on state-save ENOSPC but leaves the LV and serves it from memory.
- [#37](https://github.com/o3kio/volvisor/issues/37) — three-physical-host
  nearline campaign, the prerequisite for real host-loss evidence.
- [#38](https://github.com/o3kio/volvisor/issues/38) — three-host Rook-cell
  POC; not a substitute for D8.
