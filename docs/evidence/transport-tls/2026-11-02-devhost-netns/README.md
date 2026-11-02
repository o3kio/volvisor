# Transport/TLS evidence — same-host netns run (2026-11-02)

**What this is.** A recorded native-TLS run of `volvisord` from the
`feat/40-transport-auth` working tree (base commit `a6591d9`, binary
`a6591d9-dirty`), performed on the development host. It exercises the real
rustls listener with a private CA and captures the traffic on a veth pair.

**This is same-host evidence, not multi-host evidence.** Both network
namespaces (`vv-tls-a`, `vv-tls-b`) and both veth ends run on the same kernel
and physical host. There is no second machine, no real DRBD peer, no Ceph
cluster and no real network failure domain. The run proves the HTTPS
listener, rustls certificate verification and wire encryption in this
environment. It does **not** prove a cross-host deployment, a real third-host
witness or a production multi-host topology. Those criteria are not claimed
as run.

## Setup

- Server: `volvisord` in netns `vv-tls-a`, address `10.200.40.2:8443`,
  provider `fake` (no storage lab), scoped admin/operator/read-only
  credentials and `[tls] enabled = true`.
- Client: `curl 8.5.0` in netns `vv-tls-b`, address `10.200.40.1`.
- Capture: `tcpdump` on the `veth-b` side for TCP port 8443.
- Identity: a freshly generated private CA and an RSA server certificate with
  `subjectAltName = IP:10.200.40.2`.
- Kernel `6.8.0-142-generic`, OpenSSL `3.0.13`.

The exact reproduction script is [`run.sh`](run.sh). It creates and removes
its own namespaces and writes regenerated artifacts under
`/tmp/opencode/transport-tls-evidence`. The committed artifacts are the run's
copies; keys were generated in `/tmp` and are deliberately not committed.

## Results

| Check | Observation | Artifact |
|---|---|---|
| Trusted HTTPS request | `HTTP/1.1 200 OK`, body `{"volumes":[]}` with the read-only bearer token | [`curl-with-ca.headers`](curl-with-ca.headers), [`curl-with-ca.body`](curl-with-ca.body) |
| Server identity | certificate issued by the evidence CA, SAN `IP Address:10.200.40.2` | [`certificate.txt`](certificate.txt) |
| Untrusted client | curl exits `60`, `SSL certificate problem: unable to get local issuer certificate` | [`curl-untrusted-result.txt`](curl-untrusted-result.txt) |
| Insecure plain-HTTP listener | `--check-config` exits `1`: non-loopback plain-HTTP bind refused, recommends `[tls]` or `allow_insecure = true` | [`insecure-refusal.txt`](insecure-refusal.txt) |
| Wire capture | TLS handshake and encrypted application records; no token, `GET /v2/volumes` or `Authorization: Bearer` appears in the packet bytes | [`tls.pcap`](tls.pcap), [`tcpdump-summary.txt`](tcpdump-summary.txt), [`plaintext-scan.txt`](plaintext-scan.txt) |
| Audit/log secret handling | audit event names `reader-evidence` / `read-only` / `GET /v2/volumes` / `200`; the reader token does not appear in the daemon log | [`server.log`](server.log), [`log-scan.txt`](log-scan.txt) |
| Artifact hashes | SHA-256 of the pcap and certificates | [`sha256.txt`](sha256.txt) |

Key scan lines from [`plaintext-scan.txt`](plaintext-scan.txt):

```text
token scan: NOT FOUND (encrypted on the wire)
volume request path scan: NOT FOUND (encrypted on the wire)
bearer header scan: NOT FOUND (encrypted on the wire)
```

The audit event from [`server.log`](server.log) is the security attribution
record for the encrypted request:

```json
{"kind":"audit","principal":"reader-evidence","role":"read-only","method":"GET","route":"/v2/volumes","status":200,"outcome":"success","operation_id":""}
```

## What this does not cover

- A second physical host, a real DRBD/Ceph backend or a real third-host
  witness: **not run**, no lab available.
- mTLS at the HTTP daemon boundary: the TLS crate's mTLS tests
  (`crates/volvisor-tls/tests/tls_handshake.rs`) prove reject-without-cert and
  accept-with-cert; this packet capture uses a server-only certificate.
- Certificate rotation under live traffic, DNS/SAN failure modes, HSM/TPM key
  storage and external TLS terminators: **not run**.
- Performance, denial-of-service resistance and traffic analysis: **not
  measured**.

The evidence is intentionally scoped to what the same-host capture can show.
