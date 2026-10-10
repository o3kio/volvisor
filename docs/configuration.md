# Volvisor daemon configuration

Status: documents the implemented configuration surface (P7-A; kept
current with the code by a repository test that validates the examples)
Date: 2026-10-10
Applies to: `volvisord` and `volvisor-witnessd` — the TOML file each
daemon takes at `--config`
Normative context: [ADR-0009 (distribution and packaging)](adr/0009-distribution-and-packaging.md),
[post-P5 implementation plan](plans/2026-10-10-post-p5-implementation-plan.md)
stage P7-A, [SPEC-0002 section 9](specs/SPEC-0002-volvisor-volume-virtualization.md) (security)
Examples: [examples/volvisor.toml](../examples/volvisor.toml),
[examples/witnessd.toml](../examples/witnessd.toml)

## Command-line surface

```text
volvisord --config <path> [--check-config]
volvisor-witnessd --config <path> [--check-config]
volvisord --version
volvisor-witnessd --version
```

- `--config` (`-c`) is required and names a TOML file (see below).
- `--check-config` loads and validates the configuration, logs a
  concise summary through the daemon's structured-JSON tracing stack,
  and **exits 0 without starting the server** — the install smoke
  surface (ADR-0009): the binary runs and accepts its configuration
  with no server, journal, or outbound connection started, though a
  hostname (non-IP-literal) `witness_url` is resolved during
  validation. An invalid configuration prints the typed error to
  stderr and exits non-zero.
- `--version` prints the compiled-in version stamp: the repository's
  `git describe --tags --always --dirty` at build time (while the
  repository carries no tags, the abbreviated commit hash), or the
  crate version when the build ran outside a git checkout.

## Secrets policy

Tokens (`admin_token`, `device_claim_token`, `witness_token`,
`witness_host_token`, `peer_api_token`, `auth_token`, `host_tokens`
values) are **never logged** (SPEC-0002 section 9). The `--check-config`
summary reports posture (bind address, provider, witness URL, lease
knobs, credential counts), never values. The witness URL, directory
paths and port ranges are configuration facts, not secrets.

## `volvisord` configuration

Unknown fields are rejected at parse time (`deny_unknown_fields`) — a
typo'd key is a startup refusal, never a silently ignored setting.

### Core fields

| Field | Type | Default | Controls |
|---|---|---|---|
| `listen` | socket address | required | HTTP bind of the Volume API v2 surface. Without `admin_token` the daemon refuses non-loopback binds (fail closed). |
| `journal_dir` | path | required | Directory holding the intent journal and its lock file. A second daemon on the same directory fails at startup, not mid-flight. |
| `provider` | `lvm` \| `ceph` \| `drbd` \| `fake` | required | Backend selection. `fake` is the in-memory test provider — never production. Each provider's required fields below apply only to that provider; well-formed foreign fields are ignored. |
| `admin_token` | string | unset | Static bearer token guarding the privileged surface (every mutating endpoint and the whole `/v2/admin` group, `GET` included). Unset = the loopback-only fail-closed mode (privileged requests get `401`). Must be non-empty when set. |
| `max_body_bytes` | integer (bytes) | 1048576 (1 MiB) | Maximum accepted request body size. |

### LVM provider (`provider = "lvm"`, native-local class)

| Field | Type | Default | Controls |
|---|---|---|---|
| `lvm_vg_prefix` | string | required | Volume-group prefix used for claimed pools. 1..=64 characters of alphanumerics, `-` and `_`. |
| `device_claim_token` | string | required | Scoped destructive-authorization token for device claim/release. Never logged. |
| `lvm_state_path` | path | `<journal_dir>/lvm-state.json` | Durable LVM provider state location. |
| `sysfs_root` | path | `/` | Filesystem root for read-only device discovery. **Test isolation only** — never set in a deployment. |

### Ceph provider (`provider = "ceph"`, external-cluster RBD adapter)

The cluster is operated outside volvisor; startup is refused unless
the cluster's reported fsid matches `ceph_cluster_fsid` exactly, the
pool exists and a health query succeeds (fail-closed — a mis-pointed
cluster is never adopted). The ceph CLI resolves the keyring itself;
volvisor never reads or logs key material.

| Field | Type | Default | Controls |
|---|---|---|---|
| `ceph_cluster_fsid` | UUID string | required | The one cluster volvisor may operate on; must match the cluster's reported fsid exactly (canonical 8-4-4-4-12 hex form). |
| `ceph_mon_hosts` | list of strings | required | Monitor addresses, 1..=9 entries, each `host`, `host:port` or a bracketed IPv6 literal (`[::1]`, `[::1]:6789`; ports 1..=65535). Joined into `-m` on every invocation. |
| `ceph_pool` | string | required | The single RBD pool volumes are created in. 1..=64 characters of alphanumerics, `-` and `_`. |
| `ceph_user` | string | `client.volvisor` | The Ceph entity passed as `--name`. Must be a full `client.<id>` entity name. |
| `ceph_state_path` | path | `<journal_dir>/ceph-state.json` | Durable ceph provider state location. |

### DRBD provider (`provider = "drbd"`, nearline-replicated class)

The local end of a single-primary replicated resource
([ADR-0007](adr/0007-drbd9-nearline-replication-provider.md)), verified
fail-closed at startup (module presence via procfs, node-name match
against `uname -n`).

| Field | Type | Default | Controls |
|---|---|---|---|
| `drbd_vg_name` | string | required | Operator-designated volume group holding the nearline backing LVs. Must exist; foreign LVs in it are never touched. |
| `drbd_node_name` | string | required | This host's DRBD `on` node name; validated against `uname -n` at startup so a wrong-host daemon never adopts a peer's resources. |
| `drbd_local_address` | IPv4 string | required | The local replication address (dotted quad, no port). |
| `drbd_peer_name` | string | required | The peer's `on` node name. |
| `drbd_peer_address` | string | required | The peer's replication address as `<ipv4>:<port>` (the peer end is operator-provisioned out of band). |
| `drbd_shared_secret_file` | path | required | Path of the peer shared secret, an owner-only file (peer authentication is a binding v1 invariant). Read at resource-generation time; never configured inline, never logged. |
| `drbd_config_dir` | path | `/etc/drbd.d` | Directory for the generated `volvisor-<resource>.res` files. |
| `drbd_port_min` / `drbd_port_max` | integer (port) | 7100 / 7199 | Inclusive local replication port range. |
| `drbd_minor_min` / `drbd_minor_max` | integer | 100 / 999 | Inclusive DRBD minor range; `drbd_minor_max` must not exceed 4095 (the kernel minor space). |
| `drbd_state_path` | path | `<journal_dir>/drbd-state.json` | Durable drbd provider state location. |
| `drbd_proc_root` | path | `/proc` | Procfs root for the DRBD module check. **Test isolation only** — never set in a deployment. |

### Writer-authority witness fields (drbd provider only)

Absent `witness_url` keeps the exact pre-authority behavior. Setting
it makes the provider witness-managed: every promotion acquires a
lease first, a background task renews leases and enforces the local
W5 deadline, and startup/reconcile fail closed on unproven primaries
([P4a plan](plans/2026-10-09-p4-witness-fencing-authority.md) §3/§6).

| Field | Type | Default | Controls |
|---|---|---|---|
| `witness_url` | URL string | unset | Base URL of the witness surface (plain `http://` in this phase). Must be a **third failure domain**: a loopback witness, or one colocated with either replication end, is refused at validation. |
| `witness_token` | string | required with `witness_url` | The shared bearer token; required for a non-loopback witness. On a v2 witness this token is read-only (inspect/health). Never logged. |
| `witness_host_token` | string | required with `witness_url` | This host's per-host credential (W8): every mutating witness call (grant/renew/revoke/register) authenticates as `drbd_node_name`. Never logged. |
| `witness_renewal_interval_secs` | integer (seconds) | required with `witness_url` | Renewal interval; must be positive and stay under half the witness's `lease_ttl_secs` (checked against every grant/renew response, since the TTL lives on the witness). |

Witness fields set without `witness_url` are refused (they would
silently imply a configuration the daemon does not follow).

### Migration and VMM tables (P4b, opt-in)

The `[migration]` table is inert until `enabled = true`; enabling
requires the drbd provider, a witness, an **existing** `snapshot_dir`,
the peer daemon's URL and credential, and a configured `[vmm]` table.

| Field | Type | Default | Controls |
|---|---|---|---|
| `migration.enabled` | boolean | `false` | Whether the migration surface (mobility routes, peer routes, the coordinator's retry task) is served. |
| `migration.snapshot_dir` | path | required when enabled | The shared snapshot directory; must exist locally (its cross-host readability is verified at `PREPARED` by the destination daemon). |
| `migration.peer_api_url` | URL string | required when enabled | The peer daemon's base URL (plain `http://`; by design colocated with the peer replication end — only the witness is a third failure domain). |
| `migration.peer_api_token` | string | required when enabled | The daemon-to-daemon credential (distinct from the witness and consumer tokens). Never logged. |
| `vmm.ch_remote_bin` | path | required when migration is enabled | Path of the `ch-remote` binary. |
| `vmm.api_socket_dir` | path | required when migration is enabled | Directory holding the per-VM API sockets (`{api_socket_dir}/{vm_id}.sock`). |

## `volvisor-witnessd` configuration

The witness is the third-party writer-authority service
([P4a plan](plans/2026-10-09-p4-witness-fencing-authority.md) §3): it
owns the durable epoch/lease registry and is deliberately not a
storage daemon. The lease knobs are **witness-side** by design — the
witness is what enforces the TTL, the grace bound and the W7 fence
wait; a storage daemon never duplicates these values. Unknown fields
are rejected at parse time.

| Field | Type | Default | Controls |
|---|---|---|---|
| `listen` | socket address | required | HTTP bind of the witness surface. Without `auth_token` the witness refuses non-loopback binds (fail closed). |
| `state_dir` | path | required | Directory holding the witness journal and lock file. A competing witness on the same directory fails at startup. |
| `auth_token` | string | unset | Shared bearer token; required for non-loopback binds. On the v2 witness protocol this token is **read-only** (inspect/health) — every mutation requires a host credential. Never logged. |
| `host_tokens` | table (host id → token) | empty | Per-host credentials (W8): a mutation is accepted only when the presented token resolves to the host the request asserts. Keys must be valid host ids; token values must be non-empty, unique, and distinct from `auth_token` (ambiguous identity resolution is refused). Never logged. |
| `lease_ttl_secs` | integer (seconds) | 60 | Lease time-to-live (W5/W7). A writer's lease expires after this; renewals keep it alive. Must be positive. |
| `lease_grace_secs` | integer (seconds) | 5 | Response-latency bound the fence window assumes (W5). Must cover witness round-trip latency under load — an operator network responsibility. Must be positive. |
| `suspend_budget_secs` | integer (seconds) | 5 | Self-fencing suspend budget (W7): the assumed time for a writer to freeze its data path (`drbdsetup suspend-io`). Must be positive. |

## The checked-in examples

[examples/volvisor.toml](../examples/volvisor.toml) (the richest
validating shape: the drbd provider with witness and inert migration)
and [examples/witnessd.toml](../examples/witnessd.toml) (the
third-host witness deployment shape) are the annotated future
`/etc/volvisor/` content. A repository test runs each through
`--check-config`, so the examples and the validator cannot drift
apart. Token values in the examples are placeholders — replace them
with generated secrets in any real deployment.
