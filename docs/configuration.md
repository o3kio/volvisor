# Volvisor daemon configuration

Status: documents the implemented configuration surface (P7-A; kept
current with the code by a repository test that validates the examples)
Date: 2026-11-02
Applies to: `volvisord` and `volvisor-witnessd` — the TOML file each
daemon takes at `--config`
Normative context: [ADR-0009 (distribution and packaging)](adr/0009-distribution-and-packaging.md),
[post-P5 implementation plan](plans/2026-10-10-post-p5-implementation-plan.md)
stage P7-A, [SPEC-0002 section 9](specs/SPEC-0002-volvisor-volume-virtualization.md) (security)
Examples: [examples/volvisor.toml](../examples/volvisor.toml),
[examples/witnessd.toml](../examples/witnessd.toml)
Security deployment guide: [security/deployment.md](security/deployment.md)

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

Tokens (`admin_token`, every `[credentials]` token, `device_claim_token`,
`witness_token`, `witness_host_token`, `peer_api_token`, `auth_token`,
`host_tokens` values) are **never logged** (SPEC-0002 section 9). The
`--check-config` summary reports posture (bind address, provider, witness URL,
lease knobs, credential counts, transport mode), never values. Credential ids
are audit identities, not secrets; the witness URL, directory paths, TLS file
paths and port ranges are configuration facts, not secrets.

Private TLS keys are loaded into rustls and never formatted or logged. No
configuration has an "accept any certificate" switch: outbound `https://`
verification always checks the certificate chain and the URL hostname/IP
against the configured private CA or the Mozilla root program.

## `volvisord` configuration

Unknown fields are rejected at parse time (`deny_unknown_fields`) — a
typo'd key is a startup refusal, never a silently ignored setting.

### Core fields

| Field | Type | Default | Controls |
|---|---|---|---|
| `listen` | socket address | required | Bind of the Volume API v2 surface. A non-loopback bind is served over native TLS only when `[tls] enabled = true`; otherwise startup refuses unless `allow_insecure = true` is set explicitly. A tokenless daemon may bind loopback only. |
| `journal_dir` | path | required | Directory holding the intent journal and its lock file. A second daemon on the same directory fails at startup, not mid-flight. |
| `provider` | `lvm` \| `ceph` \| `drbd` \| `fake` | required | Backend selection. `fake` is the in-memory test provider — never production. Each provider's required fields below apply only to that provider; well-formed foreign fields are ignored. |
| `admin_token` | string | unset | Legacy static bearer token: one full **admin** principal (audit id `admin_token`). Unset = no legacy admin principal; mutations/admin still fail closed. Must be non-empty when set. Prefer `[credentials]` for named principals and role separation. |
| `credentials` | tables, below | empty | Named admin / operator / read-only bearers. When any entry exists, read routes also require authentication. |
| `tls` | table, below | disabled | Native TLS listener identity and outbound trust/mTLS. |
| `allow_insecure` | boolean | `false` | Explicit opt-in to plain HTTP on a non-loopback listener and to plain-HTTP witness/peer URLs. Emits a startup warning. Never a silent fallback. |
| `max_body_bytes` | integer (bytes) | 1048576 (1 MiB) | Maximum accepted request body size, enforced for sized and streamed bodies. |
| `admission` | table, below | defaults | HTTP admission control: read timeout, concurrency cap, opt-in per-principal rate limit and pagination maximum. |
| `journal` | table, below | defaults | Bounded intent-journal retention and compaction. Defaults bound restart memory and log growth. |

### Admission control (`[admission]`)

The long-operation model and operator guidance are documented in
[admission control](admission-control.md). Read timeouts bound **read-only**
routes only; journaled mutations are never silently aborted by a wall-clock
timeout.

| Field | Type | Default | Controls |
|---|---|---|---|
| `admission.read_timeout_secs` | integer (seconds) | `30` | Fallback bound applied to every read-only provider call; a `[admission.route_timeouts]` override wins for its matched path. Expiry is `504 REQUEST_TIMEOUT`. |
| `admission.queue_timeout_secs` | integer (seconds) | `5` | Maximum wait for a global concurrency permit; expiry is `503 OVERLOADED` with `Retry-After`. |
| `admission.max_concurrent_requests` | integer | `256` | Global in-flight request cap. `/healthz`, `/v2/healthz`, `/readyz`, `/v2/readyz` and `/metrics` bypass it. |
| `admission.rate_limit_per_second` | integer | `0` (disabled) | Per-principal token refill rate; opt-in, `0` preserves legacy behavior. |
| `admission.rate_limit_burst` | integer | `0` (disabled) | Per-principal burst; ignored while the rate is `0`, must be at least one when the rate is non-zero. |
| `admission.max_list_limit` | integer | `1000` | Maximum accepted `GET /v2/volumes?limit=` (1..=1000; never above the contract maximum). |

Optional per-route read timeouts live in `[admission.route_timeouts]`. Each
key is optional, is whole seconds in `1..=3600`, and names an axum matched
path; an absent key falls back to `admission.read_timeout_secs`. Unknown
keys are refused at startup. Mutations are never bounded by these entries.

| Key | Matched path |
|---|---|
| `list_volumes_secs` | `GET /v2/volumes` |
| `inspect_volume_secs` | `GET /v2/volumes/{volume_id}` |
| `capabilities_secs` | `GET /v2/capabilities` |
| `observe_migration_secs` | `GET /v2/migrations/{migration_id}` |
| `list_devices_secs` | `GET /v2/admin/devices` |
| `device_ownership_secs` | `GET /v2/admin/devices/{device_id}/ownership` |
| `check_mobility_secs` | `POST /v2/vms/{vm_id}/check-mobility` (read-only) |
| `verify_lineage_secs` | `POST /v2/internal/peer/verify-lineage` (read-only) |
| `peer_health_secs` | `GET /v2/internal/peer/health` |

```toml
[admission.route_timeouts]
list_volumes_secs = 60
inspect_volume_secs = 10
check_mobility_secs = 120
verify_lineage_secs = 120
```

Per-principal rate limiting is **opt-in and disabled by default** (`0`/`0`)
so unmodified deployments keep their existing client behavior. Enable it
explicitly when you want admission-level fairness:

```toml
[admission]
rate_limit_per_second = 100
rate_limit_burst = 200
```

On any shared network, configure scoped `[credentials]` **and** enable rate
limiting together: without scoped credentials every legacy caller shares the
single `anonymous` bucket. The concurrency cap, read timeout and body limit
remain on by default because they shed overload without changing the success
path for well-behaved clients.

### Journal retention and compaction (`[journal]`)

The intent journal is fsynced per record and compacted during an ordinary
append when a threshold is crossed. Compaction runs synchronously while the
journal lock is held, after the triggering record is already durable;
it atomically rewrites `journal.log` as one `snapshot` frame (tmp file,
fsync, rename, directory fsync). A failed compaction never fails that append
and is retried on the next one. Size the thresholds well above the steady
operation rate so compaction stays rare and its pause stays bounded.

| Field | Type | Default | Controls |
|---|---|---|---|
| `journal.max_retained_operations` | integer | `65536` | Target bound on terminal operations kept in memory and in the snapshot (1..=10000000). In-flight intents are always kept. Compaction may retain fewer when the encoded snapshot would exceed the one-frame limit. The terminal-count trigger fires at **twice** this value, so the in-memory registry may hold up to 2× the configured value between compactions; after the first compaction the trigger also waits for at least 64 new log records, so a tiny bound cannot rewrite the snapshot on every append. The growth floors are per daemon lifetime and reset on restart, so a restart may compact once against the configured thresholds even when the compacted snapshot is small. |
| `journal.max_tombstones` | integer | `65536` | Upper bound on newest evicted-operation fingerprints kept by the tombstone ring (1..=10000000). A zero ring would silently re-execute retries, so zero is refused. The effective ring is capped by the one-frame limit. |
| `journal.max_log_records` | integer | `500000` | Compact after this many valid records in the current log (2..=100000000). A successful compaction requires meaningful new growth (at least 64 records) before this trigger can fire again, so a tiny value cannot rewrite the snapshot on every append. |
| `journal.max_log_bytes` | integer (bytes) | 67108864 (64 MiB) | Compact after the log reaches this size; `0` disables the byte trigger (the record-count and retention triggers still apply). After a successful compaction the trigger requires at least 1 MiB of new growth. |

**One-frame limit.** A snapshot is written as a single journal frame, whose
payload is capped at 8 MiB. If the configured retention would produce a
larger snapshot, compaction first evicts the oldest unpinned terminal
operations (recording their fingerprints as tombstones, so retries still
fail closed) and then shortens the tombstone ring until the snapshot fits.
The effective retention can therefore be below the configured upper bounds;
this keeps compaction making progress instead of failing forever. If
in-flight or pinned operations alone exceed one frame, compaction fails
closed, `/metrics` shows `journal_compaction_failures_total` increasing, and
the next append retries.

```toml
[journal]
max_retained_operations = 65536
max_tombstones = 65536
max_log_records = 500000
max_log_bytes = 67108864
```

A retry whose operation was evicted **within** the tombstone window answers
`410 OPERATION_EXPIRED` (metric `operations_total{outcome="expired"}`)
instead of re-executing a possibly-completed mutation. An operation evicted
before the ring's window is indistinguishable from a new one, so keep
`max_tombstones` above the longest client retry horizon. The witness registry
folds the full record history and refuses a compacted journal; it must not
share `journal_dir` with a control-plane journal. See
[schema compatibility](schema-compatibility.md).

### TLS and transport fields

Native TLS is rustls (no OpenSSL dependency). The server identity must be
provisioned explicitly: the daemon never generates a throwaway certificate.

| Field | Type | Default | Controls |
|---|---|---|---|
| `tls.enabled` | boolean | `false` | Serve the Volume API listener over TLS. Requires `tls.cert_file` and `tls.key_file`; their presence and regular-file shape are checked at startup and by `--check-config`. |
| `tls.cert_file` | path | required when enabled | PEM server certificate chain. |
| `tls.key_file` | path | required when enabled | PEM private key matching the served certificate. Never logged. |
| `tls.client_ca_file` | path | unset | Optional PEM client CA. When set, the listener requires a client certificate issued by one of these roots (mTLS); a client without one fails the TLS handshake. |
| `tls.client.ca_file` | path | unset | Outbound trust bundle for `https://` witness/peer endpoints. Unset uses the Mozilla root program. |
| `tls.client.cert_file` / `tls.client.key_file` | path | unset | Optional outbound mTLS identity. Must be configured together; both files must exist. |
| `allow_insecure` | boolean | `false` | Permits a plain-HTTP non-loopback listener and plain-HTTP witness/peer URLs. A deliberate downgrade with a startup warning; there is no automatic `https` → `http` fallback. |

Setting `tls.cert_file`, `tls.key_file` or `tls.client_ca_file` while
`tls.enabled = false` is refused as orphan configuration. Loopback plain HTTP
remains available for local development without `allow_insecure`.

### Scoped credentials and authorization

`[credentials]` holds named principal entries per role. Each entry is
`{ id = "<stable-audit-id>", token = "<bearer-token>" }` and may be written as
an array of tables or an inline array:

```toml
[[credentials.admin]]
id = "ops-admin-2026-11"
token = "generated-secret"

[[credentials.operator]]
id = "storage-operator-2026-11"
token = "generated-secret"

[[credentials.read_only]]
id = "monitoring-2026-11"
token = "generated-secret"
```

Ids are 1..=64 characters of ASCII alphanumerics, `-`, `_` or `.`. Ids and
token values must be unique across every role; no scoped token may equal the
legacy `admin_token` (ambiguous identity resolution is a startup refusal).
Tokens are compared in constant time.

When **any** `[credentials]` entry is configured, read routes require at least
the read-only role: volume listing/inspection, capabilities, migration
observation and `/metrics`. `/healthz`, `/v2/healthz`, `/readyz` and
`/v2/readyz` stay open for probes.
Without scoped credentials, the legacy behavior is preserved: `admin_token` (or
a tokenless loopback dev instance) leaves reads open while mutations and
`/v2/admin/*` remain token-guarded. The full matrix and threat model are in the
[security deployment guide](security/deployment.md).

Role summary:

| Surface | Required role |
|---|---|
| `GET /healthz`, `GET /v2/healthz`, `GET /readyz`, `GET /v2/readyz` | public |
| `GET /v2/volumes`, `GET /v2/volumes/{id}`, `GET /v2/capabilities`, `GET /metrics`, `GET /v2/migrations/{id}` | read-only (when scoped credentials are configured) |
| Volume create/delete/attach/detach/attachment-state/grow/move-backing, migration prepare/transfer/abort | operator |
| `/v2/admin/*` (device/admin inventory included) and `POST /v2/vms/{id}/check-mobility` | admin |
| `/v2/internal/peer/*` | the separate daemon-to-daemon `peer_api_token` |

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
| `witness_url` | URL string | unset | Base URL of the witness surface. `https://` is the production shape; `http://` is allowed only on loopback or with explicit `allow_insecure = true`. Must be a **third failure domain**: a loopback witness, or one colocated with either replication end, is refused at validation. |
| `witness_token` | string | required with `witness_url` | The shared bearer token; required for a non-loopback witness. On a v2 witness this token is read-only (inspect/health). Never logged. |
| `witness_host_token` | string | required with `witness_url` | This host's per-host credential (W8): every mutating witness call (grant/renew/revoke/register) authenticates as `drbd_node_name`. Never logged. |
| `witness_renewal_interval_secs` | integer (seconds) | required with `witness_url` | Renewal interval; must be positive and stay under half the witness's `lease_ttl_secs` (checked against every grant/renew response, since the TTL lives on the witness). |

Witness fields set without `witness_url` are refused (they would
silently imply a configuration the daemon does not follow). An `https://`
witness URL verifies the witness certificate with `tls.client.ca_file` when
set, otherwise the Mozilla roots; a private-CA or mTLS identity is configured
under `[tls.client]`.

### Migration and VMM tables (P4b, opt-in)

The `[migration]` table is inert until `enabled = true`; enabling
requires the drbd provider, a witness, an **existing** `snapshot_dir`,
the peer daemon's URL and credential, and a configured `[vmm]` table.

| Field | Type | Default | Controls |
|---|---|---|---|
| `migration.enabled` | boolean | `false` | Whether the migration surface (mobility routes, peer routes, the coordinator's retry task) is served. |
| `migration.snapshot_dir` | path | required when enabled | The shared snapshot directory; must exist locally (its cross-host readability is verified at `PREPARED` by the destination daemon). |
| `migration.peer_api_url` | URL string | required when enabled | The peer daemon's base URL. `https://` is the production shape; plain `http://` is refused for a non-loopback peer unless `allow_insecure = true`. By design the peer is colocated with the peer replication end — only the witness is a third failure domain. |
| `migration.peer_api_token` | string | required when enabled | The daemon-to-daemon credential (distinct from the witness and consumer tokens). Never logged. |
| `vmm.ch_remote_bin` | path | required when migration is enabled | Path of the `ch-remote` binary. |
| `vmm.api_socket_dir` | path | required when migration is enabled | Directory holding the per-VM API sockets (`{api_socket_dir}/{vm_id}.sock`). |
| `vmm.cloud_hypervisor_bin` | path | unset | Path of the `cloud-hypervisor` binary, probed once at daemon startup for the grow-notification version gate (P6-B; lvm provider). Never probed by `--check-config`. |
| `vmm.minimum_version` | semver string | unset | The minimum cloud-hypervisor version the grow notification accepts (`37.0`, `37.0.1`, `v37.0.0`; a leading `v` is tolerated). |

### The grow-notification version gate (P6-B)

Online grow (Volume API v2 §4A) is a two-step act: the backing
grows, then the VMM is told (`PUT /api/v1/vm.resize-disk` over the
per-VM API socket) so the guest can see the new capacity. The
notification needs a VMM that handles host block devices grown
outside the VMM — support added upstream in
[cloud-hypervisor PR #7948](https://github.com/cloud-hypervisor/cloud-hypervisor/pull/7948).
The `vmm.minimum_version` gate exists for that reason: the operator
pins the release they have actually qualified, the daemon probes
`<cloud_hypervisor_bin> --version` exactly once at startup, and
below the minimum (or with either field unset, or with an
unrunnable/unparseable binary) the gate refuses — fail-closed. A
refusal never fails the grow: the volume's backing is already
resized, and the response honestly says `retry_required` with the
recorded reason until the configuration is fixed. The repository
pins no default release. What has been live-verified (the two
recorded Tier V runs, both committed under
`docs/evidence/tier-v/`): cloud-hypervisor **v43.0.0 lacks the
`vm.resize-disk` route** (the notification's PUT answers HTTP 404
— the grow reports `retry_required` carrying it), and **v51.1
carries it** (the PUT answered 2xx and the notification converged
`notified`, with the request body `{"id", "desired_size"}` —
`desired_size` in bytes — per v51.1's published OpenAPI).
Between those live-verified points, upstream's own pinned OpenAPI
places the route's introduction between v49.0 (absent) and v50.0
(present) — a document-verified bound, not a live observation. A
`minimum_version` at or above the route's introduction is what
makes the gate meaningful (the recorded runs use `51.0.0` as the
boundary pin, and record it both ways: proven on a route-carrying
binary, fail-closed below it); a minimum the binary does not meet
refuses the notification before any VMM call. Consumer
attachments carry the VMM-side
disk id (`vmm_disk_id` on the attach request) the notification
addresses; an attached volume without one cannot be notified and
reports the same recorded refusal. Outstanding notifications are
retried by a background pass every 5 seconds and reconciled at
startup; a detached volume's pending notification resolves
`not_applicable`.

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
| `listen` | socket address | required | Bind of the witness surface. A non-loopback bind requires native TLS (`[tls] enabled = true`) or explicit `allow_insecure = true`; without `auth_token` it also refuses non-loopback binds (fail closed). |
| `state_dir` | path | required | Directory holding the witness journal and lock file. A competing witness on the same directory fails at startup. |
| `auth_token` | string | unset | Shared bearer token; required for non-loopback binds. On the v2 witness protocol this token is **read-only** (inspect/health) — every mutation requires a host credential. Never logged. |
| `host_tokens` | table (host id → token) | empty | Per-host credentials (W8): a mutation is accepted only when the presented token resolves to the host the request asserts. Keys must be valid host ids; token values must be non-empty, unique, and distinct from `auth_token` (ambiguous identity resolution is refused). Never logged. |
| `lease_ttl_secs` | integer (seconds) | 60 | Lease time-to-live (W5/W7). A writer's lease expires after this; renewals keep it alive. Must be positive. |
| `lease_grace_secs` | integer (seconds) | 5 | Response-latency bound the fence window assumes (W5). Must cover witness round-trip latency under load — an operator network responsibility. Must be positive. |
| `suspend_budget_secs` | integer (seconds) | 5 | Self-fencing suspend budget (W7): the assumed time for a writer to freeze its data path (`drbdsetup suspend-io`). Must be positive. |
| `tls.enabled` | boolean | `false` | Serve the witness listener over TLS. Requires `tls.cert_file` and `tls.key_file`, checked at startup and by `--check-config`. |
| `tls.cert_file` / `tls.key_file` | path | required when enabled | PEM certificate chain and matching private key. Never logged. |
| `tls.client_ca_file` | path | unset | Optional PEM client CA: when set, every storage daemon must present a certificate issued by one of these roots (mTLS). |
| `allow_insecure` | boolean | `false` | Explicit opt-in to plain HTTP on a non-loopback witness listener; emits a startup warning. A real deployment should use native TLS instead. |

As with `volvisord`, setting TLS file fields while `tls.enabled = false` is
refused, and loopback plain HTTP remains valid without `allow_insecure`.

## Durable-state schema compatibility

The daemon's durable files are versioned and fail-closed: the intent journal
(`journal.log`, `record_version`), the provider state files
(`lvm-state.json`, `ceph-state.json`, `drbd-state.json`, `schema_version`),
the migration records (`migrations/*.json`) and the peer preparations
(`peer-preparations/*.json`). A file written by a newer schema refuses
startup with a typed `INTERNAL` error instead of being partially decoded or
rewritten; unknown fields at a supported version are rejected. Older
supported files are read in place and upgraded by their first successful
write. There is no in-place downgrade — rollback requires restoring a
pre-upgrade backup of the whole state directory and the matching binary.

Take that backup while the daemon is stopped, and keep it until the new
version is proven. See
[schema compatibility](schema-compatibility.md) for the exact store paths,
N-1/N-2 fixture matrix and rollback steps.

## The checked-in examples

[examples/volvisor.toml](../examples/volvisor.toml) (the richest
validating shape: the drbd provider with witness and inert migration,
https witness/peer URLs, and commented TLS/credential tables) and
[examples/witnessd.toml](../examples/witnessd.toml) (the third-host
witness deployment shape, plain HTTP only through an explicit
`allow_insecure = true` with commented native-TLS configuration) are the
annotated future `/etc/volvisor/` content. A repository test runs each
through `--check-config`, so the examples and the validator cannot drift
apart. Token values in the examples are placeholders — replace them with
generated secrets in any real deployment.
