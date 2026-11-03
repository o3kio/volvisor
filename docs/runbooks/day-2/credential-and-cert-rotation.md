# Credential and certificate rotation — day-2 runbook

**Status: DRILLED — restart-based. There is no hot reload.**
Evidence: [D7 raw log](../../evidence/day2/2026-11-03-devhost/raw/07-credential-and-cert-rotation.log),
[Report D7](../../evidence/day2/2026-11-03-devhost/REPORT.md#d7--credential-and-certificate-rotation).
Source `e551d26`, clean worktree, one host, 127.0.0.1, throwaway CA.

The drill proves the restart-based procedure. It does **not** prove live
certificate/token reload (there is no such feature), cross-host TLS, mTLS
client certificates, HSM/TPM key storage, or an external terminator. Those
remain **not run / not implemented as described**.

Normative reference:
[security deployment — credential and certificate rotation](../../security/deployment.md#credential-and-certificate-rotation).

## Scope and non-goals

This page covers:

- scoped API bearer-token rotation using the `[[credentials.admin]]` /
  `[[credentials.operator]]` / `[[credentials.read_only]]` lists;
- legacy `admin_token` replacement (single token, restart-based);
- same-host TLS server-certificate rotation by replacing the configured PEM
  files at stable paths and restarting;
- rollback of a bad rotation.

It does not cover issuing certificates from your PKI, distributing a new CA,
revoking at an OCSP/CRL endpoint, mTLS, or rotating witness/peer credentials
on a live multi-host fabric.

## RPO / RTO

- **RPO:** no control-plane state is changed by rotation. A bad rotation is a
  configuration/availability event, not a state-loss event.
- **RTO:** two controlled restarts for the overlap procedure (add + restart,
  remove + restart), plus client rollout. D7 measured each restart within the
  harness's 30 s health wait; it is not a zero-downtime rotation.
- **No hot reload:** replacing PEM bytes without a restart does not change the
  served certificate. D7 proved this explicitly. Plan the restart.

## Preconditions

- `--check-config` passes before every restart:

```sh
/usr/bin/volvisord --config /etc/volvisor/volvisor.toml --check-config
/usr/bin/volvisor-witnessd --config /etc/volvisor/witnessd.toml --check-config
```

Expected: exit 0 and `configuration check passed`.
- You have a maintenance window and a rollback copy of the config and PEM
  files.
- Tokens are generated with a CSPRNG, are unique, and are not reused across
  principals. The daemon refuses duplicate ids/tokens, empty tokens, and a
  token shared with `admin_token`.
- Secrets are never on a command line. Use an owner-only curl config or an
  environment variable read by a wrapper, and keep shell history clean.

## A. Rotate scoped API bearer tokens (overlap)

The list is rotation-friendly, but principal ids must be unique. Use a new id
for the new token epoch.

**Phase 1 — current state.** Config has the old admin and the operator:

```toml
listen = "127.0.0.1:8080"
journal_dir = "/var/lib/volvisor/journal"
provider = "lvm"

[[credentials.admin]]
id = "admin-2026-11-a"
token = "${OLD_ADMIN_TOKEN}"          # replace with the real value at deploy time

[[credentials.operator]]
id = "operator-2026-11-a"
token = "${OPERATOR_TOKEN}"
```

Validate, restart, and record the baseline:

```sh
/usr/bin/volvisord --config /etc/volvisor/volvisor.toml --check-config
systemctl restart volvisord.service
curl -fsS http://127.0.0.1:8080/healthz
```

Expected: config check exit 0; health `{"status":"ok"}`.

Use an owner-only curl config rather than `-H "Authorization: ..."` in
history:

```sh
umask 077
printf 'header = "Authorization: Bearer %s"\n' "$OLD_ADMIN_TOKEN" > /root/.volvisor-old.curl
chmod 600 /root/.volvisor-old.curl
curl -fsS --config /root/.volvisor-old.curl http://127.0.0.1:8080/v2/volumes >/dev/null && echo "old token works"
```

Expected: `old token works`. The new token must not work yet:

```sh
umask 077
printf 'header = "Authorization: Bearer %s"\n' "$NEW_ADMIN_TOKEN" > /root/.volvisor-new.curl
chmod 600 /root/.volvisor-new.curl
curl -sS -o /dev/null -w '%{http_code}\n' --config /root/.volvisor-new.curl \
  http://127.0.0.1:8080/v2/volumes
```

Expected: `401`.

**Phase 2 — add the new entry, restart, overlap.** Add a second admin entry
with a **new id** and the new token next to the old one. Do not reuse the id.

```toml
[[credentials.admin]]
id = "admin-2026-11-a"
token = "${OLD_ADMIN_TOKEN}"

[[credentials.admin]]
id = "admin-2026-11-b"
token = "${NEW_ADMIN_TOKEN}"
```

```sh
/usr/bin/volvisord --config /etc/volvisor/volvisor.toml --check-config
systemctl restart volvisord.service
curl -fsS http://127.0.0.1:8080/healthz
```

Expected: both tokens now mutate/read. D7 observed old 200, new 200, and both
could list volumes. Roll clients to the new token during this window. Do not
remove the old entry until every client is confirmed.

**Phase 3 — remove the old entry, restart, fail closed.** Delete the old admin
entry, keep only the new id:

```sh
/usr/bin/volvisord --config /etc/volvisor/volvisor.toml --check-config
systemctl restart volvisord.service
curl -sS -o /dev/null -w '%{http_code}\n' --config /root/.volvisor-old.curl \
  http://127.0.0.1:8080/v2/volumes
curl -sS -o /dev/null -w '%{http_code}\n' --config /root/.volvisor-new.curl \
  -H 'content-type: application/json' -d @/tmp/smoke-create.json \
  http://127.0.0.1:8080/v2/volumes
```

Expected: old token `401` for reads **and** mutations; new token `200`.
D7 observed exactly this; there is no fallback for a removed token.

**Legacy `admin_token`.** If you use the single legacy token, generate a
replacement, update the configuration, restart, and verify the old value is
rejected. It is one restart, not an overlap; plan for the outage or use scoped
credentials for a true overlap.

## B. Rotate a same-host TLS server certificate

The daemon reads the configured PEM files **at startup**. It does not watch
them. Rotation therefore requires a restart.

1. Prepare the new certificate/key under temporary names, then atomically
   replace the configured paths. Keep owner-only key permissions.

```sh
CERT_DIR=/etc/volvisor/tls
install -o root -g root -m 0644 new-server.crt "$CERT_DIR/server.crt.new"
install -o root -g root -m 0600 new-server.key "$CERT_DIR/server.key.new"
mv "$CERT_DIR/server.crt.new" "$CERT_DIR/server.crt"
mv "$CERT_DIR/server.key.new" "$CERT_DIR/server.key"
```

2. Record the old and new fingerprints before restarting:

```sh
openssl x509 -in "$CERT_DIR/server.crt" -noout -fingerprint -sha256
```

3. Prove there is no hot reload (optional but honest): the server still
   presents the old certificate after the file swap. D7 observed the old
   fingerprint unchanged:

```text
served fingerprint before rotation=7F:54:F9:...:D3:64
served fingerprint after file swap (no restart)=7F:54:F9:...:D3:64
```

4. Restart and verify the new certificate is served and trusted:

```sh
systemctl restart volvisord.service
openssl s_client -connect 127.0.0.1:8080 -servername localhost \
  -CAfile /etc/volvisor/tls/ca.crt </dev/null 2>/dev/null \
  | openssl x509 -noout -fingerprint -sha256
curl -fsS --cacert /etc/volvisor/tls/ca.crt https://127.0.0.1:8080/healthz
```

Expected: the new fingerprint, `Verification: OK` from `s_client`, and
`{"status":"ok"}`. D7 observed the cert2 fingerprint
`71:9E:40:...:17:35` after restart and a successful HTTPS create.

If the CA also changes, distribute the new trust bundle to every client
**before** the restart. A server restart with an untrusted new chain looks
like an outage even though the daemon is healthy on loopback.

5. Roll back a bad certificate by restoring the old PEM files and restarting.
   Keep the old files until the new chain is verified by every client class.

## Pass condition

- `--check-config` passes in every phase.
- The not-yet-issued token is 401; the old token works before rotation.
- Both tokens work during the overlap; the old token is 401 after removal;
  the new token still works.
- The served certificate matches the configured file after restart.
- A file swap **without** restart does not change the served certificate
  (no hot reload claim is made).
- The configured key is owner-only; no secret appears in logs or shell
  history.
- A rollback is a file restore + restart, not an in-place edit of a live
  process.

## What was drilled

D7 used a one-admin/one-operator config, added a second admin with a new id,
restarted, proved the overlap, removed the old entry, restarted, and proved
old 401 / new 200. For TLS it created a throwaway CA and two certs with
`subjectAltName=DNS:localhost,IP:127.0.0.1`, served cert1, swapped the PEM
bytes without restart (still cert1), restarted (cert2), verified the chain,
and created a volume over HTTPS. Raw:
[raw/07-credential-and-cert-rotation.log](../../evidence/day2/2026-11-03-devhost/raw/07-credential-and-cert-rotation.log).

**Limits:** single host; restart-based; no hot reload; no cross-host TLS,
mTLS, CA rotation/revocation, HSM/TPM, or external terminator. The CA and
private keys were throwaway and are not in the bundle.
