# Transport and authentication deployment guide

Status: implemented security posture for the Volume API and witness
Date: 2026-11-02
Applies to: `volvisord` and `volvisor-witnessd`
Normative context: [SPEC-0002 section 9](../specs/SPEC-0002-volvisor-volume-virtualization.md)
(security), [ADR-0009](../adr/0009-distribution-and-packaging.md)
Configuration reference: [docs/configuration.md](../configuration.md)

This guide describes how to deploy the transport and authentication
controls that are implemented today. It deliberately separates what the
software enforces from what remains an operator or environment
responsibility.

## What is protected

- **Listener transport.** Native TLS (rustls) can protect the Volume API
  and witness listeners. A non-loopback plain-HTTP listener is refused
  unless the operator explicitly sets `allow_insecure = true`; that setting
  emits a startup warning.
- **Outbound control-plane traffic.** `https://` witness and peer URLs are
  supported. Outbound certificate and hostname/IP verification is always
  performed against the configured private CA bundle or the Mozilla root
  program. There is no automatic `https://` → `http://` fallback: a
  configured `https://` URL either validates the peer or fails the request.
- **Peer authentication.** HTTPS verifies the server's identity. Optional
  mTLS verifies the client's identity at the witness or peer listener.
- **API authentication and authorization.** Named admin, operator and
  read-only bearer credentials are enforced at one middleware boundary and
  re-checked by the handlers' extractors as defense in depth. Bearer tokens
  are compared in constant time.
- **Read authorization.** When any scoped `[credentials]` entry is
  configured, volume metadata, capabilities and `/metrics` require at least
  read-only access. `/healthz` stays open for probes.
- **Audit attribution.** Every request except a successful liveness probe
  emits one structured audit event naming the resolved principal and role,
  method, matched route, status/outcome and — when the journal pipeline ran —
  the operation id. Tokens are never included.

## What is not protected

The controls above do **not** protect against:

- a compromised storage host, witness host, container or process;
- a reader of configuration files, process memory, logs you copied
  elsewhere, or a backup that contains secrets;
- a misissued or stolen certificate from a trusted CA;
- a malicious or mistaken holder of an admin credential — admin is
  intentionally full control;
- denial of service, resource exhaustion, traffic analysis or metadata
  leakage at the network layer. Application-level admission control
  ([admission control](admission-control.md)) bounds per-principal request
  rate, in-flight concurrency and body size, but it is not a network-level
  DDoS defense;
- a misconfigured external TLS terminator that forwards plaintext to an
  untrusted network;
- tamper-evident audit storage. The daemon emits structured logs; retention,
  shipping, access control and integrity of the audit sink remain deployment
  responsibilities.

TLS authenticates endpoints and protects traffic in transit. It does not
make a host trustworthy and it does not replace the witness failure-domain
and fencing rules.

## Credential and role matrix

The legacy top-level `admin_token` is one admin principal with audit id
`admin_token`. `[credentials]` adds named principals in three ordered roles:
read-only < operator < admin.

| Surface | Required role |
|---|---|
| `GET /healthz`, `GET /v2/healthz` | public |
| `GET /v2/capabilities`, `GET /metrics` | read-only when scoped credentials exist; otherwise open in the legacy/tokenless posture |
| `GET /v2/volumes`, `GET /v2/volumes/{volume_id}` | read-only when scoped credentials exist; otherwise open in the legacy/tokenless posture |
| `GET /v2/migrations/{migration_id}` | read-only when scoped credentials exist; otherwise open in the legacy/tokenless posture |
| `POST/DELETE` volume lifecycle routes, attach, detach, attachment-state, grow, move-backing | operator or admin |
| Migration prepare, transfer and abort | operator or admin |
| All `/v2/admin/*` routes, including admin `GET`s | admin |
| `POST /v2/vms/{vm_id}/check-mobility` | admin |
| `/v2/internal/peer/*` | the separate daemon-to-daemon peer token, not an API role |

The compatibility posture matters: if only the legacy `admin_token` is
configured, read routes remain open, exactly as before scoped credentials
existed. A tokenless daemon leaves reads open too but fails closed for every
mutation and admin route, and may bind loopback only. In both modes, **do not
expose the listener to an untrusted network**. For any non-loopback
deployment, configure scoped credentials and TLS.

Scoped credentials are declared as named entries:

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

Ids are audit identities, not secrets, but they must be stable and unique.
Tokens must be unique across every role and distinct from `admin_token`. A
duplicate id or token is a startup refusal, because resolving one bearer
string to two identities would make authorization and audit ambiguous.

## Deployment profiles

### 1. Loopback development and test

```toml
listen = "127.0.0.1:8787"
journal_dir = "/var/lib/volvisor/journal"
provider = "fake"
# No admin_token and no [credentials]: reads open, mutations fail closed.
```

This is the only profile where plain HTTP is appropriate without an explicit
downgrade. It is reachable only from the local host. Even here, mutations are
rejected without a credential, so use it for development and conformance
tests, not as a production control plane.

A loopback listener with an `admin_token` or scoped credentials is also valid
and is a reasonable single-host development setup. Loopback plain HTTP is not
a substitute for TLS on a real management network.

### 2. Single host with native TLS

```toml
listen = "10.0.0.1:8787"
journal_dir = "/var/lib/volvisor/journal"
provider = "lvm"
lvm_vg_prefix = "volvisor"
device_claim_token = "replace-with-a-generated-secret"

[[credentials.operator]]
id = "storage-operator-2026-11"
token = "replace-with-a-generated-secret"

[tls]
enabled = true
cert_file = "/etc/volvisor/tls/server.crt"
key_file = "/etc/volvisor/tls/server.key"
# client_ca_file = "/etc/volvisor/tls/client-ca.crt"   # mTLS
```

Rules enforced at startup:

- a non-loopback bind requires TLS or explicit `allow_insecure = true`;
- a non-loopback bind with no credential at all is refused;
- `tls.enabled = true` requires the certificate and key paths and their
  regular-file existence; missing or empty files are refused;
- orphan TLS file paths with `tls.enabled = false` are refused;
- a client certificate and key must be configured together;
- a bad certificate/key pair is refused when the server configuration is
  built, before the listener serves requests.

`--check-config` validates the configuration and the configured files'
existence without starting the listener. It does not replace a startup test
with the real PEM material: the daemon parses the certificate/key pair when
it builds the rustls configuration and fails startup if rustls rejects it.

### 3. Multi-host storage and witness

All control-plane hops should use HTTPS:

```toml
# Storage daemon
witness_url = "https://witness.example.net:9101"
witness_token = "replace-with-a-generated-secret"
witness_host_token = "replace-with-host-a-secret"
witness_renewal_interval_secs = 15

[migration]
enabled = true
snapshot_dir = "/var/lib/volvisor/snapshots"
peer_api_url = "https://10.0.0.2:7780"
peer_api_token = "replace-with-a-generated-peer-secret"

[tls.client]
ca_file = "/etc/volvisor/tls/private-ca.crt"
# For mTLS to the witness or peer:
# cert_file = "/etc/volvisor/tls/client.crt"
# key_file = "/etc/volvisor/tls/client.key"

# Witness daemon
# listen = "10.0.0.3:9101"
# [tls]
# enabled = true
# cert_file = "/etc/volvisor-witness/tls/server.crt"
# key_file = "/etc/volvisor-witness/tls/server.key"
# client_ca_file = "/etc/volvisor-witness/tls/client-ca.crt"   # mTLS
```

Deployment requirements:

1. Certificates must carry a subject alternative name matching the URL the
   storage daemon uses (DNS name or IP address). Verification is not
   disabled for convenience.
2. Use `tls.client.ca_file` for a private CA. Without it, outbound HTTPS
   verifies against the Mozilla root program, which is usually wrong for a
   private control plane.
3. The witness must remain a **third failure domain**. TLS does not relax
   the loopback/colocation refusal; the URL host is resolved and checked
   against both replication ends.
4. Each storage host presents its own `witness_host_token`; the shared
   `witness_token` is read-only on the v2 witness. Keep these distinct.
5. The peer token is distinct from both witness credentials and API bearer
   tokens. It guards `/v2/internal/peer/*`.
6. If the witness uses `tls.client_ca_file`, every storage daemon that
   mutates or inspects it needs a client certificate and key under
   `[tls.client]`; a client without a certificate fails the TLS handshake,
   not just the application authorization check.

The repository's same-host evidence cannot prove physical multi-host
placement, real network failure domains, a real DRBD/Ceph cluster or a real
second machine's TLS configuration. Those remain deployment qualifications,
not claims made by the test suite.

### 4. External TLS terminator

If a reverse proxy or load balancer terminates TLS, prefer binding
`volvisord` to loopback on the same host and forwarding from the proxy. That
keeps the daemon's own listener off the network and needs no downgrade flag.

If the proxy is on another host and the daemon must accept plaintext on a
private interface, set `allow_insecure = true` explicitly. The daemon warns at
startup:

- bearer tokens and volume metadata are visible on that network segment;
- application bearer authentication still applies, but transport
  confidentiality depends entirely on the external terminator;
- the terminator must strip untrusted inbound `Authorization` headers and
  enforce its own client authentication and access control;
- never enable this on a public or shared network.

## Credential and certificate rotation

There is no hot reload. Rotation is a controlled restart.

### API bearer tokens

The credential list is rotation-friendly because it is a list, but principal
ids must be unique. Use a new id for the new token epoch:

1. Add a new entry with a new id and the new token next to the current entry.
2. Restart the daemon and confirm `--check-config` is clean.
3. Roll clients to the new token.
4. Remove the old entry and restart again. The old token is then rejected.

If you use the legacy `admin_token`, generate a replacement, update the
configuration and restart; the old value is rejected after restart. A token
removed from the configuration is not retained as a fallback.

Rotation is fail-closed: invalid duplicate ids/tokens, an empty token, or a
token shared with `admin_token` refuse startup rather than choosing an
identity arbitrarily.

### TLS certificates

Replace the configured PEM files and restart the affected daemon. The daemon
re-reads the files at startup. There is no automatic certificate reload,
ACME client or private-key generation. Use owner-only permissions for key
files and configuration files containing tokens.

## Audit events

The audit record is a structured `tracing` event with `kind = "audit"`. It
carries:

- `principal`: credential id, `admin_token`, `peer`, or `anonymous`;
- `role`: `admin`, `operator`, `read-only`, `peer`, or `none`;
- `method` and the matched route pattern (for example
  `/v2/volumes/{volume_id}`, not the concrete id);
- `status` and `outcome` (`success`, `denied` or `failure`);
- `operation_id` when the journal pipeline attached one to the response.

Successful `/healthz` and `/v2/healthz` probes are not audited, to avoid
unbounded probe noise. Failed probes are audited. Denied requests are audited
with `outcome = "denied"` before the handler runs. No audit event contains a
bearer token, client private key or TLS key material.

Audit logs are ordinary daemon logs. Ship them to a protected sink if audit
retention matters; the daemon does not provide a tamper-evident ledger for
them.

## Operational checklist

- [ ] The listener is either loopback-only or TLS-enabled.
- [ ] `allow_insecure` is absent, or its network boundary and warning are
      understood and documented.
- [ ] Non-loopback daemons have scoped credentials or at least a legacy
      admin token; tokenless non-loopback startup is refused.
- [ ] Read routes are protected by scoped credentials on shared networks.
- [ ] Witness and peer URLs use `https://` and the certificate SAN matches the
      configured host.
- [ ] Private-CA trust is configured under `tls.client.ca_file`; mTLS is
      configured on both the server (`client_ca_file`) and the client
      (`cert_file` + `key_file`).
- [ ] Witness credentials are distinct per host, and the shared witness token
      is not used for mutations.
- [ ] TLS keys and configuration files containing tokens are owner-only and
      are not committed to source control.
- [ ] The audit sink is protected and retained.
- [ ] `[admission]` limits and timeouts are sized to provider capacity;
      `429`/`503` responses are monitored and clients honour `Retry-After`
      ([admission control](admission-control.md)).
- [ ] `--check-config` passes for every daemon, then a real TLS startup and
      request are exercised before production traffic.
